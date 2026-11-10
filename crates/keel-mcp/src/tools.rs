//! What each tool does: plain methods of [`McpServer`], each leaving a row in the audit log. The
//! engine blocks, so they run on tokio's blocking threads.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, StepCondition, StepRole, StepSpec, ToolProfile, WorkflowStep, mask_secrets,
    mask_value,
};
use keel_engine::{
    Error, MAX_PROJECT_PROPOSALS, Outcome, ProjectProposal, ProposedProject, StartRun,
    can_send_back, files_changed, request_from_issue, skipped_steps, spec_of_attempt,
    verification_of_attempt,
};
use keel_store::{Attempt, MAX_OPEN_RUNS_CEILING, McpClient, NewMcpCall, Project, Run};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::{McpServer, ToolError};

/// Runs a client may have open at once until the person sets its own number in Keel's settings. A
/// run waiting for the person counts, so an assistant can't spend a person's usage faster than
/// they're looking at it.
pub const MAX_OPEN_RUNS: i64 = keel_store::DEFAULT_MAX_OPEN_RUNS;
/// Runs one wait_for_run can watch at once.
const MAX_WAITED_RUNS: usize = 10;
/// Attempts a run's view lists that a later attempt of the same step replaced. Past this, the
/// oldest are left out and the view says how many, so a run that goes round many times still reads
/// in one go.
const MAX_EARLIER_ATTEMPTS: usize = 40;
/// Files a run's report lists. Past this, it says how many more there are.
const MAX_LISTED_FILES: usize = 100;
/// How many times a client may send back, or retry, one of its runs. After that it's the person's.
const MAX_FEEDBACK_PER_RUN: i64 = 3;
const MAX_FINDINGS: usize = 20;
const MAX_FINDING: usize = 2 * 1024;

/// Drafts a project may have waiting, so a client can't bury the person in them.
pub const MAX_DRAFTS: usize = 20;
const MAX_DRAFT_NAME: usize = 80;
const MAX_TASK_TITLE: usize = 80;
/// What a task runs on unless the client says otherwise: it only reads, and that's plenty for it.
const TASK_MODEL: &str = "sonnet";
const MAX_MODEL_NAME: usize = 100;
const MAX_PROPOSED_PATH: usize = 1024;

const MAX_TEXT: usize = 16 * 1024;
const MAX_DIFF: usize = 64 * 1024;
const MAX_LOGGED_ARGS: usize = 4 * 1024;
const MAX_LOGGED_ERROR: usize = 4 * 1024;
const WAIT_DEFAULT_SECS: u64 = 30;
const WAIT_MAX_SECS: u64 = 50;
const WAIT_TICK: Duration = Duration::from_millis(300);

