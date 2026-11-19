//! The desktop shell. Commands are thin: the rules live in `keel-engine`.

mod launch;
mod mcp;
mod menu;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use keel_core::{
    CardLane, EnvVar, Masked, Reason, Service, StepSpec, ToolProfile, WorkflowStep, mask_secrets,
    serialize_masked,
};
use keel_engine::{
    AddedProject, Answer, Board, BrainView, CiGap, CommandChange, Config, ContractEdit, Engine,
    Error as EngineError, GithubStatus, IssueWatch, JointChangeView, Preflight,
    ProjectProposalReview, ProjectSetup, RawCheck, RetryOptions, RunJoint, StartRun, Workflow,
    WorkflowDraft, request_from_issue, title_from_issue,
};
use keel_harness::Tools;
use keel_harness::github::{Account, Issue};
use keel_harness::toolchains::Found;
use keel_store::{
    Attempt, BrainNote, Decision, Event, Project, ProjectSettings, Run, RunRow, Store, Totals,
    WorkflowResults,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, RunEvent, State, WindowEvent};

/// The program Claude Code runs around a builder's Bash commands, built by `build.rs` from
/// `crates/keel-hook`.
const BASH_HOOK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/keel-hook"));

const LANGUAGE_KEY: &str = "language";
const CHANGED_EVENT: &str = "keel://changed";
const QUIT_EVENT: &str = "keel://quit-requested";

/// Finding the tools can take seconds, so it happens off the main thread and commands that
/// need them wait here.
#[derive(Default)]
struct Ready {
    done: Mutex<bool>,
    signal: Condvar,
}

impl Ready {
    fn set(&self) {
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signal.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.signal.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct AppState {
    store: Arc<Store>,
    engine: Engine,
    ready: Arc<Ready>,
    tools: Arc<Mutex<Tools>>,
    data_dir: PathBuf,
    quit_allowed: AtomicBool,
}

/// Every command's error: a code the UI translates, with the reason when there is one. These can
/// quote what git, `gh` or an agent said.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandError {
    code: &'static str,
    #[serde(serialize_with = "serialize_masked")]
    reason: Option<Reason>,
    #[serde(serialize_with = "serialize_masked")]
    detail: Option<String>,
}

impl From<EngineError> for CommandError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Blocked(reason) => Self {
                code: "blocked",
                reason: Some(reason),
                detail: None,
            },
            EngineError::NotARepo(path) => Self {
                code: "notARepo",
                reason: None,
                detail: Some(path),
            },
            EngineError::InvalidState => Self {
                code: "invalidState",
                reason: None,
                detail: None,
            },
            other => Self {
                code: "internal",
                reason: None,
                detail: Some(other.to_string()),
            },
        }
    }
}

impl From<keel_store::Error> for CommandError {
    fn from(error: keel_store::Error) -> Self {
        Self {
            code: "internal",
            reason: None,
            detail: Some(error.to_string()),
        }
    }
}

type CommandResult<T> = Result<T, CommandError>;

#[tauri::command(async)]
fn preflight(state: State<'_, AppState>) -> Preflight {
    state.ready.wait();
    state.engine.preflight()
}

