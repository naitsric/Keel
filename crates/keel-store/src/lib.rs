//! Local SQLite store: projects, runs, attempts and the audit log.
//!
//! `event` is append-only, and an attempt's identifying facts can't change after it starts.
//! Triggers enforce both, so a bug in the app can't rewrite history. This isn't protection
//! against someone who owns the machine: the raw stream file and its SHA-256 back it up.

use std::path::Path;
use std::sync::Mutex;

use keel_core::{
    AttemptState, BoardColumn, CardLane, EnvVar, PrState, Reason, Service, card_column,
};
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
    // 8: Cards on the board, and the workflow they start with.
    r#"
    create table card (
        id text primary key,
        project_id text not null references project (id),
        title text not null,
        body text not null default '',
        issue text,
        issue_number integer,
        lane text not null,
        position integer not null,
        language text,
        todo_at text,
        run_id text references run (id),
        start_blocked text,
        created_at text not null
    );
    create index card_by_project on card (project_id, lane, position);
    create unique index card_issue_once on card (project_id, issue_number) where issue_number is not null;
    alter table project add column default_workflow text;
"#,
    // 9: Removing a project only hides it. Its runs, audit and Brain stay, and adding its folder
    // again brings it back.
    r#"
    alter table project add column removed_at text;
"#,
    // 10: Which workflow a run used, since its copy only holds the steps, and what became of its
    // pull request as of the last time Keel looked. Older runs have nulls.
    r#"
    alter table run add column workflow_id text;
    alter table run add column workflow_name text;
    alter table run add column pr_state text;
    alter table run add column pr_state_at text;
    alter table run add column pr_checked_at text;
"#,
    // 11: What a person was asked, on a decision that came from answering it. Older ones have null.
    r#"
    alter table decision add column question text;
    create trigger decision_question_fixed before update of question on decision
    begin select raise(abort, 'decisions are never rewritten'); end;
"#,
];

/// The attempt that says where a run stands, for a query that calls its run `r`. Of each step's
/// latest attempt: how the run was ended, then whatever needs a person, then what's running, then
/// the newest. The newest alone isn't enough: in a group that runs at once, the last one to start
/// can succeed while another failed. A proposal step that's over never decides.
const ATTEMPT_THAT_MATTERS: &str = "select b.rowid from attempt b where b.run_id = r.id
    and not exists (select 1 from attempt later
                    where later.run_id = b.run_id and later.step_id = b.step_id and later.rowid > b.rowid)
    order by case when b.role = 'learner' and b.state not in ('queued', 'preparing', 'running', 'stalled') then 4
                  when b.state in ('rejected', 'abandoned') then 0
                  when b.state in ('stalled', 'blocked', 'waitingApproval', 'waitingAnswers', 'interrupted', 'failed', 'stopped') then 1
                  when b.state in ('queued', 'preparing', 'running') then 2 else 3 end,
             b.rowid desc
    limit 1";

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
    /// The workflow its cards start with. `None` means the full one.
    pub default_workflow: Option<String>,
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
    pub default_workflow: Option<String>,
}

const PROJECT_COLUMNS: &str = "id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env, setup_command, default_workflow";

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
        default_workflow: r.get(12)?,
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
    /// The saved workflow its steps came from, as it was called then. None for a run that had no
    /// saved workflow, or from before Keel kept this.
    pub workflow_id: Option<String>,
    pub workflow_name: Option<String>,
    /// What became of its pull request, as of `pr_checked_at`. None until Keel knows.
    pub pr_state: Option<PrState>,
    /// When that happened, going by GitHub.
    pub pr_state_at: Option<String>,
    pub pr_checked_at: Option<String>,
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
    pub workflow_id: Option<String>,
    pub workflow_name: Option<String>,
}

const RUN_COLUMNS: &str = "id, project_id, title, base_ref, request, workflow, language, source_issue, branch, pr_url, pr_number, created_at, finished_at, round, started_by, workflow_id, workflow_name, pr_state, pr_state_at, pr_checked_at";

fn pr_state_from(text: Option<String>) -> Option<PrState> {
    text.as_deref().and_then(PrState::parse)
}

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
        workflow_id: r.get(15)?,
        workflow_name: r.get(16)?,
        pr_state: pr_state_from(r.get(17)?),
        pr_state_at: r.get(18)?,
        pr_checked_at: r.get(19)?,
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

/// What a person said at an approval step, or answered and asked to remember. These reach later
/// runs of the project. `answer` is `approve`, `reject` or `answered`.
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
    /// What they were asked, when `answer` is `answered`.
    pub question: Option<String>,
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

/// How a workflow's runs in one project have gone. A run that doesn't say which workflow it used
/// isn't in here: it had no saved one, or it's from before Keel kept this.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowResults {
    pub workflow_id: String,
    /// What its latest run called it. It may have been renamed or deleted since.
    pub workflow_name: String,
    pub runs: i64,
    /// Those that ended, however they ended.
    pub finished: i64,
    /// Pull requests merged, and closed without merging, as of the last time Keel looked.
    pub merged: i64,
    pub closed: i64,
    /// What a run whose pull request got merged cost on average. None if none did.
    pub cost_per_merged_usd: Option<f64>,
    /// How many times a finished run was sent back on average. None if none finished, since an open
    /// run's count isn't final.
    pub average_rounds: Option<f64>,
    /// Runs that stopped to ask a person something.
    pub stopped_to_ask: i64,
    /// Questions a person answered, and how many of those got the recommended option. A high share
    /// means it asks too much.
    pub answers: i64,
    pub answers_recommended: i64,
    /// How many times a person sent work back from what it decided alone. A high count means it
    /// asks too little.
    pub sent_back: i64,
}

/// A card on the board, with what its column depends on. The issue is a copy taken on import.
#[derive(Debug, Clone)]
pub struct Card {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub body: String,
    pub issue: Option<serde_json::Value>,
    pub issue_number: Option<i64>,
    pub lane: CardLane,
    pub position: i64,
    pub language: Option<String>,
    pub todo_at: Option<String>,
    pub run_id: Option<String>,
    pub start_blocked: Option<serde_json::Value>,
    pub created_at: String,
    pub run_started_at: Option<String>,
    pub run_finished_at: Option<String>,
    /// Where its run stands, and why. See `ATTEMPT_THAT_MATTERS`.
    pub run_state: Option<AttemptState>,
    pub run_reason: Option<serde_json::Value>,
    pub pr_url: Option<String>,
    pub pr_state: Option<PrState>,
    pub pr_state_at: Option<String>,
    /// What the person wrote when rejecting it. Empty if they wrote nothing.
    pub rejection: Option<String>,
}

impl Card {
    pub fn column(&self) -> BoardColumn {
        card_column(self.lane, self.run_finished_at.is_some(), self.run_state)
    }

    /// Whether a person can still move or change it. Once Keel has it, it's out of their hands.
    fn is_waiting(&self) -> bool {
        matches!(self.column(), BoardColumn::Backlog | BoardColumn::Todo)
    }
}

#[derive(Debug, Clone)]
pub struct NewCard {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub body: String,
    pub issue: Option<String>,
    pub issue_number: Option<i64>,
}

fn card_from(r: &Row) -> rusqlite::Result<Card> {
    let lane: String = r.get("lane")?;
    let state: Option<String> = r.get("attempt_state")?;
    let run_state = state.as_deref().and_then(AttemptState::parse);
    let rejection: Option<String> = r.get("rejection")?;
    Ok(Card {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        title: r.get("title")?,
        body: r.get("body")?,
        issue: json_col(r, "issue")?,
        issue_number: r.get("issue_number")?,
        lane: CardLane::parse(&lane).unwrap_or(CardLane::Backlog),
        position: r.get("position")?,
        language: r.get("language")?,
        todo_at: r.get("todo_at")?,
        run_id: r.get("run_id")?,
        start_blocked: json_col(r, "start_blocked")?,
        created_at: r.get("created_at")?,
        run_started_at: r.get("run_started_at")?,
        run_finished_at: r.get("run_finished_at")?,
        run_state,
        run_reason: json_col(r, "attempt_reason")?,
        pr_url: r.get("run_pr_url")?,
        pr_state: pr_state_from(r.get("run_pr_state")?),
        pr_state_at: r.get("run_pr_state_at")?,
        // A rejection with nothing written leaves no decision, and is still a rejection.
        rejection: (run_state == Some(AttemptState::Rejected))
            .then(|| rejection.unwrap_or_default()),
    })
}

/// Cards matching a filter, by position.
fn read_cards<P: rusqlite::Params>(
    conn: &Connection,
    filter: &str,
    params: P,
) -> rusqlite::Result<Vec<Card>> {
    let mut stmt = conn.prepare(&format!(
        "select c.*, r.created_at as run_started_at, r.finished_at as run_finished_at, r.pr_url as run_pr_url,
                r.pr_state as run_pr_state, r.pr_state_at as run_pr_state_at,
                a.state as attempt_state, a.reason as attempt_reason,
                (select d.text from decision d where d.attempt_id = a.id and d.answer = 'reject'
                 order by d.seq desc limit 1) as rejection
         from card c
         left join run r on r.id = c.run_id
         left join attempt a on a.rowid = ({ATTEMPT_THAT_MATTERS})
         where {filter}
         order by c.position, c.created_at, c.id"
    ))?;
    let rows = stmt.query_map(params, card_from)?;
    rows.collect()
}