/// What a run waits on someone for. Answering and approving are only ever the person's. Of the
/// rest, a client the person lets give feedback can send back, retry or close its own runs.
const NEEDS_PERSON: [&str; 7] = [
    "waitingApproval",
    "waitingAnswers",
    "stalled",
    "blocked",
    "interrupted",
    "failed",
    "stopped",
];
/// What stop_run may stop. A step waiting for an approval or for answers isn't working, and only a
/// person answers it.
const STOPPABLE: [&str; 4] = ["queued", "preparing", "running", "stalled"];
/// The tools that only look. Every other one changes something, so it waits its turn: see `call`.
const ONLY_LOOK: [&str; 8] = [
    "list_projects",
    "list_workflows",
    "list_issues",
    "list_runs",
    "get_run",
    "get_step",
    "get_brain",
    "wait_for_run",
];

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListProjectsArgs {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListWorkflowsArgs {
    /// A project's id or name.
    pub project: String,
    /// `en` or `es`, for the built-in workflows' names and prompts. The app's language by default.
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListIssuesArgs {
    /// A project's id or name.
    pub project: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListRunsArgs {
    /// Only this project's runs: its id or name.
    #[serde(default)]
    pub project: Option<String>,
    /// How many, up to 100. 20 by default.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Only the runs this client started, to find them again.
    #[serde(default)]
    pub mine: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetRunArgs {
    /// A run's id.
    pub run: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetStepArgs {
    /// An `attemptId` from get_run.
    pub attempt: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetBrainArgs {
    /// A project's id or name.
    pub project: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitForRunArgs {
    /// A run's id.
    #[serde(default)]
    pub run: Option<String>,
    /// Several runs' ids, up to 10, to wait until any of them stops working.
    #[serde(default)]
    pub runs: Vec<String>,
    /// How long to wait, up to 50. 30 by default.
    #[serde(default)]
    pub seconds: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartRunArgs {
    /// A project's id or name.
    pub project: String,
    /// A workflow's id, from list_workflows.
    pub workflow: String,
    /// What to do. With `issue`, what to add to it.
    #[serde(default)]
    pub request: Option<String>,
    /// An open issue's number, from list_issues.
    #[serde(default)]
    pub issue: Option<i64>,
    /// `en` or `es`, for what agents write. The app's language by default.
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StopRunArgs {
    /// A run's id.
    pub run: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendBackArgs {
    /// A run's id.
    pub run: String,
    /// What to change, plainly and concretely, in up to 16 KB. The agent that does the work again
    /// gets these words.
    pub instructions: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetryStepArgs {
    /// A run's id.
    pub run: String,
    /// Have the agent go on from where it stopped, instead of starting the step over. A step that
    /// isn't an agent always starts over. False by default.
    #[serde(default)]
    pub resume: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloseRunArgs {
    /// A run's id.
    pub run: String,
}

// Nothing here can make it edit, reach the network or deliver. Anything else is refused rather than
// ignored, so a client doesn't go on believing it got it.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartTaskArgs {
    /// A project's id or name.
    pub project: String,
    /// What to read, find out or review, and what to answer with, in up to 16 KB.
    pub prompt: String,
    /// `fable`, `opus`, `sonnet`, `haiku` or a full model id. `sonnet` by default.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<DraftEffort>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    /// What its card is called, in up to 80 characters. The prompt's first line by default.
    #[serde(default)]
    pub title: Option<String>,
    /// `en` or `es`, for what the agent writes. The app's language by default.
    #[serde(default)]
    pub language: Option<String>,
}

// Only the folder. The account that pushes and the commands Keel runs are the person's to choose, so
// a client has nowhere to name them.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeProjectArgs {
    /// The full path of a git repository on this Mac, starting with `/`.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DraftWorkflowArgs {
    /// A project's id or name.
    pub project: String,
    /// What to call the workflow, in up to 80 characters.
    pub name: String,
    /// The steps, in the order they run.
    pub steps: Vec<DraftStep>,
}

// What a client may propose, which is what the person can add in the editor and no more. A setup
// step is left out because it's the one with network, and a mapper because what it writes goes into
// the Brain. Plain comments, as doc comments become the schema's descriptions for the model.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum DraftStep {
    /// An agent working in the run's private copy.
    Agent {
        #[serde(flatten)]
        agent: DraftAgent,
    },
    /// A command Keel runs in a sandbox, with no model involved. Exit 0 passes.
    Check { id: String, run: String },
    /// Read-only agents that run at the same time.
    Parallel { id: String, steps: Vec<DraftMember> },
    /// Waits for the person, for as long as it takes.
    Approval { id: String },
    /// Keel pushes a branch and opens a draft pull request. Needs an `approval` earlier.
    Deliver { id: String },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DraftAgent {
    /// Unique in the workflow: letters, digits, `-` and `_`, up to 40.
    pub id: String,
    /// `fable`, `opus`, `sonnet`, `haiku` or a full model id.
    pub model: String,
    /// What it's told to do. It can use {{request}}, {{diff}}, {{rework}}, {{answers}}, {{check}} and
    /// {{steps.<id>.output}}.
    pub prompt: String,
    /// What it may do. Read-only unless you say otherwise.
    #[serde(default)]
    pub tools: Option<DraftTools>,
    /// What it's for. A worker unless you say otherwise.
    #[serde(default)]
    pub role: Option<DraftRole>,
    #[serde(default)]
    pub effort: Option<DraftEffort>,
    /// Run it only if the run changed files matching one of these globs.
    #[serde(default)]
    pub paths: Vec<String>,
    /// `notSmall` skips it for a change Keel finds small. Keel decides that itself, from the change
    /// and whether the check passed, and a person still approves. It has to come after the first
    /// worker that builds.
    #[serde(default)]
    pub when: Option<DraftCondition>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
}

/// One agent of a `parallel`. A step that asks for something Keel doesn't do is refused, not ignored,
/// so a client doesn't go on believing it got it.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftMember {
    #[serde(flatten)]
    agent: DraftAgent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DraftTools {
    /// Reads code. No shell, so it can't run anything.
    ReadOnly,
    /// Edits files and runs commands inside the sandbox.
    Build,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DraftCondition {
    /// Only if the change isn't small.
    NotSmall,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DraftEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DraftRole {
    Worker,
    /// Reads the change and returns a verdict with findings. Read-only.
    Reviewer,
    /// Weighs the reviews and decides. Sending work back reruns everything from `reworkTo`, an
    /// earlier step outside a group, at most `maxRounds` times (5 at most). Read-only.
    Approver {
        rework_to: String,
        max_rounds: u32,
    },
    /// Says what the run taught, for the person to accept. Read-only, and only as the last step.
    Learner,
    /// Works out what a request will change, and may ask the person what only they can decide, in
    /// Keel. Read-only.
    Specifier,
    /// Says, for each thing a specifier earlier in the workflow said has to be true, whether a test
    /// really proves it. Keel corrects its answer. Read-only.
    Verifier,
}

impl McpServer {
    pub(crate) async fn list_projects(
        &self,
        client: McpClient,
        args: ListProjectsArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "list_projects", args, |server, _, _| {
            let projects = server.0.store.projects().map_err(store_error)?;
            let projects: Vec<Json> = projects.iter().map(project_view).collect();
            let proposals: Vec<Json> = server
                .0
                .engine
                .project_proposals()
                .map_err(engine_error)?
                .iter()
                .map(proposal_view)
                .collect();
            Ok((
                json!({ "projects": projects, "proposals": proposals }),
                None,
            ))
        })
        .await
    }

    pub(crate) async fn list_workflows(
        &self,
        client: McpClient,
        args: ListWorkflowsArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "list_workflows", args, |server, _, args| {
            let project = server.project(&args.project)?;
            let language = server.language(args.language.as_deref())?;
            let workflows = server
                .0
                .engine
                .workflows(&project.id, &language)
                .map_err(engine_error)?;
            let workflows: Vec<Json> = workflows
                .iter()
                .map(|workflow| {
                    json!({
                        "id": workflow.id,
                        "name": workflow.name,
                        "builtin": workflow.builtin,
                        "steps": workflow.steps.iter().map(step_summary).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let drafts = server
                .0
                .engine
                .workflow_drafts_of(&project.id)
                .map_err(engine_error)?;
            let drafts: Vec<Json> = drafts
                .iter()
                .map(|draft| {
                    json!({
                        "id": draft.id,
                        "name": draft.name,
                        "proposedBy": draft.proposed_by,
                        "createdAt": draft.created_at,
                    })
                })
                .collect();
            Ok((json!({ "workflows": workflows, "drafts": drafts }), None))
        })
        .await
    }

    pub(crate) async fn list_issues(
        &self,
        client: McpClient,
        args: ListIssuesArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "list_issues", args, |server, _, args| {
            let project = server.project(&args.project)?;
            let issues = server.0.engine.issues(&project.id).map_err(engine_error)?;
            let issues: Vec<Json> = issues
                .iter()
                .map(|issue| {
                    json!({
                        "number": issue.number,
                        "title": issue.title,
                        "author": issue.author,
                        "url": issue.url,
                    })
                })
                .collect();
            Ok((json!({ "issues": issues }), None))
        })
        .await
    }

    pub(crate) async fn list_runs(
        &self,
        client: McpClient,
        args: ListRunsArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "list_runs", args, |server, client, args| {
            let limit = args.limit.unwrap_or(20).clamp(1, 100) as usize;
            let project = args
                .project
                .as_deref()
                .map(|wanted| server.project(wanted))
                .transpose()?;
            let scan = if project.is_some() {
                1000
            } else {
                limit as i64
            };
            let store = &server.0.store;
            let runs = if args.mine {
                store.runs_started_by_mcp_client(&client.id, scan)
            } else {
                store.runs(scan)
            }
            .map_err(store_error)?;
            let runs: Vec<_> = runs
                .into_iter()
                .filter(|run| project.as_ref().is_none_or(|p| p.id == run.project_id))
                .take(limit)
                .collect();
            Ok((json!({ "runs": runs }), None))
        })
        .await
    }

    pub(crate) async fn get_run(
        &self,
        client: McpClient,
        args: GetRunArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "get_run", args, |server, client, args| {
            Ok((server.run_view(client, &args.run)?, Some(args.run)))
        })
        .await
    }

    pub(crate) async fn get_step(
        &self,
        client: McpClient,
        args: GetStepArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "get_step", args, |server, _, args| {
            let attempt = server.0.store.attempt(&args.attempt).map_err(|_| {
                ToolError::new("Keel has no attempt with that id. get_run lists them.")
            })?;
            let diff = server.0.engine.diff(&attempt.id).ok();
            let mut view = attempt_view(&attempt);
            view["prompt"] = json!(capped(&attempt.prompt, MAX_TEXT));
            view["brainReceived"] = attempt.system_prompt_sources.clone();
            view["diff"] = json!(diff.as_deref().map(|text| capped(text, MAX_DIFF)));
            // An agent's events can be many, and only a command has this.
            if matches!(attempt.kind.as_str(), "setup" | "check")
                && let Some(output) = server
                    .0
                    .store
                    .events(&attempt.id, 0)
                    .ok()
                    .as_deref()
                    .and_then(command_output)
            {
                view["commandOutput"] = output;
            }
            Ok((view, Some(attempt.run_id.clone())))
        })
        .await
    }

    pub(crate) async fn get_brain(
        &self,
        client: McpClient,
        args: GetBrainArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "get_brain", args, |server, _, args| {
            let project = server.project(&args.project)?;
            let brain = server.0.engine.brain(&project.id).map_err(engine_error)?;
            let view = serde_json::to_value(&brain).map_err(|e| ToolError::new(e.to_string()))?;
            Ok((view, None))
        })
        .await
    }

    pub(crate) async fn wait_for_run(
        &self,
        client: McpClient,
        args: WaitForRunArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "wait_for_run", args, |server, client, args| {
            let seconds = args.seconds.unwrap_or(WAIT_DEFAULT_SECS).min(WAIT_MAX_SECS);
            let deadline = Instant::now() + Duration::from_secs(seconds);
            if args.runs.is_empty() {
                let run = args.run.ok_or_else(|| {
                    ToolError::new("Say which run to wait for with `run`, or several with `runs`.")
                })?;
                loop {
                    let (row, attempts) = server.run_with_attempts(&run)?;
                    if run_state(&row, &attempts).0 != "working" || Instant::now() >= deadline {
                        let view = server.view_of(client, &row, &attempts, false)?;
                        return Ok((view, Some(run)));
                    }
                    std::thread::sleep(WAIT_TICK);
                }
            }
            // Before anything goes through them: repeats only come out below, one by one.
            if args.runs.len() > MAX_WAITED_RUNS {
                return Err(too_many_to_wait_for());
            }
            let mut ids: Vec<String> = Vec::new();
            for id in args.run.into_iter().chain(args.runs) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            if ids.len() > MAX_WAITED_RUNS {
                return Err(too_many_to_wait_for());
            }
            for id in &ids {
                server.0.store.run(id).map_err(|_| {
                    ToolError::new(format!("Keel has no run {id:?}. list_runs lists them."))
                })?;
            }
            loop {
                let runs = ids
                    .iter()
                    .map(|id| server.run_with_attempts(id))
                    .collect::<Result<Vec<_>, _>>()?;
                let ready: Vec<&str> = runs
                    .iter()
                    .filter(|(run, attempts)| run_state(run, attempts).0 != "working")
                    .map(|(run, _)| run.id.as_str())
                    .collect();
                if !ready.is_empty() || Instant::now() >= deadline {
                    // Only the first that's ready comes in full, so this reads in one go however
                    // many are ready at once. get_run has each of the others.
                    let views = runs
                        .iter()
                        .map(|(run, attempts)| match ready.first() {
                            Some(&first) if first == run.id => {
                                server.view_of(client, run, attempts, false)
                            }
                            _ => Ok(brief_view(run, attempts)),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok((json!({ "ready": ready, "runs": views }), None));
                }
                std::thread::sleep(WAIT_TICK);
            }
        })
        .await
    }

    pub(crate) async fn start_run(
        &self,
        client: McpClient,
        args: StartRunArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "start_run", args, |server, client, args| {
            if !client.can_run {
                return Err(may_only_read());
            }
            server.room_for_another_run(client)?;
            let project = server.project(&args.project)?;
            let language = server.language(args.language.as_deref())?;
            let engine = &server.0.engine;
            let workflow = engine
                .workflows(&project.id, &language)
                .map_err(engine_error)?
                .into_iter()
                .find(|workflow| workflow.id == args.workflow)
                .ok_or_else(|| {
                    ToolError::new(format!(
                        "{} has no workflow with the id {:?}. list_workflows lists them.",
                        project.name, args.workflow
                    ))
                })?;
            let extra = args.request.as_deref().unwrap_or("").trim();
            let issue = args
                .issue
                .map(|number| {
                    engine
                        .issues(&project.id)
                        .map_err(engine_error)?
                        .into_iter()
                        .find(|issue| issue.number == number)
                        .ok_or_else(|| {
                            ToolError::new(format!(
                                "{} has no open issue #{number}. list_issues lists them.",
                                project.name
                            ))
                        })
                })
                .transpose()?;
            let request = match &issue {
                Some(issue) => request_from_issue(issue, extra),
                None if extra.is_empty() => {
                    return Err(ToolError::new("Say what to do with `request` or `issue`."));
                }
                None => extra.to_owned(),
            };
            let issue_number = issue.as_ref().map(|issue| issue.number);
            let run = engine
                .start_run_on_board(StartRun {
                    project_id: project.id,
                    title: issue
                        .as_ref()
                        .map(|issue| format!("#{} {}", issue.number, issue.title)),
                    request,
                    saved_workflow: Some(workflow.reference()),
                    workflow: workflow.steps,
                    language,
                    source_issue: issue,
                    started_by: Some(client.name.clone()),
                })
                .map_err(|error| match (error, issue_number) {
                    (Error::InvalidState, Some(number)) => ToolError::new(format!(
                        "Issue #{number} already has a run open on the board. list_runs shows it."
                    )),
                    (error, _) => engine_error(error),
                })?;
            engine.announce(&run);
            Ok((server.new_run_view(client, &run)?, Some(run)))
        })
        .await
    }

    /// A single agent that only reads, in the run's own copy of the project. It can't edit, so no
    /// setup runs before it, and there's nothing to deliver.
    pub(crate) async fn start_task(
        &self,
        client: McpClient,
        args: StartTaskArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "start_task", args, |server, client, args| {
            if !client.can_run {
                return Err(may_only_read());
            }
            server.room_for_another_run(client)?;
            let project = server.project(&args.project)?;
            let language = server.language(args.language.as_deref())?;
            let prompt = args.prompt.trim();
            if prompt.is_empty() || prompt.len() > MAX_TEXT {
                return Err(ToolError::new(
                    "Say what to find out in `prompt`, in up to 16 KB.",
                ));
            }
            let model = args.model.as_deref().map_or(TASK_MODEL, str::trim);
            // It goes to `claude` as an argument, so it can't be left looking like a flag.
            if !plain_model_name(model) {
                return Err(ToolError::new(
                    "`model` has to be a model's name, like sonnet or claude-opus-5-5.",
                ));
            }
            if args.max_turns == Some(0) || args.max_budget_usd.is_some_and(|budget| budget <= 0.0)
            {
                return Err(ToolError::new(
                    "`maxTurns` and `maxBudgetUsd` have to be above zero.",
                ));
            }
            let title = match args.title.as_deref().map(str::trim) {
                None | Some("") => None,
                Some(title) if title.chars().count() > MAX_TASK_TITLE => {
                    return Err(ToolError::new(format!(
                        "Give the task a title of up to {MAX_TASK_TITLE} characters, or none."
                    )));
                }
                Some(title) => Some(title.to_owned()),
            };
            let step = StepSpec {
                id: "task".into(),
                model: model.to_owned(),
                effort: args.effort.map(|effort| effort.as_str().to_owned()),
                tools: ToolProfile::ReadOnly,
                prompt: prompt.to_owned(),
                max_turns: args.max_turns,
                max_budget_usd: args.max_budget_usd,
                paths: Vec::new(),
                when: None,
                role: StepRole::Worker,
            };
            let mut request = StartRun::one_step(project.id, step, language);
            request.title = title;
            request.started_by = Some(client.name.clone());
            let engine = &server.0.engine;
            let run = engine.start_run_on_board(request).map_err(engine_error)?;
            engine.announce(&run);
            Ok((server.new_run_view(client, &run)?, Some(run)))
        })
        .await
    }

    pub(crate) async fn draft_workflow(
        &self,
        client: McpClient,
        args: DraftWorkflowArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "draft_workflow", args, |server, client, args| {
            if !client.can_run {
                return Err(may_only_read());
            }
            let project = server.project(&args.project)?;
            let name = args.name.trim();
            if name.is_empty() || name.chars().count() > MAX_DRAFT_NAME {
                return Err(ToolError::new(format!(
                    "Give the workflow a name of up to {MAX_DRAFT_NAME} characters."
                )));
            }
            let steps = args
                .steps
                .into_iter()
                .map(DraftStep::into_step)
                .collect::<Result<Vec<_>, _>>()?;
            let engine = &server.0.engine;
            let waiting = engine
                .workflow_drafts_of(&project.id)
                .map_err(engine_error)?
                .len();
            if waiting >= MAX_DRAFTS {
                return Err(ToolError::new(format!(
                    "{} already has {waiting} drafts waiting, which is as many as it may have. The person saves or discards them in Keel.",
                    project.name
                )));
            }
            let draft = engine
                .draft_workflow(&project.id, name, steps, &client.name)
                .map_err(engine_error)?;
            Ok((
                json!({
                    "draftId": draft.id,
                    "projectId": draft.project_id,
                    "name": draft.name,
                    "steps": draft.steps.iter().map(step_summary).collect::<Vec<_>>(),
                    "state": "waitingForPerson",
                    "who": "The person, on Keel's Workflows page. There is no tool for this. It can't run until they save it, and they may change it first. Tell them it's there.",
                }),
                None,
            ))
        })
        .await
    }

    pub(crate) async fn propose_project(
        &self,
        client: McpClient,
        args: ProposeProjectArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "propose_project", args, |server, client, args| {
            if !client.can_run {
                return Err(may_only_read());
            }
            let path = Path::new(&args.path);
            if !path.is_absolute() || args.path.chars().count() > MAX_PROPOSED_PATH {
                return Err(ToolError::new(format!(
                    "Give the repository's full path, starting with `/`, in up to {MAX_PROPOSED_PATH} characters."
                )));
            }
            let answer = match server
                .0
                .engine
                .propose_project(path, &client.name)
                .map_err(engine_error)?
            {
                ProposedProject::Waiting(proposal) => {
                    let mut answer = proposal_view(&proposal);
                    answer["state"] = json!("waitingForPerson");
                    answer["who"] = json!(
                        "The person, in Keel. There is no tool for this. It isn't a project, and no run can start on it, until they add it. They choose its GitHub account and commands, and adding it starts an agent that maps the repo. Tell them it's there."
                    );
                    answer
                }
                ProposedProject::InKeel(project) => {
                    json!({ "state": "alreadyInKeel", "project": project_view(&project) })
                }
                ProposedProject::Full => {
                    return Err(ToolError::new(format!(
                        "{MAX_PROJECT_PROPOSALS} projects are already waiting, which is as many as may wait. The person adds or discards them in Keel."
                    )));
                }
                ProposedProject::UnreadablePath => {
                    return Err(ToolError::new(
                        "This folder's path has characters Keel won't show as a proposal, because they could hide what it is. The person can add it by hand in Keel.",
                    ));
                }
            };
            Ok((answer, None))
        })
        .await
    }

    pub(crate) async fn stop_run(
        &self,
        client: McpClient,
        args: StopRunArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "stop_run", args, |server, client, args| {
            if !client.can_run {
                return Err(may_only_read());
            }
            let attempts = server.attempts(&args.run)?;
            let working: Vec<&Attempt> = attempts
                .iter()
                .filter(|attempt| STOPPABLE.contains(&attempt.state.as_str()))
                .collect();
            if working.is_empty() {
                return Err(ToolError::new(
                    "Nothing in this run is working. If it's waiting for the person, only they can answer it, in Keel.",
                ));
            }
            for attempt in working {
                server.0.engine.stop(&attempt.id).map_err(engine_error)?;
            }
            Ok((server.run_view(client, &args.run)?, Some(args.run)))
        })
        .await
    }

    pub(crate) async fn send_back(
        &self,
        client: McpClient,
        args: SendBackArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "send_back", args, |server, client, args| {
            let (run, attempts) = server.steerable(client, &args.run)?;
            let approval = send_back_target(&run, &steps_of(&run), &attempts)?;
            let instructions = args.instructions.trim();
            if instructions.is_empty() || instructions.len() > MAX_TEXT {
                return Err(ToolError::new(
                    "Say what to change in `instructions`, in up to 16 KB.",
                ));
            }
            if !server.feedback_left(client, &run.id, "send_back")? {
                return Err(ToolError::new(format!(
                    "This client has sent this run back {MAX_FEEDBACK_PER_RUN} times already, which is as many as it may. Bring it to the person, who decides in Keel."
                )));
            }
            server
                .0
                .engine
                .send_back_by(&approval.id, instructions, &client.name)
                .map_err(changed_meanwhile)?;
            Ok((server.run_view(client, &args.run)?, Some(args.run)))
        })
        .await
    }

    pub(crate) async fn retry_step(
        &self,
        client: McpClient,
        args: RetryStepArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "retry_step", args, |server, client, args| {
            let (run, attempts) = server.steerable(client, &args.run)?;
            let step = retry_target(&run, &attempts)?;
            if !server.feedback_left(client, &run.id, "retry_step")? {
                return Err(ToolError::new(format!(
                    "This client has retried this run {MAX_FEEDBACK_PER_RUN} times already, which is as many as it may. Bring it to the person, who decides in Keel."
                )));
            }
            // As the app's Retry button: the step starts over unless asked to go on.
            let resume = args.resume.unwrap_or(false);
            server
                .0
                .engine
                .retry_by(&step.id, resume, &client.name)
                .map_err(changed_meanwhile)?;
            Ok((server.run_view(client, &args.run)?, Some(args.run)))
        })
        .await
    }

    pub(crate) async fn close_run(
        &self,
        client: McpClient,
        args: CloseRunArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "close_run", args, |server, client, args| {
            let (run, attempts) = server.steerable(client, &args.run)?;
            let target = close_target(&run, &attempts)?;
            server
                .0
                .engine
                .abandon_by(&target.id, &client.name)
                .map_err(changed_meanwhile)?;
            Ok((server.run_view(client, &args.run)?, Some(args.run)))
        })
        .await
    }

    /// Runs one call on a blocking thread and records it. A result never leaves without its row.
    async fn call<A, F>(
        &self,
        client: McpClient,
        tool: &'static str,
        args: A,
        work: F,
    ) -> Result<Json, ToolError>
    where
        A: Serialize + Send + 'static,
        F: FnOnce(&McpServer, &McpClient, A) -> Result<(Json, Option<String>), ToolError>
            + Send
            + 'static,
    {
        let server = self.clone();
        let done = tokio::task::spawn_blocking(move || {
            // What a client may still do, its open runs and its retries, is counted from these rows,
            // and a row is only written once the work is done. So a client's calls that change
            // something go one at a time, until the row is in: two at once would count the same
            // rows, and both go through. One that only looks, like a long wait_for_run, holds
            // nothing up.
            let turn = (!ONLY_LOOK.contains(&tool)).then(|| server.turn_of(&client.id));
            let _turn = turn.as_deref().map(locked);
            let started = Instant::now();
            let logged = serde_json::to_string(&args).unwrap_or_default();
            let logged = capped(&logged, MAX_LOGGED_ARGS);
            let outcome = work(&server, &client, args);
            let (run_id, error) = match &outcome {
                Ok((_, run_id)) => (run_id.clone(), None),
                Err(error) => (None, Some(capped(&error.0, MAX_LOGGED_ERROR))),
            };
            server
                .0
                .store
                .add_mcp_call(&NewMcpCall {
                    client: &client,
                    tool,
                    args: &logged,
                    error: error.as_deref(),
                    run_id: run_id.as_deref(),
                    duration_ms: started.elapsed().as_millis() as i64,
                })
                .map_err(store_error)?;
            // Only what leaves here is masked. The row above keeps what really happened.
            outcome
                .map(|(json, _)| mask_value(json))
                .map_err(|error| ToolError(mask_secrets(&error.0)))
        });
        done.await
            .unwrap_or_else(|e| Err(ToolError::new(format!("Keel failed on this call: {e}"))))
    }

    /// What a client's calls that change something take turns on.
    fn turn_of(&self, client_id: &str) -> Arc<Mutex<()>> {
        locked(&self.0.changing)
            .entry(client_id.to_owned())
            .or_default()
            .clone()
    }

    /// A project by its id or, when only one has it, its name.
    fn project(&self, wanted: &str) -> Result<Project, ToolError> {
        let projects = self.0.store.projects().map_err(store_error)?;
        if let Some(project) = projects.iter().find(|project| project.id == wanted) {
            return Ok(project.clone());
        }
        let mut named = projects
            .into_iter()
            .filter(|project| project.name == wanted);
        match (named.next(), named.next()) {
            (Some(project), None) => Ok(project),
            (Some(_), Some(_)) => Err(ToolError::new(format!(
                "Several projects are called {wanted:?}. Use the id from list_projects."
            ))),
            _ => Err(ToolError::new(format!(
                "Keel has no project {wanted:?}. list_projects lists them."
            ))),
        }
    }

    fn language(&self, wanted: Option<&str>) -> Result<String, ToolError> {
        match wanted {
            Some(language @ ("en" | "es")) => Ok(language.to_owned()),
            Some(other) => Err(ToolError::new(format!(
                "Keel writes in `en` or `es`, not {other:?}."
            ))),
            None => Ok(self
                .0
                .store
                .setting("language")
                .map_err(store_error)?
                .filter(|language| language == "es")
                .unwrap_or_else(|| "en".to_owned())),
        }
    }

    fn attempts(&self, run: &str) -> Result<Vec<Attempt>, ToolError> {
        self.0
            .store
            .run(run)
            .map_err(|_| ToolError::new("Keel has no run with that id. list_runs lists them."))?;
        self.0.store.attempts_of_run(run).map_err(store_error)
    }

    fn run_with_attempts(&self, run_id: &str) -> Result<(Run, Vec<Attempt>), ToolError> {
        let attempts = self.attempts(run_id)?;
        let run = self.0.store.run(run_id).map_err(store_error)?;
        Ok((run, attempts))
    }

    /// Refused once this client has as many runs open as the person allows it.
    fn room_for_another_run(&self, client: &McpClient) -> Result<(), ToolError> {
        let open = self
            .0
            .store
            .open_runs_of_mcp_client(&client.id)
            .map_err(store_error)?;
        if open >= client.max_open_runs.clamp(1, MAX_OPEN_RUNS_CEILING) {
            return Err(ToolError::new(format!(
                "This client has as many runs open as the person allows it ({open}). They finish when the person decides in Keel, and the person can raise the limit in Keel's settings."
            )));
        }
        Ok(())
    }

    /// A run this client may give feedback on, with its attempts, or why it may not.
    fn steerable(
        &self,
        client: &McpClient,
        run_id: &str,
    ) -> Result<(Run, Vec<Attempt>), ToolError> {
        may_steer(client)?;
        let attempts = self.attempts(run_id)?;
        let store = &self.0.store;
        if !store
            .run_started_by_mcp_client(run_id, &client.id)
            .map_err(store_error)?
        {
            return Err(ToolError::new(
                "Only the person acts on a run this client didn't start, in Keel.",
            ));
        }
        let run = store.run(run_id).map_err(store_error)?;
        Ok((run, attempts))
    }

    /// Whether this client may still use a feedback tool on one of its runs.
    fn feedback_left(
        &self,
        client: &McpClient,
        run_id: &str,
        tool: &str,
    ) -> Result<bool, ToolError> {
        let used = self
            .0
            .store
            .mcp_calls_on_run(&client.id, run_id, tool)
            .map_err(store_error)?;
        Ok(used < MAX_FEEDBACK_PER_RUN)
    }

    /// The tools this client may use on this run right now, by the same rules the tools follow.
    fn you_can(
        &self,
        client: &McpClient,
        mine: bool,
        run: &Run,
        steps: &[WorkflowStep],
        attempts: &[Attempt],
    ) -> Result<Vec<&'static str>, ToolError> {
        let mut tools = Vec::new();
        if may_steer(client).is_ok() && mine {
            if send_back_target(run, steps, attempts).is_ok()
                && self.feedback_left(client, &run.id, "send_back")?
            {
                tools.push("send_back");
            }
            if retry_target(run, attempts).is_ok()
                && self.feedback_left(client, &run.id, "retry_step")?
            {
                tools.push("retry_step");
            }
            if close_target(run, attempts).is_ok() {
                tools.push("close_run");
            }
        }
        if client.can_run && attempts.iter().any(working) {
            tools.push("stop_run");
        }
        Ok(tools)
    }

    fn run_view(&self, client: &McpClient, run_id: &str) -> Result<Json, ToolError> {
        let (run, attempts) = self.run_with_attempts(run_id)?;
        self.view_of(client, &run, &attempts, false)
    }

    /// For the call that started the run: its row, which is how a run is known to be this
    /// client's, is only written once it returns.
    fn new_run_view(&self, client: &McpClient, run_id: &str) -> Result<Json, ToolError> {
        let (run, attempts) = self.run_with_attempts(run_id)?;
        self.view_of(client, &run, &attempts, true)
    }

    /// A run as get_run returns it. What it waits for and the report are whole, and each attempt
    /// is listed without what it wrote, which get_step has, so it reads in one go however long the
    /// run went on.
    fn view_of(
        &self,
        client: &McpClient,
        run: &Run,
        attempts: &[Attempt],
        just_started: bool,
    ) -> Result<Json, ToolError> {
        let store = &self.0.store;
        let mine = just_started
            || store
                .run_started_by_mcp_client(&run.id, &client.id)
                .map_err(store_error)?;
        let steps = steps_of(run);
        let (state, waiting) = run_state(run, attempts);
        let waiting_for = match waiting {
            Some(attempt) => {
                let mut view = json!({
                    "step": attempt.step_id,
                    "attemptId": attempt.id,
                    "state": attempt.state,
                    "reason": attempt.reason,
                    "youCan": self.you_can(client, mine, run, &steps, attempts)?,
                    "who": who_decides(&attempt.state),
                });
                if attempt.state == AttemptState::WaitingAnswers.as_str() {
                    let questions = spec_of_attempt(attempt).map(|spec| spec.questions);
                    view["questions"] = json!(questions.unwrap_or_default());
                }
                if attempt.state == AttemptState::WaitingApproval.as_str() {
                    view["canSendBack"] = json!(can_send_back(&steps, &attempt.step_id));
                }
                Some(view)
            }
            None => None,
        };
        let total_cost: f64 = attempts.iter().filter_map(|a| a.cost_usd).sum();
        let (step_views, left_out) = steps_view(attempts);
        let mut view = json!({
            "id": run.id,
            "projectId": run.project_id,
            "title": run.title,
            "request": capped(&run.request, MAX_TEXT),
            "state": state,
            "waitingFor": waiting_for,
            "report": self.report(run, attempts)?,
            "startedBy": run.started_by,
            "startedByYou": mine,
            "cardId": store.card_of_run(&run.id).map_err(store_error)?,
            "language": run.language,
            "branch": run.branch,
            "prUrl": run.pr_url,
            "prState": run.pr_state,
            "prStateAt": run.pr_state_at,
            "prCheckedAt": run.pr_checked_at,
            "workflowId": run.workflow_id,
            "workflowName": run.workflow_name,
            "round": run.round,
            "createdAt": run.created_at,
            "finishedAt": run.finished_at,
            "totalCostUsd": total_cost,
            "steps": step_views,
        });
        if left_out > 0 {
            view["earlierAttemptsLeftOut"] = json!(left_out);
        }
        Ok(view)
    }

    /// What the run did, in short, the way a subagent answers: what an orchestrator reads first.
    /// What agents wrote is cut here, and masked on the way out like the rest. A change to many
    /// files lists the first ones and says how many more there are.
    fn report(&self, run: &Run, attempts: &[Attempt]) -> Result<Json, ToolError> {
        let mut files = files_changed(attempts);
        let more_files = files.len().saturating_sub(MAX_LISTED_FILES);
        files.truncate(MAX_LISTED_FILES);
        let mut report = json!({
            "answer": answer(attempts),
            "filesChanged": files,
            "check": self.check_report(run, attempts)?,
            "notProven": not_proven(attempts),
            "reviews": reviews(attempts),
            "skipped": skipped_steps(attempts),
            "prUrl": run.pr_url,
            "prState": run.pr_state,
        });
        if more_files > 0 {
            report["moreFilesChanged"] = json!(more_files);
        }
        Ok(report)
    }

    /// Whether the check passed on the code as it is in this round, and what it printed when it
    /// failed. One from an earlier round checked code that has changed since, so it says nothing.
    /// With several, a failed one is what matters.
    fn check_report(&self, run: &Run, attempts: &[Attempt]) -> Result<Json, ToolError> {
        let passed = AttemptState::Succeeded.as_str();
        let failed = AttemptState::Failed.as_str();
        let checks: Vec<&Attempt> = latest_of_each_step(attempts)
            .into_iter()
            .filter(|a| a.kind == "check" && a.round == run.round)
            .filter(|a| a.state == passed || a.state == failed)
            .collect();
        let failure = checks.iter().find(|a| a.state == failed);
        let Some(check) = failure.or(checks.last()) else {
            return Ok(Json::Null);
        };
        let mut view = json!({ "step": check.step_id, "passed": failure.is_none() });
        if failure.is_some() {
            let events = self.0.store.events(&check.id, 0).map_err(store_error)?;
            view["output"] = json!(command_output(&events));
        }
        Ok(view)
    }
}

/// Each step's latest attempt, in the order they came.
fn latest_of_each_step(attempts: &[Attempt]) -> Vec<&Attempt> {
    attempts
        .iter()
        .enumerate()
        .filter(|(index, attempt)| {
            !attempts[index + 1..]
                .iter()
                .any(|later| later.step_id == attempt.step_id)
        })
        .map(|(_, attempt)| attempt)
        .collect()
}

/// The step a person has to act on, if any: looking only at each step's latest attempt, and never
/// at a learner, since its failure doesn't stop a run.
fn needs_person(attempts: &[Attempt]) -> Option<&Attempt> {
    latest_of_each_step(attempts)
        .into_iter()
        .find(|attempt| attempt.role != "learner" && NEEDS_PERSON.contains(&attempt.state.as_str()))
}

/// Where a run stands, and the attempt the person has to act on when it waits for them.
fn run_state<'a>(run: &Run, attempts: &'a [Attempt]) -> (&'static str, Option<&'a Attempt>) {
    if run.finished_at.is_some() {
        return ("finished", None);
    }
    match needs_person(attempts) {
        Some(attempt) => ("waitingForPerson", Some(attempt)),
        None => ("working", None),
    }
}

