//! Local SQLite store: projects, runs, attempts and the audit log.
//!
//! `event` is append-only, and an attempt's identifying facts can't change after it starts.
//! Triggers enforce both, so a bug in the app can't rewrite history. This isn't protection
//! against someone who owns the machine: the raw stream file and its SHA-256 back it up.

use std::path::Path;
use std::sync::Mutex;

use keel_core::{AttemptState, EnvVar, Reason, Service};
pub use rusqlite::types as rusqlite_value;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Applied in order; `PRAGMA user_version` records how many ran. Only ever append.
const MIGRATIONS: &[&str] = &[
    r#"
    create table setting (key text primary key, value text not null);

    create table project (
        id text primary key,
        name text not null,
        path text not null unique,
        created_at text not null
    );

    create table run (
        id text primary key,
        project_id text not null,
        title text not null,
        base_ref text,
        created_at text not null
    );

    create table attempt (
        id text primary key,
        run_id text not null,
        step_id text not null,
        number integer not null,
        state text not null,
        reason text,
        model_requested text not null,
        model_actual text,
        effort text,
        tools text not null,
        max_turns integer,
        max_budget_usd real,
        language text not null,
        cli_version text,
        api_key_source text,
        account text,
        prompt text not null,
        system_prompt_sources text not null,
        settings text not null,
        session_id text not null,
        resumed_from text,
        pid integer,
        pid_started text,
        workspace text not null,
        raw_path text not null,
        raw_offset integer not null default 0,
        raw_sha256 text,
        diff_path text,
        sha_before text,
        sha_after text,
        files_changed text,
        result_text text,
        cost_usd real,
        input_tokens integer,
        output_tokens integer,
        turns integer,
        duration_ms integer,
        started_at text not null,
        finished_at text,
        last_activity_at text
    );
    create index attempt_by_run on attempt (run_id, number);
    create index attempt_by_state on attempt (state);

    create table event (
        seq integer primary key autoincrement,
        attempt_id text not null,
        at text not null,
        kind text not null,
        data text not null
    );
    create index event_by_attempt on event (attempt_id, seq);

    create trigger event_no_update before update on event
    begin select raise(abort, 'audit events are append-only'); end;
    create trigger event_no_delete before delete on event
    begin select raise(abort, 'audit events are append-only'); end;

    create trigger attempt_facts_fixed before update of
        run_id, step_id, number, model_requested, effort, tools, prompt, settings, session_id,
        resumed_from, workspace, raw_path, started_at on attempt
    begin select raise(abort, 'attempt facts are fixed once it starts'); end;
    create trigger attempt_no_delete before delete on attempt
    begin select raise(abort, 'attempts are never deleted'); end;
"#,
    r#"
    alter table project add column github_account text;
    alter table project add column github_repo text;
    alter table project add column check_command text;
    alter table project add column allowed_domains text not null default '[]';
    alter table project add column allowed_read text not null default '[]';

    alter table run add column request text not null default '';
    alter table run add column workflow text not null default '[]';
    alter table run add column language text not null default 'en';
    alter table run add column source_issue text;
    alter table run add column branch text;
    alter table run add column pr_url text;
    alter table run add column pr_number integer;
    alter table run add column finished_at text;

    alter table attempt add column kind text not null default 'agent';
    alter table attempt add column exit_code integer;
    create trigger attempt_kind_fixed before update of kind on attempt
    begin select raise(abort, 'attempt facts are fixed once it starts'); end;

    create table decision (
        seq integer primary key autoincrement,
        project_id text not null,
        run_id text not null,
        attempt_id text not null,
        at text not null,
        answer text not null,
        text text not null,
        retired integer not null default 0
    );
    create index decision_by_project on decision (project_id, seq);
    create trigger decision_facts_fixed before update of project_id, run_id, attempt_id, at, answer, text on decision
    begin select raise(abort, 'decisions are never rewritten'); end;
    create trigger decision_no_delete before delete on decision
    begin select raise(abort, 'decisions are never deleted'); end;
"#,
    r#"
    alter table project add column services text not null default '[]';
    alter table project add column env text not null default '[]';
"#,
    r#"
    alter table project add column setup_command text;
"#,
    r#"
    alter table run add column round integer not null default 0;
    alter table attempt add column round integer not null default 0;
    alter table attempt add column role text not null default 'worker';
    alter table attempt add column output text;
    alter table attempt add column run_files text;
    create trigger attempt_round_fixed before update of round, role on attempt
    begin select raise(abort, 'attempt facts are fixed once it starts'); end;
"#,
    r#"
    create table brain_note (
        seq integer primary key autoincrement,
        project_id text not null references project (id),
        kind text not null,
        state text not null,
        text text not null,
        evidence text,
        paths text not null,
        commit_sha text,
        run_id text not null references run (id),
        attempt_id text not null,
        created_at text not null,
        decided_at text
    );
    create index brain_note_by_project on brain_note (project_id, seq);
    create trigger brain_note_facts_fixed
    before update of project_id, kind, text, evidence, paths, commit_sha, run_id, attempt_id, created_at on brain_note
    begin select raise(abort, 'brain notes are never rewritten'); end;
    create trigger brain_note_no_delete before delete on brain_note
    begin select raise(abort, 'brain notes are never deleted'); end;
"#,
    // 7: MCP clients, what they called, and which client started a run.
    r#"
    alter table run add column started_by text;
    create table mcp_client (
        id text primary key,
        name text not null,
        token_hash blob not null unique,
        token_prefix text not null,
        can_run integer not null default 0,
        created_at text not null,
        last_seen_at text,
        revoked_at text
    );
    create table mcp_call (
        seq integer primary key autoincrement,
        client_id text not null references mcp_client (id),
        client_name text not null,
        tool text not null,
        args text not null,
        ok integer not null,
        error text,
        run_id text,
        at text not null,
        duration_ms integer not null
    );
    create index mcp_call_by_run on mcp_call (run_id);
    create trigger mcp_call_no_update before update on mcp_call
    begin select raise(abort, 'mcp calls are never rewritten'); end;
    create trigger mcp_call_no_delete before delete on mcp_call
    begin select raise(abort, 'mcp calls are never deleted'); end;
    create trigger mcp_client_no_delete before delete on mcp_client
    begin select raise(abort, 'mcp clients are revoked, not deleted'); end;
"#,
];