fn read_card(conn: &Connection, id: &str) -> Result<Card> {
    read_cards(conn, "c.id = ?1", [id])?
        .pop()
        .ok_or_else(|| Error::NotFound(format!("card {id}")))
}

/// How a person reads each column. Backlog and Todo are theirs to arrange, so they stay by position.
/// In Progress and Human Review go oldest first, as what's waited longest, and Done has the latest
/// on top. A card that has only just been taken has no run yet, and is the newest of its column.
fn board_order(a: &Card, b: &Card) -> std::cmp::Ordering {
    let started = |c: &Card| (c.run_started_at.is_none(), c.run_started_at.clone());
    a.column().cmp(&b.column()).then_with(|| match a.column() {
        BoardColumn::Backlog | BoardColumn::Todo => std::cmp::Ordering::Equal,
        BoardColumn::InProgress | BoardColumn::HumanReview => started(a).cmp(&started(b)),
        BoardColumn::Done => b.run_finished_at.cmp(&a.run_finished_at),
    })
}

/// One past the last card showing in the backlog, which includes rejected ones still on their run.
fn bottom_of_backlog(conn: &Connection, project_id: &str) -> rusqlite::Result<i64> {
    Ok(read_cards(conn, "c.project_id = ?1", [project_id])?
        .iter()
        .filter(|c| c.column() == BoardColumn::Backlog)
        .map(|c| c.position + 1)
        .max()
        .unwrap_or(0))
}

fn insert_run(conn: &Connection, run: &NewRun) -> rusqlite::Result<()> {
    conn.execute(
        "insert into run (id, project_id, title, request, workflow, language, source_issue, created_at, workflow_id, workflow_name)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![run.id, run.project_id, run.title, run.request, run.workflow, run.language, run.source_issue, now(), run.workflow_id, run.workflow_name],
    )?;
    Ok(())
}