fn working(attempt: &Attempt) -> bool {
    STOPPABLE.contains(&attempt.state.as_str())
}

fn steps_of(run: &Run) -> Vec<WorkflowStep> {
    serde_json::from_value(run.workflow.clone()).unwrap_or_default()
}

/// Who moves a run on from where it waits. Answering and approving are only ever the person's.
fn who_decides(state: &str) -> &'static str {
    match state {
        "waitingAnswers" => {
            "Only the person answers these questions, in Keel. There is no tool for it. Bring them the questions as they are, and don't answer for them."
        }
        "waitingApproval" => {
            "Only the person approves or rejects this, in Keel. There is no tool for it. Tell them it's waiting, and don't decide for them."
        }
        _ => {
            "The person decides what happens next, in Keel. This client may only use what youCan lists."
        }
    }
}

/// Whether the person lets this client give feedback at all. Every feedback tool asks this first,
/// in this order, so the refusal says what's missing. The run also has to be the client's own.
fn may_steer(client: &McpClient) -> Result<(), ToolError> {
    if !client.can_run {
        return Err(may_only_read());
    }
    if !client.can_steer {
        return Err(ToolError::new(
            "This client may not give feedback. The person can allow it in Keel's settings.",
        ));
    }
    Ok(())
}

fn still_open(run: &Run) -> Result<(), ToolError> {
    match run.finished_at {
        Some(_) => Err(ToolError::new(
            "This run has finished, so there's nothing left to do on it.",
        )),
        None => Ok(()),
    }
}