/// An assistant allowed to reach Keel over MCP. Its token's hash is never read back.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClient {
    pub id: String,
    pub name: String,
    /// The token's first characters, to tell tokens apart.
    pub token_prefix: String,
    /// May start and stop runs, and not only read.
    pub can_run: bool,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// One tool call by an MCP client, as it happened. Never rewritten.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCall {
    pub seq: i64,
    pub client_id: String,
    pub client_name: String,
    pub tool: String,
    pub args: String,
    pub ok: bool,
    pub error: Option<String>,
    pub run_id: Option<String>,
    pub at: String,
    pub duration_ms: i64,
}

#[derive(Debug, Clone)]
pub struct NewMcpCall<'a> {
    pub client: &'a McpClient,
    pub tool: &'a str,
    pub args: &'a str,
    pub error: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub duration_ms: i64,
}

const MCP_CLIENT_COLUMNS: &str =
    "id, name, token_prefix, can_run, created_at, last_seen_at, revoked_at";

fn mcp_client_from(r: &Row) -> rusqlite::Result<McpClient> {
    Ok(McpClient {
        id: r.get(0)?,
        name: r.get(1)?,
        token_prefix: r.get(2)?,
        can_run: r.get(3)?,
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        revoked_at: r.get(6)?,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
    /// The `gh` account this project uses. Keel never switches the active one.
    pub github_account: Option<String>,
    /// `owner/name`, when the repo's remote is on GitHub.
    pub github_repo: Option<String>,
    pub setup_command: Option<String>,
    pub check_command: Option<String>,
    pub allowed_domains: Vec<String>,
    pub allowed_read: Vec<String>,
    pub services: Vec<Service>,
    pub env: Vec<EnvVar>,
}

/// What a person can change about a project.
#[derive(Debug, Clone, Default)]
pub struct ProjectSettings {
    pub github_account: Option<String>,
    pub setup_command: Option<String>,
    pub check_command: Option<String>,
    pub allowed_domains: Vec<String>,
    pub allowed_read: Vec<String>,
    pub services: Vec<Service>,
    pub env: Vec<EnvVar>,
}

const PROJECT_COLUMNS: &str = "id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env, setup_command";

fn project_from(r: &Row) -> rusqlite::Result<Project> {
    let list = |text: String| serde_json::from_str(&text).unwrap_or_default();
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        path: r.get(2)?,
        created_at: r.get(3)?,
        github_account: r.get(4)?,
        github_repo: r.get(5)?,
        check_command: r.get(6)?,
        allowed_domains: list(r.get(7)?),
        allowed_read: list(r.get(8)?),
        services: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        env: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
        setup_command: r.get(11)?,
    })
}

/// A run with what it needs to continue. The workflow is a copy taken when it started.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub base_ref: Option<String>,
    pub request: String,
    pub workflow: serde_json::Value,
    pub language: String,
    pub source_issue: Option<serde_json::Value>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    /// How many times an approver has sent the work back.
    pub round: i64,
    pub created_at: String,
    pub finished_at: Option<String>,
    /// The MCP client that started it. None when a person did, in the app.
    pub started_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewRun {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub request: String,
    pub workflow: String,
    pub language: String,
    pub source_issue: Option<String>,
}

const RUN_COLUMNS: &str = "id, project_id, title, base_ref, request, workflow, language, source_issue, branch, pr_url, pr_number, created_at, finished_at, round, started_by";

fn run_from(r: &Row) -> rusqlite::Result<Run> {
    let source_issue: Option<String> = r.get(7)?;
    let workflow: String = r.get(5)?;
    Ok(Run {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        base_ref: r.get(3)?,
        request: r.get(4)?,
        workflow: serde_json::from_str(&workflow).unwrap_or_default(),
        language: r.get(6)?,
        source_issue: source_issue.and_then(|t| serde_json::from_str(&t).ok()),
        branch: r.get(8)?,
        pr_url: r.get(9)?,
        pr_number: r.get(10)?,
        created_at: r.get(11)?,
        finished_at: r.get(12)?,
        round: r.get(13)?,
        started_by: r.get(14)?,
    })
}

/// Something the Brain knows or could know. `kind` is `map` or `learning`. `state` is `proposed`,
/// `active`, `dismissed`, `retired` or `replaced`, and only the state ever changes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrainNote {
    pub seq: i64,
    pub project_id: String,
    pub kind: String,
    pub state: String,
    pub text: String,
    pub evidence: Option<String>,
    pub paths: Vec<String>,
    pub commit_sha: Option<String>,
    pub run_id: String,
    pub attempt_id: String,
    pub created_at: String,
    pub decided_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewBrainNote<'a> {
    pub project_id: &'a str,
    pub kind: &'a str,
    pub state: &'a str,
    pub text: &'a str,
    pub evidence: Option<&'a str>,
    pub paths: &'a [String],
    pub commit_sha: Option<&'a str>,
    pub run_id: &'a str,
    pub attempt_id: &'a str,
}

fn brain_note_from(r: &rusqlite::Row) -> rusqlite::Result<BrainNote> {
    let paths: String = r.get("paths")?;
    Ok(BrainNote {
        seq: r.get("seq")?,
        project_id: r.get("project_id")?,
        kind: r.get("kind")?,
        state: r.get("state")?,
        text: r.get("text")?,
        evidence: r.get("evidence")?,
        paths: serde_json::from_str(&paths).unwrap_or_default(),
        commit_sha: r.get("commit_sha")?,
        run_id: r.get("run_id")?,
        attempt_id: r.get("attempt_id")?,
        created_at: r.get("created_at")?,
        decided_at: r.get("decided_at")?,
    })
}

