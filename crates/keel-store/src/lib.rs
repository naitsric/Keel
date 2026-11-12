//! Local SQLite store: projects, runs, attempts and the audit log.
//!
//! `event` is append-only, and an attempt's identifying facts can't change after it starts.
//! Triggers enforce both, so a bug in the app can't rewrite history. This isn't protection
//! against someone who owns the machine: the raw stream file and its SHA-256 back it up.

use std::collections::BTreeMap;
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
    /// The issue's card is where a change across projects is agreed, which only the board starts.
    #[error("the issue's card is a change across several projects")]
    AcrossProjects,
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
    // 12: What a project counts as too sensitive for a change to be small, on top of what always is.
    r#"
    alter table project add column sensitive_paths text not null default '[]';
"#,
    // 13: Whether an MCP client may give feedback on its own runs, and how many it may keep open.
    r#"
    alter table mcp_client add column can_steer integer not null default 0;
    alter table mcp_client add column max_open_runs integer not null default 3;
"#,
    // 14: A change that touches several projects. The card where it's agreed shares its id, and
    // each project that changes gets a card of its own, in order, waiting for the one before it.
    // What only works with every part in place is tried by a person. A part keeps which commit it
    // read of the ones before, and a run the commit its pull request had last time Keel looked.
    r#"
    create table joint_change (
        id text primary key,
        projects text not null default '[]',
        agreed text,
        created_at text not null
    );
    create trigger joint_change_agreed_fixed before update of projects, agreed on joint_change
    when old.agreed is not null
    begin select raise(abort, 'an agreed change is never rewritten'); end;
    create table joint_check (
        change_id text not null references joint_change (id),
        seq integer not null,
        text text not null,
        confirmed_at text,
        primary key (change_id, seq)
    );
    alter table card add column joint_change_id text;
    alter table card add column joint_position integer;
    alter table card add column waits_for_card text;
    alter table card add column basis text not null default '{}';
    create index card_by_joint_change on card (joint_change_id, joint_position);
    alter table run add column pr_head text;
"#,
    // 15: How many tests a check skipped, as its test tools said. Null when Keel didn't recognise
    // how they say it, and on older attempts.
    r#"
    alter table attempt add column skipped_tests integer;
"#,
    // 16: Until now every project could read these toolchains in the home folder. Keel finds each
    // project's own from here on, and a person confirms them, so a project from before gets them in
    // its own list, where the person sees them, and nothing it ran stops working. A list that isn't
    // one is left as it is. A run can read folders of its own besides: what a person was trying,
    // before saving it.
    r#"
    update project set allowed_read = json_insert(allowed_read, '$[#]', '~/Library/pnpm')
    where json_valid(allowed_read) and json_type(allowed_read) = 'array'
      and not exists (select 1 from json_each(case when json_valid(project.allowed_read) then project.allowed_read else '[]' end)
                      where value = '~/Library/pnpm');
    update project set allowed_read = json_insert(allowed_read, '$[#]', '~/.cargo/bin')
    where json_valid(allowed_read) and json_type(allowed_read) = 'array'
      and not exists (select 1 from json_each(case when json_valid(project.allowed_read) then project.allowed_read else '[]' end)
                      where value = '~/.cargo/bin');
    update project set allowed_read = json_insert(allowed_read, '$[#]', '~/.rustup')
    where json_valid(allowed_read) and json_type(allowed_read) = 'array'
      and not exists (select 1 from json_each(case when json_valid(project.allowed_read) then project.allowed_read else '[]' end)
                      where value = '~/.rustup');
    alter table run add column extra_read text not null default '[]';
"#,
    // 17: Whether a project's tests may start servers on localhost and reach them, and so anything
    // else listening there. Closed until a person opens it, a project from before included.
    r#"
    alter table project add column allow_localhost integer not null default 0;
"#,
    // 18: Whether a run opens localhost to its check though its project doesn't: what a person was
    // trying, before saving it. Never for a run from before.
    r#"
    alter table run add column extra_localhost integer not null default 0;
"#,
];

/// Whether the card a query calls `card` can start as far as other cards go: it waits for none,
/// the one it waits for is gone, or that one's run has a pull request that isn't closed. Finishing
/// isn't enough, since a rejected run finishes too. A state nobody has looked at yet counts as open:
/// a delivery that was tried again has none until the next look.
fn ready(card: &str) -> String {
    format!(
        "({card}.waits_for_card is null
          or not exists (select 1 from card prev where prev.id = {card}.waits_for_card)
          or exists (select 1 from card prev join run prev_run on prev_run.id = prev.run_id
                     where prev.id = {card}.waits_for_card and prev_run.pr_url is not null
                       and coalesce(prev_run.pr_state, '') <> 'closed'))"
    )
}

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

/// How many runs an MCP client may keep open until the person says otherwise.
pub const DEFAULT_MAX_OPEN_RUNS: i64 = 3;
/// The most the person can allow, so a client can't spend their usage faster than they can follow.
pub const MAX_OPEN_RUNS_CEILING: i64 = 10;

/// An assistant allowed to reach Keel over MCP. Its token's hash is never read back.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClient {
    pub id: String,
    pub name: String,
    /// The token's first characters, to tell tokens apart.
    pub token_prefix: String,
    /// May start and stop runs, close the ones it started, and not only read.
    pub can_run: bool,
    /// May send work back and retry on the runs it started, besides `can_run`. Off unless the
    /// person turns it on.
    pub can_steer: bool,
    /// How many of its runs may be open at once, counting those that wait for the person.
    pub max_open_runs: i64,
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

const MCP_CLIENT_COLUMNS: &str = "id, name, token_prefix, can_run, can_steer, max_open_runs, created_at, last_seen_at, revoked_at";

fn mcp_client_from(r: &Row) -> rusqlite::Result<McpClient> {
    Ok(McpClient {
        id: r.get(0)?,
        name: r.get(1)?,
        token_prefix: r.get(2)?,
        can_run: r.get(3)?,
        can_steer: r.get(4)?,
        max_open_runs: r.get(5)?,
        created_at: r.get(6)?,
        last_seen_at: r.get(7)?,
        revoked_at: r.get(8)?,
    })
}

/// The tools that start a run, which is how a client's runs are told from everyone else's.
const MCP_STARTS_A_RUN: &str = "('start_run', 'start_task', 'propose_change')";

/// Whether the run `{run}` is the client `{client}`'s: it started it, or the run is a part of a
/// change across projects whose agreement it started. The parts open later, from the board, with
/// no call of the client's, and are still the client's to follow.
fn run_of_client(run: &str, client: &str) -> String {
    format!(
        "(exists (select 1 from mcp_call c
                  where c.run_id = {run} and c.client_id = {client}
                    and c.tool in {MCP_STARTS_A_RUN} and c.ok = 1)
          or exists (select 1 from card part
                     join card agreeing on agreeing.id = part.joint_change_id
                     join mcp_call c on c.run_id = agreeing.run_id
                     where part.run_id = {run} and part.joint_position is not null
                       and c.client_id = {client} and c.tool = 'propose_change' and c.ok = 1))"
    )
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
    /// Patterns for files too sensitive for a change to count as small, on top of what always is.
    pub sensitive_paths: Vec<String>,
    /// Its tests start servers on localhost, so its check and its builder's commands may reach
    /// anything there. Setup never may.
    pub allow_localhost: bool,
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
    pub sensitive_paths: Vec<String>,
    pub allow_localhost: bool,
}

const PROJECT_COLUMNS: &str = "id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env, setup_command, default_workflow, sensitive_paths, allow_localhost";

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
        sensitive_paths: list(r.get(13)?),
        allow_localhost: r.get(14)?,
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
    /// The commit its pull request had, as of `pr_checked_at`. None until Keel knows.
    pub pr_head: Option<String>,
    /// Folders its steps may read besides the project's: what a person was trying, before saving
    /// it. Empty for any other run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_read: Vec<String>,
    /// Its check gets localhost though the project doesn't open it: what a person was trying,
    /// before saving it. False for any other run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub extra_localhost: bool,
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
    /// The assistant that started it over MCP, if one did.
    pub started_by: Option<String>,
}

const RUN_COLUMNS: &str = "id, project_id, title, base_ref, request, workflow, language, source_issue, branch, pr_url, pr_number, created_at, finished_at, round, started_by, workflow_id, workflow_name, pr_state, pr_state_at, pr_checked_at, pr_head, extra_read, extra_localhost";

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
        pr_head: r.get(20)?,
        extra_read: serde_json::from_str(&r.get::<_, String>(21)?).unwrap_or_default(),
        extra_localhost: r.get(22)?,
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
    /// How many tests a check skipped, as its test tools said, though it may have passed. `None`
    /// when Keel didn't recognise how they say it, and for anything that isn't a check.
    pub skipped_tests: Option<i64>,
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
    /// Of what had to be true, how much was met, not met and not verified, as the last verifier of
    /// each finished run left it. An open run's can still change, and earlier rounds are about code
    /// that's gone. Many not verified means a step that really runs tests would be worth building.
    pub criteria_met: i64,
    pub criteria_unmet: i64,
    pub criteria_unverified: i64,
    /// Finished runs the engine last found small, so they skipped what only a bigger change needs.
    /// An open run's can still change.
    pub short_route: i64,
    /// Runs the engine found weren't small. That's final the moment it happens.
    pub moved_up: i64,
    /// Why they weren't, most common first. A run counts once per reason, and can have several.
    pub moved_up_because: Vec<MovedUpBecause>,
}