fn renumber(conn: &Connection, ids: &[&str]) -> rusqlite::Result<()> {
    for (position, id) in ids.iter().enumerate() {
        conn.execute(
            "update card set position = ?2 where id = ?1",
            params![id, position as i64],
        )?;
    }
    Ok(())
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
            "insert into project (id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env, setup_command, default_workflow)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                project.default_workflow,
            ],
        )?;
        Ok(())
    }

    pub fn update_project(&self, id: &str, settings: &ProjectSettings) -> Result<()> {
        self.lock().execute(
            "update project set github_account = ?2, check_command = ?3, allowed_domains = ?4, allowed_read = ?5, services = ?6, env = ?7, setup_command = ?8, default_workflow = ?9 where id = ?1",
            params![
                id,
                settings.github_account,
                settings.check_command,
                serde_json::to_string(&settings.allowed_domains).unwrap_or_default(),
                serde_json::to_string(&settings.allowed_read).unwrap_or_default(),
                serde_json::to_string(&settings.services).unwrap_or_default(),
                serde_json::to_string(&settings.env).unwrap_or_default(),
                settings.setup_command,
                settings.default_workflow,
            ],
        )?;
        Ok(())
    }

    /// The projects in use. A removed one is only reachable by `project` and `project_by_path`.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {PROJECT_COLUMNS} from project where removed_at is null order by name"
        ))?;
        let rows = stmt.query_map([], project_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Hides a project, unless a run of its hasn't finished or a card of its is being started, which
    /// is a run about to exist. False if it changed nothing.
    pub fn remove_project(&self, id: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "update project set removed_at = ?2 where id = ?1 and removed_at is null
             and not exists (select 1 from run where project_id = ?1 and finished_at is null)
             and not exists (select 1 from card where project_id = ?1 and lane = 'started' and run_id is null)",
            params![id, now()],
        )?;
        Ok(changed > 0)
    }

    pub fn restore_project(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("update project set removed_at = null where id = ?1", [id])?;
        Ok(())
    }

    pub fn project_is_removed(&self, id: &str) -> Result<bool> {
        Ok(self.lock().query_row(
            "select exists (select 1 from project where id = ?1 and removed_at is not null)",
            [id],
            |r| r.get(0),
        )?)
    }

    pub fn set_project_repo(&self, id: &str, repo: Option<&str>) -> Result<()> {
        self.lock().execute(
            "update project set github_repo = ?2 where id = ?1",
            params![id, repo],
        )?;
        Ok(())
    }

    /// Runs of this project that still have steps ahead, oldest first.
    pub fn open_runs_of_project(&self, project_id: &str) -> Result<Vec<Run>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {RUN_COLUMNS} from run where project_id = ?1 and finished_at is null order by created_at"
        ))?;
        let rows = stmt.query_map([project_id], run_from)?;
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
        Ok(insert_run(&self.lock(), run)?)
    }

    /// Adds the run of a card that was just taken from Todo, and links them in one go. If the app
    /// dies, there's either a card with no run, which goes back to Todo, or both together, never a
    /// run that nothing points at while its card starts another.
    pub fn add_run_for_card(&self, run: &NewRun, card_id: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        insert_run(&tx, run)?;
        let linked = tx.execute(
            "update card set run_id = ?2 where id = ?1 and lane = 'started' and run_id is null",
            params![card_id, run.id],
        )?;
        if linked != 1 {
            return Err(Error::NotFound(format!("card {card_id} waiting for a run")));
        }
        tx.commit()?;
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

    /// Runs with a pull request that could still change and that nobody has looked at since `before`,
    /// the longest waiting first. A merged one can't change. A closed one can still be reopened. A
    /// removed project's runs wait until it's back.
    pub fn runs_with_pr_due(&self, before: &str, limit: i64) -> Result<Vec<Run>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {RUN_COLUMNS} from run
             where pr_url is not null
               and coalesce(pr_state, '') <> 'merged'
               and (pr_checked_at is null or pr_checked_at < ?1)
               and project_id in (select id from project where removed_at is null)
             order by pr_checked_at is not null, pr_checked_at, created_at
             limit ?2"
        ))?;
        let rows = stmt.query_map(params![before, limit], run_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn workflow_results(&self, project_id: &str) -> Result<Vec<WorkflowResults>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select r.workflow_id,
                    (select latest.workflow_name from run latest
                     where latest.project_id = r.project_id and latest.workflow_id = r.workflow_id
                     order by latest.created_at desc, latest.rowid desc limit 1),
                    count(*),
                    coalesce(sum(r.finished_at is not null), 0),
                    coalesce(sum(r.pr_state = 'merged'), 0),
                    coalesce(sum(r.pr_state = 'closed'), 0),
                    coalesce(sum(case when r.pr_state = 'merged'
                                      then (select coalesce(sum(a.cost_usd), 0) from attempt a where a.run_id = r.id)
                                      end), 0),
                    coalesce(sum(case when r.finished_at is not null then r.round end), 0),
                    coalesce(sum(exists (select 1 from event e join attempt a on a.id = e.attempt_id
                                         where a.run_id = r.id and e.kind = 'questionsAsked')), 0),
                    coalesce(sum((select coalesce(sum(json_extract(e.data, '$.total')), 0) from event e
                                  join attempt a on a.id = e.attempt_id
                                  where a.run_id = r.id and e.kind = 'answers')), 0),
                    coalesce(sum((select coalesce(sum(json_extract(e.data, '$.recommended')), 0) from event e
                                  join attempt a on a.id = e.attempt_id
                                  where a.run_id = r.id and e.kind = 'answers')), 0),
                    coalesce(sum((select count(*) from attempt a
                                  where a.run_id = r.id and a.state = 'returned')), 0)
             from run r
             where r.project_id = ?1 and r.workflow_id is not null
             group by r.workflow_id
             order by max(r.created_at) desc",
        )?;
        let rows = stmt.query_map([project_id], |r| {
            let workflow_id: String = r.get(0)?;
            let workflow_name: Option<String> = r.get(1)?;
            let finished: i64 = r.get(3)?;
            let merged: i64 = r.get(4)?;
            let merged_cost: f64 = r.get(6)?;
            let rounds: i64 = r.get(7)?;
            Ok(WorkflowResults {
                workflow_name: workflow_name.unwrap_or_else(|| workflow_id.clone()),
                workflow_id,
                runs: r.get(2)?,
                finished,
                merged,
                closed: r.get(5)?,
                cost_per_merged_usd: (merged > 0).then(|| merged_cost / merged as f64),
                average_rounds: (finished > 0).then(|| rounds as f64 / finished as f64),
                stopped_to_ask: r.get(8)?,
                answers: r.get(9)?,
                answers_recommended: r.get(10)?,
                sent_back: r.get(11)?,
            })
        })?;
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

    /// What a person answered to a question, and asked to have remembered.
    pub fn add_answered_decision(
        &self,
        project_id: &str,
        run_id: &str,
        attempt_id: &str,
        question: &str,
        answer: &str,
    ) -> Result<()> {
        self.lock().execute(
            "insert into decision (project_id, run_id, attempt_id, at, answer, text, question)
             values (?1, ?2, ?3, ?4, 'answered', ?5, ?6)",
            params![project_id, run_id, attempt_id, now(), answer, question],
        )?;
        Ok(())
    }

    /// A project's decisions still in force, newest first.
    pub fn decisions(&self, project_id: &str) -> Result<Vec<Decision>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select seq, project_id, run_id, attempt_id, at, answer, text, retired, question from decision
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
                question: r.get(8)?,
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

    /// Proposals waiting for a person, across all projects. Not those of a removed project, since
    /// the Brain can't show them and nothing could clear the count.
    pub fn pending_proposals(&self) -> Result<i64> {
        Ok(self.lock().query_row(
            "select count(*) from brain_note n join project p on p.id = n.project_id
             where n.state = 'proposed' and p.removed_at is null",
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

    /// Moves an attempt on only if it's still where the caller found it. False if it isn't, which
    /// is how a second answer to the same question loses.
    pub fn change_state(&self, id: &str, from: AttemptState, to: AttemptState) -> Result<bool> {
        let changed = self.lock().execute(
            "update attempt set state = ?3, reason = null where id = ?1 and state = ?2",
            params![id, from.as_str(), to.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// A person sends the work back from an approval: its attempt says so, with what they want, and
    /// the run moves to its next round. All or nothing, because an attempt that says it went back
    /// in a round that never moved on would hold the run forever. False if it wasn't waiting.
    pub fn return_attempt(&self, id: &str, run_id: &str, instructions: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let at = now();
        let output = serde_json::json!({ "instructions": instructions }).to_string();
        let changed = tx.execute(
            "update attempt set state = 'returned', reason = null, output = ?2, finished_at = ?3
             where id = ?1 and state = 'waitingApproval'",
            params![id, output, at],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        tx.execute(
            "update run set round = round + 1 where id = ?1",
            params![run_id],
        )?;
        let round: i64 = tx.query_row("select round from run where id = ?1", [run_id], |r| {
            r.get(0)
        })?;
        tx.execute(
            "insert into event (attempt_id, at, kind, data) values (?1, ?2, 'returned', ?3)",
            params![
                id,
                at,
                serde_json::json!({ "instructions": instructions, "round": round }).to_string()
            ],
        )?;
        tx.commit()?;
        Ok(true)
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
        let mut stmt = conn.prepare(&format!(
            "select r.id, r.title, r.project_id, p.name as project_name, a.step_id, a.id as attempt_id, a.number,
                    a.state, a.reason, coalesce(a.model_actual, a.model_requested) as model, a.turns,
                    a.started_at, a.finished_at, a.last_activity_at, r.pr_url, r.started_by
             from run r
             join project p on p.id = r.project_id
             join attempt a on a.run_id = r.id and a.rowid = ({ATTEMPT_THAT_MATTERS})
             order by r.created_at desc limit ?1"
        ))?;
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

    /// Adds a card at the bottom of the backlog. An issue that's already on the board isn't added
    /// again, and the id that comes back is the existing card's.
    pub fn add_card(&self, card: &NewCard) -> Result<String> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "insert or ignore into card (id, project_id, title, body, issue, issue_number, lane, position, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, 'backlog', ?7, ?8)",
            params![
                card.id,
                card.project_id,
                card.title,
                card.body,
                card.issue,
                card.issue_number,
                bottom_of_backlog(&tx, &card.project_id)?,
                now()
            ],
        )?;
        let id = match card.issue_number {
            Some(number) => tx.query_row(
                "select id from card where project_id = ?1 and issue_number = ?2",
                params![card.project_id, number],
                |r| r.get(0),
            )?,
            None => card.id.clone(),
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn card(&self, id: &str) -> Result<Card> {
        read_card(&self.lock(), id)
    }

    /// A project's cards, in the order the board shows them.
    pub fn cards(&self, project_id: &str) -> Result<Vec<Card>> {
        let mut cards = read_cards(&self.lock(), "c.project_id = ?1", [project_id])?;
        // Stable, so that what compares equal stays by position.
        cards.sort_by(board_order);
        Ok(cards)
    }

    /// Changes what a card says, if a person can still change it. `false` if not.
    pub fn update_card(&self, id: &str, title: &str, body: &str) -> Result<bool> {
        let conn = self.lock();
        if !read_card(&conn, id)?.is_waiting() {
            return Ok(false);
        }
        conn.execute(
            "update card set title = ?2, body = ?3 where id = ?1",
            params![id, title, body],
        )?;
        Ok(true)
    }

    /// Removes a card, unless Keel or a person still has work to do on it. `false` if so. Its run
    /// stays in history.
    pub fn delete_card(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        if matches!(
            read_card(&conn, id)?.column(),
            BoardColumn::InProgress | BoardColumn::HumanReview
        ) {
            return Ok(false);
        }
        conn.execute("delete from card where id = ?1", [id])?;
        Ok(true)
    }

    /// Moves a card to a place in Backlog or Todo and renumbers both. `false`, with nothing
    /// changed, if the card isn't in one of them, which is also how this loses the race against
    /// the scheduler taking it. Reordering a rejected card leaves it attached to its run.
    pub fn move_card(
        &self,
        id: &str,
        lane: CardLane,
        index: usize,
        language: &str,
    ) -> Result<bool> {
        let to = match lane {
            CardLane::Backlog => BoardColumn::Backlog,
            CardLane::Todo => BoardColumn::Todo,
            CardLane::Started => return Ok(false),
        };
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let card = read_card(&tx, id)?;
        if !card.is_waiting() {
            return Ok(false);
        }
        let from = card.column();
        let others = read_cards(
            &tx,
            "c.project_id = ?1 and c.id != ?2",
            [&card.project_id, id],
        )?;
        let column = |wanted: BoardColumn| -> Vec<&str> {
            others
                .iter()
                .filter(|c| c.column() == wanted)
                .map(|c| c.id.as_str())
                .collect()
        };
        if from != to {
            if to == BoardColumn::Todo {
                // A new run replaces the old one, which stays in history.
                tx.execute(
                    "update card set lane = 'todo', language = ?2, todo_at = ?3, run_id = null, start_blocked = null where id = ?1",
                    params![id, language, now()],
                )?;
            } else {
                tx.execute(
                    "update card set lane = 'backlog', language = null, todo_at = null where id = ?1",
                    [id],
                )?;
            }
            renumber(&tx, &column(from))?;
        }
        let mut destination = column(to);
        destination.insert(index.min(destination.len()), id);
        renumber(&tx, &destination)?;
        tx.commit()?;
        Ok(true)
    }

    /// Cards that could start now. A removed project's wait where they are until it comes back.
    pub fn has_todo_cards(&self) -> Result<bool> {
        Ok(self.lock().query_row(
            "select exists (select 1 from card c join project p on p.id = c.project_id
                            where c.lane = 'todo' and p.removed_at is null)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Takes the card to start next out of Todo, and returns it as it is at that moment: the top
    /// one of each project's Todo, and of those, whichever has waited longest. After this nobody
    /// can drag it away or edit it, and it can't start twice. Never a removed project's.
    pub fn claim_next_todo_card(&self) -> Result<Option<Card>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let tops = read_cards(
            &tx,
            "c.lane = 'todo'
             and c.project_id in (select id from project where removed_at is null)
             and not exists (
                select 1 from card o where o.project_id = c.project_id and o.lane = 'todo'
                  and (o.position, o.created_at, o.id) < (c.position, c.created_at, c.id))",
            [],
        )?;
        let Some(mut card) = tops
            .into_iter()
            .min_by(|a, b| (&a.todo_at, &a.id).cmp(&(&b.todo_at, &b.id)))
        else {
            return Ok(None);
        };
        tx.execute(
            "update card set lane = 'started', run_id = null, start_blocked = null where id = ?1",
            [&card.id],
        )?;
        tx.commit()?;
        card.lane = CardLane::Started;
        card.start_blocked = None;
        Ok(Some(card))
    }

    /// A card for a run that's starting right now from outside the board, left as the scheduler
    /// leaves one it has claimed, so nothing starts it a second time. An issue only ever has one
    /// card, so this takes over the one it has. `None`, with nothing changed, if that card's run is
    /// still open.
    pub fn add_started_card(&self, card: &NewCard, language: &str) -> Result<Option<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let existing = match card.issue_number {
            Some(number) => read_cards(
                &tx,
                "c.project_id = ?1 and c.issue_number = ?2",
                params![card.project_id, number],
            )?
            .pop(),
            None => None,
        };
        let id = match existing {
            Some(existing)
                if matches!(
                    existing.column(),
                    BoardColumn::InProgress | BoardColumn::HumanReview
                ) =>
            {
                return Ok(None);
            }
            Some(existing) => {
                tx.execute(
                    "update card set lane = 'started', run_id = null, language = ?2, start_blocked = null
                     where id = ?1",
                    params![existing.id, language],
                )?;
                existing.id
            }
            None => {
                tx.execute(
                    "insert into card (id, project_id, title, body, issue, issue_number, lane, position, language, created_at)
                     values (?1, ?2, ?3, ?4, ?5, ?6, 'started', ?7, ?8, ?9)",
                    params![
                        card.id,
                        card.project_id,
                        card.title,
                        card.body,
                        card.issue,
                        card.issue_number,
                        bottom_of_backlog(&tx, &card.project_id)?,
                        language,
                        now()
                    ],
                )?;
                card.id.clone()
            }
        };
        tx.commit()?;
        Ok(Some(id))
    }

    /// The card a run is on, if it's on the board.
    pub fn card_of_run(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("select id from card where run_id = ?1", [run_id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Hands a card that couldn't start back to the person, at the bottom of the backlog.
    pub fn card_start_failed(&self, id: &str, reason: &Reason) -> Result<()> {
        let conn = self.lock();
        let bottom = bottom_of_backlog(&conn, &read_card(&conn, id)?.project_id)?;
        conn.execute(
            "update card set lane = 'backlog', run_id = null, language = null, todo_at = null, start_blocked = ?2, position = ?3
             where id = ?1",
            params![id, serde_json::to_string(reason).unwrap_or_default(), bottom],
        )?;
        Ok(())
    }

    /// Puts back in Todo any card taken from it by an app that died before starting its run.
    pub fn release_claimed_cards(&self) -> Result<()> {
        self.lock().execute(
            "update card set lane = 'todo' where lane = 'started' and run_id is null",
            [],
        )?;
        Ok(())
    }

    /// Runs holding a place right now, across the whole app, however they started. That's an open
    /// run with anything active, whatever else it's waiting for: a stalled agent or a reviewer still
    /// going keep their agent slots, so the board can't give their places away. It's also one with
    /// no attempt yet, or between two steps: the next step is on its way, and nothing should take
    /// its place in the moment before it's recorded.
    pub fn working_runs(&self) -> Result<usize> {
        let count: i64 = self.lock().query_row(
            &format!(
                "select count(*) from run r
                 where r.finished_at is null
                   and (exists (select 1 from attempt x where x.run_id = r.id
                                  and x.state in ('queued', 'preparing', 'running', 'stalled'))
                        or coalesce((select a.state from attempt a where a.rowid = ({ATTEMPT_THAT_MATTERS})), 'none')
                           in ('none', 'succeeded', 'skipped', 'returned'))"
            ),
            [],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Why open runs are blocked right now, newest first. A step that has been tried again is no
    /// longer blocked on what it was.
    pub fn blocked_reasons(&self) -> Result<Vec<Reason>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select a.reason from attempt a
             join run r on r.id = a.run_id
             where a.state = 'blocked' and r.finished_at is null
               and not exists (select 1 from attempt later
                               where later.run_id = a.run_id and later.step_id = a.step_id and later.rowid > a.rowid)
             order by a.rowid desc",
        )?;
        let reasons = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
        let mut found = Vec::new();
        for reason in reasons {
            found.extend(reason?.and_then(|text| serde_json::from_str(&text).ok()));
        }
        Ok(found)
    }

    pub fn totals(&self) -> Result<Totals> {
        Ok(self.lock().query_row(
            "select
                coalesce(sum(state in ('preparing', 'running') and number = 1), 0),
                coalesce(sum(state in ('preparing', 'running') and number > 1), 0),
                coalesce(sum(state in ('stalled', 'blocked', 'interrupted', 'waitingApproval', 'waitingAnswers')
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
                default_workflow: None,
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
                workflow_id: None,
                workflow_name: None,
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
                 drop trigger decision_question_fixed;
                 alter table decision drop column question;
                 alter table run drop column workflow_id;
                 alter table run drop column workflow_name;
                 alter table run drop column pr_state;
                 alter table run drop column pr_state_at;
                 alter table run drop column pr_checked_at;
                 alter table project drop column removed_at;
                 drop table card;
                 alter table project drop column default_workflow;
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
                    default_workflow: Some("builtin:simple".into()),
                },
            )
            .unwrap();
        let project = store.project("p").unwrap();
        assert_eq!(project.default_workflow.as_deref(), Some("builtin:simple"));
        assert_eq!(project.github_account.as_deref(), Some("octo"));
        assert_eq!(project.setup_command.as_deref(), Some("uv sync"));
        assert_eq!(project.check_command.as_deref(), Some("make test"));
        assert_eq!(project.allowed_domains, ["pypi.org"]);
        assert_eq!(project.allowed_read, ["~/.cargo"]);
        assert_eq!(project.services[0].name(), "db");
        assert_eq!(project.env[0].name, "DB");
    }

    fn card(store: &Store, id: &str) -> String {
        store
            .add_card(&NewCard {
                id: id.into(),
                project_id: "p".into(),
                title: id.into(),
                body: String::new(),
                issue: None,
                issue_number: None,
            })
            .unwrap()
    }

    /// The ids in a column, in the order the board shows them, with their positions.
    fn column(store: &Store, wanted: BoardColumn) -> Vec<(String, i64)> {
        store
            .cards("p")
            .unwrap()
            .into_iter()
            .filter(|c| c.column() == wanted)
            .map(|c| (c.id, c.position))
            .collect()
    }

    fn ids(store: &Store, wanted: BoardColumn) -> Vec<String> {
        column(store, wanted)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn new_attempt(id: &str, run: &str, step: &str, state: AttemptState) -> NewAttempt {
        NewAttempt {
            id: id.into(),
            run_id: run.into(),
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
        }
    }

    fn claim(store: &Store) -> Option<String> {
        store.claim_next_todo_card().unwrap().map(|c| c.id)
    }

    /// A card that went through Todo and has this run, the seeded one, with its only attempt in `state`.
    fn started(store: &Store, id: &str, state: AttemptState, finished: bool) {
        card(store, id);
        assert!(store.move_card(id, CardLane::Todo, 0, "es").unwrap());
        assert_eq!(claim(store).as_deref(), Some(id));
        store
            .lock()
            .execute("update card set run_id = 'r' where id = ?1", [id])
            .unwrap();
        store.set_state("a", state, None).unwrap();
        if finished {
            store
                .patch_run("r", &[("finished_at", Value::Text(now()))])
                .unwrap();
        }
    }

    #[test]
    fn the_board_arrives_on_a_database_people_already_have() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        // Exactly what the last release left: every migration before the board, and real rows.
        let before_the_board = 7;
        {
            let conn = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..before_the_board] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before_the_board as i64)
                .unwrap();
            conn.execute_batch(
                "insert into setting (key, value) values ('language', 'es');
                 insert into project (id, name, path, created_at, github_account)
                     values ('p', 'demo', '/demo', 'then', 'octo');
                 insert into run (id, project_id, title, created_at) values ('r', 'p', 'old', 'then');",
            )
            .unwrap();
            assert!(
                conn.execute("select * from card", []).is_err(),
                "no board yet"
            );
        }

        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .lock()
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert_eq!(store.setting("language").unwrap().as_deref(), Some("es"));
        let project = store.project("p").unwrap();
        assert_eq!(project.github_account.as_deref(), Some("octo"));
        assert_eq!(project.default_workflow, None);
        assert_eq!(store.run("r").unwrap().title, "old");
        assert!(store.cards("p").unwrap().is_empty());

        // It works from here, and opening again changes nothing.
        card(&store, "new");
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(ids(&store, BoardColumn::Backlog), ["new"]);
    }

    #[test]
    fn runs_from_before_workflows_and_pull_requests_were_kept_load_with_nothing_known() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        // Exactly what the last release left, with a run that opened a pull request.
        let before = 9;
        {
            let conn = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute_batch(
                "insert into project (id, name, path, created_at) values ('p', 'demo', '/demo', 'then');
                 insert into run (id, project_id, title, created_at, pr_url, pr_number, finished_at)
                     values ('r', 'p', 'old', 'then', 'https://github.com/acme/app/pull/3', 3, 'then');
                 insert into card (id, project_id, title, lane, position, run_id, created_at)
                     values ('c', 'p', 'old', 'started', 0, 'r', 'then');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let run = store.run("r").unwrap();
        assert_eq!(run.pr_number, Some(3));
        assert_eq!(run.workflow_id, None);
        assert_eq!(run.workflow_name, None);
        assert_eq!(run.pr_state, None);
        assert_eq!(run.pr_state_at, None);
        assert_eq!(run.pr_checked_at, None);
        let cards = store.cards("p").unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].pr_state, None);
        assert_eq!(cards[0].pr_url, run.pr_url);
        assert_eq!(cards[0].column(), BoardColumn::Done);
        assert!(
            store.workflow_results("p").unwrap().is_empty(),
            "it doesn't say which workflow it used, so it counts for none"
        );
        // It can still find out what became of its pull request.
        let due = store.runs_with_pr_due(&now(), 10).unwrap();
        assert_eq!(due.len(), 1);
    }

    /// A run in project `p` with a pull request Keel last looked at then, or not yet.
    fn run_with_pr(store: &Store, id: &str, state: Option<&str>, checked: Option<&str>) {
        store
            .add_run(&NewRun {
                id: id.into(),
                project_id: "p".into(),
                title: id.into(),
                request: id.into(),
                workflow: "[]".into(),
                language: "en".into(),
                source_issue: None,
                workflow_id: None,
                workflow_name: None,
            })
            .unwrap();
        let text = |value: Option<&str>| value.map_or(Value::Null, |v| Value::Text(v.into()));
        store
            .patch_run(
                id,
                &[
                    (
                        "pr_url",
                        Value::Text(format!("https://github.com/acme/app/pull/{id}")),
                    ),
                    ("pr_state", text(state)),
                    ("pr_checked_at", text(checked)),
                ],
            )
            .unwrap();
    }

    #[test]
    fn only_a_pull_request_that_can_still_change_and_has_waited_long_enough_is_due() {
        let (_dir, store) = store();
        seeded(&store);
        run_with_pr(&store, "never", None, None);
        run_with_pr(
            &store,
            "older",
            Some("draft"),
            Some("2026-01-01T00:00:00.000Z"),
        );
        run_with_pr(
            &store,
            "oldest",
            Some("open"),
            Some("2025-01-01T00:00:00.000Z"),
        );
        run_with_pr(
            &store,
            "closed",
            Some("closed"),
            Some("2026-02-01T00:00:00.000Z"),
        );
        run_with_pr(
            &store,
            "merged",
            Some("merged"),
            Some("2025-01-01T00:00:00.000Z"),
        );
        run_with_pr(
            &store,
            "recent",
            Some("open"),
            Some("2026-11-01T00:00:00.000Z"),
        );

        let before = "2026-06-01T00:00:00.000Z";
        let due = |limit| -> Vec<String> {
            store
                .runs_with_pr_due(before, limit)
                .unwrap()
                .into_iter()
                .map(|run| run.id)
                .collect()
        };
        // Never looked at first, then the longest waiting. `r` has no pull request at all.
        assert_eq!(due(10), ["never", "oldest", "older", "closed"]);
        assert_eq!(due(2), ["never", "oldest"]);

        store
            .lock()
            .execute("update project set removed_at = 'now' where id = 'p'", [])
            .unwrap();
        assert!(due(10).is_empty(), "a removed project's runs wait");
    }

    #[test]
    fn results_count_each_workflow_on_its_own_and_only_what_is_final() {
        let (_dir, store) = store();
        seeded(&store);
        let add = |id: &str,
                   workflow: Option<(&str, &str)>,
                   state: Option<&str>,
                   round: i64,
                   finished: bool,
                   cost: f64| {
            store
                .add_run(&NewRun {
                    id: id.into(),
                    project_id: "p".into(),
                    title: id.into(),
                    request: id.into(),
                    workflow: "[]".into(),
                    language: "en".into(),
                    source_issue: None,
                    workflow_id: workflow.map(|w| w.0.into()),
                    workflow_name: workflow.map(|w| w.1.into()),
                })
                .unwrap();
            store
                .patch_run(
                    id,
                    &[
                        (
                            "pr_state",
                            state.map_or(Value::Null, |s| Value::Text(s.into())),
                        ),
                        ("round", Value::Integer(round)),
                        (
                            "finished_at",
                            if finished {
                                Value::Text(now())
                            } else {
                                Value::Null
                            },
                        ),
                    ],
                )
                .unwrap();
            let attempt = format!("{id}-a");
            store
                .add_attempt(&new_attempt(
                    &attempt,
                    id,
                    "builder",
                    AttemptState::Succeeded,
                ))
                .unwrap();
            store
                .patch_attempt(&attempt, &[("cost_usd", Value::Real(cost))])
                .unwrap();
        };
        let full = Some(("builtin:full", "Full"));
        add("merged-1", full, Some("merged"), 2, true, 1.0);
        add("merged-2", full, Some("merged"), 0, true, 3.0);
        add("closed", full, Some("closed"), 1, true, 50.0);
        // Its rounds aren't final, and what it cost so far bought no merged pull request.
        add("still-going", full, None, 9, false, 50.0);
        add(
            "simple",
            Some(("builtin:simple", "Simple")),
            None,
            0,
            true,
            7.0,
        );
        add("no-workflow", None, Some("merged"), 5, true, 100.0);
        // A later run has the workflow under a new name.
        add("renamed", Some(("mine", "Old name")), None, 0, false, 0.0);
        store
            .lock()
            .execute(
                "update run set created_at = '2020-01-01' where id = 'renamed'",
                [],
            )
            .unwrap();
        add("renamed-2", Some(("mine", "New name")), None, 0, false, 0.0);

        let results = store.workflow_results("p").unwrap();
        let of = |id: &str| results.iter().find(|r| r.workflow_id == id).unwrap();
        assert_eq!(results.len(), 3, "{results:?}");
        assert_eq!(
            *of("builtin:full"),
            WorkflowResults {
                workflow_id: "builtin:full".into(),
                workflow_name: "Full".into(),
                runs: 4,
                finished: 3,
                merged: 2,
                closed: 1,
                cost_per_merged_usd: Some(2.0),
                average_rounds: Some(1.0),
                stopped_to_ask: 0,
                answers: 0,
                answers_recommended: 0,
                sent_back: 0,
            }
        );
        assert_eq!(of("builtin:simple").merged, 0);
        assert_eq!(of("builtin:simple").cost_per_merged_usd, None);
        assert_eq!(of("builtin:simple").average_rounds, Some(0.0));
        assert_eq!(of("mine").workflow_name, "New name");
        assert_eq!(of("mine").average_rounds, None, "none has finished");
        assert!(store.workflow_results("nobody").unwrap().is_empty());
    }

    #[test]
    fn a_state_only_changes_from_where_the_caller_found_it() {
        let (_dir, store) = store();
        let id = seeded(&store);
        store
            .set_state(&id, AttemptState::WaitingAnswers, Some(&Reason::AgentLost))
            .unwrap();
        assert!(
            store
                .change_state(&id, AttemptState::WaitingAnswers, AttemptState::Succeeded)
                .unwrap()
        );
        let attempt = store.attempt(&id).unwrap();
        assert_eq!(
            (attempt.state.as_str(), attempt.reason),
            ("succeeded", None)
        );
        assert!(
            !store
                .change_state(&id, AttemptState::WaitingAnswers, AttemptState::Abandoned)
                .unwrap(),
            "the second one finds it moved"
        );
        assert_eq!(store.attempt(&id).unwrap().state, "succeeded");
    }

    #[test]
    fn sending_back_moves_the_attempt_and_the_round_together_and_only_once() {
        let (_dir, store) = store();
        let id = seeded(&store);
        assert!(!store.return_attempt(&id, "r", "Change it").unwrap());
        assert_eq!(store.run("r").unwrap().round, 0, "it wasn't waiting");

        store
            .set_state(&id, AttemptState::WaitingApproval, None)
            .unwrap();
        assert!(store.return_attempt(&id, "r", "Change it").unwrap());
        let attempt = store.attempt(&id).unwrap();
        assert_eq!(attempt.state, "returned");
        assert_eq!(attempt.output.unwrap()["instructions"], "Change it");
        assert!(attempt.finished_at.is_some());
        assert_eq!(store.run("r").unwrap().round, 1);
        let events = store.events(&id, 0).unwrap();
        assert_eq!(events[0].kind, "returned");
        assert_eq!(events[0].data["round"], 1);

        assert!(!store.return_attempt(&id, "r", "Again").unwrap());
        assert_eq!(store.run("r").unwrap().round, 1);
    }

    #[test]
    fn an_answer_kept_as_a_decision_says_what_was_asked_and_cannot_be_rewritten() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .add_answered_decision("p", "r", "a", "If it's 0?", "Show an error")
            .unwrap();
        store
            .add_decision("p", "r", "a", "approve", "Fine")
            .unwrap();
        let decisions = store.decisions("p").unwrap();
        assert_eq!(decisions[0].answer, "approve");
        assert_eq!(decisions[0].question, None);
        assert_eq!(decisions[1].answer, "answered");
        assert_eq!(decisions[1].question.as_deref(), Some("If it's 0?"));
        assert_eq!(decisions[1].text, "Show an error");
        assert!(
            store
                .lock()
                .execute("update decision set question = 'other'", [])
                .is_err()
        );
    }

    #[test]
    fn waiting_for_answers_counts_as_waiting_for_a_person_and_shows_on_the_home_table() {
        let (_dir, store) = store();
        let id = seeded(&store);
        store
            .set_state(&id, AttemptState::WaitingAnswers, None)
            .unwrap();
        assert_eq!(store.totals().unwrap().blocked, 1);
        // A step that finished later doesn't take its place.
        store
            .add_attempt(&new_attempt("later", "r", "other", AttemptState::Succeeded))
            .unwrap();
        assert_eq!(store.runs(10).unwrap()[0].state, "waitingAnswers");
        assert_eq!(store.working_runs().unwrap(), 0);
    }

    #[test]
    fn an_issue_imported_twice_is_one_card() {
        let (_dir, store) = store();
        seeded(&store);
        let issue = |id: &str, project: &str, title: &str| NewCard {
            id: id.into(),
            project_id: project.into(),
            title: title.into(),
            body: String::new(),
            issue: Some(r#"{"number":7}"#.into()),
            issue_number: Some(7),
        };
        let first = store.add_card(&issue("one", "p", "Broken")).unwrap();
        assert_eq!(first, "one");
        let again = store.add_card(&issue("two", "p", "Renamed")).unwrap();
        assert_eq!(again, "one", "the id of the card that's already there");
        let cards = store.cards("p").unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Broken", "and the first copy stands");

        // The same number in another repo is another issue, and tasks have no number at all.
        store
            .add_project(&Project {
                id: "q".into(),
                name: "other".into(),
                path: "/other".into(),
                created_at: now(),
                github_account: None,
                github_repo: None,
                setup_command: None,
                check_command: None,
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
            })
            .unwrap();
        assert_eq!(
            store.add_card(&issue("three", "q", "Other")).unwrap(),
            "three"
        );
        card(&store, "task-a");
        card(&store, "task-b");
        assert_eq!(store.cards("p").unwrap().len(), 3);
    }

    #[test]
    fn moving_renumbers_both_columns() {
        let (_dir, store) = store();
        seeded(&store);
        for id in ["a", "b", "c", "d"] {
            card(&store, id);
        }
        assert_eq!(
            column(&store, BoardColumn::Backlog),
            [
                ("a".into(), 0),
                ("b".into(), 1),
                ("c".into(), 2),
                ("d".into(), 3)
            ]
        );

        // Within a column.
        assert!(store.move_card("d", CardLane::Backlog, 0, "en").unwrap());
        assert_eq!(ids(&store, BoardColumn::Backlog), ["d", "a", "b", "c"]);

        // To another, which closes the gap it left.
        assert!(store.move_card("a", CardLane::Todo, 0, "es").unwrap());
        assert_eq!(
            column(&store, BoardColumn::Backlog),
            [("d".into(), 0), ("b".into(), 1), ("c".into(), 2)]
        );
        assert_eq!(column(&store, BoardColumn::Todo), [("a".into(), 0)]);

        // Into the middle, and past the end.
        assert!(store.move_card("b", CardLane::Todo, 99, "en").unwrap());
        assert!(store.move_card("c", CardLane::Todo, 1, "en").unwrap());
        assert_eq!(
            column(&store, BoardColumn::Todo),
            [("a".into(), 0), ("c".into(), 1), ("b".into(), 2)]
        );
    }

    #[test]
    fn entering_todo_keeps_the_language_and_the_time_and_reordering_there_keeps_them_too() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "a");
        card(&store, "b");
        store
            .card_start_failed("a", &Reason::GithubAccountMissing)
            .unwrap();
        assert!(store.card("a").unwrap().start_blocked.is_some());

        assert!(store.move_card("a", CardLane::Todo, 0, "es").unwrap());
        let entered = store.card("a").unwrap();
        assert_eq!(entered.language.as_deref(), Some("es"));
        assert!(entered.todo_at.is_some());
        assert_eq!(entered.start_blocked, None, "it's being tried again");

        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(store.move_card("b", CardLane::Todo, 0, "en").unwrap());
        assert!(store.move_card("a", CardLane::Todo, 0, "en").unwrap());
        let reordered = store.card("a").unwrap();
        assert_eq!(reordered.language.as_deref(), Some("es"));
        assert_eq!(reordered.todo_at, entered.todo_at, "it didn't enter again");

        assert!(store.move_card("a", CardLane::Backlog, 0, "en").unwrap());
        let back = store.card("a").unwrap();
        assert_eq!(back.lane, CardLane::Backlog);
        assert_eq!((back.language, back.todo_at), (None, None));
    }

    #[test]
    fn a_card_keel_has_cannot_be_moved_changed_or_deleted() {
        for (state, finished, wanted) in [
            (AttemptState::Queued, false, BoardColumn::InProgress),
            (AttemptState::Running, false, BoardColumn::InProgress),
            (
                AttemptState::WaitingApproval,
                false,
                BoardColumn::HumanReview,
            ),
            (
                AttemptState::WaitingAnswers,
                false,
                BoardColumn::HumanReview,
            ),
            (AttemptState::Failed, false, BoardColumn::HumanReview),
            (AttemptState::Interrupted, false, BoardColumn::HumanReview),
        ] {
            let (_dir, store) = store();
            seeded(&store);
            started(&store, "c", state, finished);
            let before = store.card("c").unwrap();
            assert_eq!(before.column(), wanted, "{state:?}");
            for lane in [CardLane::Backlog, CardLane::Todo, CardLane::Started] {
                assert!(!store.move_card("c", lane, 0, "en").unwrap(), "{state:?}");
            }
            assert!(!store.update_card("c", "other", "other").unwrap());
            assert!(!store.delete_card("c").unwrap());
            let after = store.card("c").unwrap();
            assert_eq!(after.lane, before.lane);
            assert_eq!(after.run_id.as_deref(), Some("r"));
            assert_eq!(after.title, "c");
        }
    }

    #[test]
    fn a_done_card_cannot_move_but_can_be_cleared_away_and_its_run_stays() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "c", AttemptState::Succeeded, true);
        assert_eq!(store.card("c").unwrap().column(), BoardColumn::Done);
        assert!(!store.move_card("c", CardLane::Todo, 0, "en").unwrap());
        assert!(!store.update_card("c", "other", "").unwrap());
        assert!(store.delete_card("c").unwrap());
        assert!(store.cards("p").unwrap().is_empty());
        assert_eq!(store.run("r").unwrap().id, "r");
    }

    #[test]
    fn a_card_that_was_just_taken_from_todo_cannot_be_dragged() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "c");
        assert!(store.move_card("c", CardLane::Todo, 0, "en").unwrap());
        assert_eq!(claim(&store).as_deref(), Some("c"));
        assert_eq!(store.card("c").unwrap().column(), BoardColumn::InProgress);
        assert!(!store.move_card("c", CardLane::Backlog, 0, "en").unwrap());
        assert!(!store.update_card("c", "other", "").unwrap());
        assert_eq!(claim(&store), None, "and it can't start twice");
    }

    #[test]
    fn a_rejected_card_shows_in_the_backlog_and_keeps_its_run_until_it_goes_again() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "other");
        started(&store, "c", AttemptState::Rejected, true);
        store
            .add_decision("p", "r", "a", "reject", "not like this")
            .unwrap();
        let rejected = store.card("c").unwrap();
        assert_eq!(rejected.column(), BoardColumn::Backlog);
        assert_eq!(rejected.rejection.as_deref(), Some("not like this"));

        // Dragging it around the backlog counts the same as any card there, and leaves it attached.
        assert_eq!(ids(&store, BoardColumn::Backlog).len(), 2);
        assert!(store.move_card("c", CardLane::Backlog, 0, "en").unwrap());
        assert_eq!(ids(&store, BoardColumn::Backlog), ["c", "other"]);
        assert!(
            store
                .move_card("other", CardLane::Backlog, 0, "en")
                .unwrap()
        );
        assert_eq!(ids(&store, BoardColumn::Backlog), ["other", "c"]);
        let moved = store.card("c").unwrap();
        assert_eq!(moved.lane, CardLane::Started);
        assert_eq!(moved.run_id.as_deref(), Some("r"));
        assert_eq!(moved.rejection.as_deref(), Some("not like this"));

        // Sending it again replaces the old run, which stays in history.
        assert!(store.move_card("c", CardLane::Todo, 0, "es").unwrap());
        let again = store.card("c").unwrap();
        assert_eq!(again.column(), BoardColumn::Todo);
        assert_eq!(
            (again.run_id, again.run_state, again.rejection),
            (None, None, None)
        );
        assert_eq!(store.run("r").unwrap().id, "r");
    }

    #[test]
    fn rejecting_without_writing_anything_is_still_a_rejection() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "c", AttemptState::Rejected, true);
        assert_eq!(store.card("c").unwrap().rejection.as_deref(), Some(""));
    }

    #[test]
    fn an_abandoned_card_shows_in_the_backlog_with_no_rejection() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "c", AttemptState::Abandoned, true);
        let abandoned = store.card("c").unwrap();
        assert_eq!(abandoned.column(), BoardColumn::Backlog);
        assert_eq!(abandoned.run_state, Some(AttemptState::Abandoned));
        assert_eq!(abandoned.rejection, None);
    }

    #[test]
    fn a_card_follows_the_step_that_matters_not_the_newest_attempt() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "c", AttemptState::Failed, false);
        // Reviews run at once: the first failed, and the one that started last went fine.
        store
            .add_attempt(&new_attempt("later", "r", "ux", AttemptState::Succeeded))
            .unwrap();
        let card = store.card("c").unwrap();
        assert_eq!(card.run_state, Some(AttemptState::Failed));
        assert_eq!(card.column(), BoardColumn::HumanReview);
        assert_eq!(store.working_runs().unwrap(), 0);

        // Abandoning one of them ends the run, wherever the other one stands.
        store.set_state("a", AttemptState::Abandoned, None).unwrap();
        store
            .set_state("later", AttemptState::Failed, None)
            .unwrap();
        store
            .patch_run("r", &[("finished_at", Value::Text(now()))])
            .unwrap();
        assert_eq!(store.card("c").unwrap().column(), BoardColumn::Backlog);
    }

    #[test]
    fn a_finished_proposal_step_never_decides_but_a_stalled_one_needs_a_person() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "c", AttemptState::Succeeded, false);
        let mut learner = new_attempt("l", "r", "learnings", AttemptState::Failed);
        learner.role = "learner".into();
        store.add_attempt(&learner).unwrap();
        assert_eq!(
            store.card("c").unwrap().run_state,
            Some(AttemptState::Succeeded)
        );
        store.set_state("l", AttemptState::Stalled, None).unwrap();
        assert_eq!(store.card("c").unwrap().column(), BoardColumn::HumanReview);
    }

    #[test]
    fn working_counts_every_open_run_holding_a_place_and_nothing_else() {
        let (_dir, store) = store();
        seeded(&store);
        for (state, working) in [
            (AttemptState::Queued, 1),
            (AttemptState::Preparing, 1),
            (AttemptState::Running, 1),
            // Its agent still has its slot, even though a person is being asked to look.
            (AttemptState::Stalled, 1),
            // Between two steps: the next one is on its way.
            (AttemptState::Succeeded, 1),
            (AttemptState::Skipped, 1),
            // The work went back and the next step is on its way.
            (AttemptState::Returned, 1),
            (AttemptState::WaitingApproval, 0),
            // Nothing is running, and it could wait for days.
            (AttemptState::WaitingAnswers, 0),
            (AttemptState::Blocked, 0),
            (AttemptState::Failed, 0),
            (AttemptState::Stopped, 0),
            (AttemptState::Interrupted, 0),
            (AttemptState::Rejected, 0),
            (AttemptState::Abandoned, 0),
        ] {
            store.set_state("a", state, None).unwrap();
            assert_eq!(store.working_runs().unwrap(), working, "{state:?}");
        }
        store.set_state("a", AttemptState::Running, None).unwrap();
        store
            .patch_run("r", &[("finished_at", Value::Text(now()))])
            .unwrap();
        assert_eq!(
            store.working_runs().unwrap(),
            0,
            "a finished run isn't working"
        );
    }

    #[test]
    fn a_run_with_something_still_active_holds_its_place_whatever_else_it_waits_for() {
        let (_dir, store) = store();
        seeded(&store);
        // A review failed while another still runs: the card shows in Human Review, and the other
        // agent still has its slot.
        store.set_state("a", AttemptState::Failed, None).unwrap();
        store
            .add_attempt(&new_attempt("b", "r", "code", AttemptState::Running))
            .unwrap();
        assert_eq!(store.working_runs().unwrap(), 1);
        store.set_state("b", AttemptState::Succeeded, None).unwrap();
        assert_eq!(store.working_runs().unwrap(), 0, "now it's only waiting");

        // Tried again: the failure is history, and the run is moving.
        let mut retried = new_attempt("c", "r", "builder", AttemptState::Running);
        retried.number = 2;
        store.add_attempt(&retried).unwrap();
        assert_eq!(store.working_runs().unwrap(), 1);
        store.set_state("c", AttemptState::Succeeded, None).unwrap();
        assert_eq!(store.working_runs().unwrap(), 1, "and between two steps");
    }

    #[test]
    fn a_run_with_no_attempt_yet_holds_a_place() {
        let (_dir, store) = store();
        seeded(&store);
        store.set_state("a", AttemptState::Failed, None).unwrap();
        assert_eq!(store.working_runs().unwrap(), 0);
        store
            .add_run(&NewRun {
                id: "new".into(),
                project_id: "p".into(),
                title: "t".into(),
                request: "t".into(),
                workflow: "[]".into(),
                language: "en".into(),
                source_issue: None,
                workflow_id: None,
                workflow_name: None,
            })
            .unwrap();
        assert_eq!(store.working_runs().unwrap(), 1);
    }

    #[test]
    fn a_run_and_its_card_are_linked_together_or_not_at_all() {
        let (_dir, store) = store();
        seeded(&store);
        let run = |id: &str| NewRun {
            id: id.into(),
            project_id: "p".into(),
            title: "t".into(),
            request: "t".into(),
            workflow: "[]".into(),
            language: "en".into(),
            source_issue: None,
            workflow_id: None,
            workflow_name: None,
        };
        card(&store, "c");
        assert!(store.move_card("c", CardLane::Todo, 0, "en").unwrap());

        // Not taken from Todo, so there's nothing to link it to, and no run comes of it.
        assert!(store.add_run_for_card(&run("early"), "c").is_err());
        assert!(matches!(store.run("early"), Err(Error::NotFound(_))));
        assert!(store.add_run_for_card(&run("gone"), "no-card").is_err());
        assert!(matches!(store.run("gone"), Err(Error::NotFound(_))));

        assert_eq!(claim(&store).as_deref(), Some("c"));
        store.add_run_for_card(&run("linked"), "c").unwrap();
        assert_eq!(store.card("c").unwrap().run_id.as_deref(), Some("linked"));
        assert_eq!(store.run("linked").unwrap().id, "linked");

        // Only one, ever.
        assert!(store.add_run_for_card(&run("second"), "c").is_err());
        assert!(matches!(store.run("second"), Err(Error::NotFound(_))));
        assert_eq!(store.card("c").unwrap().run_id.as_deref(), Some("linked"));
        store.release_claimed_cards().unwrap();
        assert_eq!(store.card("c").unwrap().lane, CardLane::Started, "it stays");
    }

    #[test]
    fn claiming_returns_the_card_as_it_is_at_that_moment() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "first");
        card(&store, "second");
        assert!(store.move_card("first", CardLane::Todo, 0, "es").unwrap());
        assert!(store.move_card("second", CardLane::Todo, 1, "en").unwrap());

        // Changes made up to the last moment count, both which card and what it says.
        assert!(store.update_card("second", "second", "Edited").unwrap());
        assert!(store.move_card("second", CardLane::Todo, 0, "en").unwrap());
        let claimed = store.claim_next_todo_card().unwrap().unwrap();
        assert_eq!(claimed.id, "second");
        assert_eq!(claimed.body, "Edited");
        assert_eq!(claimed.language.as_deref(), Some("en"));
        assert_eq!(claimed.lane, CardLane::Started);
        assert!(store.has_todo_cards().unwrap(), "the other one still waits");
    }

    #[test]
    fn each_column_reads_the_way_a_person_reads_it() {
        let (_dir, store) = store();
        seeded(&store);
        let set = |sql: &str| {
            store.lock().execute_batch(sql).unwrap();
        };
        // A run per card, started and finished out of step with each other and with creation order.
        let started_card = |id: &str, started_at: &str, finished_at: Option<&str>, state: &str| {
            card(&store, id);
            assert!(store.move_card(id, CardLane::Todo, 0, "en").unwrap());
            assert_eq!(claim(&store).as_deref(), Some(id));
            let run = format!("run-{id}");
            store
                .add_run_for_card(
                    &NewRun {
                        id: run.clone(),
                        project_id: "p".into(),
                        title: id.into(),
                        request: id.into(),
                        workflow: "[]".into(),
                        language: "en".into(),
                        source_issue: None,
                        workflow_id: None,
                        workflow_name: None,
                    },
                    id,
                )
                .unwrap();
            store
                .add_attempt(&new_attempt(
                    &format!("attempt-{id}"),
                    &run,
                    "step",
                    AttemptState::parse(state).unwrap(),
                ))
                .unwrap();
            set(&format!(
                "update run set created_at = '{started_at}', finished_at = {} where id = '{run}';",
                finished_at.map_or("null".to_owned(), |t| format!("'{t}'")),
            ));
        };
        started_card("done-old", "2026-01-01", Some("2026-01-05"), "succeeded");
        started_card("done-new", "2026-01-02", Some("2026-01-09"), "succeeded");
        started_card("done-mid", "2026-01-03", Some("2026-01-07"), "failed");
        started_card("work-late", "2026-02-03", None, "running");
        started_card("work-early", "2026-02-01", None, "queued");
        started_card("work-mid", "2026-02-02", None, "preparing");
        started_card("ask-late", "2026-03-03", None, "failed");
        started_card("ask-early", "2026-03-01", None, "waitingApproval");
        started_card("ask-mid", "2026-03-02", None, "blocked");
        card(&store, "back-a");
        card(&store, "back-b");
        card(&store, "todo-a");
        card(&store, "todo-b");
        assert!(store.move_card("todo-a", CardLane::Todo, 0, "en").unwrap());
        assert!(store.move_card("todo-b", CardLane::Todo, 0, "en").unwrap());
        assert!(
            store
                .move_card("back-b", CardLane::Backlog, 0, "en")
                .unwrap()
        );

        let titles: Vec<String> = store
            .cards("p")
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            titles,
            [
                "back-b",
                "back-a",
                "todo-b",
                "todo-a",
                "work-early",
                "work-mid",
                "work-late",
                "ask-early",
                "ask-mid",
                "ask-late",
                "done-new",
                "done-mid",
                "done-old",
            ]
        );
    }

    #[test]
    fn a_card_just_taken_with_no_run_yet_is_the_newest_of_in_progress() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "has-a-run", AttemptState::Running, false);
        card(&store, "taken");
        assert!(store.move_card("taken", CardLane::Todo, 0, "en").unwrap());
        assert_eq!(claim(&store).as_deref(), Some("taken"));
        assert_eq!(ids(&store, BoardColumn::InProgress), ["has-a-run", "taken"]);
    }

    #[test]
    fn the_bottom_of_the_backlog_counts_the_rejected_cards_showing_there() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "a");
        card(&store, "b");
        started(&store, "rejected", AttemptState::Rejected, true);
        // Dragged below the others, where a person put it.
        assert!(
            store
                .move_card("rejected", CardLane::Backlog, 2, "en")
                .unwrap()
        );
        assert_eq!(ids(&store, BoardColumn::Backlog), ["a", "b", "rejected"]);

        // Counting only the lane would put these level with it, and above it by age.
        card(&store, "z-new");
        assert_eq!(
            ids(&store, BoardColumn::Backlog),
            ["a", "b", "rejected", "z-new"]
        );

        card(&store, "todo");
        assert!(store.move_card("todo", CardLane::Todo, 0, "en").unwrap());
        assert_eq!(claim(&store).as_deref(), Some("todo"));
        store
            .card_start_failed("todo", &Reason::GithubAccountMissing)
            .unwrap();
        assert_eq!(
            ids(&store, BoardColumn::Backlog),
            ["a", "b", "rejected", "z-new", "todo"]
        );
    }

    #[test]
    fn blocked_reasons_are_the_ones_of_open_runs_that_nobody_has_tried_again() {
        let (_dir, store) = store();
        seeded(&store);
        assert_eq!(store.blocked_reasons().unwrap(), vec![]);
        let limit = Reason::RateLimited { resets_at: None };
        store
            .set_state("a", AttemptState::Blocked, Some(&limit))
            .unwrap();
        assert_eq!(store.blocked_reasons().unwrap(), vec![limit.clone()]);

        // A later block in another step of the run comes first.
        let lost = Reason::SessionRejected {
            detail: "gone".into(),
        };
        store
            .add_attempt(&new_attempt("b", "r", "other", AttemptState::Blocked))
            .unwrap();
        store
            .set_state("b", AttemptState::Blocked, Some(&lost))
            .unwrap();
        assert_eq!(
            store.blocked_reasons().unwrap(),
            vec![lost.clone(), limit.clone()]
        );

        // Trying a step again puts its block behind it, while it runs and however it ends.
        let mut retried = new_attempt("c", "r", "builder", AttemptState::Running);
        retried.number = 2;
        store.add_attempt(&retried).unwrap();
        assert_eq!(store.blocked_reasons().unwrap(), vec![lost.clone()]);

        // Leaving the run does too.
        store
            .patch_run("r", &[("finished_at", Value::Text(now()))])
            .unwrap();
        assert_eq!(store.blocked_reasons().unwrap(), vec![]);
    }

    #[test]
    fn the_next_card_is_the_longest_waiting_of_each_projects_top_card() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .add_project(&Project {
                id: "q".into(),
                name: "other".into(),
                path: "/other".into(),
                created_at: now(),
                github_account: None,
                github_repo: None,
                setup_command: None,
                check_command: None,
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
            })
            .unwrap();
        let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
        let queue = |id: &str, project: &str, index: usize| {
            store
                .add_card(&NewCard {
                    id: id.into(),
                    project_id: project.into(),
                    title: id.into(),
                    body: String::new(),
                    issue: None,
                    issue_number: None,
                })
                .unwrap();
            assert!(store.move_card(id, CardLane::Todo, index, "en").unwrap());
            pause();
        };
        assert_eq!(claim(&store), None);
        queue("p-oldest", "p", 0);
        queue("q-middle", "q", 0);
        // This one is the newest of all, and it's on top of its project.
        queue("p-top", "p", 0);

        // `p-oldest` has waited longest but isn't first in line in its project.
        assert_eq!(claim(&store).as_deref(), Some("q-middle"));
        assert_eq!(claim(&store).as_deref(), Some("p-top"));
        assert_eq!(claim(&store).as_deref(), Some("p-oldest"));
        assert_eq!(claim(&store), None);
    }

    #[test]
    fn a_card_that_could_not_start_goes_to_the_bottom_of_the_backlog_and_says_why() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "first");
        card(&store, "c");
        assert!(store.move_card("c", CardLane::Todo, 0, "es").unwrap());
        assert_eq!(claim(&store).as_deref(), Some("c"));
        card(&store, "second");
        store
            .card_start_failed("c", &Reason::DefaultWorkflowMissing)
            .unwrap();
        assert_eq!(ids(&store, BoardColumn::Backlog), ["first", "second", "c"]);
        let card = store.card("c").unwrap();
        assert_eq!(card.lane, CardLane::Backlog);
        assert_eq!(card.todo_at, None);
        assert_eq!(
            serde_json::from_value::<Reason>(card.start_blocked.unwrap()).unwrap(),
            Reason::DefaultWorkflowMissing
        );
        assert!(!store.has_todo_cards().unwrap());
    }

    #[test]
    fn a_card_left_taken_by_an_app_that_died_goes_back_to_todo() {
        let (_dir, store) = store();
        seeded(&store);
        started(&store, "has-a-run", AttemptState::Running, false);
        card(&store, "taken");
        assert!(store.move_card("taken", CardLane::Todo, 0, "es").unwrap());
        assert_eq!(claim(&store).as_deref(), Some("taken"));
        store.release_claimed_cards().unwrap();
        let taken = store.card("taken").unwrap();
        assert_eq!(taken.column(), BoardColumn::Todo);
        assert_eq!(taken.language.as_deref(), Some("es"));
        assert_eq!(store.card("has-a-run").unwrap().lane, CardLane::Started);
    }

    #[test]
    fn deleting_a_card_keel_has_not_taken_works_and_a_missing_one_says_so() {
        let (_dir, store) = store();
        seeded(&store);
        card(&store, "in-backlog");
        card(&store, "in-todo");
        assert!(store.move_card("in-todo", CardLane::Todo, 0, "en").unwrap());
        assert!(store.delete_card("in-backlog").unwrap());
        assert!(store.delete_card("in-todo").unwrap());
        assert!(store.cards("p").unwrap().is_empty());
        assert!(matches!(store.delete_card("gone"), Err(Error::NotFound(_))));
        assert!(matches!(
            store.move_card("gone", CardLane::Todo, 0, "en"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn a_removed_project_leaves_the_lists_and_keeps_everything_else() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .patch_run("r", &[("finished_at", Value::Text(now()))])
            .unwrap();
        store.add_decision("p", "r", "a", "reject", "why").unwrap();
        let paths = vec!["a.py".to_owned()];
        let note = store
            .add_brain_note(&NewBrainNote {
                project_id: "p",
                kind: "learning",
                state: "proposed",
                text: "known",
                evidence: None,
                paths: &paths,
                commit_sha: None,
                run_id: "r",
                attempt_id: "a",
            })
            .unwrap();
        let event = store
            .append_event("a", "note", &serde_json::json!({}))
            .unwrap();
        assert_eq!(store.pending_proposals().unwrap(), 1);

        assert!(store.remove_project("p").unwrap());
        assert!(!store.remove_project("p").unwrap(), "it already is");
        assert!(store.project_is_removed("p").unwrap());
        assert!(store.projects().unwrap().is_empty());
        assert_eq!(
            store.pending_proposals().unwrap(),
            0,
            "nothing could clear it"
        );

        assert_eq!(store.project("p").unwrap().name, "demo");
        assert_eq!(store.project_by_path("/demo").unwrap().unwrap().id, "p");
        assert_eq!(store.runs(10).unwrap()[0].project_name, "demo");
        assert_eq!(store.attempts_of_run("r").unwrap().len(), 1);
        assert_eq!(store.events("a", event - 1).unwrap().len(), 1);
        assert_eq!(store.decisions("p").unwrap().len(), 1);
        assert_eq!(store.brain_note(note).unwrap().text, "known");

        store.restore_project("p").unwrap();
        assert!(!store.project_is_removed("p").unwrap());
        assert_eq!(store.projects().unwrap()[0].id, "p");
        assert_eq!(store.pending_proposals().unwrap(), 1);
    }

    #[test]
    fn a_project_with_a_run_that_has_not_finished_stays() {
        let (_dir, store) = store();
        seeded(&store);
        assert_eq!(store.open_runs_of_project("p").unwrap().len(), 1);
        assert!(!store.remove_project("p").unwrap());
        assert_eq!(store.projects().unwrap().len(), 1);
    }

    #[test]
    fn a_database_from_before_removing_projects_keeps_them_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        {
            let store = Store::open(&path).unwrap();
            seeded(&store);
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "pragma user_version = 8;
                 drop trigger decision_question_fixed;
                 alter table decision drop column question;
                 alter table run drop column workflow_id;
                 alter table run drop column workflow_name;
                 alter table run drop column pr_state;
                 alter table run drop column pr_state_at;
                 alter table run drop column pr_checked_at;
                 alter table project drop column removed_at;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.projects().unwrap().len(), 1);
        assert!(!store.project_is_removed("p").unwrap());
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