/// The approval a client could send the work back from: only one this run waits at, with something
/// before it that would do the work again.
fn send_back_target<'a>(
    run: &Run,
    steps: &[WorkflowStep],
    attempts: &'a [Attempt],
) -> Result<&'a Attempt, ToolError> {
    still_open(run)?;
    let approval = needs_person(attempts)
        .filter(|a| a.state == AttemptState::WaitingApproval.as_str())
        .ok_or_else(|| {
            ToolError::new(
                "This run isn't waiting at an approval, which is the only place work can be sent back from.",
            )
        })?;
    if !can_send_back(steps, &approval.step_id) {
        return Err(ToolError::new(
            "Nothing before this approval would do the work again, so it can't be sent back. Bring it to the person, who decides in Keel.",
        ));
    }
    Ok(approval)
}

/// The step a client could retry: only one that failed, was interrupted or was stopped. One that's
/// blocked or stalled needs the person's judgment.
fn retry_target<'a>(run: &Run, attempts: &'a [Attempt]) -> Result<&'a Attempt, ToolError> {
    still_open(run)?;
    match needs_person(attempts) {
        Some(step) if matches!(step.state.as_str(), "failed" | "interrupted" | "stopped") => {
            Ok(step)
        }
        Some(step) if matches!(step.state.as_str(), "blocked" | "stalled") => {
            Err(ToolError::new(format!(
                "Step {:?} is {}. What happens next is the person's to decide, in Keel.",
                step.step_id, step.state
            )))
        }
        _ => Err(ToolError::new(
            "No step in this run failed, was interrupted or was stopped, so there's nothing to retry.",
        )),
    }
}