/// Looks for the tools again, for after the user installs something or signs in.
#[tauri::command(async)]
fn refresh_session(state: State<'_, AppState>) -> Preflight {
    state.ready.wait();
    let tools = Tools::detect();
    *state.tools.lock().unwrap_or_else(|e| e.into_inner()) = tools.clone();
    state.engine.set_tools(tools);
    state.engine.preflight()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Overview {
    runs: Masked<Vec<RunRow>>,
    totals: Totals,
    /// Not masked: Settings saves back what it gets, which would overwrite the real values.
    projects: Vec<Project>,
    pending_proposals: i64,
    /// Where they wait, for the Brain page: only the projects that have any.
    proposals_by_project: std::collections::BTreeMap<String, i64>,
    workflow_drafts: usize,
    project_proposals: usize,
}

#[tauri::command(async)]
fn overview(state: State<'_, AppState>) -> CommandResult<Overview> {
    Ok(Overview {
        runs: Masked(state.store.runs(200)?),
        totals: state.store.totals()?,
        projects: state.store.projects()?,
        pending_proposals: state.store.pending_proposals()?,
        proposals_by_project: state.store.pending_proposals_by_project()?,
        // A folder that can't be read shouldn't take the home page down with it.
        workflow_drafts: state
            .engine
            .workflow_drafts()
            .map_or(0, |drafts| drafts.len()),
        project_proposals: state
            .engine
            .project_proposals()
            .map_or(0, |proposals| proposals.len()),
    })
}

/// The project isn't masked, for the same reason as in `overview`: the dialog saves it back.
#[tauri::command(async)]
fn project_add(
    state: State<'_, AppState>,
    path: String,
    language: String,
) -> CommandResult<AddedProject> {
    state.ready.wait();
    Ok(state
        .engine
        .add_project_with_map(Path::new(&path), &language)?)
}

#[tauri::command(async)]
fn project_proposals_list(state: State<'_, AppState>) -> CommandResult<Vec<ProjectProposalReview>> {
    Ok(state.engine.project_proposal_reviews()?)
}

/// The project isn't masked, for the same reason as in `project_add`.
#[tauri::command(async)]
fn project_proposal_accept(
    state: State<'_, AppState>,
    proposal_id: String,
    language: String,
) -> CommandResult<AddedProject> {
    state.ready.wait();
    Ok(state
        .engine
        .accept_project_proposal(&proposal_id, &language)?)
}

#[tauri::command(async)]
fn project_proposal_discard(state: State<'_, AppState>, proposal_id: String) -> CommandResult<()> {
    Ok(state.engine.discard_project_proposal(&proposal_id)?)
}

/// Not masked, for the same reason as in `overview`: the dialog saves it back.
#[tauri::command(async)]
fn project_setup(state: State<'_, AppState>, project_id: String) -> CommandResult<ProjectSetup> {
    state.ready.wait();
    Ok(state.engine.project_setup(&project_id)?)
}

/// Not masked either: what it lists can be added to the check, which is saved back.
#[tauri::command(async)]
fn project_ci_gap(state: State<'_, AppState>, project_id: String) -> CommandResult<CiGap> {
    Ok(state.engine.project_ci_gap(&project_id)?)
}

/// Where in the home folder the programs these commands use are, for the person to let them be
/// read. Only asks the shell where they are: nothing from the repo runs, and nothing is saved.
#[tauri::command(async)]
fn project_tool_folders(
    state: State<'_, AppState>,
    project_id: String,
    setup: String,
    check: String,
) -> CommandResult<Vec<Found>> {
    state.ready.wait();
    Ok(state.engine.tool_folders(&project_id, &setup, &check)?)
}

/// Returns the run. Its output is read the way any run's is, masked. `read` is the folders the
/// person is about to let the commands read, and `localhost` whether they're about to open it to
/// the check, which only this run gets.
#[tauri::command(async)]
fn project_try_check(
    state: State<'_, AppState>,
    project_id: String,
    setup: String,
    check: String,
    read: Vec<String>,
    localhost: bool,
    language: String,
) -> CommandResult<String> {
    state.ready.wait();
    Ok(state
        .engine
        .try_check_opening(&project_id, &setup, &check, &read, localhost, &language)?)
}

#[tauri::command(async)]
fn project_remove(state: State<'_, AppState>, project_id: String) -> CommandResult<()> {
    Ok(state.engine.remove_project(&project_id)?)
}

#[tauri::command(async)]
fn brain_get(state: State<'_, AppState>, project_id: String) -> CommandResult<Masked<BrainView>> {
    Ok(Masked(state.engine.brain(&project_id)?))
}

#[tauri::command(async)]
fn brain_generate_map(
    state: State<'_, AppState>,
    project_id: String,
    model: String,
    language: String,
) -> CommandResult<String> {
    Ok(state.engine.generate_map(&project_id, &model, &language)?)
}

#[tauri::command(async)]
fn brain_accept(state: State<'_, AppState>, seq: i64) -> CommandResult<()> {
    Ok(state.engine.accept_note(seq)?)
}

#[tauri::command(async)]
fn brain_dismiss(state: State<'_, AppState>, seq: i64) -> CommandResult<()> {
    Ok(state.engine.dismiss_note(seq)?)
}

#[tauri::command(async)]
fn brain_retire(state: State<'_, AppState>, seq: i64) -> CommandResult<()> {
    Ok(state.engine.retire_note(seq)?)
}

#[tauri::command(async)]
fn run_proposals(
    state: State<'_, AppState>,
    run_id: String,
) -> CommandResult<Masked<Vec<BrainNote>>> {
    Ok(Masked(state.engine.proposals_of_run(&run_id)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSettingsInput {
    github_account: Option<String>,
    setup_command: Option<String>,
    check_command: Option<String>,
    allowed_domains: Vec<String>,
    allowed_read: Vec<String>,
    services: Vec<Service>,
    env: Vec<EnvVar>,
    default_workflow: Option<String>,
    sensitive_paths: Vec<String>,
    allow_localhost: bool,
    /// The label whose issues Keel brings to the Backlog. Empty or missing: it watches none.
    #[serde(default)]
    issue_label: Option<String>,
}

#[tauri::command(async)]
fn project_update(
    state: State<'_, AppState>,
    project_id: String,
    settings: ProjectSettingsInput,
) -> CommandResult<Project> {
    let clean =
        |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    state.engine.update_project(
        &project_id,
        &ProjectSettings {
            github_account: clean(settings.github_account),
            setup_command: clean(settings.setup_command),
            check_command: clean(settings.check_command),
            allowed_domains: settings.allowed_domains,
            allowed_read: settings.allowed_read,
            services: settings.services,
            env: settings.env,
            default_workflow: clean(settings.default_workflow),
            sensitive_paths: settings.sensitive_paths,
            allow_localhost: settings.allow_localhost,
        },
    )?;
    Ok(state
        .engine
        .set_issue_label(&project_id, settings.issue_label.as_deref())?)
}

/// What came of the last look at a project's issues since the app opened. None if it watches none,
/// or hasn't looked yet.
#[tauri::command(async)]
fn issues_watch(
    state: State<'_, AppState>,
    project_id: String,
) -> CommandResult<Masked<Option<IssueWatch>>> {
    Ok(Masked(state.engine.issue_watch(&project_id)))
}

/// Looks at a project's issues now, as the background does, and says what came of it.
#[tauri::command(async)]
fn issues_look(
    state: State<'_, AppState>,
    project_id: String,
) -> CommandResult<Masked<Option<IssueWatch>>> {
    state.ready.wait();
    Ok(Masked(state.engine.look_at_issues(&project_id)?))
}

#[tauri::command(async)]
fn github_status(
    state: State<'_, AppState>,
    project_id: String,
    account: Option<String>,
) -> CommandResult<Masked<GithubStatus>> {
    state.ready.wait();
    Ok(Masked(
        state
            .engine
            .github_status(&project_id, account.as_deref())?,
    ))
}

/// Not masked: the dialog hands the chosen issue back to `run_start`, and agents get it as written.
#[tauri::command(async)]
fn issues_list(state: State<'_, AppState>, project_id: String) -> CommandResult<Vec<Issue>> {
    state.ready.wait();
    Ok(state.engine.issues(&project_id)?)
}

#[tauri::command(async)]
fn decisions_list(
    state: State<'_, AppState>,
    project_id: String,
) -> CommandResult<Masked<Vec<Decision>>> {
    Ok(Masked(state.engine.decisions(&project_id)?))
}

#[tauri::command(async)]
fn decision_retire(state: State<'_, AppState>, seq: i64) -> CommandResult<()> {
    Ok(state.engine.retire_decision(seq)?)
}

/// How a run is defined: the full workflow, or one agent step.
#[derive(Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum WorkflowChoice {
    /// A built-in workflow or one of the project's.
    Saved { workflow_id: String },
    Single {
        step_id: String,
        model: String,
        effort: Option<String>,
        tools: ToolProfile,
        max_turns: Option<u32>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRequest {
    project_id: String,
    prompt: String,
    /// The issue this comes from. `prompt` then holds anything the person added to it.
    issue: Option<Issue>,
    workflow: WorkflowChoice,
    language: String,
}

#[tauri::command(async)]
fn run_start(state: State<'_, AppState>, request: RunRequest) -> CommandResult<String> {
    state.ready.wait();
    let text = match &request.issue {
        Some(issue) => request_from_issue(issue, &request.prompt),
        None => request.prompt.clone(),
    };
    let title = request
        .issue
        .as_ref()
        .map(|issue| title_from_issue(issue.number, &issue.title));
    let (workflow, saved_workflow) = match request.workflow {
        WorkflowChoice::Saved { workflow_id } => {
            let saved = state
                .engine
                .workflow(&request.project_id, &workflow_id, &request.language)?
                .ok_or(CommandError {
                    code: "invalidState",
                    reason: None,
                    detail: None,
                })?;
            let reference = saved.reference();
            (saved.steps, Some(reference))
        }
        WorkflowChoice::Single {
            step_id,
            model,
            effort,
            tools,
            max_turns,
        } => {
            let step = WorkflowStep::Agent(StepSpec {
                id: step_id,
                model,
                effort,
                tools,
                prompt: text.clone(),
                max_turns,
                max_budget_usd: None,
                paths: vec![],
                when: None,
                risks: vec![],
                model_when_risky: None,
                role: Default::default(),
            });
            (vec![step], None)
        }
    };
    Ok(state.engine.start_run(StartRun {
        project_id: request.project_id,
        title,
        request: text,
        workflow,
        saved_workflow,
        language: request.language,
        source_issue: request.issue,
        started_by: None,
    })?)
}

/// Not masked: the editor saves back what it gets, which would overwrite the real commands.
#[tauri::command(async)]
fn board_get(state: State<'_, AppState>, project_id: Option<String>) -> CommandResult<Board> {
    // Without a project, every project's cards.
    Ok(match project_id {
        Some(project_id) => state.engine.board(&project_id)?,
        None => state.engine.board_all()?,
    })
}

#[tauri::command(async)]
fn card_create(
    state: State<'_, AppState>,
    project_id: String,
    title: String,
    body: String,
) -> CommandResult<String> {
    Ok(state.engine.card_create(&project_id, &title, &body)?)
}

#[tauri::command(async)]
fn card_import_issue(
    state: State<'_, AppState>,
    project_id: String,
    issue: Issue,
) -> CommandResult<String> {
    Ok(state.engine.card_import_issue(&project_id, &issue)?)
}

#[tauri::command(async)]
fn card_update(
    state: State<'_, AppState>,
    card_id: String,
    title: String,
    body: String,
) -> CommandResult<()> {
    Ok(state.engine.card_update(&card_id, &title, &body)?)
}

#[tauri::command(async)]
fn card_move(
    state: State<'_, AppState>,
    card_id: String,
    lane: CardLane,
    index: usize,
    language: String,
) -> CommandResult<()> {
    state.ready.wait();
    Ok(state.engine.card_move(&card_id, lane, index, &language)?)
}

#[tauri::command(async)]
fn card_delete(state: State<'_, AppState>, card_id: String) -> CommandResult<()> {
    Ok(state.engine.card_delete(&card_id)?)
}

/// Which other projects a card's request touches. None makes it an ordinary card again.
#[tauri::command(async)]
fn card_set_projects(
    state: State<'_, AppState>,
    card_id: String,
    projects: Vec<String>,
) -> CommandResult<()> {
    Ok(state.engine.card_set_projects(&card_id, &projects)?)
}

#[tauri::command(async)]
fn card_stop_waiting(state: State<'_, AppState>, card_id: String) -> CommandResult<()> {
    Ok(state.engine.card_stop_waiting(&card_id)?)
}

/// What was agreed is what an agent wrote.
#[tauri::command(async)]
fn joint_change_get(
    state: State<'_, AppState>,
    change_id: String,
) -> CommandResult<Masked<JointChangeView>> {
    Ok(Masked(state.engine.joint_change(&change_id)?))
}

/// Only from here: no agent and no assistant can say these work.
#[tauri::command(async)]
fn joint_check_set(
    state: State<'_, AppState>,
    change_id: String,
    seq: i64,
    works: bool,
) -> CommandResult<()> {
    Ok(state.engine.joint_check_set(&change_id, seq, works)?)
}

/// Only from here too: no agent and no assistant can correct what the parts agree on.
#[tauri::command(async)]
fn joint_contract_set(
    state: State<'_, AppState>,
    change_id: String,
    points: Vec<ContractEdit>,
) -> CommandResult<()> {
    Ok(state.engine.joint_contract_set(&change_id, &points)?)
}

#[tauri::command(async)]
fn run_joint(state: State<'_, AppState>, run_id: String) -> CommandResult<Option<RunJoint>> {
    Ok(state.engine.run_joint(&run_id)?)
}

#[tauri::command(async)]
fn max_agents_get(state: State<'_, AppState>) -> usize {
    state.engine.max_agents()
}

#[tauri::command(async)]
fn max_agents_set(state: State<'_, AppState>, value: usize) -> CommandResult<()> {
    Ok(state.engine.set_max_agents(value)?)
}

#[tauri::command(async)]
fn workflows_list(
    state: State<'_, AppState>,
    project_id: String,
    language: String,
) -> CommandResult<Vec<Workflow>> {
    Ok(state.engine.workflows(&project_id, &language)?)
}

#[tauri::command(async)]
fn workflow_save(
    state: State<'_, AppState>,
    project_id: String,
    workflow: Workflow,
) -> CommandResult<Workflow> {
    Ok(state.engine.save_workflow(&project_id, &workflow)?)
}

/// How each workflow's runs have gone. Nothing here gets saved back, so it can be masked.
#[tauri::command(async)]
fn workflow_results(
    state: State<'_, AppState>,
    project_id: String,
) -> CommandResult<Masked<Vec<WorkflowResults>>> {
    Ok(Masked(state.engine.workflow_results(&project_id)?))
}

#[tauri::command(async)]
fn workflow_delete(
    state: State<'_, AppState>,
    project_id: String,
    workflow_id: String,
) -> CommandResult<()> {
    Ok(state.engine.delete_workflow(&project_id, &workflow_id)?)
}

/// Not masked, for the same reason as workflows: the editor saves back what it gets.
#[tauri::command(async)]
fn workflow_drafts_list(state: State<'_, AppState>) -> CommandResult<Vec<WorkflowDraft>> {
    Ok(state.engine.workflow_drafts()?)
}

#[tauri::command(async)]
fn workflow_draft_save(
    state: State<'_, AppState>,
    project_id: String,
    draft_id: String,
    workflow: Workflow,
) -> CommandResult<Workflow> {
    Ok(state
        .engine
        .save_workflow_draft(&project_id, &draft_id, &workflow)?)
}

#[tauri::command(async)]
fn workflow_draft_discard(
    state: State<'_, AppState>,
    project_id: String,
    draft_id: String,
) -> CommandResult<()> {
    Ok(state
        .engine
        .discard_workflow_draft(&project_id, &draft_id)?)
}

#[tauri::command(async)]
fn run_diff(state: State<'_, AppState>, run_id: String) -> CommandResult<String> {
    Ok(mask_secrets(&state.engine.run_diff(&run_id)?))
}

/// Whether what approving would push looks like it holds a secret, which masking only hides here.
#[tauri::command(async)]
fn run_adds_secret(state: State<'_, AppState>, run_id: String) -> CommandResult<bool> {
    Ok(keel_core::diff_adds_secret(
        &state.engine.run_diff(&run_id)?,
    ))
}

/// Masking works on text, so it would only break a picture. Their names are text, and are masked.
#[tauri::command(async)]
fn run_screenshots(
    state: State<'_, AppState>,
    run_id: String,
) -> CommandResult<keel_engine::Screenshots> {
    let mut shots = state.engine.run_screenshots(&run_id)?;
    for shot in &mut shots.shown {
        shot.name = mask_secrets(&shot.name);
    }
    for name in &mut shots.left_out {
        *name = mask_secrets(name);
    }
    Ok(shots)
}

#[tauri::command(async)]
fn rework_limit_answer(
    state: State<'_, AppState>,
    attempt_id: String,
    another_round: bool,
) -> CommandResult<()> {
    Ok(state
        .engine
        .resolve_rework_limit(&attempt_id, another_round)?)
}

#[tauri::command(async)]
fn run_get(state: State<'_, AppState>, run_id: String) -> CommandResult<Masked<Run>> {
    Ok(Masked(state.store.run(&run_id)?))
}

/// A person opened this run, so see what became of its pull request. It comes back as a change
/// when there's something new, so this returns at once and has nothing to report.
#[tauri::command(async)]
fn run_look_at_pr(state: State<'_, AppState>, run_id: String) {
    state.ready.wait();
    state.engine.look_at_pr(&run_id);
}

#[tauri::command(async)]
fn approval_answer(
    state: State<'_, AppState>,
    attempt_id: String,
    approve: bool,
    text: String,
) -> CommandResult<()> {
    Ok(state.engine.answer_approval(&attempt_id, approve, &text)?)
}

#[tauri::command(async)]
fn questions_answer(
    state: State<'_, AppState>,
    attempt_id: String,
    answers: Vec<Answer>,
) -> CommandResult<()> {
    Ok(state.engine.answer_questions(&attempt_id, &answers)?)
}

#[tauri::command(async)]
fn approval_send_back(
    state: State<'_, AppState>,
    attempt_id: String,
    instructions: String,
) -> CommandResult<()> {
    Ok(state.engine.send_back(&attempt_id, &instructions)?)
}

#[tauri::command(async)]
fn approval_run_skipped(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.run_skipped(&attempt_id)?)
}

/// Opens a pull request in the browser. Only GitHub URLs, so a value from a run can't open anything else.
#[tauri::command(async)]
fn open_github_url(url: String) -> CommandResult<()> {
    if !url.starts_with("https://github.com/") {
        return Err(CommandError {
            code: "invalidState",
            reason: None,
            detail: None,
        });
    }
    std::process::Command::new("/usr/bin/open")
        .arg(&url)
        .status()
        .map(|_| ())
        .map_err(|e| CommandError {
            code: "internal",
            reason: None,
            detail: Some(e.to_string()),
        })
}

#[tauri::command(async)]
fn run_attempts(state: State<'_, AppState>, run_id: String) -> CommandResult<Masked<Vec<Attempt>>> {
    Ok(Masked(state.store.attempts_of_run(&run_id)?))
}

#[tauri::command(async)]
fn attempt_events(
    state: State<'_, AppState>,
    attempt_id: String,
    after_seq: i64,
) -> CommandResult<Masked<Vec<Event>>> {
    Ok(Masked(state.store.events(&attempt_id, after_seq)?))
}

#[tauri::command(async)]
fn attempt_diff(state: State<'_, AppState>, attempt_id: String) -> CommandResult<String> {
    Ok(mask_secrets(&state.engine.diff(&attempt_id)?))
}

#[tauri::command(async)]
fn attempt_raw_check(state: State<'_, AppState>, attempt_id: String) -> CommandResult<RawCheck> {
    Ok(state.engine.verify_raw(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_stop(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.stop(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_wait(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.wait_longer(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_kill_command(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.kill_command(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_retry(
    state: State<'_, AppState>,
    attempt_id: String,
    resume: bool,
    current_commands: bool,
) -> CommandResult<String> {
    let options = RetryOptions {
        resume,
        by: None,
        current_commands,
    };
    Ok(state.engine.retry_with(&attempt_id, options)?)
}

/// The run's setup and check whose command the project changed after it started.
#[tauri::command(async)]
fn run_command_changes(
    state: State<'_, AppState>,
    run_id: String,
) -> CommandResult<Masked<Vec<CommandChange>>> {
    Ok(Masked(state.engine.command_changes(&run_id)?))
}

#[tauri::command(async)]
fn attempt_abandon(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.abandon(&attempt_id)?)
}

#[tauri::command(async)]
fn language_get(state: State<'_, AppState>) -> CommandResult<Option<String>> {
    Ok(state.store.setting(LANGUAGE_KEY)?)
}

#[tauri::command(async)]
fn language_set(state: State<'_, AppState>, language: String) -> CommandResult<()> {
    Ok(state.store.set_setting(LANGUAGE_KEY, &language)?)
}

/// Opens Terminal running `claude auth login`. The login happens in the user's own CLI: Keel
/// never sees it. A `.command` file avoids asking for permission to control Terminal.
#[tauri::command(async)]
fn open_claude_login(state: State<'_, AppState>) -> CommandResult<()> {
    let claude = state
        .tools
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .claude
        .clone();
    let Some(claude) = claude else {
        return Err(CommandError {
            code: "blocked",
            reason: Some(Reason::ClaudeMissing),
            detail: None,
        });
    };
    open_in_terminal(&state, "claude-login.command", &claude, "auth login")
}

/// Signing in to `gh` is between the person and GitHub, in Terminal. Keel never sees it.
#[tauri::command(async)]
fn open_github_login(state: State<'_, AppState>) -> CommandResult<()> {
    let gh = state
        .tools
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .gh
        .clone();
    let Some(gh) = gh else {
        return Err(CommandError {
            code: "blocked",
            reason: Some(Reason::GhMissing),
            detail: None,
        });
    };
    open_in_terminal(&state, "gh-login.command", &gh, "auth login")
}

fn open_in_terminal(
    state: &State<'_, AppState>,
    name: &str,
    program: &Path,
    arguments: &str,
) -> CommandResult<()> {
    let script = state.data_dir.join(name);
    let quoted = program.to_string_lossy().replace('\'', r"'\''");
    let write = std::fs::write(&script, format!("#!/bin/sh\nexec '{quoted}' {arguments}\n"))
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        });
    let opened = write.and_then(|()| {
        std::process::Command::new("/usr/bin/open")
            .arg(&script)
            .status()
    });
    opened.map(|_| ()).map_err(|e| CommandError {
        code: "internal",
        reason: None,
        detail: Some(e.to_string()),
    })
}

/// Where to install what Keel needs. Fixed addresses, never one from the UI.
#[tauri::command(async)]
fn open_help(topic: String) -> CommandResult<()> {
    let url = match topic.as_str() {
        "claude" => "https://claude.com/product/claude-code",
        "git" => "https://git-scm.com/downloads/mac",
        "gh" => "https://cli.github.com",
        _ => {
            return Err(CommandError {
                code: "invalidState",
                reason: None,
                detail: None,
            });
        }
    };
    std::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .map(|_| ())
        .map_err(|e| CommandError {
            code: "internal",
            reason: None,
            detail: Some(e.to_string()),
        })
}

#[tauri::command(async)]
fn github_accounts(state: State<'_, AppState>) -> CommandResult<Vec<Account>> {
    Ok(state.engine.github_accounts()?)
}

/// Shows a file of this attempt in Finder. Only paths the store recorded, never one from the UI.
#[tauri::command(async)]
fn reveal_raw(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    let attempt = state.store.attempt(&attempt_id)?;
    std::process::Command::new("/usr/bin/open")
        .args(["-R", &attempt.raw_path])
        .status()
        .map(|_| ())
        .map_err(|e| CommandError {
            code: "internal",
            reason: None,
            detail: Some(e.to_string()),
        })
}

#[tauri::command(async)]
fn active_count(state: State<'_, AppState>) -> CommandResult<usize> {
    Ok(state.engine.active_count()?)
}

/// Ends the app. Agents keep running unless `stop_runs` is set; Keel reconnects on reopening.
#[tauri::command(async)]
fn app_quit(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    stop_runs: bool,
) -> CommandResult<()> {
    // Gives the agents a moment to record how they ended.
    state
        .engine
        .shut_down(stop_runs, std::time::Duration::from_secs(12));
    state.quit_allowed.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

fn wants_confirmation(app: &tauri::AppHandle) -> bool {
    // Without its data Keel has no state, and nothing running to ask about: see `launch`.
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    !state.quit_allowed.load(Ordering::SeqCst) && state.engine.must_ask_before_quitting()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut context = tauri::generate_context!();
    let config = context.config_mut();
    config.identifier = launch::identifier(&config.identifier, launch::BRANCH_BUILD);
    let app = tauri::Builder::default()
        // First, so a second copy leaves before it opens the database. Two copies would each
        // supervise the same runs.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .on_menu_event(menu::on_menu_event)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && wants_confirmation(window.app_handle())
            {
                api.prevent_close();
                let _ = window.emit(QUIT_EVENT, ());
            }
        })
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let database = data_dir.join("keel.db");
            let opened = std::fs::create_dir_all(&data_dir)
                .map_err(|e| (launch::Why::Other, e.to_string()))
                .and_then(|()| {
                    Store::open(&database).map_err(|e| (launch::Why::from(&e), e.to_string()))
                });
            let store = match opened {
                Ok(store) => Arc::new(store),
                Err((why, detail)) => {
                    let language = launch::language(&database);
                    let notice = launch::cant_open(language, why, &detail, &data_dir);
                    launch::tell_and_quit(app, notice);
                    return Ok(());
                }
            };
            let handle = app.handle().clone();
            let mut config = Config::new(data_dir.clone());
            // No spaces, and macOS doesn't clear it on its own.
            if let Some(home) = std::env::var_os("HOME") {
                let keel =
                    std::path::PathBuf::from(home).join(launch::home_folder(launch::BRANCH_BUILD));
                config.runs_dir = keel.join("runs");
                // Without it, builders just get no note.
                config.bash_hook = keel_harness::hook::install(&keel.join("bin"), BASH_HOOK).ok();
                if let Some(hook) = config.bash_hook.clone() {
                    std::thread::spawn(move || keel_harness::hook::warm_up(&hook));
                }
            }
            let engine = Engine::new(store.clone(), config, Tools::default(), move |run_id| {
                let _ = handle.emit(CHANGED_EVENT, run_id);
            });
            let ready = Arc::new(Ready::default());
            let tools_shared = Arc::new(Mutex::new(Tools::default()));
            let mcp_server = keel_mcp::McpServer::new(engine.clone(), store.clone());
            app.manage(mcp::McpState(mcp_server.clone()));
            {
                let engine = engine.clone();
                let ready = ready.clone();
                let tools_shared = tools_shared.clone();
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let tools = Tools::detect();
                    *tools_shared.lock().unwrap_or_else(|e| e.into_inner()) = tools.clone();
                    engine.set_tools(tools);
                    // Reconnects to steps that outlived the last session.
                    let _ = engine.recover();
                    // After that, so a run that's still going counts before anything new starts.
                    engine.start_board();
                    engine.start_pr_watch();
                    engine.start_issue_watch();
                    ready.set();
                    let _ = handle.emit(CHANGED_EVENT, "");
                    // After the tools are known, so no client ever finds Keel half ready.
                    mcp::start_if_enabled(mcp_server, handle);
                });
            }
            app.manage(AppState {
                store,
                engine,
                ready,
                tools: tools_shared,
                data_dir,
                quit_allowed: AtomicBool::new(false),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            preflight,
            refresh_session,
            overview,
            project_add,
            project_setup,
            project_ci_gap,
            project_tool_folders,
            project_try_check,
            project_remove,
            run_start,
            mcp::mcp_overview,
            mcp::mcp_settings_save,
            mcp::mcp_client_create,
            mcp::mcp_client_set_can_run,
            mcp::mcp_client_set_can_steer,
            mcp::mcp_client_set_max_open_runs,
            mcp::mcp_client_revoke,
            mcp::mcp_connect_claude,
            run_get,
            run_look_at_pr,
            run_proposals,
            brain_get,
            brain_generate_map,
            brain_accept,
            brain_dismiss,
            brain_retire,
            run_diff,
            run_adds_secret,
            run_screenshots,
            board_get,
            card_create,
            card_import_issue,
            card_update,
            card_move,
            card_delete,
            card_set_projects,
            card_stop_waiting,
            joint_change_get,
            joint_check_set,
            joint_contract_set,
            run_joint,
            max_agents_get,
            max_agents_set,
            workflows_list,
            workflow_save,
            workflow_results,
            workflow_delete,
            workflow_drafts_list,
            workflow_draft_save,
            workflow_draft_discard,
            project_proposals_list,
            project_proposal_accept,
            project_proposal_discard,
            rework_limit_answer,
            run_attempts,
            project_update,
            github_status,
            issues_list,
            issues_watch,
            issues_look,
            decisions_list,
            decision_retire,
            approval_answer,
            questions_answer,
            approval_send_back,
            approval_run_skipped,
            open_github_url,
            attempt_events,
            attempt_diff,
            attempt_raw_check,
            attempt_stop,
            attempt_wait,
            attempt_kill_command,
            attempt_retry,
            run_command_changes,
            attempt_abandon,
            language_get,
            language_set,
            open_claude_login,
            open_github_login,
            open_help,
            github_accounts,
            reveal_raw,
            active_count,
            app_quit,
            menu::menu_set,
        ])
        .build(context)
        .expect("error while building the app");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { api, .. } = event
            // With no window left there's nobody to ask. Anything running carries on, as when a
            // person chooses that, and Keel reconnects on reopening.
            && !handle.webview_windows().is_empty()
            && wants_confirmation(handle)
        {
            api.prevent_exit();
            let _ = handle.emit(QUIT_EVENT, ());
        }
    });
}

#[cfg(test)]
mod tests {
    use keel_core::MASK;
    use serde_json::json;

    use super::*;

    const SECRET: &str = "made-up-Value-123";

    fn row(title: &str) -> RunRow {
        RunRow {
            id: "r1".into(),
            title: title.into(),
            pr_url: None,
            started_by: None,
            project_id: "p1".into(),
            project_name: "demo".into(),
            step_id: "builder".into(),
            attempt_id: "a1".into(),
            attempt_number: 1,
            state: "failed".into(),
            reason: Some(json!({ "code": "prepareFailed", "detail": format!("API_KEY={SECRET}") })),
            model: "sonnet".into(),
            turns: None,
            started_at: "2026-11-05T10:00:00Z".into(),
            finished_at: None,
            last_activity_at: None,
        }
    }

    #[test]
    fn the_settings_can_be_read_back_from_a_whole_project() {
        // The setup dialog sends the project with only what changed replaced.
        let mut sent = serde_json::to_value(Project {
            id: "p1".into(),
            name: "demo".into(),
            path: "/Users/you/demo".into(),
            created_at: "2026-11-05T10:00:00Z".into(),
            github_account: None,
            github_repo: Some("acme/app".into()),
            setup_command: None,
            check_command: None,
            allowed_domains: vec!["pypi.org".into()],
            allowed_read: vec!["~/.nvm".into()],
            services: vec![Service::Bridge {
                name: "db".into(),
                port: 5432,
                socket: None,
            }],
            env: vec![EnvVar {
                name: "DB".into(),
                value: SECRET.into(),
            }],
            default_workflow: Some("mine".into()),
            sensitive_paths: vec!["billing/".into()],
            allow_localhost: true,
            issue_label: None,
        })
        .unwrap();
        sent["checkCommand"] = json!("pnpm test");
        let settings: ProjectSettingsInput = serde_json::from_value(sent).unwrap();
        assert_eq!(settings.check_command.as_deref(), Some("pnpm test"));
        assert_eq!(settings.allowed_domains, ["pypi.org"]);
        assert_eq!(settings.allowed_read, ["~/.nvm"]);
        assert_eq!(settings.services.len(), 1);
        assert_eq!(settings.env[0].value, SECRET);
        assert_eq!(settings.default_workflow.as_deref(), Some("mine"));
        assert_eq!(settings.sensitive_paths, ["billing/"]);
        assert!(settings.allow_localhost);
    }

    #[test]
    fn an_error_hides_what_it_quotes() {
        let error = CommandError::from(EngineError::Blocked(Reason::DeliveryFailed {
            kind: "other".into(),
            detail: format!("https://bot:{SECRET}@example.com/a.git: denied"),
        }));
        assert_eq!(
            serde_json::to_value(&error).unwrap()["reason"]["detail"],
            format!("https://bot:{MASK}@example.com/a.git: denied")
        );

        let error = CommandError {
            code: "internal",
            reason: None,
            detail: Some(format!("password={SECRET}1")),
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap()["detail"],
            format!("password={MASK}")
        );
    }

    #[test]
    fn the_board_masks_what_was_said_and_not_what_the_edit_dialog_saves_back() {
        use keel_engine::{BoardCard, BoardIssue};

        let said = format!("API_KEY={SECRET}");
        let failed = Reason::PrepareFailed {
            detail: said.clone(),
        };
        let board = Board {
            cards: vec![BoardCard {
                id: "c1".into(),
                project_id: "p1".into(),
                title: said.clone(),
                body: said.clone(),
                issue: Some(BoardIssue {
                    number: 7,
                    title: said.clone(),
                    author: "someone".into(),
                    url: "https://github.com/acme/app/issues/7".into(),
                }),
                column: keel_core::BoardColumn::HumanReview,
                position: 0,
                run_id: Some("r1".into()),
                run_state: None,
                reason: Some(failed.clone()),
                pr_url: None,
                pr_state: None,
                pr_state_at: None,
                pr_ci: None,
                pr_ci_failed: Vec::new(),
                rejection: Some(said.clone()),
                start_blocked: Some(failed.clone()),
                created_at: "2026-11-05T10:00:00Z".into(),
                joint_change_id: None,
                also_touches: Vec::new(),
                part: None,
            }],
            working: 0,
            max_agents: 2,
            default_workflow: None,
            paused: Some(failed),
        };
        let shown = serde_json::to_value(&board).unwrap();
        let card = &shown["cards"][0];
        for quoted in [
            &card["issue"]["title"],
            &card["reason"]["detail"],
            &card["rejection"],
            &card["startBlocked"]["detail"],
            &shown["paused"]["detail"],
        ] {
            let quoted = quoted.as_str().unwrap();
            assert!(!quoted.contains(SECRET), "{quoted}");
            assert!(quoted.contains(keel_core::MASK), "{quoted}");
        }
        assert_eq!(card["issue"]["number"], 7);
        assert_eq!(card["title"], said.as_str());
        assert_eq!(card["body"], said.as_str());
    }

    #[test]
    fn the_overview_masks_runs_and_not_the_settings_that_get_saved_back() {
        let overview = Overview {
            runs: Masked(vec![row(&format!("Use API_KEY={SECRET} to test"))]),
            totals: Totals::default(),
            projects: vec![Project {
                id: "p1".into(),
                name: "demo".into(),
                path: "/Users/you/demo".into(),
                created_at: "2026-11-05T10:00:00Z".into(),
                github_account: None,
                github_repo: None,
                setup_command: None,
                check_command: Some(format!("API_KEY={SECRET} make test")),
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![EnvVar {
                    name: "API_KEY".into(),
                    value: SECRET.into(),
                }],
                default_workflow: None,
                sensitive_paths: vec![],
                allow_localhost: false,
                issue_label: None,
            }],
            pending_proposals: 0,
            proposals_by_project: Default::default(),
            workflow_drafts: 0,
            project_proposals: 0,
        };
        let shown = serde_json::to_value(&overview).unwrap();
        assert_eq!(
            shown["runs"][0]["title"],
            format!("Use API_KEY={MASK} to test")
        );
        assert_eq!(
            shown["runs"][0]["reason"]["detail"],
            format!("API_KEY={MASK}")
        );
        assert_eq!(shown["projects"][0]["env"][0]["value"], SECRET);
        assert_eq!(
            shown["projects"][0]["checkCommand"],
            format!("API_KEY={SECRET} make test")
        );
    }
}