/// What a person said at an approval step. These reach later runs of the project.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub seq: i64,
    pub project_id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub at: String,
    pub answer: String,
    pub text: String,
    pub retired: bool,
}

#[derive(Debug, Clone)]
pub struct NewAttempt {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub kind: String,
    pub role: String,
    pub round: i64,
    pub number: i64,
    pub state: AttemptState,
    pub model_requested: String,
    pub effort: Option<String>,
    pub tools: String,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub language: String,
    pub prompt: String,
    pub system_prompt_sources: String,
    pub settings: String,
    pub session_id: String,
    pub resumed_from: Option<String>,
    pub workspace: String,
    pub raw_path: String,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub kind: String,
    pub role: String,
    pub round: i64,
    /// The structured answer of a reviewer or approver.
    pub output: Option<serde_json::Value>,
    /// What the whole run had changed when this step ended. A file added and later removed
    /// isn't here, because it won't be in the pull request.
    pub run_files: Option<serde_json::Value>,
    pub exit_code: Option<i64>,
    pub number: i64,
    pub state: String,
    pub reason: Option<serde_json::Value>,
    pub model_requested: String,
    pub model_actual: Option<String>,
    pub effort: Option<String>,
    pub tools: String,
    pub max_turns: Option<i64>,
    pub max_budget_usd: Option<f64>,
    pub language: String,
    pub cli_version: Option<String>,
    pub api_key_source: Option<String>,
    pub account: Option<String>,
    pub prompt: String,
    pub system_prompt_sources: serde_json::Value,
    pub settings: serde_json::Value,
    pub session_id: String,
    pub resumed_from: Option<String>,
    pub pid: Option<i64>,
    pub pid_started: Option<String>,
    pub workspace: String,
    pub raw_path: String,
    pub raw_offset: i64,
    pub raw_sha256: Option<String>,
    pub diff_path: Option<String>,
    pub sha_before: Option<String>,
    pub sha_after: Option<String>,
    pub files_changed: Option<serde_json::Value>,
    pub result_text: Option<String>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub turns: Option<i64>,
    pub duration_ms: Option<i64>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: i64,
    pub attempt_id: String,
    pub at: String,
    pub kind: String,
    pub data: serde_json::Value,
}

/// One row of the home table: a run and its latest attempt.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    pub id: String,
    pub title: String,
    pub pr_url: Option<String>,
    pub started_by: Option<String>,
    pub project_id: String,
    pub project_name: String,
    pub step_id: String,
    pub attempt_id: String,
    pub attempt_number: i64,
    pub state: String,
    pub reason: Option<serde_json::Value>,
    pub model: String,
    pub turns: Option<i64>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub last_activity_at: Option<String>,
}

/// The numbers on the home cards.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub running: i64,
    pub retrying: i64,
    pub blocked: i64,
    pub tokens: i64,
    pub duration_ms: i64,
    pub cost_usd: f64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn json_col(row: &Row, name: &str) -> rusqlite::Result<Option<serde_json::Value>> {
    let text: Option<String> = row.get(name)?;
    Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
}