/// The attempt a client could close the run on: the one the person would act on, or else the
/// latest step that didn't go through. Never while something works, since closing doesn't stop it.
/// The engine looks again as it closes, in case the run moved meanwhile.
fn close_target<'a>(run: &Run, attempts: &'a [Attempt]) -> Result<&'a Attempt, ToolError> {
    still_open(run)?;
    if attempts.iter().any(working) {
        return Err(ToolError::new(
            "Something in this run is still working. Stop it with stop_run first, then close it.",
        ));
    }
    needs_person(attempts)
        .or_else(|| {
            latest_of_each_step(attempts)
                .into_iter()
                .rev()
                .find(|a| !matches!(a.state.as_str(), "succeeded" | "skipped"))
        })
        // On its way to the next round, or to what was skipped.
        .filter(|a| !matches!(a.state.as_str(), "returned" | "postponed"))
        .ok_or_else(|| {
            ToolError::new(
                "This run is moving between steps, so it can't be closed right now. Try again in a moment.",
            )
        })
}

/// What the last worker wrote last. For a task that only reads, that's what it found.
fn answer(attempts: &[Attempt]) -> Option<String> {
    attempts
        .iter()
        .rev()
        .find(|a| {
            a.kind == "agent" && a.role == "worker" && a.state != AttemptState::Skipped.as_str()
        })
        .and_then(|a| a.result_text.as_deref())
        .map(|text| capped(text, MAX_TEXT))
}