/// How many of a workflow's runs weren't small for one reason. `code` is a `WhyNotSmall`'s.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovedUpBecause {
    pub code: String,
    pub runs: i64,
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
    /// The change across several projects it belongs to, as the card where it's agreed or as one
    /// of its parts.
    pub joint_change_id: Option<String>,
    /// Which part it is, from 1. None on the card where it's agreed.
    pub joint_position: Option<i64>,
    /// The part it starts after, once that one's pull request is open.
    pub waits_for_card: Option<String>,
    /// Whether it's still waiting for that.
    pub waiting: bool,
    /// Which commit it read of each earlier part, by that part's run.
    pub basis: BTreeMap<String, String>,
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

/// A change across several projects. The card where it's agreed has the same id.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JointChange {
    pub id: String,
    /// What else it touches, as the person chose. The agreeing card's own project isn't among them.
    pub projects: Vec<String>,
    /// What was approved. None until the parts are open, and fixed from then on.
    pub agreed: Option<serde_json::Value>,
    pub created_at: String,
    /// The card of each part, first to last. A part that was deleted is no longer here.
    pub parts: Vec<String>,
    /// What only works with every part in place, which only a person can try.
    pub checks: Vec<JointCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JointCheck {
    pub seq: i64,
    pub text: String,
    /// When a person said it works. None until then.
    pub confirmed_at: Option<String>,
}

/// One project's part of an agreed change.
#[derive(Debug, Clone)]
pub struct NewPart {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub body: String,
}

fn card_from(r: &Row) -> rusqlite::Result<Card> {
    let lane: String = r.get("lane")?;
    let state: Option<String> = r.get("attempt_state")?;
    let run_state = state.as_deref().and_then(AttemptState::parse);
    let rejection: Option<String> = r.get("rejection")?;
    let basis: String = r.get("basis")?;
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
        joint_change_id: r.get("joint_change_id")?,
        joint_position: r.get("joint_position")?,
        waits_for_card: r.get("waits_for_card")?,
        waiting: !r.get::<_, bool>("is_ready")?,
        basis: serde_json::from_str(&basis).unwrap_or_default(),
    })
}