fn attempt_from(row: &Row) -> rusqlite::Result<Attempt> {
    Ok(Attempt {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        step_id: row.get("step_id")?,
        kind: row.get("kind")?,
        role: row.get("role")?,
        round: row.get("round")?,
        output: json_col(row, "output")?,
        run_files: json_col(row, "run_files")?,
        exit_code: row.get("exit_code")?,
        number: row.get("number")?,
        state: row.get("state")?,
        reason: json_col(row, "reason")?,
        model_requested: row.get("model_requested")?,
        model_actual: row.get("model_actual")?,
        effort: row.get("effort")?,
        tools: row.get("tools")?,
        max_turns: row.get("max_turns")?,
        max_budget_usd: row.get("max_budget_usd")?,
        language: row.get("language")?,
        cli_version: row.get("cli_version")?,
        api_key_source: row.get("api_key_source")?,
        account: row.get("account")?,
        prompt: row.get("prompt")?,
        system_prompt_sources: json_col(row, "system_prompt_sources")?.unwrap_or_default(),
        settings: json_col(row, "settings")?.unwrap_or_default(),
        session_id: row.get("session_id")?,
        resumed_from: row.get("resumed_from")?,
        pid: row.get("pid")?,
        pid_started: row.get("pid_started")?,
        workspace: row.get("workspace")?,
        raw_path: row.get("raw_path")?,
        raw_offset: row.get("raw_offset")?,
        raw_sha256: row.get("raw_sha256")?,
        diff_path: row.get("diff_path")?,
        sha_before: row.get("sha_before")?,
        sha_after: row.get("sha_after")?,
        files_changed: json_col(row, "files_changed")?,
        result_text: row.get("result_text")?,
        cost_usd: row.get("cost_usd")?,
        input_tokens: row.get("input_tokens")?,
        output_tokens: row.get("output_tokens")?,
        turns: row.get("turns")?,
        duration_ms: row.get("duration_ms")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        last_activity_at: row.get("last_activity_at")?,
    })
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("pragma user_version", [], |r| r.get(0))?;
        let tx = conn.transaction()?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        tx.commit()?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("select value from setting where key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "insert into setting (key, value) values (?1, ?2) on conflict (key) do update set value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn mcp_clients(&self) -> Result<Vec<McpClient>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {MCP_CLIENT_COLUMNS} from mcp_client order by revoked_at is not null, created_at desc"
        ))?;
        let rows = stmt.query_map([], mcp_client_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mcp_client(&self, id: &str) -> Result<McpClient> {
        self.lock()
            .query_row(
                &format!("select {MCP_CLIENT_COLUMNS} from mcp_client where id = ?1"),
                [id],
                mcp_client_from,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("mcp client {id}")))
    }

    pub fn add_mcp_client(
        &self,
        id: &str,
        name: &str,
        token_hash: &[u8; 32],
        token_prefix: &str,
        can_run: bool,
    ) -> Result<McpClient> {
        self.lock().execute(
            "insert into mcp_client (id, name, token_hash, token_prefix, can_run, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, name, &token_hash[..], token_prefix, can_run, now()],
        )?;
        self.mcp_client(id)
    }

    /// The client this token belongs to, unless it was revoked. Read on every request, so
    /// revoking or changing a client takes effect at once. Records that it was seen.
    pub fn mcp_client_by_token(&self, token_hash: &[u8; 32]) -> Result<Option<McpClient>> {
        let conn = self.lock();
        let client = conn
            .query_row(
                &format!(
                    "select {MCP_CLIENT_COLUMNS} from mcp_client
                     where token_hash = ?1 and revoked_at is null"
                ),
                [&token_hash[..]],
                mcp_client_from,
            )
            .optional()?;
        if let Some(client) = &client {
            conn.execute(
                "update mcp_client set last_seen_at = ?2 where id = ?1",
                rusqlite::params![client.id, now()],
            )?;
        }
        Ok(client)
    }

    pub fn set_mcp_client_can_run(&self, id: &str, can_run: bool) -> Result<McpClient> {
        self.lock().execute(
            "update mcp_client set can_run = ?2 where id = ?1",
            rusqlite::params![id, can_run],
        )?;
        self.mcp_client(id)
    }

    pub fn revoke_mcp_client(&self, id: &str) -> Result<McpClient> {
        self.lock().execute(
            "update mcp_client set revoked_at = ?2 where id = ?1 and revoked_at is null",
            rusqlite::params![id, now()],
        )?;
        self.mcp_client(id)
    }

    pub fn add_mcp_call(&self, call: &NewMcpCall) -> Result<()> {
        self.lock().execute(
            "insert into mcp_call (client_id, client_name, tool, args, ok, error, run_id, at, duration_ms)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                call.client.id,
                call.client.name,
                call.tool,
                call.args,
                call.error.is_none(),
                call.error,
                call.run_id,
                now(),
                call.duration_ms,
            ],
        )?;
        Ok(())
    }

    /// The latest calls, newest first.
    pub fn mcp_calls(&self, limit: i64) -> Result<Vec<McpCall>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select seq, client_id, client_name, tool, args, ok, error, run_id, at, duration_ms
             from mcp_call order by seq desc limit ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(McpCall {
                seq: r.get(0)?,
                client_id: r.get(1)?,
                client_name: r.get(2)?,
                tool: r.get(3)?,
                args: r.get(4)?,
                ok: r.get(5)?,
                error: r.get(6)?,
                run_id: r.get(7)?,
                at: r.get(8)?,
                duration_ms: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Runs this client started that haven't finished.
    pub fn open_runs_of_mcp_client(&self, client_id: &str) -> Result<i64> {
        Ok(self.lock().query_row(
            "select count(distinct r.id) from run r
             join mcp_call c on c.run_id = r.id
             where c.client_id = ?1 and c.tool = 'start_run' and c.ok = 1 and r.finished_at is null",
            [client_id],
            |r| r.get(0),
        )?)
    }

    pub fn set_run_started_by(&self, run_id: &str, client_name: &str) -> Result<()> {
        self.lock().execute(
            "update run set started_by = ?2 where id = ?1",
            rusqlite::params![run_id, client_name],
        )?;
        Ok(())
    }

    pub fn add_project(&self, project: &Project) -> Result<()> {
        self.lock().execute(
            "insert into project (id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env, setup_command)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                project.id,
                project.name,
                project.path,
                project.created_at,
                project.github_account,
                project.github_repo,
                project.check_command,
                serde_json::to_string(&project.allowed_domains).unwrap_or_default(),
                serde_json::to_string(&project.allowed_read).unwrap_or_default(),
                serde_json::to_string(&project.services).unwrap_or_default(),
                serde_json::to_string(&project.env).unwrap_or_default(),
                project.setup_command,
            ],
        )?;
        Ok(())
    }

    pub fn update_project(&self, id: &str, settings: &ProjectSettings) -> Result<()> {
        self.lock().execute(
            "update project set github_account = ?2, check_command = ?3, allowed_domains = ?4, allowed_read = ?5, services = ?6, env = ?7, setup_command = ?8 where id = ?1",
            params![
                id,
                settings.github_account,
                settings.check_command,
                serde_json::to_string(&settings.allowed_domains).unwrap_or_default(),
                serde_json::to_string(&settings.allowed_read).unwrap_or_default(),
                serde_json::to_string(&settings.services).unwrap_or_default(),
                serde_json::to_string(&settings.env).unwrap_or_default(),
                settings.setup_command,
            ],
        )?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {PROJECT_COLUMNS} from project order by name"
        ))?;
        let rows = stmt.query_map([], project_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, id: &str) -> Result<Project> {
        self.lock()
            .query_row(
                &format!("select {PROJECT_COLUMNS} from project where id = ?1"),
                [id],
                project_from,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("project {id}")))
    }

    pub fn project_by_path(&self, path: &str) -> Result<Option<Project>> {
        Ok(self
            .lock()
            .query_row(
                &format!("select {PROJECT_COLUMNS} from project where path = ?1"),
                [path],
                project_from,
            )
            .optional()?)
    }

    pub fn add_run(&self, run: &NewRun) -> Result<()> {
        self.lock().execute(
            "insert into run (id, project_id, title, request, workflow, language, source_issue, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![run.id, run.project_id, run.title, run.request, run.workflow, run.language, run.source_issue, now()],
        )?;
        Ok(())
    }

    pub fn run(&self, id: &str) -> Result<Run> {
        self.lock()
            .query_row(
                &format!("select {RUN_COLUMNS} from run where id = ?1"),
                [id],
                run_from,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("run {id}")))
    }

    /// Runs that still have steps ahead.
    pub fn open_runs(&self) -> Result<Vec<Run>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {RUN_COLUMNS} from run where finished_at is null order by created_at"
        ))?;
        let rows = stmt.query_map([], run_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn patch_run(&self, id: &str, fields: &[(&'static str, Value)]) -> Result<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let assignments: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (col, _))| format!("{col} = ?{}", i + 2))
            .collect();
        let sql = format!("update run set {} where id = ?1", assignments.join(", "));
        let mut values = vec![Value::Text(id.to_owned())];
        values.extend(fields.iter().map(|(_, v)| v.clone()));
        self.lock().execute(&sql, params_from_iter(values))?;
        Ok(())
    }

    pub fn project_of_run(&self, run_id: &str) -> Result<String> {
        self.lock()
            .query_row("select project_id from run where id = ?1", [run_id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("run {run_id}")))
    }

    pub fn add_decision(
        &self,
        project_id: &str,
        run_id: &str,
        attempt_id: &str,
        answer: &str,
        text: &str,
    ) -> Result<()> {
        self.lock().execute(
            "insert into decision (project_id, run_id, attempt_id, at, answer, text) values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![project_id, run_id, attempt_id, now(), answer, text],
        )?;
        Ok(())
    }

    /// A project's decisions still in force, newest first.
    pub fn decisions(&self, project_id: &str) -> Result<Vec<Decision>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select seq, project_id, run_id, attempt_id, at, answer, text, retired from decision
             where project_id = ?1 and retired = 0 order by seq desc",
        )?;
        let rows = stmt.query_map([project_id], |r| {
            Ok(Decision {
                seq: r.get(0)?,
                project_id: r.get(1)?,
                run_id: r.get(2)?,
                attempt_id: r.get(3)?,
                at: r.get(4)?,
                answer: r.get(5)?,
                text: r.get(6)?,
                retired: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_brain_note(&self, note: &NewBrainNote) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "insert into brain_note (project_id, kind, state, text, evidence, paths, commit_sha, run_id, attempt_id, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                note.project_id,
                note.kind,
                note.state,
                note.text,
                note.evidence,
                serde_json::to_string(note.paths).unwrap_or_else(|_| "[]".to_owned()),
                note.commit_sha,
                note.run_id,
                note.attempt_id,
                now()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// A project's notes still in play, oldest first: the active ones and the proposals.
    pub fn brain_notes(&self, project_id: &str) -> Result<Vec<BrainNote>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select * from brain_note where project_id = ?1 and state in ('active', 'proposed') order by seq",
        )?;
        let rows = stmt.query_map([project_id], brain_note_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn brain_note(&self, seq: i64) -> Result<BrainNote> {
        self.lock()
            .query_row(
                "select * from brain_note where seq = ?1",
                [seq],
                brain_note_from,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("brain note {seq}")))
    }

    pub fn proposals_of_run(&self, run_id: &str) -> Result<Vec<BrainNote>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select * from brain_note where run_id = ?1 and state = 'proposed' order by seq",
        )?;
        let rows = stmt.query_map([run_id], brain_note_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_brain_note_state(&self, seq: i64, state: &str) -> Result<()> {
        self.lock().execute(
            "update brain_note set state = ?2, decided_at = ?3 where seq = ?1",
            params![seq, state, now()],
        )?;
        Ok(())
    }

    /// Proposals waiting for a person, across all projects.
    pub fn pending_proposals(&self) -> Result<i64> {
        Ok(self.lock().query_row(
            "select count(*) from brain_note where state = 'proposed'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Whether anything has ever tried to map this project.
    pub fn has_map_attempt(&self, project_id: &str) -> Result<bool> {
        Ok(self.lock().query_row(
            "select exists (select 1 from attempt a join run r on r.id = a.run_id
                            where r.project_id = ?1 and a.role = 'mapper')",
            [project_id],
            |r| r.get(0),
        )?)
    }

    pub fn retire_decision(&self, seq: i64) -> Result<()> {
        self.lock()
            .execute("update decision set retired = 1 where seq = ?1", [seq])?;
        Ok(())
    }

    pub fn add_attempt(&self, a: &NewAttempt) -> Result<()> {
        self.lock().execute(
            "insert into attempt (id, run_id, step_id, number, state, model_requested, effort, tools, max_turns,
                max_budget_usd, language, prompt, system_prompt_sources, settings, session_id, resumed_from,
                workspace, raw_path, started_at, kind, role, round)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            params![
                a.id,
                a.run_id,
                a.step_id,
                a.number,
                a.state.as_str(),
                a.model_requested,
                a.effort,
                a.tools,
                a.max_turns,
                a.max_budget_usd,
                a.language,
                a.prompt,
                a.system_prompt_sources,
                a.settings,
                a.session_id,
                a.resumed_from,
                a.workspace,
                a.raw_path,
                a.started_at,
                a.kind,
                a.role,
                a.round
            ],
        )?;
        Ok(())
    }

    pub fn set_state(&self, id: &str, state: AttemptState, reason: Option<&Reason>) -> Result<()> {
        let reason = reason.map(|r| serde_json::to_string(r).unwrap_or_default());
        self.lock().execute(
            "update attempt set state = ?2, reason = ?3 where id = ?1",
            params![id, state.as_str(), reason],
        )?;
        Ok(())
    }

    /// Updates progress columns. Column names come from this crate's callers, never from data.
    pub fn patch_attempt(&self, id: &str, fields: &[(&'static str, Value)]) -> Result<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let assignments: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (col, _))| format!("{col} = ?{}", i + 2))
            .collect();
        let sql = format!(
            "update attempt set {} where id = ?1",
            assignments.join(", ")
        );
        let mut values = vec![Value::Text(id.to_owned())];
        values.extend(fields.iter().map(|(_, v)| v.clone()));
        self.lock().execute(&sql, params_from_iter(values))?;
        Ok(())
    }

    pub fn attempt(&self, id: &str) -> Result<Attempt> {
        self.lock()
            .query_row("select * from attempt where id = ?1", [id], attempt_from)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("attempt {id}")))
    }

    pub fn attempts_of_run(&self, run_id: &str) -> Result<Vec<Attempt>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("select * from attempt where run_id = ?1 order by rowid")?;
        let rows = stmt.query_map([run_id], attempt_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn active_attempts(&self) -> Result<Vec<Attempt>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select * from attempt where state in ('queued', 'preparing', 'running', 'stalled') order by started_at",
        )?;
        let rows = stmt.query_map([], attempt_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn append_event(
        &self,
        attempt_id: &str,
        kind: &str,
        data: &serde_json::Value,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "insert into event (attempt_id, at, kind, data) values (?1, ?2, ?3, ?4)",
            params![attempt_id, now(), kind, data.to_string()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn events(&self, attempt_id: &str, after_seq: i64) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select seq, attempt_id, at, kind, data from event where attempt_id = ?1 and seq > ?2 order by seq",
        )?;
        let rows = stmt.query_map(params![attempt_id, after_seq], |r| {
            let data: String = r.get(4)?;
            Ok(Event {
                seq: r.get(0)?,
                attempt_id: r.get(1)?,
                at: r.get(2)?,
                kind: r.get(3)?,
                data: serde_json::from_str(&data).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Runs, newest first, each with its latest attempt.
    pub fn runs(&self, limit: i64) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select r.id, r.title, r.project_id, p.name as project_name, a.step_id, a.id as attempt_id, a.number,
                    a.state, a.reason, coalesce(a.model_actual, a.model_requested) as model, a.turns,
                    a.started_at, a.finished_at, a.last_activity_at, r.pr_url, r.started_by
             from run r
             join project p on p.id = r.project_id
             join attempt a on a.run_id = r.id
              and a.rowid = (
                select b.rowid from attempt b where b.run_id = r.id
                  and not exists (select 1 from attempt later
                                  where later.run_id = b.run_id and later.step_id = b.step_id and later.rowid > b.rowid)
                order by case when b.role = 'learner' and b.state not in ('queued', 'preparing', 'running') then 3
                              when b.state in ('stalled', 'blocked', 'waitingApproval', 'interrupted', 'failed', 'stopped', 'rejected', 'abandoned') then 0
                              when b.state in ('queued', 'preparing', 'running') then 1 else 2 end,
                         b.rowid desc
                limit 1)
             order by r.created_at desc limit ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                title: r.get(1)?,
                pr_url: r.get(14)?,
                started_by: r.get(15)?,
                project_id: r.get(2)?,
                project_name: r.get(3)?,
                step_id: r.get(4)?,
                attempt_id: r.get(5)?,
                attempt_number: r.get(6)?,
                state: r.get(7)?,
                reason: json_col(r, "reason")?,
                model: r.get(9)?,
                turns: r.get(10)?,
                started_at: r.get(11)?,
                finished_at: r.get(12)?,
                last_activity_at: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn totals(&self) -> Result<Totals> {
        Ok(self.lock().query_row(
            "select
                coalesce(sum(state in ('preparing', 'running') and number = 1), 0),
                coalesce(sum(state in ('preparing', 'running') and number > 1), 0),
                coalesce(sum(state in ('stalled', 'blocked', 'interrupted', 'waitingApproval')
                    and not exists (select 1 from attempt later
                                    where later.run_id = attempt.run_id and later.step_id = attempt.step_id and later.rowid > attempt.rowid)), 0),
                coalesce(sum(coalesce(input_tokens, 0) + coalesce(output_tokens, 0)), 0),
                coalesce(sum(coalesce(duration_ms, 0)), 0),
                coalesce(sum(coalesce(cost_usd, 0)), 0)
             from attempt",
            [],
            |r| {
                Ok(Totals {
                    running: r.get(0)?,
                    retrying: r.get(1)?,
                    blocked: r.get(2)?,
                    tokens: r.get(3)?,
                    duration_ms: r.get(4)?,
                    cost_usd: r.get(5)?,
                })
            },
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("keel.db")).unwrap();
        (dir, store)
    }

    fn seeded(store: &Store) -> String {
        store
            .add_project(&Project {
                id: "p".into(),
                name: "demo".into(),
                path: "/demo".into(),
                created_at: now(),
                github_account: None,
                github_repo: None,
                setup_command: None,
                check_command: None,
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
            })
            .unwrap();
        store
            .add_run(&NewRun {
                id: "r".into(),
                project_id: "p".into(),
                title: "fix it".into(),
                request: "fix it".into(),
                workflow: "[]".into(),
                language: "en".into(),
                source_issue: None,
            })
            .unwrap();
        store
            .add_attempt(&NewAttempt {
                id: "a".into(),
                run_id: "r".into(),
                step_id: "builder".into(),
                kind: "agent".into(),
                role: "worker".into(),
                round: 0,
                number: 1,
                state: AttemptState::Running,
                model_requested: "haiku".into(),
                effort: None,
                tools: "build".into(),
                max_turns: None,
                max_budget_usd: None,
                language: "en".into(),
                prompt: "do it".into(),
                system_prompt_sources: "[]".into(),
                settings: "{}".into(),
                session_id: "s".into(),
                resumed_from: None,
                workspace: "/w".into(),
                raw_path: "/w.jsonl".into(),
                started_at: now(),
            })
            .unwrap();
        "a".into()
    }

    #[test]
    fn events_cannot_be_edited_or_deleted() {
        let (_dir, store) = store();
        let id = seeded(&store);
        let seq = store
            .append_event(&id, "note", &serde_json::json!({"a": 1}))
            .unwrap();
        let conn = store.lock();
        let updated = conn.execute("update event set kind = 'x' where seq = ?1", [seq]);
        assert!(updated.unwrap_err().to_string().contains("append-only"));
        let deleted = conn.execute("delete from event where seq = ?1", [seq]);
        assert!(deleted.unwrap_err().to_string().contains("append-only"));
    }

    #[test]
    fn attempt_facts_are_fixed_but_progress_is_not() {
        let (_dir, store) = store();
        let id = seeded(&store);
        let conn = store.lock();
        for sql in [
            "update attempt set prompt = 'other' where id = 'a'",
            "update attempt set model_requested = 'opus' where id = 'a'",
            "update attempt set settings = '{}' where id = 'a'",
            "delete from attempt where id = 'a'",
        ] {
            assert!(conn.execute(sql, []).is_err(), "{sql} should be refused");
        }
        drop(conn);
        store
            .patch_attempt(&id, &[("turns", Value::Integer(3))])
            .unwrap();
        store.set_state(&id, AttemptState::Succeeded, None).unwrap();
        let attempt = store.attempt(&id).unwrap();
        assert_eq!(attempt.turns, Some(3));
        assert_eq!(attempt.state, "succeeded");
    }

    #[test]
    fn home_table_and_cards_follow_the_latest_attempt() {
        let (_dir, store) = store();
        let id = seeded(&store);
        store
            .patch_attempt(
                &id,
                &[
                    ("input_tokens", Value::Integer(10)),
                    ("output_tokens", Value::Integer(5)),
                ],
            )
            .unwrap();
        let rows = store.runs(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "running");
        assert_eq!(rows[0].model, "haiku");
        let totals = store.totals().unwrap();
        assert_eq!((totals.running, totals.tokens), (1, 15));
    }

    #[test]
    fn a_database_from_before_reviews_opens_with_its_runs_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        // The version people have on their machines: everything up to the services and setup.
        {
            let store = Store::open(&path).unwrap();
            seeded(&store);
        }
        let run_row = {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "pragma user_version = 4;
                 drop table mcp_call;
                 drop table mcp_client;
                 alter table run drop column started_by;
                 drop table brain_note;
                 drop trigger attempt_round_fixed;
                 alter table run drop column round;
                 alter table attempt drop column round;
                 alter table attempt drop column role;
                 alter table attempt drop column output;
                 alter table attempt drop column run_files;",
            )
            .unwrap();
            conn.query_row("select id from run limit 1", [], |r| r.get::<_, String>(0))
                .unwrap()
        };

        let store = Store::open(&path).unwrap();
        let attempts = store.attempts_of_run(&run_row).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].role, "worker");
        assert_eq!(attempts[0].round, 0);
        assert_eq!(attempts[0].output, None);
        assert_eq!(store.run(&run_row).unwrap().round, 0);
        assert_eq!(store.runs(10).unwrap().len(), 1);
    }

    #[test]
    fn brain_notes_can_change_state_but_never_what_they_say() {
        let (_dir, store) = store();
        seeded(&store);
        let paths = vec!["a.py".to_owned()];
        let seq = store
            .add_brain_note(&NewBrainNote {
                project_id: "p",
                kind: "learning",
                state: "proposed",
                text: "the db needs a socket",
                evidence: Some("the check failed without it"),
                paths: &paths,
                commit_sha: Some("abc"),
                run_id: "r",
                attempt_id: "a",
            })
            .unwrap();
        assert_eq!(store.pending_proposals().unwrap(), 1);
        assert_eq!(store.proposals_of_run("r").unwrap().len(), 1);

        store.set_brain_note_state(seq, "active").unwrap();
        let note = store.brain_note(seq).unwrap();
        assert_eq!(note.state, "active");
        assert!(note.decided_at.is_some());
        assert_eq!(note.paths, paths);
        assert_eq!(store.pending_proposals().unwrap(), 0);

        store.set_brain_note_state(seq, "retired").unwrap();
        assert!(
            store.brain_notes("p").unwrap().is_empty(),
            "retired notes leave play"
        );
        assert_eq!(
            store.brain_note(seq).unwrap().text,
            "the db needs a socket",
            "but stay on record"
        );

        let conn = store.lock();
        assert!(
            conn.execute("update brain_note set text = 'x' where seq = ?1", [seq])
                .is_err()
        );
        assert!(
            conn.execute("delete from brain_note where seq = ?1", [seq])
                .is_err()
        );
    }

    #[test]
    fn a_failed_proposal_step_is_not_what_the_home_table_shows() {
        let (_dir, store) = store();
        seeded(&store);
        store.set_state("a", AttemptState::Succeeded, None).unwrap();
        let mut learner = NewAttempt {
            id: "l".into(),
            run_id: "r".into(),
            step_id: "learnings".into(),
            kind: "agent".into(),
            role: "learner".into(),
            round: 0,
            number: 1,
            state: AttemptState::Failed,
            model_requested: "haiku".into(),
            effort: None,
            tools: "readOnly".into(),
            max_turns: None,
            max_budget_usd: None,
            language: "en".into(),
            prompt: String::new(),
            system_prompt_sources: "[]".into(),
            settings: "{}".into(),
            session_id: "s".into(),
            resumed_from: None,
            workspace: "/w".into(),
            raw_path: "/w.jsonl".into(),
            started_at: now(),
        };
        store.add_attempt(&learner).unwrap();
        assert_eq!(store.runs(10).unwrap()[0].state, "succeeded");
        learner.id = "l2".into();
        learner.step_id = "other".into();
        learner.state = AttemptState::Running;
        store.add_attempt(&learner).unwrap();
        assert_eq!(
            store.runs(10).unwrap()[0].state,
            "running",
            "while it works, it does show"
        );
    }

    #[test]
    fn a_workflow_shows_once_on_the_home_table_at_the_step_that_matters() {
        let (_dir, store) = store();
        seeded(&store);
        let attempt = |id: &str, step: &str, state: AttemptState| NewAttempt {
            id: id.into(),
            run_id: "r".into(),
            step_id: step.into(),
            kind: "agent".into(),
            role: "worker".into(),
            round: 0,
            number: 1,
            state,
            model_requested: "haiku".into(),
            effort: None,
            tools: "readOnly".into(),
            max_turns: None,
            max_budget_usd: None,
            language: "en".into(),
            prompt: String::new(),
            system_prompt_sources: "[]".into(),
            settings: "{}".into(),
            session_id: id.into(),
            resumed_from: None,
            workspace: "/w".into(),
            raw_path: "/w.jsonl".into(),
            started_at: now(),
        };
        store.set_state("a", AttemptState::Succeeded, None).unwrap();
        store
            .add_attempt(&attempt("b", "ux", AttemptState::Succeeded))
            .unwrap();
        store
            .add_attempt(&attempt("c", "security", AttemptState::Running))
            .unwrap();
        let rows = store.runs(10).unwrap();
        assert_eq!(rows.len(), 1, "one row per run");
        assert_eq!(rows[0].step_id, "security");
        // Something that needs a person outranks what's still running.
        store
            .add_attempt(&attempt("d", "code", AttemptState::Blocked))
            .unwrap();
        assert_eq!(store.runs(10).unwrap()[0].step_id, "code");
        assert_eq!(store.runs(10).unwrap()[0].state, "blocked");
        assert_eq!(store.totals().unwrap().blocked, 1);
        // Unless it's been retried, and then it's history.
        store
            .add_attempt(&attempt("e", "code", AttemptState::Running))
            .unwrap();
        assert_eq!(store.runs(10).unwrap()[0].state, "running");
        assert_eq!(store.totals().unwrap().blocked, 0);
    }

    #[test]
    fn decisions_can_be_retired_but_never_rewritten() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .add_decision("p", "r", "a", "reject", "use the existing helper")
            .unwrap();
        store
            .add_decision("p", "r", "a", "approve", "ok, but keep it small")
            .unwrap();
        let decisions = store.decisions("p").unwrap();
        assert_eq!(decisions[0].text, "ok, but keep it small");
        assert_eq!(decisions.len(), 2);
        {
            let conn = store.lock();
            assert!(
                conn.execute("update decision set text = 'other'", [])
                    .is_err()
            );
            assert!(conn.execute("delete from decision", []).is_err());
        }
        store.retire_decision(decisions[0].seq).unwrap();
        assert_eq!(store.decisions("p").unwrap().len(), 1);
    }

    #[test]
    fn a_run_keeps_its_workflow_and_its_pull_request() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .patch_run(
                "r",
                &[
                    (
                        "pr_url",
                        Value::Text("https://github.com/o/r/pull/7".into()),
                    ),
                    ("pr_number", Value::Integer(7)),
                ],
            )
            .unwrap();
        let run = store.run("r").unwrap();
        assert_eq!(run.pr_number, Some(7));
        assert_eq!(
            store.runs(10).unwrap()[0].pr_url.as_deref(),
            Some("https://github.com/o/r/pull/7")
        );
        assert_eq!(store.open_runs().unwrap().len(), 1);
        store
            .patch_run("r", &[("finished_at", Value::Text(now()))])
            .unwrap();
        assert!(store.open_runs().unwrap().is_empty());
    }

    #[test]
    fn project_settings_round_trip() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .update_project(
                "p",
                &ProjectSettings {
                    github_account: Some("octo".into()),
                    setup_command: Some("uv sync".into()),
                    check_command: Some("make test".into()),
                    allowed_domains: vec!["pypi.org".into()],
                    allowed_read: vec!["~/.cargo".into()],
                    services: vec![Service::Bridge {
                        name: "db".into(),
                        port: 5432,
                        socket: None,
                    }],
                    env: vec![EnvVar {
                        name: "DB".into(),
                        value: "{{socket:db}}".into(),
                    }],
                },
            )
            .unwrap();
        let project = store.project("p").unwrap();
        assert_eq!(project.github_account.as_deref(), Some("octo"));
        assert_eq!(project.setup_command.as_deref(), Some("uv sync"));
        assert_eq!(project.check_command.as_deref(), Some("make test"));
        assert_eq!(project.allowed_domains, ["pypi.org"]);
        assert_eq!(project.allowed_read, ["~/.cargo"]);
        assert_eq!(project.services[0].name(), "db");
        assert_eq!(project.env[0].name, "DB");
    }

    #[test]
    fn reopening_keeps_data_and_does_not_rerun_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        {
            let store = Store::open(&path).unwrap();
            store.set_setting("language", "es").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.setting("language").unwrap().as_deref(), Some("es"));
    }

    #[test]
    fn an_mcp_client_is_found_by_its_token_until_it_is_revoked() {
        let (_dir, store) = store();
        let hash = [7u8; 32];
        let client = store
            .add_mcp_client("c1", "Claude Code", &hash, "keel_abc", false)
            .unwrap();
        assert!(!client.can_run);
        assert_eq!(client.last_seen_at, None);

        let found = store.mcp_client_by_token(&hash).unwrap().unwrap();
        assert_eq!(found.id, "c1");
        assert!(store.mcp_client("c1").unwrap().last_seen_at.is_some());
        assert_eq!(store.mcp_client_by_token(&[8u8; 32]).unwrap(), None);

        assert!(store.set_mcp_client_can_run("c1", true).unwrap().can_run);
        let revoked = store.revoke_mcp_client("c1").unwrap();
        assert!(revoked.revoked_at.is_some());
        assert_eq!(store.mcp_client_by_token(&hash).unwrap(), None);
        assert_eq!(store.mcp_clients().unwrap().len(), 1);
    }

    #[test]
    fn what_an_mcp_client_did_cannot_be_rewritten_or_deleted() {
        let (_dir, store) = store();
        let client = store
            .add_mcp_client("c1", "Claude Code", &[7u8; 32], "keel_abc", true)
            .unwrap();
        store
            .add_mcp_call(&NewMcpCall {
                client: &client,
                tool: "list_projects",
                args: "{}",
                error: None,
                run_id: None,
                duration_ms: 3,
            })
            .unwrap();
        store
            .add_mcp_call(&NewMcpCall {
                client: &client,
                tool: "start_run",
                args: "{}",
                error: Some("no"),
                run_id: None,
                duration_ms: 1,
            })
            .unwrap();
        let calls = store.mcp_calls(10).unwrap();
        assert_eq!(calls[0].tool, "start_run");
        assert!(!calls[0].ok);
        assert!(calls[1].ok);

        let conn = store.lock();
        assert!(conn.execute("update mcp_call set ok = 1", []).is_err());
        assert!(conn.execute("delete from mcp_call", []).is_err());
        assert!(conn.execute("delete from mcp_client", []).is_err());
    }
}