/// What the verifier says no test proves, after Keel corrected what it said. Null when no verifier
/// has answered, which isn't the same as everything being proven.
fn not_proven(attempts: &[Attempt]) -> Json {
    let verification = attempts
        .iter()
        .rev()
        .find(|a| a.role == "verifier")
        .and_then(verification_of_attempt);
    match verification {
        Some(verification) => json!(
            verification
                .criteria
                .into_iter()
                .filter(|checked| checked.result != Outcome::Met)
                .collect::<Vec<_>>()
        ),
        None => Json::Null,
    }
}

/// Each reviewer's verdict and each approver's decision, from the latest of each. As in the
/// builder's rework prompt, an approver's instructions only count when it sent the work back.
fn reviews(attempts: &[Attempt]) -> Vec<Json> {
    latest_of_each_step(attempts)
        .into_iter()
        .filter(|a| a.kind == "agent" && a.state == AttemptState::Succeeded.as_str())
        .filter_map(|a| {
            let output = a.output.as_ref()?;
            let text = |key: &str| {
                output
                    .get(key)
                    .and_then(Json::as_str)
                    .map(|text| capped(text, MAX_FINDING))
            };
            match a.role.as_str() {
                "reviewer" => {
                    let findings: Vec<String> = output
                        .get("findings")
                        .and_then(Json::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Json::as_str)
                        .take(MAX_FINDINGS)
                        .map(|finding| capped(finding, MAX_FINDING))
                        .collect();
                    Some(json!({
                        "step": a.step_id,
                        "verdict": text("verdict"),
                        "findings": findings,
                    }))
                }
                "approver" => {
                    let rework = text("decision").as_deref() == Some("rework");
                    Some(json!({
                        "step": a.step_id,
                        "decision": text("decision"),
                        "risk": text("risk"),
                        "reasons": text("reasons"),
                        "instructions": text("instructions").filter(|_| rework),
                    }))
                }
                _ => None,
            }
        })
        .collect()
}