/// Cards matching a filter, by position.
fn read_cards<P: rusqlite::Params>(
    conn: &Connection,
    filter: &str,
    params: P,
) -> rusqlite::Result<Vec<Card>> {
    let ready = ready("c");
    let mut stmt = conn.prepare(&format!(
        "select c.*, r.created_at as run_started_at, r.finished_at as run_finished_at, r.pr_url as run_pr_url,
                r.pr_state as run_pr_state, r.pr_state_at as run_pr_state_at,
                a.state as attempt_state, a.reason as attempt_reason, {ready} as is_ready,
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
    bottom_of(conn, project_id, BoardColumn::Backlog)
}

fn bottom_of(conn: &Connection, project_id: &str, column: BoardColumn) -> rusqlite::Result<i64> {
    Ok(read_cards(conn, "c.project_id = ?1", [project_id])?
        .iter()
        .filter(|c| c.column() == column)
        .map(|c| c.position + 1)
        .max()
        .unwrap_or(0))
}

/// The projects a change touches besides the card's own, in the order given and without repeats.
/// One that's missing or was removed is an error.
fn chosen_projects(conn: &Connection, own: &str, projects: &[String]) -> Result<Vec<String>> {
    let mut chosen: Vec<String> = Vec::new();
    for project in projects {
        if project == own || chosen.contains(project) {
            continue;
        }
        let there: bool = conn.query_row(
            "select exists (select 1 from project where id = ?1 and removed_at is null)",
            [project],
            |r| r.get(0),
        )?;
        if !there {
            return Err(Error::NotFound(format!("project {project}")));
        }
        chosen.push(project.clone());
    }
    Ok(chosen)
}

/// Makes a card the one where a change across these projects is agreed.
fn write_joint_change(conn: &Connection, card_id: &str, chosen: &[String]) -> Result<()> {
    let chosen = serde_json::to_string(chosen).unwrap_or_default();
    conn.execute(
        "insert into joint_change (id, projects, created_at) values (?1, ?2, ?3)
         on conflict (id) do update set projects = excluded.projects",
        params![card_id, chosen, now()],
    )?;
    conn.execute(
        "update card set joint_change_id = ?1 where id = ?1",
        [card_id],
    )?;
    Ok(())
}

fn read_joint_change(conn: &Connection, id: &str) -> Result<JointChange> {
    let found = conn
        .query_row(
            "select projects, agreed, created_at from joint_change where id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((projects, agreed, created_at)) = found else {
        return Err(Error::NotFound(format!("joint change {id}")));
    };
    let mut parts = conn.prepare(
        "select id from card where joint_change_id = ?1 and joint_position is not null
         order by joint_position",
    )?;
    let parts = parts
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut checks = conn.prepare(
        "select seq, text, confirmed_at from joint_check where change_id = ?1 order by seq",
    )?;
    let checks = checks
        .query_map([id], |r| {
            Ok(JointCheck {
                seq: r.get(0)?,
                text: r.get(1)?,
                confirmed_at: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(JointChange {
        id: id.to_owned(),
        projects: serde_json::from_str(&projects).unwrap_or_default(),
        agreed: agreed.and_then(|text| serde_json::from_str(&text).ok()),
        created_at,
        parts,
        checks,
    })
}

fn insert_run(conn: &Connection, run: &NewRun) -> rusqlite::Result<()> {
    conn.execute(
        "insert into run (id, project_id, title, request, workflow, language, source_issue, created_at, workflow_id, workflow_name, started_by)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![run.id, run.project_id, run.title, run.request, run.workflow, run.language, run.source_issue, now(), run.workflow_id, run.workflow_name, run.started_by],
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
        skipped_tests: row.get("skipped_tests")?,
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

    pub fn set_mcp_client_can_steer(&self, id: &str, can_steer: bool) -> Result<McpClient> {
        self.lock().execute(
            "update mcp_client set can_steer = ?2 where id = ?1",
            rusqlite::params![id, can_steer],
        )?;
        self.mcp_client(id)
    }

    /// Kept between one and the ceiling, whatever the caller asks for.
    pub fn set_mcp_client_max_open_runs(&self, id: &str, max: i64) -> Result<McpClient> {
        self.lock().execute(
            "update mcp_client set max_open_runs = ?2 where id = ?1",
            rusqlite::params![id, max.clamp(1, MAX_OPEN_RUNS_CEILING)],
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
            &format!(
                "select count(distinct r.id) from run r
                 join mcp_call c on c.run_id = r.id
                 where c.client_id = ?1 and c.tool in {MCP_STARTS_A_RUN} and c.ok = 1
                   and r.finished_at is null"
            ),
            [client_id],
            |r| r.get(0),
        )?)
    }

    /// Whether this client started that run. A client only acts on its own runs.
    pub fn run_started_by_mcp_client(&self, run_id: &str, client_id: &str) -> Result<bool> {
        Ok(self.lock().query_row(
            &format!("select {}", run_of_client("?1", "?2")),
            [run_id, client_id],
            |r| r.get(0),
        )?)
    }

    /// The runs this client started, newest first, as `runs` has them. So a client that lost track
    /// can find its own again.
    pub fn runs_started_by_mcp_client(&self, client_id: &str, limit: i64) -> Result<Vec<RunRow>> {
        self.run_rows(&run_of_client("r.id", "?2"), params![limit, client_id])
    }

    /// How many times this client called that tool on that run and it went through. What caps how
    /// often a client can send back or retry the same run.
    pub fn mcp_calls_on_run(&self, client_id: &str, run_id: &str, tool: &str) -> Result<i64> {
        Ok(self.lock().query_row(
            "select count(*) from mcp_call where client_id = ?1 and run_id = ?2 and tool = ?3 and ok = 1",
            [client_id, run_id, tool],
            |r| r.get(0),
        )?)
    }

    pub fn add_project(&self, project: &Project) -> Result<()> {
        self.lock().execute(
            "insert into project (id, name, path, created_at, github_account, github_repo, check_command, allowed_domains, allowed_read, services, env, setup_command, default_workflow, sensitive_paths, allow_localhost)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                serde_json::to_string(&project.sensitive_paths).unwrap_or_default(),
                project.allow_localhost,
            ],
        )?;
        Ok(())
    }

    pub fn update_project(&self, id: &str, settings: &ProjectSettings) -> Result<()> {
        self.lock().execute(
            "update project set github_account = ?2, check_command = ?3, allowed_domains = ?4, allowed_read = ?5, services = ?6, env = ?7, setup_command = ?8, default_workflow = ?9, sensitive_paths = ?10, allow_localhost = ?11 where id = ?1",
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
                serde_json::to_string(&settings.sensitive_paths).unwrap_or_default(),
                settings.allow_localhost,
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

    /// A run whose steps may also read these folders, and whose check gets localhost if `localhost`,
    /// in one go, so no step starts without them.
    pub fn add_run_reading(&self, run: &NewRun, read: &[String], localhost: bool) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        insert_run(&tx, run)?;
        tx.execute(
            "update run set extra_read = ?2, extra_localhost = ?3 where id = ?1",
            params![
                run.id,
                serde_json::to_string(read).unwrap_or_else(|_| "[]".into()),
                localhost
            ],
        )?;
        tx.commit()?;
        Ok(())
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

    /// The runs that have ended, oldest first.
    pub fn ended_run_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("select id from run where finished_at is not null order by created_at")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Runs with a pull request that could still change and that nobody has looked at since `before`,
    /// the longest waiting first. A merged one can't change. A closed one can still be reopened. A
    /// removed project's runs wait until it's back.
    /// Pull requests last looked at before `before`, longest waiting first. A merged one is done, and
    /// one closed before `closed_before` is left alone: it could still be reopened, but asking about
    /// it forever would crowd out the ones that are moving.
    pub fn runs_with_pr_due(
        &self,
        before: &str,
        closed_before: &str,
        limit: i64,
    ) -> Result<Vec<Run>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select {RUN_COLUMNS} from run
             where pr_url is not null
               and coalesce(pr_state, '') <> 'merged'
               and not (pr_state = 'closed' and pr_state_at is not null and pr_state_at < ?2)
               and (pr_checked_at is null or pr_checked_at < ?1)
               and project_id in (select id from project where removed_at is null)
             order by pr_checked_at is not null, pr_checked_at, created_at
             limit ?3"
        ))?;
        let rows = stmt.query_map(params![before, closed_before, limit], run_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn workflow_results(&self, project_id: &str) -> Result<Vec<WorkflowResults>> {
        let conn = self.lock();
        let criteria = |result: &str| {
            format!(
                "coalesce(sum(case when r.finished_at is not null then
                     (select count(*) from json_each((select a.output from attempt a
                                                      where a.run_id = r.id and a.role = 'verifier' and a.state = 'succeeded'
                                                      order by a.rowid desc limit 1), '$.criteria') c
                      where json_extract(c.value, '$.result') = '{result}') end), 0)"
            )
        };
        let mut stmt = conn.prepare(&format!(
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
                                  where a.run_id = r.id and a.state = 'returned')), 0),
                    {},
                    {},
                    {},
                    coalesce(sum(case when r.finished_at is not null then
                        (select json_extract(e.data, '$.small') from event e
                         join attempt a on a.id = e.attempt_id
                         where a.run_id = r.id and e.kind = 'sizeDecided'
                         order by e.seq desc limit 1) end), 0),
                    coalesce(sum(exists (select 1 from event e join attempt a on a.id = e.attempt_id
                                         where a.run_id = r.id and e.kind = 'sizeDecided'
                                           and json_extract(e.data, '$.small') = 0)), 0)
             from run r
             where r.project_id = ?1 and r.workflow_id is not null
             group by r.workflow_id
             order by max(r.created_at) desc",
            criteria("met"),
            criteria("unmet"),
            criteria("unverified"),
        ))?;
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
                criteria_met: r.get(12)?,
                criteria_unmet: r.get(13)?,
                criteria_unverified: r.get(14)?,
                short_route: r.get(15)?,
                moved_up: r.get(16)?,
                moved_up_because: Vec::new(),
            })
        })?;
        let mut results: Vec<WorkflowResults> = rows.collect::<rusqlite::Result<_>>()?;
        let mut stmt = conn.prepare(
            "select r.workflow_id, json_extract(why.value, '$.code'), count(distinct r.id)
             from run r
             join attempt a on a.run_id = r.id
             join event e on e.attempt_id = a.id and e.kind = 'sizeDecided'
             join json_each(e.data, '$.because') why
             where r.project_id = ?1 and r.workflow_id is not null
             group by r.workflow_id, json_extract(why.value, '$.code')
             order by 3 desc, 2",
        )?;
        let rows = stmt.query_map([project_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                MovedUpBecause {
                    code: r.get(1)?,
                    runs: r.get(2)?,
                },
            ))
        })?;
        for row in rows {
            let (workflow_id, because) = row?;
            if let Some(found) = results.iter_mut().find(|w| w.workflow_id == workflow_id) {
                found.moved_up_because.push(because);
            }
        }
        Ok(results)
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
    /// `by` names the MCP client that did it, so the person sees who. None when it was them.
    pub fn return_attempt(
        &self,
        id: &str,
        run_id: &str,
        instructions: &str,
        by: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let at = now();
        let mut output = serde_json::json!({ "instructions": instructions });
        if let Some(by) = by {
            output["by"] = by.into();
        }
        let output = output.to_string();
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
        let mut data = serde_json::json!({ "instructions": instructions, "round": round });
        if let Some(by) = by {
            data["by"] = by.into();
        }
        tx.execute(
            "insert into event (attempt_id, at, kind, data) values (?1, ?2, 'returned', ?3)",
            params![id, at, data.to_string()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// A person asks for what was skipped to run before they answer: its attempt says so, and reads
    /// as neither approved nor rejected. The round stays, since nothing has to be built again. All or
    /// nothing, like `return_attempt`. False if it wasn't waiting.
    pub fn postpone_attempt(&self, id: &str, skipped: &[String]) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let at = now();
        let changed = tx.execute(
            "update attempt set state = 'postponed', reason = null, finished_at = ?2
             where id = ?1 and state = 'waitingApproval'",
            params![id, at],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        tx.execute(
            "insert into event (attempt_id, at, kind, data) values (?1, ?2, 'postponed', ?3)",
            params![id, at, serde_json::json!({ "steps": skipped }).to_string()],
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

    /// The events of one kind from all of a run's attempts, in the order they happened.
    pub fn run_events(&self, run_id: &str, kind: &str) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select e.seq, e.attempt_id, e.at, e.kind, e.data from event e join attempt a on a.id = e.attempt_id
             where a.run_id = ?1 and e.kind = ?2 order by e.seq",
        )?;
        let rows = stmt.query_map(params![run_id, kind], |r| {
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
        self.run_rows("1", params![limit])
    }

    /// Runs matching a filter on `r`, newest first. `?1` is the limit, and the filter's own
    /// parameters come after it.
    fn run_rows<P: rusqlite::Params>(&self, filter: &str, params: P) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "select r.id, r.title, r.project_id, p.name as project_name, a.step_id, a.id as attempt_id, a.number,
                    a.state, a.reason, coalesce(a.model_actual, a.model_requested) as model, a.turns,
                    a.started_at, a.finished_at, a.last_activity_at, r.pr_url, r.started_by
             from run r
             join project p on p.id = r.project_id
             join attempt a on a.run_id = r.id and a.rowid = ({ATTEMPT_THAT_MATTERS})
             where {filter}
             order by r.created_at desc limit ?1"
        ))?;
        let rows = stmt.query_map(params, |r| {
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
    /// stays in history. A part that waited for it waits for what it waited for instead. A change
    /// not agreed yet goes with the card where it would be. An agreed one stays for its parts.
    pub fn delete_card(&self, id: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let card = read_card(&tx, id)?;
        if matches!(
            card.column(),
            BoardColumn::InProgress | BoardColumn::HumanReview
        ) {
            return Ok(false);
        }
        tx.execute(
            "update card set waits_for_card = ?2 where waits_for_card = ?1",
            params![id, card.waits_for_card],
        )?;
        if card.joint_change_id.as_deref() == Some(id) {
            tx.execute(
                "delete from joint_change where id = ?1 and agreed is null",
                [id],
            )?;
        }
        tx.execute("delete from card where id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    /// Says which other projects a card's request touches, which makes it the card where the change
    /// is agreed. None makes it an ordinary card again. `false`, with nothing changed, once Keel has
    /// the card, on a part, or once the parts are open. A project that's missing or removed is an
    /// error, and the card's own isn't counted.
    pub fn set_joint_projects(&self, card_id: &str, projects: &[String]) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let card = read_card(&tx, card_id)?;
        if !card.is_waiting() || card.joint_position.is_some() {
            return Ok(false);
        }
        let agreed: Option<bool> = tx
            .query_row(
                "select agreed is not null from joint_change where id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .optional()?;
        if agreed == Some(true) {
            return Ok(false);
        }
        let chosen = chosen_projects(&tx, &card.project_id, projects)?;
        if chosen.is_empty() {
            tx.execute("delete from joint_change where id = ?1", [card_id])?;
            tx.execute(
                "update card set joint_change_id = null where id = ?1",
                [card_id],
            )?;
        } else {
            write_joint_change(&tx, card_id, &chosen)?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn joint_change(&self, id: &str) -> Result<JointChange> {
        read_joint_change(&self.lock(), id)
    }

    /// Opens the parts of an approved change: a card in Todo for each, in order, each waiting for
    /// the one before. They carry the issue but not its number, which belongs to another repo or
    /// to the card where it was agreed. Answers a person asked to have remembered there go to the
    /// Brain of each project with a part. Once is all it does: on a change already agreed it
    /// changes nothing, and gives back the parts it has.
    pub fn open_joint_parts(
        &self,
        id: &str,
        agreed: &serde_json::Value,
        parts: &[NewPart],
        checks: &[String],
    ) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let change = read_joint_change(&tx, id)?;
        if change.agreed.is_some() {
            return Ok(change.parts);
        }
        let card = read_card(&tx, id)?;
        let at = now();
        let mut opened: Vec<String> = Vec::new();
        for part in parts {
            tx.execute(
                "insert into card (id, project_id, title, body, issue, lane, position, language, todo_at,
                                   joint_change_id, joint_position, waits_for_card, created_at)
                 values (?1, ?2, ?3, ?4, (select issue from card where id = ?5), 'todo', ?6, ?7, ?8,
                         ?5, ?9, ?10, ?8)",
                params![
                    part.id,
                    part.project_id,
                    part.title,
                    part.body,
                    id,
                    bottom_of(&tx, &part.project_id, BoardColumn::Todo)?,
                    card.language,
                    at,
                    opened.len() as i64 + 1,
                    opened.last(),
                ],
            )?;
            opened.push(part.id.clone());
        }
        for (seq, text) in checks.iter().enumerate() {
            tx.execute(
                "insert into joint_check (change_id, seq, text) values (?1, ?2, ?3)",
                params![id, seq as i64 + 1, text],
            )?;
        }
        if let Some(run_id) = &card.run_id {
            let mut copied: Vec<&str> = Vec::new();
            for part in parts {
                if part.project_id == card.project_id || copied.contains(&part.project_id.as_str())
                {
                    continue;
                }
                tx.execute(
                    "insert into decision (project_id, run_id, attempt_id, at, answer, text, question)
                     select ?1, run_id, attempt_id, ?2, answer, text, question from decision
                     where run_id = ?3 and project_id = ?4 and answer = 'answered' and retired = 0
                     order by seq",
                    params![part.project_id, at, run_id, card.project_id],
                )?;
                copied.push(&part.project_id);
            }
        }
        tx.execute(
            "update joint_change set agreed = ?2 where id = ?1",
            params![id, agreed.to_string()],
        )?;
        tx.commit()?;
        Ok(opened)
    }

    /// Marks something that only works with every part in place as tried, or not tried after all.
    /// `false` if there's no such thing. Trying it again keeps when it was first said to work.
    pub fn confirm_joint_check(&self, id: &str, seq: i64, works: bool) -> Result<bool> {
        let changed = self.lock().execute(
            "update joint_check set confirmed_at = case when ?3 then coalesce(confirmed_at, ?4) end
             where change_id = ?1 and seq = ?2",
            params![id, seq, works, now()],
        )?;
        Ok(changed > 0)
    }

    /// Lets a part start without the one before it. `false`, with nothing changed, once Keel has
    /// it or if it waits for nothing.
    pub fn stop_waiting(&self, card_id: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let card = read_card(&tx, card_id)?;
        if !card.is_waiting() || card.waits_for_card.is_none() {
            return Ok(false);
        }
        tx.execute(
            "update card set waits_for_card = null where id = ?1",
            [card_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Notes which commit a part read of an earlier part, by that part's run. What it read before
    /// stays, and a run read again replaces its commit.
    pub fn add_card_basis(&self, card_id: &str, run_id: &str, commit: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut basis = read_card(&tx, card_id)?.basis;
        basis.insert(run_id.to_owned(), commit.to_owned());
        tx.execute(
            "update card set basis = ?2 where id = ?1",
            params![card_id, serde_json::to_string(&basis).unwrap_or_default()],
        )?;
        tx.commit()?;
        Ok(())
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

    /// Cards that could start now. A removed project's wait where they are until it comes back, and
    /// a part until the one before it has a pull request.
    pub fn has_todo_cards(&self) -> Result<bool> {
        Ok(self.lock().query_row(
            &format!(
                "select exists (select 1 from card c join project p on p.id = c.project_id
                                where c.lane = 'todo' and p.removed_at is null and {})",
                ready("c")
            ),
            [],
            |r| r.get(0),
        )?)
    }

    /// Takes the card to start next out of Todo, and returns it as it is at that moment: the top
    /// one of each project's Todo, and of those, whichever has waited longest. After this nobody
    /// can drag it away or edit it, and it can't start twice. Never a removed project's, nor a part
    /// still waiting for the one before it, which doesn't hold up the cards below it either.
    pub fn claim_next_todo_card(&self) -> Result<Option<Card>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let tops = read_cards(
            &tx,
            &format!(
                "c.lane = 'todo' and {}
                 and c.project_id in (select id from project where removed_at is null)
                 and not exists (
                    select 1 from card o where o.project_id = c.project_id and o.lane = 'todo' and {}
                      and (o.position, o.created_at, o.id) < (c.position, c.created_at, c.id))",
                ready("c"),
                ready("o")
            ),
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
    /// still open. With `joint`, the card is where a change across those projects is agreed, and
    /// that's written with it, so no card is left without it: a project that's missing or removed is
    /// an error with nothing created, and so is having none besides the card's own. An issue whose
    /// card is where a change across projects is agreed only starts from that card, so neither
    /// kind of run takes it over: `AcrossProjects`, with nothing changed. That's looked at here,
    /// with the card written in the same transaction, so «Also touches» can't slip in between.
    pub fn add_started_card(
        &self,
        card: &NewCard,
        language: &str,
        joint: &[String],
    ) -> Result<Option<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let chosen = if joint.is_empty() {
            Vec::new()
        } else {
            let chosen = chosen_projects(&tx, &card.project_id, joint)?;
            if chosen.is_empty() {
                return Err(Error::NotFound("another project".into()));
            }
            chosen
        };
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
            Some(existing) if existing.joint_change_id.is_some() => {
                return Err(Error::AcrossProjects);
            }
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
        if !chosen.is_empty() {
            write_joint_change(&tx, &id, &chosen)?;
        }
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
                           in ('none', 'succeeded', 'skipped', 'returned', 'postponed'))"
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
                sensitive_paths: vec![],
                allow_localhost: false,
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
                started_by: None,
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
    fn a_runs_events_of_one_kind_come_from_all_its_attempts_in_order() {
        let (_dir, store) = store();
        let id = seeded(&store);
        store
            .add_attempt(&new_attempt("b", "r", "check", AttemptState::Running))
            .unwrap();
        store
            .add_run(&NewRun {
                id: "other".into(),
                project_id: "p".into(),
                title: "other".into(),
                request: "other".into(),
                workflow: "[]".into(),
                language: "en".into(),
                source_issue: None,
                workflow_id: None,
                workflow_name: None,
                started_by: None,
            })
            .unwrap();
        store
            .add_attempt(&new_attempt("c", "other", "check", AttemptState::Running))
            .unwrap();
        for (attempt, kind, n) in [
            ("b", "shot", 1),
            (id.as_str(), "note", 2),
            (id.as_str(), "shot", 3),
            ("c", "shot", 4),
        ] {
            store
                .append_event(attempt, kind, &serde_json::json!({ "n": n }))
                .unwrap();
        }
        let found: Vec<_> = store
            .run_events("r", "shot")
            .unwrap()
            .into_iter()
            .map(|event| (event.attempt_id, event.data["n"].as_i64().unwrap()))
            .collect();
        assert_eq!(found, [("b".to_owned(), 1), ("a".to_owned(), 3)]);
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
                 alter table run drop column extra_localhost;
                 alter table project drop column allow_localhost;
                 alter table run drop column extra_read;
                 alter table attempt drop column skipped_tests;
                 alter table run drop column pr_head;
                 drop table joint_check;
                 drop trigger joint_change_agreed_fixed;
                 drop table joint_change;
                 alter table project drop column sensitive_paths;
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
                    sensitive_paths: vec!["billing/".into()],
                    allow_localhost: true,
                },
            )
            .unwrap();
        let project = store.project("p").unwrap();
        assert!(project.allow_localhost);
        assert_eq!(project.sensitive_paths, ["billing/"]);
        assert_eq!(project.default_workflow.as_deref(), Some("builtin:simple"));
        assert_eq!(project.github_account.as_deref(), Some("octo"));
        assert_eq!(project.setup_command.as_deref(), Some("uv sync"));
        assert_eq!(project.check_command.as_deref(), Some("make test"));
        assert_eq!(project.allowed_domains, ["pypi.org"]);
        assert_eq!(project.allowed_read, ["~/.cargo"]);
        assert_eq!(project.services[0].name(), "db");
        assert_eq!(project.env[0].name, "DB");
    }

    fn project_reading(store: &Store, id: &str, read: &[&str]) {
        store
            .add_project(&Project {
                id: id.into(),
                name: id.into(),
                path: format!("/{id}"),
                created_at: now(),
                github_account: None,
                github_repo: None,
                setup_command: None,
                check_command: None,
                allowed_domains: vec![],
                allowed_read: read.iter().map(|path| (*path).to_owned()).collect(),
                services: vec![],
                env: vec![],
                default_workflow: None,
                sensitive_paths: vec![],
                allow_localhost: false,
            })
            .unwrap();
    }

    /// #120: every project could read these until Keel found each one's own toolchains. A project
    /// from before keeps them in its own list, where the person sees them, so nothing it ran stops
    /// working. Once, and a project added later gets none.
    #[test]
    fn projects_from_before_keep_the_toolchain_folders_every_project_had() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        {
            let store = Store::open(&path).unwrap();
            seeded(&store);
            project_reading(&store, "q", &["~/.nvm", "~/.cargo/bin"]);
            project_reading(&store, "gone", &[]);
            store.remove_project("gone").unwrap();
            project_reading(&store, "bad", &[]);
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "pragma user_version = 15;
                 alter table run drop column extra_localhost;
                 alter table project drop column allow_localhost;
                 alter table run drop column extra_read;
                 update project set allowed_read = 'not a list' where id = 'bad';",
            )
            .unwrap();
        }
        let every = ["~/Library/pnpm", "~/.cargo/bin", "~/.rustup"];
        let store = Store::open(&path).unwrap();
        assert_eq!(store.project("p").unwrap().allowed_read, every);
        assert_eq!(
            store.project("q").unwrap().allowed_read,
            ["~/.nvm", "~/.cargo/bin", "~/Library/pnpm", "~/.rustup"]
        );
        assert_eq!(store.project("gone").unwrap().allowed_read, every);
        let bad: String = store
            .lock()
            .query_row(
                "select allowed_read from project where id = 'bad'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, "not a list");
        assert!(store.run("r").unwrap().extra_read.is_empty());

        project_reading(&store, "new", &[]);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.project("p").unwrap().allowed_read, every);
        assert!(store.project("new").unwrap().allowed_read.is_empty());
    }

    /// #123: localhost is closed to every project until a person opens it, a project from before
    /// included, and a person can close it again.
    #[test]
    fn localhost_is_closed_unless_a_person_opens_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        {
            let store = Store::open(&path).unwrap();
            seeded(&store);
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "pragma user_version = 16;
                 alter table run drop column extra_localhost;
                 alter table project drop column allow_localhost;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(!store.project("p").unwrap().allow_localhost);
        project_reading(&store, "new", &[]);
        assert!(!store.project("new").unwrap().allow_localhost);

        let open = ProjectSettings {
            allow_localhost: true,
            ..ProjectSettings::default()
        };
        store.update_project("new", &open).unwrap();
        assert!(store.project("new").unwrap().allow_localhost);
        assert!(!store.project("p").unwrap().allow_localhost);
        store
            .update_project("new", &ProjectSettings::default())
            .unwrap();
        assert!(!store.project("new").unwrap().allow_localhost);
    }

    /// Trying commands before saving them, a person can let them read more than the project does,
    /// and open localhost to them. Only that run gets it.
    #[test]
    fn a_run_can_read_folders_of_its_own_and_the_project_is_left_as_it_was() {
        let (_dir, store) = store();
        seeded(&store);
        let run = store.run("r").unwrap();
        assert!(run.extra_read.is_empty());
        assert!(!run.extra_localhost);
        let trial = |id: &str| NewRun {
            id: id.into(),
            project_id: "p".into(),
            title: "try".into(),
            request: "try".into(),
            workflow: "[]".into(),
            language: "en".into(),
            source_issue: None,
            workflow_id: None,
            workflow_name: None,
            started_by: None,
        };
        store
            .add_run_reading(&trial("t"), &["~/.nvm".to_owned()], false)
            .unwrap();
        let run = store.run("t").unwrap();
        assert_eq!(run.extra_read, ["~/.nvm"]);
        assert!(!run.extra_localhost);
        store.add_run_reading(&trial("u"), &[], true).unwrap();
        let run = store.run("u").unwrap();
        assert!(run.extra_read.is_empty());
        assert!(run.extra_localhost);
        let project = store.project("p").unwrap();
        assert!(project.allowed_read.is_empty());
        assert!(!project.allow_localhost);
        let run = store.run("r").unwrap();
        assert!(run.extra_read.is_empty());
        assert!(!run.extra_localhost);
    }

    /// #124: a run from before opens no localhost of its own.
    #[test]
    fn runs_from_before_open_no_localhost_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        {
            let store = Store::open(&path).unwrap();
            seeded(&store);
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "pragma user_version = 17;
                 alter table run drop column extra_localhost;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(!store.run("r").unwrap().extra_localhost);
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
        let due = store.runs_with_pr_due(&now(), "0", 10).unwrap();
        assert_eq!(due.len(), 1);
    }

    /// A run in project `p` with a pull request Keel last looked at then, or not yet.
    fn run_with_pr(store: &Store, id: &str, state: Option<&str>, checked: Option<&str>) {
        run_with_pr_at(store, id, state, None, checked);
    }

    /// The same, saying when it got to that state.
    fn run_with_pr_at(
        store: &Store,
        id: &str,
        state: Option<&str>,
        state_at: Option<&str>,
        checked: Option<&str>,
    ) {
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
                started_by: None,
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
                    ("pr_state_at", text(state_at)),
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
        run_with_pr_at(
            &store,
            "closed",
            Some("closed"),
            Some("2026-05-20T00:00:00Z"),
            Some("2026-02-01T00:00:00.000Z"),
        );
        run_with_pr_at(
            &store,
            "closedLongAgo",
            Some("closed"),
            Some("2026-04-01T00:00:00Z"),
            Some("2025-01-01T00:00:00.000Z"),
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
        let closed_before = "2026-05-01T00:00:00.000Z";
        let due = |limit| -> Vec<String> {
            store
                .runs_with_pr_due(before, closed_before, limit)
                .unwrap()
                .into_iter()
                .map(|run| run.id)
                .collect()
        };
        // Never looked at first, then the longest waiting. `r` has no pull request at all, and one
        // closed long enough ago isn't looked at however long it has waited.
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
                    started_by: None,
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
                criteria_met: 0,
                criteria_unmet: 0,
                criteria_unverified: 0,
                short_route: 0,
                moved_up: 0,
                moved_up_because: vec![],
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
    fn results_count_what_the_last_verifier_of_each_finished_run_left() {
        let (_dir, store) = store();
        seeded(&store);
        let add_run = |id: &str, finished: bool| {
            store
                .add_run(&NewRun {
                    id: id.into(),
                    project_id: "p".into(),
                    title: id.into(),
                    request: id.into(),
                    workflow: "[]".into(),
                    language: "en".into(),
                    source_issue: None,
                    workflow_id: Some("w".into()),
                    workflow_name: Some("W".into()),
                    started_by: None,
                })
                .unwrap();
            if finished {
                store
                    .patch_run(id, &[("finished_at", Value::Text(now()))])
                    .unwrap();
            }
        };
        let verifier = |id: &str, run: &str, state: AttemptState, results: &[&str]| {
            let mut attempt = new_attempt(id, run, "verify", state);
            attempt.role = "verifier".into();
            store.add_attempt(&attempt).unwrap();
            let criteria: Vec<serde_json::Value> = results
                .iter()
                .map(|result| serde_json::json!({ "id": "C", "when": "w", "then": "t", "result": result, "evidence": "" }))
                .collect();
            let output = serde_json::json!({ "criteria": criteria, "summary": "s" }).to_string();
            store
                .patch_attempt(id, &[("output", Value::Text(output))])
                .unwrap();
        };
        add_run("one", true);
        // An earlier round's answer is about code that's gone.
        verifier("one-a", "one", AttemptState::Succeeded, &["unmet", "unmet"]);
        verifier(
            "one-b",
            "one",
            AttemptState::Succeeded,
            &["met", "unverified"],
        );
        // It didn't succeed, so nobody corrected it, and what the agent said isn't counted.
        verifier("one-c", "one", AttemptState::Failed, &["met", "met", "met"]);
        add_run("two", true);
        verifier(
            "two-a",
            "two",
            AttemptState::Succeeded,
            &["met", "unmet", "unverified"],
        );
        add_run("open", false);
        verifier("open-a", "open", AttemptState::Succeeded, &["met", "met"]);
        add_run("no-verifier", true);

        let results = store.workflow_results("p").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].runs, 4);
        assert_eq!(
            (
                results[0].criteria_met,
                results[0].criteria_unmet,
                results[0].criteria_unverified
            ),
            (2, 1, 2)
        );
    }

    #[test]
    fn results_count_the_short_route_when_it_is_final_and_moving_up_by_reason() {
        let (_dir, store) = store();
        seeded(&store);
        let add_run = |id: &str, finished: bool| {
            store
                .add_run(&NewRun {
                    id: id.into(),
                    project_id: "p".into(),
                    title: id.into(),
                    request: id.into(),
                    workflow: "[]".into(),
                    language: "en".into(),
                    source_issue: None,
                    workflow_id: Some("w".into()),
                    workflow_name: Some("W".into()),
                    started_by: None,
                })
                .unwrap();
            if finished {
                store
                    .patch_run(id, &[("finished_at", Value::Text(now()))])
                    .unwrap();
            }
        };
        let decided = |run: &str, attempt: &str, data: serde_json::Value| {
            store
                .add_attempt(&new_attempt(attempt, run, "review", AttemptState::Skipped))
                .unwrap();
            store.append_event(attempt, "sizeDecided", &data).unwrap();
        };
        let small = serde_json::json!({ "small": true, "lines": 3, "files": 1, "round": 0 });
        let up = |codes: &[&str]| {
            let because: Vec<serde_json::Value> = codes
                .iter()
                .map(|code| serde_json::json!({ "code": code }))
                .collect();
            serde_json::json!({ "small": false, "because": because, "round": 0 })
        };
        add_run("short", true);
        decided("short", "a1", small.clone());
        // Sent back after being small, and found otherwise the second time.
        add_run("changed-its-mind", true);
        decided("changed-its-mind", "a2", small.clone());
        decided("changed-its-mind", "a3", up(&["tooManyLines", "noCheck"]));
        add_run("twice", true);
        decided("twice", "a4", up(&["tooManyLines"]));
        decided("twice", "a5", up(&["tooManyLines"]));
        // Still going, so being small doesn't count yet. Going up already does.
        add_run("open-small", false);
        decided("open-small", "a6", small);
        add_run("open-up", false);
        decided("open-up", "a7", up(&["personAsked"]));
        // Nothing in its workflow asks.
        add_run("plain", true);

        let results = store.workflow_results("p").unwrap();
        assert_eq!(results[0].runs, 6);
        assert_eq!(results[0].short_route, 1);
        assert_eq!(results[0].moved_up, 3);
        assert_eq!(
            results[0].moved_up_because,
            [
                MovedUpBecause {
                    code: "tooManyLines".into(),
                    runs: 2
                },
                MovedUpBecause {
                    code: "noCheck".into(),
                    runs: 1
                },
                MovedUpBecause {
                    code: "personAsked".into(),
                    runs: 1
                },
            ]
        );
    }

    #[test]
    fn asking_for_what_was_skipped_reads_as_neither_answer_and_only_happens_once() {
        let (_dir, store) = store();
        seeded(&store);
        store
            .add_attempt(&new_attempt(
                "h",
                "r",
                "human",
                AttemptState::WaitingApproval,
            ))
            .unwrap();
        let steps = vec!["security".to_owned(), "approver".to_owned()];
        assert!(store.postpone_attempt("h", &steps).unwrap());
        let attempt = store.attempt("h").unwrap();
        assert_eq!(attempt.state, "postponed");
        assert!(attempt.finished_at.is_some());
        assert_eq!(store.run("r").unwrap().round, 0, "nothing is built again");
        let events = store.events("h", 0).unwrap();
        assert_eq!(events[0].kind, "postponed");
        assert_eq!(events[0].data["steps"][1], "approver");
        // Whichever answer got there first stands.
        assert!(!store.postpone_attempt("h", &steps).unwrap());
        assert_eq!(store.events("h", 0).unwrap().len(), 1);
        store
            .add_attempt(&new_attempt("g", "r", "other", AttemptState::Succeeded))
            .unwrap();
        assert!(!store.postpone_attempt("g", &steps).unwrap());
        assert_eq!(store.attempt("g").unwrap().state, "succeeded");
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
        assert!(!store.return_attempt(&id, "r", "Change it", None).unwrap());
        assert_eq!(store.run("r").unwrap().round, 0, "it wasn't waiting");

        store
            .set_state(&id, AttemptState::WaitingApproval, None)
            .unwrap();
        assert!(store.return_attempt(&id, "r", "Change it", None).unwrap());
        let attempt = store.attempt(&id).unwrap();
        assert_eq!(attempt.state, "returned");
        assert_eq!(
            attempt.output.unwrap(),
            serde_json::json!({ "instructions": "Change it" })
        );
        assert!(attempt.finished_at.is_some());
        assert_eq!(store.run("r").unwrap().round, 1);
        let events = store.events(&id, 0).unwrap();
        assert_eq!(events[0].kind, "returned");
        assert_eq!(
            events[0].data,
            serde_json::json!({ "instructions": "Change it", "round": 1 })
        );

        assert!(!store.return_attempt(&id, "r", "Again", None).unwrap());
        assert_eq!(store.run("r").unwrap().round, 1);
    }

    #[test]
    fn work_a_client_sent_back_says_which_client_it_was() {
        let (_dir, store) = store();
        let id = seeded(&store);
        store
            .set_state(&id, AttemptState::WaitingApproval, None)
            .unwrap();
        assert!(
            store
                .return_attempt(&id, "r", "Change it", Some("Claude Code"))
                .unwrap()
        );
        assert_eq!(
            store.attempt(&id).unwrap().output.unwrap(),
            serde_json::json!({ "instructions": "Change it", "by": "Claude Code" })
        );
        assert_eq!(
            store.events(&id, 0).unwrap()[0].data,
            serde_json::json!({ "instructions": "Change it", "round": 1, "by": "Claude Code" })
        );
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
                sensitive_paths: vec![],
                allow_localhost: false,
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
            // What was skipped is on its way.
            (AttemptState::Postponed, 1),
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
                started_by: None,
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
            started_by: None,
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
                        started_by: None,
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
                sensitive_paths: vec![],
                allow_localhost: false,
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
                 alter table run drop column extra_localhost;
                 alter table project drop column allow_localhost;
                 alter table run drop column extra_read;
                 alter table attempt drop column skipped_tests;
                 alter table run drop column pr_head;
                 drop index card_by_joint_change;
                 alter table card drop column basis;
                 alter table card drop column waits_for_card;
                 alter table card drop column joint_position;
                 alter table card drop column joint_change_id;
                 drop table joint_check;
                 drop trigger joint_change_agreed_fixed;
                 drop table joint_change;
                 alter table mcp_client drop column max_open_runs;
                 alter table mcp_client drop column can_steer;
                 alter table project drop column sensitive_paths;
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
        assert_eq!(
            store.project("p").unwrap().sensitive_paths,
            Vec::<String>::new()
        );
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

    #[test]
    fn a_client_from_before_feedback_cannot_give_it_and_keeps_three_runs_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        // Exactly what the last release left, with a client that can run.
        let before = 12;
        {
            let conn = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute(
                "insert into mcp_client (id, name, token_hash, token_prefix, can_run, created_at)
                 values ('c1', 'Claude Code', ?1, 'keel_abc', 1, 'then')",
                [&[7u8; 32][..]],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let client = store.mcp_client("c1").unwrap();
        assert!(client.can_run);
        assert!(!client.can_steer);
        assert_eq!(client.max_open_runs, DEFAULT_MAX_OPEN_RUNS);
        assert_eq!(client.max_open_runs, 3);
        assert_eq!(store.mcp_client_by_token(&[7u8; 32]).unwrap(), Some(client));
    }

    #[test]
    fn a_person_lets_a_client_give_feedback_and_keep_more_runs_open_within_bounds() {
        let (_dir, store) = store();
        let client = store
            .add_mcp_client("c1", "Claude Code", &[7u8; 32], "keel_abc", true)
            .unwrap();
        assert!(!client.can_steer, "off until the person turns it on");
        assert_eq!(client.max_open_runs, DEFAULT_MAX_OPEN_RUNS);

        assert!(
            store
                .set_mcp_client_can_steer("c1", true)
                .unwrap()
                .can_steer
        );
        assert!(store.mcp_client("c1").unwrap().can_steer);
        assert!(
            !store
                .set_mcp_client_can_steer("c1", false)
                .unwrap()
                .can_steer
        );

        let max = |n| {
            store
                .set_mcp_client_max_open_runs("c1", n)
                .unwrap()
                .max_open_runs
        };
        assert_eq!(max(7), 7);
        assert_eq!(max(50), MAX_OPEN_RUNS_CEILING);
        assert_eq!(max(0), 1);
        assert_eq!(max(-4), 1);
        assert!(store.mcp_client("c1").unwrap().can_run, "the rest stays");

        let json = serde_json::to_value(store.mcp_client("c1").unwrap()).unwrap();
        assert_eq!(json["canSteer"], false);
        assert_eq!(json["maxOpenRuns"], 1);
    }

    #[test]
    fn a_client_owns_only_the_runs_it_started_and_only_calls_that_went_through_count() {
        let (_dir, store) = store();
        seeded(&store);
        for (id, at) in [
            ("task", "2026-01-02T00:00:00.000Z"),
            ("other", "2026-01-03T00:00:00.000Z"),
        ] {
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
                    started_by: None,
                })
                .unwrap();
            store
                .add_attempt(&new_attempt(id, id, "work", AttemptState::Running))
                .unwrap();
            store
                .patch_run(id, &[("created_at", Value::Text(at.into()))])
                .unwrap();
        }
        store
            .patch_run(
                "r",
                &[("created_at", Value::Text("2026-01-01T00:00:00.000Z".into()))],
            )
            .unwrap();
        let mine = store
            .add_mcp_client("c1", "Claude Code", &[7u8; 32], "keel_abc", true)
            .unwrap();
        let theirs = store
            .add_mcp_client("c2", "Other", &[8u8; 32], "keel_def", true)
            .unwrap();
        let call = |client: &McpClient, tool: &str, run: &str, error: Option<&str>| {
            store
                .add_mcp_call(&NewMcpCall {
                    client,
                    tool,
                    args: "{}",
                    error,
                    run_id: Some(run),
                    duration_ms: 1,
                })
                .unwrap();
        };
        call(&mine, "start_run", "r", None);
        call(&mine, "start_task", "task", None);
        call(&mine, "start_run", "other", Some("too many open"));
        call(&theirs, "start_run", "other", None);
        call(&mine, "send_back", "r", None);
        call(&mine, "send_back", "r", None);
        call(&mine, "send_back", "r", Some("nothing waits for approval"));
        call(&theirs, "send_back", "r", None);
        call(&mine, "retry_step", "r", None);

        assert!(store.run_started_by_mcp_client("r", "c1").unwrap());
        assert!(
            store.run_started_by_mcp_client("task", "c1").unwrap(),
            "a read-only task is theirs too"
        );
        assert!(
            !store.run_started_by_mcp_client("other", "c1").unwrap(),
            "it was refused"
        );
        assert!(store.run_started_by_mcp_client("other", "c2").unwrap());
        assert!(
            !store.run_started_by_mcp_client("r", "c2").unwrap(),
            "acting on a run doesn't make it yours"
        );
        assert!(!store.run_started_by_mcp_client("r", "nobody").unwrap());

        assert_eq!(store.mcp_calls_on_run("c1", "r", "send_back").unwrap(), 2);
        assert_eq!(store.mcp_calls_on_run("c2", "r", "send_back").unwrap(), 1);
        assert_eq!(store.mcp_calls_on_run("c1", "r", "retry_step").unwrap(), 1);
        assert_eq!(
            store.mcp_calls_on_run("c1", "task", "send_back").unwrap(),
            0
        );

        let ids = |client: &str| {
            store
                .runs_started_by_mcp_client(client, 10)
                .unwrap()
                .into_iter()
                .map(|run| run.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("c1"), ["task", "r"], "newest first");
        assert_eq!(ids("c2"), ["other"]);
        assert_eq!(
            store.runs_started_by_mcp_client("c1", 1).unwrap()[0].id,
            "task"
        );

        assert_eq!(store.open_runs_of_mcp_client("c1").unwrap(), 2);
        store
            .patch_run("task", &[("finished_at", Value::Text(now()))])
            .unwrap();
        assert_eq!(store.open_runs_of_mcp_client("c1").unwrap(), 1);
        assert_eq!(ids("c1"), ["task", "r"], "finished ones stay theirs");
    }

    /// A project next to the seeded one.
    fn other_project(store: &Store, id: &str) {
        let mut project = store.project("p").unwrap();
        project.id = id.into();
        project.name = id.into();
        project.path = format!("/{id}");
        store.add_project(&project).unwrap();
    }

    fn part(id: &str, project: &str) -> NewPart {
        NewPart {
            id: id.into(),
            project_id: project.into(),
            title: id.into(),
            body: String::new(),
        }
    }

    /// A card in `p` where a change across `q` and `s` is agreed, with `parts` open.
    fn agreed(store: &Store, parts: &[(&str, &str)]) {
        other_project(store, "q");
        other_project(store, "s");
        card(store, "plan");
        assert!(
            store
                .set_joint_projects("plan", &["q".into(), "s".into()])
                .unwrap()
        );
        let parts: Vec<NewPart> = parts.iter().map(|(id, p)| part(id, p)).collect();
        store
            .open_joint_parts("plan", &serde_json::json!({}), &parts, &[])
            .unwrap();
    }

    /// Gives a card a finished run, with a pull request if `pr` says so: in that state, or in one
    /// Keel hasn't looked at yet.
    fn ran(store: &Store, card: &str, run: &str, pr: Option<Option<&str>>) {
        store
            .add_run(&NewRun {
                id: run.into(),
                project_id: store.card(card).unwrap().project_id,
                title: run.into(),
                request: run.into(),
                workflow: "[]".into(),
                language: "en".into(),
                source_issue: None,
                workflow_id: None,
                workflow_name: None,
                started_by: None,
            })
            .unwrap();
        let mut fields = vec![("finished_at", Value::Text(now()))];
        if let Some(state) = pr {
            fields.push((
                "pr_url",
                Value::Text(format!("https://github.com/acme/{run}/pull/1")),
            ));
            fields.push((
                "pr_state",
                state.map_or(Value::Null, |s| Value::Text(s.into())),
            ));
        }
        store.patch_run(run, &fields).unwrap();
        store
            .lock()
            .execute(
                "update card set lane = 'started', run_id = ?2 where id = ?1",
                params![card, run],
            )
            .unwrap();
    }

    #[test]
    fn a_change_across_projects_is_agreed_on_one_card_and_opens_its_parts_once() {
        let (_dir, store) = store();
        seeded(&store);
        other_project(&store, "q");
        other_project(&store, "s");
        let issue = r#"{"number":434,"title":"Log in","body":"","url":"https://github.com/acme/back/issues/434"}"#;
        store
            .add_card(&NewCard {
                id: "plan".into(),
                project_id: "p".into(),
                title: "Log in".into(),
                body: String::new(),
                issue: Some(issue.into()),
                issue_number: Some(434),
            })
            .unwrap();

        // The card's own project and repeats don't count, and a project that isn't there is refused.
        let chosen = ["q", "p", "q", "s"].map(String::from);
        assert!(store.set_joint_projects("plan", &chosen).unwrap());
        assert_eq!(store.joint_change("plan").unwrap().projects, ["q", "s"]);
        assert!(matches!(
            store.set_joint_projects("plan", &["nope".into()]),
            Err(Error::NotFound(_))
        ));
        assert_eq!(store.joint_change("plan").unwrap().projects, ["q", "s"]);
        let plan = store.card("plan").unwrap();
        assert_eq!(plan.joint_change_id.as_deref(), Some("plan"));
        assert_eq!(plan.joint_position, None);

        // It runs as any card does, and the person answers a question there.
        assert!(store.move_card("plan", CardLane::Todo, 0, "es").unwrap());
        assert_eq!(claim(&store).as_deref(), Some("plan"));
        store
            .lock()
            .execute("update card set run_id = 'r' where id = 'plan'", [])
            .unwrap();
        store
            .add_answered_decision(
                "p",
                "r",
                "a",
                "Which channel carries the token?",
                "A header",
            )
            .unwrap();
        store
            .add_decision("p", "r", "a", "approve", "go ahead")
            .unwrap();

        let agreed = serde_json::json!({"summary": "log in through Valora"});
        let opened = store
            .open_joint_parts(
                "plan",
                &agreed,
                &[part("valora", "q"), part("back", "p")],
                &["Log in end to end".into()],
            )
            .unwrap();
        assert_eq!(opened, ["valora", "back"]);
        let first = store.card("valora").unwrap();
        let second = store.card("back").unwrap();
        for (card, position) in [(&first, 1), (&second, 2)] {
            assert_eq!(card.column(), BoardColumn::Todo);
            assert_eq!(card.joint_change_id.as_deref(), Some("plan"));
            assert_eq!(card.joint_position, Some(position));
            assert_eq!(card.language.as_deref(), Some("es"));
            // The issue comes along, but its number belongs to another card.
            assert_eq!(card.issue, plan.issue);
            assert_eq!(card.issue_number, None);
            assert!(card.basis.is_empty());
        }
        assert_eq!(first.waits_for_card, None);
        assert!(!first.waiting);
        assert_eq!(second.waits_for_card.as_deref(), Some("valora"));
        assert!(second.waiting);

        let change = store.joint_change("plan").unwrap();
        assert_eq!(change.agreed, Some(agreed.clone()));
        assert_eq!(change.parts, ["valora", "back"]);
        assert_eq!(
            change.checks,
            [JointCheck {
                seq: 1,
                text: "Log in end to end".into(),
                confirmed_at: None,
            }]
        );

        // The answer they asked to have remembered reaches the other project, and nothing else does.
        let elsewhere = store.decisions("q").unwrap();
        assert_eq!(elsewhere.len(), 1);
        assert_eq!(elsewhere[0].answer, "answered");
        assert_eq!(elsewhere[0].text, "A header");
        assert_eq!(
            elsewhere[0].question.as_deref(),
            Some("Which channel carries the token?")
        );
        assert!(store.decisions("s").unwrap().is_empty(), "s has no part");
        assert_eq!(
            store.decisions("p").unwrap().len(),
            2,
            "nothing copied twice"
        );

        // Opening again changes nothing, and what was agreed stays as it was.
        let again = store
            .open_joint_parts("plan", &serde_json::json!({}), &[part("more", "s")], &[])
            .unwrap();
        assert_eq!(again, ["valora", "back"]);
        assert!(matches!(store.card("more"), Err(Error::NotFound(_))));
        assert_eq!(store.decisions("q").unwrap().len(), 1);
        assert_eq!(store.joint_change("plan").unwrap().agreed, Some(agreed));
        assert!(
            store
                .lock()
                .execute("update joint_change set projects = '[]'", [])
                .is_err()
        );
        // A part is never where a change is agreed.
        assert!(!store.set_joint_projects("back", &["s".into()]).unwrap());
    }

    #[test]
    fn an_issue_that_became_a_change_across_projects_meanwhile_does_not_start_from_outside() {
        let (_dir, store) = store();
        seeded(&store);
        other_project(&store, "q");
        let issue = |id: &str| NewCard {
            id: id.into(),
            project_id: "p".into(),
            title: "Log in".into(),
            body: String::new(),
            issue: Some(r#"{"number":434}"#.into()),
            issue_number: Some(434),
        };
        store.add_card(&issue("plan")).unwrap();
        // The person marks «Also touches» after the engine looked at the board, and before the
        // run's card is written.
        assert!(store.set_joint_projects("plan", &["q".into()]).unwrap());

        // Neither as an ordinary run nor proposed again: only its card starts it.
        for joint in [vec![], vec!["q".to_owned()]] {
            let started = store.add_started_card(&issue("outside"), "en", &joint);
            assert!(matches!(started, Err(Error::AcrossProjects)), "{started:?}");
        }
        let cards = store.cards("p").unwrap();
        assert_eq!(cards.len(), 1, "no second card");
        assert_eq!(cards[0].column(), BoardColumn::Backlog);
        assert_eq!(cards[0].language, None);
        assert_eq!(store.joint_change("plan").unwrap().projects, ["q"]);
    }

    #[test]
    fn only_a_person_says_what_works_together_and_can_take_it_back() {
        let (_dir, store) = store();
        seeded(&store);
        other_project(&store, "q");
        card(&store, "plan");
        store.set_joint_projects("plan", &["q".into()]).unwrap();
        store
            .open_joint_parts(
                "plan",
                &serde_json::json!({}),
                &[part("one", "q")],
                &["first".into(), "second".into()],
            )
            .unwrap();

        assert!(store.confirm_joint_check("plan", 2, true).unwrap());
        let checks = store.joint_change("plan").unwrap().checks;
        assert_eq!(checks[0].confirmed_at, None);
        let when = checks[1].confirmed_at.clone().expect("confirmed");
        assert!(store.confirm_joint_check("plan", 2, true).unwrap());
        assert_eq!(
            store.joint_change("plan").unwrap().checks[1].confirmed_at,
            Some(when),
            "trying it again keeps when it first worked"
        );
        assert!(store.confirm_joint_check("plan", 2, false).unwrap());
        assert_eq!(
            store.joint_change("plan").unwrap().checks[1].confirmed_at,
            None
        );
        assert!(!store.confirm_joint_check("plan", 3, true).unwrap());
    }

    #[test]
    fn a_part_starts_once_the_one_before_has_a_pull_request_nobody_closed() {
        let (_dir, store) = store();
        seeded(&store);
        agreed(&store, &[("one", "q"), ("two", "p")]);
        // An ordinary card under the waiting part, in the same project.
        card(&store, "later");
        assert!(store.move_card("later", CardLane::Todo, 1, "en").unwrap());
        assert_eq!(ids(&store, BoardColumn::Todo), ["two", "later"]);

        // The part waiting doesn't hold up the card below it.
        let mut claimed = vec![claim(&store).unwrap(), claim(&store).unwrap()];
        claimed.sort();
        assert_eq!(claimed, ["later", "one"]);
        assert!(!store.has_todo_cards().unwrap());
        assert_eq!(claim(&store), None);

        // Finishing isn't enough: a rejected run finishes too.
        ran(&store, "one", "r1", None);
        assert!(store.card("two").unwrap().waiting);
        assert!(!store.has_todo_cards().unwrap());

        // Tried again, it has a pull request whose state nobody has looked at yet.
        ran(&store, "one", "r2", Some(None));
        assert!(!store.card("two").unwrap().waiting);
        assert!(store.has_todo_cards().unwrap());

        for (state, waiting) in [
            ("closed", true),
            ("open", false),
            ("draft", false),
            ("merged", false),
        ] {
            store
                .patch_run("r2", &[("pr_state", Value::Text(state.into()))])
                .unwrap();
            assert_eq!(store.card("two").unwrap().waiting, waiting, "{state}");
            assert_eq!(store.has_todo_cards().unwrap(), !waiting, "{state}");
        }
        assert_eq!(claim(&store).as_deref(), Some("two"));
    }

    #[test]
    fn deleting_a_part_or_not_waiting_keeps_the_rest_in_order() {
        let (_dir, store) = store();
        seeded(&store);
        agreed(&store, &[("one", "q"), ("two", "q"), ("three", "q")]);

        assert!(store.delete_card("two").unwrap());
        let last = store.card("three").unwrap();
        assert_eq!(last.waits_for_card.as_deref(), Some("one"));
        assert!(last.waiting);
        assert_eq!(store.joint_change("plan").unwrap().parts, ["one", "three"]);

        assert!(store.stop_waiting("three").unwrap());
        let last = store.card("three").unwrap();
        assert_eq!(last.waits_for_card, None);
        assert!(!last.waiting);
        assert!(
            !store.stop_waiting("three").unwrap(),
            "it waits for nothing"
        );

        // Once Keel has it, it's too late.
        assert!(store.delete_card("three").unwrap());
        assert!(store.delete_card("one").unwrap());
        agreed_again(&store);
        assert_eq!(claim(&store).as_deref(), Some("first"));
        store
            .lock()
            .execute(
                "update card set waits_for_card = 'gone' where id = 'first'",
                [],
            )
            .unwrap();
        assert!(!store.stop_waiting("first").unwrap());
    }

    #[test]
    fn a_part_keeps_which_commit_it_read_of_each_earlier_part() {
        let (_dir, store) = store();
        seeded(&store);
        agreed(&store, &[("one", "q"), ("two", "q")]);
        store.add_card_basis("two", "r1", "aaa").unwrap();
        store.add_card_basis("two", "r0", "bbb").unwrap();
        store.add_card_basis("two", "r1", "ccc").unwrap();
        let basis = store.card("two").unwrap().basis;
        assert_eq!(basis.len(), 2);
        assert_eq!(basis["r1"], "ccc");
        assert_eq!(basis["r0"], "bbb");
        assert!(store.card("one").unwrap().basis.is_empty());
    }

    fn agreed_again(store: &Store) {
        card(store, "plan2");
        store.set_joint_projects("plan2", &["q".into()]).unwrap();
        store
            .open_joint_parts("plan2", &serde_json::json!({}), &[part("first", "q")], &[])
            .unwrap();
    }

    #[test]
    fn a_part_whose_part_before_is_gone_doesnt_wait() {
        let (_dir, store) = store();
        seeded(&store);
        agreed(&store, &[("one", "q"), ("two", "q")]);
        store
            .lock()
            .execute("delete from card where id = 'one'", [])
            .unwrap();
        assert!(!store.card("two").unwrap().waiting);
        assert_eq!(claim(&store).as_deref(), Some("two"));
    }

    #[test]
    fn a_change_not_agreed_goes_with_its_card_and_an_agreed_one_stays() {
        let (_dir, store) = store();
        seeded(&store);
        other_project(&store, "q");
        card(&store, "plan");
        store.set_joint_projects("plan", &["q".into()]).unwrap();
        assert!(store.delete_card("plan").unwrap());
        assert!(matches!(
            store.joint_change("plan"),
            Err(Error::NotFound(_))
        ));

        // Choosing nothing makes it an ordinary card again.
        card(&store, "undecided");
        store
            .set_joint_projects("undecided", &["q".into()])
            .unwrap();
        assert!(store.set_joint_projects("undecided", &[]).unwrap());
        assert_eq!(store.card("undecided").unwrap().joint_change_id, None);
        assert!(store.joint_change("undecided").is_err());

        agreed_again(&store);
        assert!(store.delete_card("plan2").unwrap());
        assert_eq!(store.joint_change("plan2").unwrap().parts, ["first"]);
    }

    #[test]
    fn parts_arrive_on_a_database_people_already_have() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        // Exactly what the version before left, with a card waiting in Todo and a delivered run.
        let before = 13;
        {
            let conn = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute_batch(
                "insert into project (id, name, path, created_at) values ('p', 'demo', '/demo', 'then');
                 insert into run (id, project_id, title, created_at, pr_url, pr_number, pr_state, finished_at)
                     values ('r', 'p', 'old', 'then', 'https://github.com/acme/app/pull/3', 3, 'open', 'then');
                 insert into card (id, project_id, title, lane, position, run_id, created_at)
                     values ('done', 'p', 'old', 'started', 0, 'r', 'then');
                 insert into card (id, project_id, title, lane, position, language, todo_at, created_at)
                     values ('next', 'p', 'next', 'todo', 0, 'en', 'then', 'then');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.run("r").unwrap().pr_head, None);
        for card in store.cards("p").unwrap() {
            assert_eq!(card.joint_change_id, None);
            assert_eq!(card.joint_position, None);
            assert_eq!(card.waits_for_card, None);
            assert!(!card.waiting);
            assert!(card.basis.is_empty());
        }
        assert!(store.has_todo_cards().unwrap());
        assert_eq!(claim(&store).as_deref(), Some("next"));
    }

    #[test]
    fn how_many_tests_a_check_skipped_is_unknown_until_keel_counts_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        // What the version before left.
        let before = 14;
        {
            let conn = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute_batch(
                "insert into project (id, name, path, created_at) values ('p', 'demo', '/demo', 'then');
                 insert into run (id, project_id, title, created_at) values ('r', 'p', 'old', 'then');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        store
            .add_attempt(&new_attempt("a", "r", "check", AttemptState::Running))
            .unwrap();
        assert_eq!(store.attempt("a").unwrap().skipped_tests, None);
        store
            .patch_attempt("a", &[("skipped_tests", Value::Integer(385))])
            .unwrap();
        assert_eq!(store.attempt("a").unwrap().skipped_tests, Some(385));
        assert_eq!(
            serde_json::to_value(store.attempt("a").unwrap()).unwrap()["skippedTests"],
            385
        );
    }
}
