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
];

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
    pub check_command: Option<String>,
    pub allowed_domains: Vec<String>,
    pub allowed_read: Vec<String>,
    pub services: Vec<Service>,
    pub env: Vec<EnvVar>,
}

const PROJECT_COLUMNS: &str = "id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env";

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
    pub created_at: String,
    pub finished_at: Option<String>,
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

const RUN_COLUMNS: &str = "id, project_id, title, base_ref, request, workflow, language, source_issue, branch, pr_url, pr_number, created_at, finished_at";

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

    pub fn add_project(&self, project: &Project) -> Result<()> {
        self.lock().execute(
            "insert into project (id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            ],
        )?;
        Ok(())
    }

    pub fn update_project(&self, id: &str, settings: &ProjectSettings) -> Result<()> {
        self.lock().execute(
            "update project set github_account = ?2, check_command = ?3, allowed_domains = ?4, allowed_read = ?5, services = ?6, env = ?7 where id = ?1",
            params![
                id,
                settings.github_account,
                settings.check_command,
                serde_json::to_string(&settings.allowed_domains).unwrap_or_default(),
                serde_json::to_string(&settings.allowed_read).unwrap_or_default(),
                serde_json::to_string(&settings.services).unwrap_or_default(),
                serde_json::to_string(&settings.env).unwrap_or_default(),
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

    pub fn retire_decision(&self, seq: i64) -> Result<()> {
        self.lock()
            .execute("update decision set retired = 1 where seq = ?1", [seq])?;
        Ok(())
    }

    pub fn add_attempt(&self, a: &NewAttempt) -> Result<()> {
        self.lock().execute(
            "insert into attempt (id, run_id, step_id, number, state, model_requested, effort, tools, max_turns,
                max_budget_usd, language, prompt, system_prompt_sources, settings, session_id, resumed_from,
                workspace, raw_path, started_at, kind)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
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
                a.kind
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
        let mut stmt = conn.prepare("select * from attempt where run_id = ?1 order by number")?;
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
                    a.started_at, a.finished_at, a.last_activity_at, r.pr_url
             from run r
             join project p on p.id = r.project_id
             join attempt a on a.run_id = r.id
              and a.number = (select max(number) from attempt where run_id = r.id)
             order by r.created_at desc limit ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                title: r.get(1)?,
                pr_url: r.get(14)?,
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
                coalesce(sum(state in ('stalled', 'blocked', 'interrupted', 'waitingApproval')), 0),
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
}