/// A run's attempts in order, for its view. One that a later attempt of the same step replaced
/// comes back shorter, and past MAX_EARLIER_ATTEMPTS of those the oldest are left out. Also says
/// how many were.
fn steps_view(attempts: &[Attempt]) -> (Vec<Json>, usize) {
    let latest: Vec<&str> = latest_of_each_step(attempts)
        .into_iter()
        .map(|attempt| attempt.id.as_str())
        .collect();
    let left_out = (attempts.len() - latest.len()).saturating_sub(MAX_EARLIER_ATTEMPTS);
    let mut to_leave = left_out;
    let views = attempts
        .iter()
        .filter_map(|attempt| {
            if latest.contains(&attempt.id.as_str()) {
                Some(step_view(attempt))
            } else if to_leave > 0 {
                to_leave -= 1;
                None
            } else {
                Some(earlier_view(attempt))
            }
        })
        .collect();
    (views, left_out)
}

/// An attempt in a run's view: what it is and how it went. What it wrote and the files it changed
/// are in get_step, and the report says what matters of them.
fn step_view(attempt: &Attempt) -> Json {
    json!({
        "attemptId": attempt.id,
        "step": attempt.step_id,
        "number": attempt.number,
        "round": attempt.round,
        "kind": attempt.kind,
        "role": attempt.role,
        "state": attempt.state,
        "reason": attempt.reason,
        "modelRequested": attempt.model_requested,
        "modelActual": attempt.model_actual,
        "costUsd": attempt.cost_usd,
        "inputTokens": attempt.input_tokens,
        "outputTokens": attempt.output_tokens,
        "turns": attempt.turns,
        "durationMs": attempt.duration_ms,
        "startedAt": attempt.started_at,
        "finishedAt": attempt.finished_at,
    })
}

/// An attempt that a later one of the same step replaced: enough to know how it went and to find
/// it with get_step.
fn earlier_view(attempt: &Attempt) -> Json {
    json!({
        "attemptId": attempt.id,
        "step": attempt.step_id,
        "number": attempt.number,
        "round": attempt.round,
        "state": attempt.state,
        "reason": attempt.reason,
        "superseded": true,
    })
}

/// One attempt in full, for get_step. What it answered comes once: its structured answer as Keel
/// read it, which Keel may have corrected, or else what it wrote.
fn attempt_view(attempt: &Attempt) -> Json {
    let mut view = step_view(attempt);
    view["account"] = json!(attempt.account);
    view["filesChanged"] = json!(attempt.files_changed);
    match &attempt.output {
        Some(output) => view["output"] = output.clone(),
        None => {
            let text = attempt.result_text.as_deref();
            view["result"] = json!(text.map(|text| capped(text, MAX_TEXT)));
        }
    }
    view
}

/// A run in a few words, for a wait on several: where it stands and the step it's on, or the one
/// it waits at. get_run has the rest.
fn brief_view(run: &Run, attempts: &[Attempt]) -> Json {
    let (state, waiting) = run_state(run, attempts);
    let step = waiting.or(attempts.last());
    json!({
        "id": run.id,
        "title": run.title,
        "state": state,
        "round": run.round,
        "step": step.map(|a| &a.step_id),
        "stepState": step.map(|a| &a.state),
        "brief": true,
    })
}

/// What a setup or check printed, which is how a client learns why it failed. Masked before it's
/// cut, because a cut can part a value from the name that gets it masked.
fn command_output(events: &[keel_store::Event]) -> Option<Json> {
    let result = &events
        .iter()
        .find(|event| event.kind == "checkResult")?
        .data;
    let text = |name: &str| {
        capped_end(
            &mask_secrets(result[name].as_str().unwrap_or_default()),
            MAX_TEXT,
        )
    };
    Some(json!({
        "exitCode": result["exitCode"],
        "stdout": text("stdout"),
        "stderr": text("stderr"),
    }))
}

fn project_view(project: &Project) -> Json {
    json!({
        "id": project.id,
        "name": project.name,
        "path": project.path,
        "githubRepo": project.github_repo,
        "githubAccount": project.github_account,
        "checkCommand": project.check_command,
    })
}

fn proposal_view(proposal: &ProjectProposal) -> Json {
    json!({
        "proposalId": proposal.id,
        "path": proposal.path,
        "proposedBy": proposal.proposed_by,
        "createdAt": proposal.created_at,
    })
}

/// A step without its prompt, which the workflow's owner wrote and start_run doesn't need.
fn step_summary(step: &WorkflowStep) -> Json {
    match step {
        WorkflowStep::Agent(spec) => json!({
            "type": "agent", "id": spec.id, "model": spec.model, "tools": spec.tools, "role": spec.role, "when": spec.when,
        }),
        WorkflowStep::Setup { id, run } => json!({ "type": "setup", "id": id, "run": run }),
        WorkflowStep::Check { id, run } => json!({ "type": "check", "id": id, "run": run }),
        WorkflowStep::Parallel { id, steps } => json!({
            "type": "parallel",
            "id": id,
            "steps": steps.iter().map(|spec| json!({
                "id": spec.id, "model": spec.model, "tools": spec.tools, "paths": spec.paths, "when": spec.when,
            })).collect::<Vec<_>>(),
        }),
        WorkflowStep::Approval { id } => json!({ "type": "approval", "id": id }),
        WorkflowStep::Deliver { id } => json!({ "type": "deliver", "id": id }),
    }
}

impl DraftStep {
    fn into_step(self) -> Result<WorkflowStep, ToolError> {
        Ok(match self {
            Self::Agent { agent } => WorkflowStep::Agent(agent.into_spec()?),
            Self::Check { id, run } => WorkflowStep::Check { id, run },
            Self::Parallel { id, steps } => WorkflowStep::Parallel {
                id,
                steps: steps
                    .into_iter()
                    .map(|member| member.agent.into_spec())
                    .collect::<Result<_, _>>()?,
            },
            Self::Approval { id } => WorkflowStep::Approval { id },
            Self::Deliver { id } => WorkflowStep::Deliver { id },
        })
    }
}

impl DraftAgent {
    fn into_spec(self) -> Result<StepSpec, ToolError> {
        // It goes to `claude` as an argument, so it can't be left looking like a flag.
        if !plain_model_name(&self.model) {
            return Err(ToolError::new(format!(
                "Step {:?}: `model` has to be a model's name, like opus or claude-opus-5-5.",
                self.id
            )));
        }
        if self.max_turns == Some(0) || self.max_budget_usd.is_some_and(|budget| budget <= 0.0) {
            return Err(ToolError::new(format!(
                "Step {:?}: `maxTurns` and `maxBudgetUsd` have to be above zero.",
                self.id
            )));
        }
        Ok(StepSpec {
            id: self.id,
            model: self.model,
            effort: self.effort.map(|effort| effort.as_str().to_owned()),
            tools: match self.tools {
                Some(DraftTools::Build) => ToolProfile::Build,
                Some(DraftTools::ReadOnly) | None => ToolProfile::ReadOnly,
            },
            prompt: self.prompt,
            max_turns: self.max_turns,
            max_budget_usd: self.max_budget_usd,
            paths: self.paths,
            when: self.when.map(|when| match when {
                DraftCondition::NotSmall => StepCondition::NotSmall,
            }),
            role: match self.role {
                Some(DraftRole::Worker) | None => StepRole::Worker,
                Some(DraftRole::Reviewer) => StepRole::Reviewer,
                Some(DraftRole::Approver {
                    rework_to,
                    max_rounds,
                }) => StepRole::Approver {
                    rework_to,
                    max_rounds,
                },
                Some(DraftRole::Learner) => StepRole::Learner,
                Some(DraftRole::Specifier) => StepRole::Specifier,
                Some(DraftRole::Verifier) => StepRole::Verifier,
            },
        })
    }
}

impl DraftEffort {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

fn plain_model_name(model: &str) -> bool {
    model.len() <= MAX_MODEL_NAME
        && model.starts_with(|c: char| c.is_ascii_alphanumeric())
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._:[]".contains(c))
}

/// A call that panicked doesn't keep the client's next ones waiting.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn too_many_to_wait_for() -> ToolError {
    ToolError::new(format!("Wait for up to {MAX_WAITED_RUNS} runs at once."))
}

fn may_only_read() -> ToolError {
    ToolError::new(
        "This client may only read. The person can let it start runs in Keel's settings.",
    )
}

fn engine_error(error: Error) -> ToolError {
    match error {
        Error::Blocked(reason) => ToolError::new(format!(
            "Keel refused: {}",
            serde_json::to_string(&reason).unwrap_or_default()
        )),
        other => ToolError::new(other.to_string()),
    }
}

/// The person, or the run itself, can act between a client looking and the engine acting on it.
fn changed_meanwhile(error: Error) -> ToolError {
    match error {
        Error::InvalidState => ToolError::new(
            "The run changed in the meantime, so Keel did nothing. Look at it again with get_run.",
        ),
        other => engine_error(other),
    }
}

fn store_error(error: keel_store::Error) -> ToolError {
    ToolError::new(error.to_string())
}

/// `text` up to `limit` bytes, on a character boundary, saying when it was cut. It stops before the
/// word it would cut, because half a token no longer has the shape that gets it masked.
fn capped(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &text[..end];
    let kept = kept
        .rfind(char::is_whitespace)
        .map_or(kept, |space| &kept[..space]);
    format!("{kept}\n… [cut at {limit} bytes]")
}

/// Like `capped` but keeping the end, where a command says what went wrong. It starts on a whole
/// line, since Keel keeps only the end of what a command prints and the first line may be half of one.
fn capped_end(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let kept = text[start..].split_once('\n').map_or("", |(_, rest)| rest);
    format!("… [only the last {limit} bytes]\n{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_name_cannot_pass_for_a_flag_or_anything_else() {
        for fine in [
            "opus",
            "claude-opus-5-5",
            "claude-opus-4-6[1m]",
            "us.anthropic.x:0",
        ] {
            assert!(plain_model_name(fine), "{fine}");
        }
        let long = "a".repeat(MAX_MODEL_NAME + 1);
        for bad in [
            "",
            "--dangerously-skip-permissions",
            "-p",
            "opus model",
            "opus\n",
            &long,
        ] {
            assert!(!plain_model_name(bad), "{bad}");
        }
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary_and_says_so() {
        assert_eq!(capped("hola", 10), "hola");
        let cut = capped("ñññ", 3);
        assert!(cut.starts_with('ñ'), "{cut}");
        assert!(cut.contains("[cut at 3 bytes]"), "{cut}");
    }

    #[test]
    fn the_end_of_long_output_stays_and_starts_on_a_whole_line() {
        assert_eq!(capped_end("hola", 10), "hola");
        let cut = capped_end(&format!("{}\nthe error\n", "x".repeat(100)), 20);
        assert_eq!(cut, "… [only the last 20 bytes]\nthe error\n");
        // Right after a character's first byte, then with nothing whole left to show.
        assert!(capped_end("ñññ\nfin", 5).ends_with("]\nfin"));
        assert_eq!(capped_end("ñññ", 3), "… [only the last 3 bytes]\n");
    }

    #[test]
    fn output_is_masked_before_a_cut_could_part_a_key_from_its_header() {
        let body = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo\n".repeat(1000);
        let stderr = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{body}-----END RSA PRIVATE KEY-----\nerror: boom\n"
        );
        assert!(stderr.len() > MAX_TEXT);
        let events = [keel_store::Event {
            seq: 1,
            attempt_id: "a".into(),
            at: String::new(),
            kind: "checkResult".into(),
            data: json!({ "exitCode": 3, "stdout": "", "stderr": stderr }),
        }];
        let output = command_output(&events).unwrap();
        assert_eq!(output["exitCode"], 3);
        let stderr = output["stderr"].as_str().unwrap();
        assert!(!stderr.contains("QUJD"), "{stderr}");
        assert!(stderr.ends_with("error: boom\n"), "{stderr}");
    }

    #[test]
    fn a_step_that_ran_no_command_has_no_command_output() {
        assert_eq!(command_output(&[]), None);
    }

    #[test]
    fn a_cut_leaves_no_half_token_that_masking_would_not_recognize() {
        let token = format!("{}{}", "ghp_", "x".repeat(36));
        let text = format!("the token is {token} and more");
        let cut = capped(&text, "the token is ghp_xxxx".len());
        assert_eq!(mask_secrets(&cut), cut);
        assert!(!cut.contains("ghp_"), "{cut}");
        assert!(cut.starts_with("the token is\n"), "{cut}");
    }

    /// An agent's attempt that answered with a verdict, also as its text.
    fn attempt(step: &str, number: i64) -> Attempt {
        let verdict = json!({ "verdict": "approved", "findings": [] });
        Attempt {
            id: format!("{step}-{number}"),
            run_id: "run".into(),
            step_id: step.into(),
            kind: "agent".into(),
            role: "reviewer".into(),
            round: number - 1,
            output: Some(verdict.clone()),
            run_files: None,
            exit_code: None,
            number,
            state: "succeeded".into(),
            reason: None,
            model_requested: "haiku".into(),
            model_actual: None,
            effort: None,
            tools: "readOnly".into(),
            max_turns: None,
            max_budget_usd: None,
            language: "en".into(),
            cli_version: None,
            api_key_source: None,
            account: None,
            prompt: String::new(),
            system_prompt_sources: Json::Null,
            settings: Json::Null,
            session_id: String::new(),
            resumed_from: None,
            pid: None,
            pid_started: None,
            workspace: String::new(),
            raw_path: String::new(),
            raw_offset: 0,
            raw_sha256: None,
            diff_path: None,
            sha_before: None,
            sha_after: None,
            files_changed: Some(json!(["a.rs"])),
            result_text: Some(verdict.to_string()),
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            turns: None,
            duration_ms: None,
            started_at: String::new(),
            finished_at: None,
            last_activity_at: None,
        }
    }

    #[test]
    fn a_run_that_went_round_many_times_leaves_out_its_oldest_attempts_and_says_how_many() {
        let rounds = 30;
        let attempts: Vec<Attempt> = (1..=rounds)
            .flat_map(|number| [attempt("build", number), attempt("review", number)])
            .collect();
        let (views, left_out) = steps_view(&attempts);
        let earlier = attempts.len() - 2;
        assert_eq!(left_out, earlier - MAX_EARLIER_ATTEMPTS);
        assert_eq!(views.len(), MAX_EARLIER_ATTEMPTS + 2);
        // The newest stay, in the order they came, and each step's latest is whole.
        let first_kept = &attempts[left_out];
        assert_eq!(views[0]["attemptId"], json!(first_kept.id));
        assert_eq!(views[0]["superseded"], true);
        let [.., build, review] = views.as_slice() else {
            panic!("{views:?}")
        };
        assert_eq!(build["attemptId"], "build-30");
        assert_eq!(review["attemptId"], "review-30");
        assert_eq!(review["role"], "reviewer");
        assert!(review.get("superseded").is_none(), "{review}");
        // None of them says what it wrote: get_step does, once.
        assert!(views.iter().all(|view| view.get("output").is_none()));
        let full = attempt_view(&attempts[0]);
        assert_eq!(full["output"]["verdict"], "approved");
        assert!(full.get("result").is_none(), "{full}");
        assert_eq!(full["filesChanged"], json!(["a.rs"]));

        let (views, left_out) = steps_view(&attempts[..8]);
        assert_eq!((views.len(), left_out), (8, 0));
    }
}
