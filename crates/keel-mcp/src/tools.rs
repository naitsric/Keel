//! What each tool does: plain methods of [`McpServer`], each leaving a row in the audit log. The
//! engine blocks, so they run on tokio's blocking threads.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, Reason, StepCondition, StepRole, StepSpec, ToolProfile, WorkflowStep,
    mask_secrets, mask_value,
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
/// run waiting for the person counts until they decide it or the client closes it, so an assistant
/// can't pile up work faster than they're looking at it.
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
/// About the most get_step returns at once, as JSON: Claude Code won't show a tool's answer much
/// longer than this. What doesn't fit is named in `leftOut`, to read in pieces with `part`.
const MAX_STEP: usize = 18 * 1024;
/// What an attempt answered, as get_step shows it before cutting it.
const MAX_ANSWER: usize = 8 * 1024;
/// The end of each of what a setup or check printed, stdout and stderr, as get_step shows it.
const MAX_TAIL: usize = 6 * 1024;
/// One piece of a part read with `part`, as JSON, so that with the rest it stays under MAX_STEP.
const MAX_PIECE: usize = 16 * 1024;
const MAX_LOGGED_ARGS: usize = 4 * 1024;
const MAX_LOGGED_ERROR: usize = 4 * 1024;
const WAIT_DEFAULT_SECS: u64 = 30;
const WAIT_MAX_SECS: u64 = 50;
const WAIT_TICK: Duration = Duration::from_millis(300);

/// What a run waits on someone for. Answering and approving are only ever the person's. Of the
/// rest, a client that may start runs can close its own, and one the person also lets give feedback
/// can send them back or retry them.
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
    /// One part to read whole, in pieces, instead of the attempt: `diff`, `files`, every file it
    /// changed, `prompt`, `answer`, or what a setup or check printed, `stdout` or `stderr`.
    /// `leftOut` names the ones too long to come with the attempt.
    #[serde(default)]
    pub part: Option<StepPart>,
    /// Only this file's diff: a `path` from filesChanged. It goes with `part` diff, or alone.
    #[serde(default)]
    pub file: Option<String>,
    /// Where the piece starts, in bytes: 0 by default, then what the last piece's `next` says.
    #[serde(default)]
    pub from: Option<u64>,
}

// Plain comments, as doc comments on variants would split the schema's list of names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StepPart {
    Diff,
    Files,
    Prompt,
    Answer,
    Stdout,
    Stderr,
}

impl StepPart {
    fn as_str(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Files => "files",
            Self::Prompt => "prompt",
            Self::Answer => "answer",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
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
pub struct ProposeChangeArgs {
    /// The project where the change is agreed, by id or name.
    pub project: String,
    /// The other projects the change touches, each by id or name. The project above and repeats are
    /// left out. At least one is needed.
    pub also_touches: Vec<String>,
    /// What to change, across all of them. With `issue`, what to add to it.
    #[serde(default)]
    pub request: Option<String>,
    /// An open issue's number in `project`, from list_issues.
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
            let run = Some(attempt.run_id.clone());
            let part = match (args.part, &args.file) {
                (None, Some(_)) => Some(StepPart::Diff),
                (Some(part), Some(_)) if part != StepPart::Diff => {
                    return Err(ToolError::new("`file` only goes with part diff."));
                }
                (part, _) => part,
            };
            let Some(part) = part else {
                if args.from.is_some() {
                    return Err(ToolError::new(
                        "`from` goes with `part`, to say where a piece of it starts.",
                    ));
                }
                let long = LongParts {
                    prompt: mask_secrets(&attempt.prompt),
                    diff: server.diff_of(&attempt),
                    printed: server.printed(&attempt),
                };
                return Ok((step_overview(&attempt, &long), run));
            };
            let text = server.part_text(&attempt, part)?;
            let text = match &args.file {
                Some(file) => file_diff(&text, file)?,
                None => &text,
            };
            let from = usize::try_from(args.from.unwrap_or(0)).unwrap_or(usize::MAX);
            let view = piece_view(&attempt, part, args.file.as_deref(), text, from)?;
            Ok((view, run))
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
                .map_err(|error| start_run_error(error, issue_number))?;
            engine.announce(&run);
            Ok((server.new_run_view(client, &run)?, Some(run)))
        })
        .await
    }

    /// Starts the run where a change across several projects is agreed, in the first project. It
    /// only reads, and waits for the person to approve in the app: then each part opens as it does
    /// from a card on the board. There's no workflow to choose, and nothing here approves.
    pub(crate) async fn propose_change(
        &self,
        client: McpClient,
        args: ProposeChangeArgs,
    ) -> Result<Json, ToolError> {
        self.call(client, "propose_change", args, |server, client, args| {
            if !client.can_run {
                return Err(may_only_read());
            }
            server.room_for_another_run(client)?;
            let project = server.project(&args.project)?;
            let language = server.language(args.language.as_deref())?;
            let mut also_touches: Vec<String> = Vec::new();
            for wanted in &args.also_touches {
                let other = server
                    .project(wanted)
                    .map_err(|error| ToolError::new(format!("alsoTouches: {}", error.0)))?;
                if other.id != project.id && !also_touches.contains(&other.id) {
                    also_touches.push(other.id);
                }
            }
            if also_touches.is_empty() {
                return Err(ToolError::new(format!(
                    "A change across projects needs at least one project besides {} in `alsoTouches`. For a change in one project, use start_run.",
                    project.name
                )));
            }
            let engine = &server.0.engine;
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
                .propose_change_on_board(
                    StartRun {
                        project_id: project.id,
                        title: issue
                            .as_ref()
                            .map(|issue| format!("#{} {}", issue.number, issue.title)),
                        request,
                        // The agreement is always the same, so the engine puts it in.
                        saved_workflow: None,
                        workflow: Vec::new(),
                        language,
                        source_issue: issue,
                        started_by: Some(client.name.clone()),
                    },
                    &also_touches,
                )
                .map_err(|error| propose_change_error(error, issue_number))?;
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
            // Withdrawing its own request: it delivers nothing and decides nothing for the person,
            // so starting runs is enough. It's how a client lets go of a run it won't take further.
            let (run, attempts) = server.own_run(client, &args.run)?;
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

    /// An attempt's diff, masked. Empty when it changed nothing, and None when Keel can't say.
    fn diff_of(&self, attempt: &Attempt) -> Option<String> {
        let diff = self.0.engine.diff(&attempt.id).ok()?;
        Some(mask_secrets(&diff))
    }

    /// What a setup or check printed. An agent's events can be many, and only a command has this.
    fn printed(&self, attempt: &Attempt) -> Option<Printed> {
        if !ran_a_command(attempt) {
            return None;
        }
        let events = self.0.store.events(&attempt.id, 0).ok()?;
        printed(&events)
    }

    /// One part of an attempt whole, masked, to read in pieces.
    fn part_text(&self, attempt: &Attempt, part: StepPart) -> Result<String, ToolError> {
        Ok(match part {
            StepPart::Diff => self.diff_of(attempt).unwrap_or_default(),
            StepPart::Files => {
                let diff = self.diff_of(attempt).unwrap_or_default();
                let diff = diff_files(&diff);
                let files = changed_files(attempt.files_changed.as_ref(), &diff);
                mask_secrets(&files_text(&files.unwrap_or_default()))
            }
            StepPart::Prompt => mask_secrets(&attempt.prompt),
            StepPart::Answer => answer_text(attempt),
            StepPart::Stdout | StepPart::Stderr => {
                if !ran_a_command(attempt) {
                    return Err(ToolError::new(
                        "Only a setup or check prints `stdout` and `stderr`, and this attempt isn't one.",
                    ));
                }
                // Nothing yet while it runs.
                let printed = self.printed(attempt);
                match (part, printed) {
                    (StepPart::Stdout, Some(printed)) => printed.stdout,
                    (_, Some(printed)) => printed.stderr,
                    (_, None) => String::new(),
                }
            }
        })
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
                "This client has as many runs open as the person allows it ({open}). A run that waits for the person stays open until they decide in Keel, or until you close it with close_run if you won't take it further. list_runs with `mine` finds them. The person can also raise the limit in Keel's settings."
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
        self.own_run(client, run_id)
    }

    /// A run this client started and may act on, with its attempts, or why it may not.
    fn own_run(&self, client: &McpClient, run_id: &str) -> Result<(Run, Vec<Attempt>), ToolError> {
        if !client.can_run {
            return Err(may_only_read());
        }
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
        }
        if client.can_run && mine && close_target(run, attempts).is_ok() {
            tools.push("close_run");
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
        if let Some(joint) = self.joint_view(run)? {
            view["jointChange"] = joint;
        }
        Ok(view)
    }

    /// What a run says about the change across projects it's in, if it's in one: whether it's where
    /// the change is agreed or one of its parts, which part it is and what it waits for, and every
    /// part with its project, its order and the run it started, if it has. So a client that follows
    /// the agreement finds each part's run, and one that follows a part sees where it stands.
    fn joint_view(&self, run: &Run) -> Result<Option<Json>, ToolError> {
        let engine = &self.0.engine;
        let Some(here) = engine.run_joint(&run.id).map_err(engine_error)? else {
            return Ok(None);
        };
        let change = engine.joint_change(&here.change_id).map_err(engine_error)?;
        let project = self
            .0
            .store
            .project(&run.project_id)
            .map_err(store_error)?
            .name;
        let parts: Vec<Json> = change
            .parts
            .iter()
            .map(|card| {
                let part = card.part.as_ref();
                let position = part.map(|part| part.position);
                let waits_for = part
                    .and_then(|part| part.waits_for.as_ref())
                    .map(neighbour_view);
                json!({
                    "position": position,
                    "project": card.project,
                    "projectId": card.project_id,
                    "column": card.column,
                    "runId": card.run_id,
                    "runState": card.run_state,
                    "waitsFor": waits_for,
                    "prUrl": card.pr_url,
                    "prState": card.pr_state,
                })
            })
            .collect();
        let role = if here.agrees { "agreement" } else { "part" };
        let mut view = json!({
            "changeId": here.change_id,
            "role": role,
            "project": project,
            "agreed": change.agreed.is_some(),
            "projects": change.projects,
            "parts": parts,
        });
        if let Some(part) = &here.part {
            let waits_for = part.waits_for.as_ref().map(neighbour_view);
            view["part"] = json!({
                "position": part.position,
                "of": part.total,
                "waitsFor": waits_for,
                "warnings": part.warnings,
            });
        }
        Ok(Some(view))
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

fn neighbour_view(neighbour: &keel_engine::Neighbour) -> Json {
    json!({ "position": neighbour.position, "project": neighbour.project })
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

/// Whether the person lets this client give feedback at all: send back or retry. Both ask this
/// first, in this order, so the refusal says what's missing. The run also has to be the client's own.
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

/// What an attempt has that can be long, masked whole: a cut, or a piece, could part a value from
/// the name that gets it masked. `from` counts bytes of these.
struct LongParts {
    prompt: String,
    diff: Option<String>,
    printed: Option<Printed>,
}

/// What a setup or check printed, masked.
#[derive(Debug, PartialEq)]
struct Printed {
    exit_code: Json,
    stdout: String,
    stderr: String,
}

/// What get_step says when it leaves something out.
const TO_READ: &str = "leftOut names what's too long to come whole here, with its size in bytes. \
                       get_step with `part` set to one of them reads it in pieces: each says how much \
                       is `left` and gives the arguments for the `next`. `file` reads one file's diff.";
/// Kept for `leftOut` and TO_READ while the rest of the overview is fitted.
const LEFT_OUT_ROOM: usize = 512;
/// Kept for what a cut says about itself.
const CUT_NOTE_ROOM: usize = 100;

/// One attempt as get_step returns it without `part`: what it is and how it went, what it
/// answered, once, the files it changed with the lines each adds and removes, and the end of what
/// a command printed. Its diff and prompt come whole when there's room. Whatever is cut or left out
/// is in `leftOut` with its size, to read with `part`, so it stays about MAX_STEP long however much
/// the attempt wrote.
fn step_overview(attempt: &Attempt, long: &LongParts) -> Json {
    let mut view = step_view(attempt);
    view["account"] = json!(attempt.account);
    view["brainReceived"] = attempt.system_prompt_sources.clone();
    let mut left_out = serde_json::Map::new();
    // Its structured answer as Keel read it, which Keel may have corrected, or else what it wrote.
    match &attempt.output {
        Some(output) => {
            let output = mask_value(output.clone());
            if output.to_string().len() <= MAX_ANSWER {
                view["output"] = output;
            } else {
                left_out.insert("answer".into(), json!(answer_text(attempt).len()));
            }
        }
        None => {
            let text = attempt.result_text.as_deref().map(mask_secrets);
            let answer =
                text.map(|text| cut_head(&text, MAX_ANSWER, StepPart::Answer, &mut left_out));
            view["result"] = json!(answer);
        }
    }
    if let Some(printed) = &long.printed {
        view["commandOutput"] = json!({
            "exitCode": printed.exit_code,
            "stdout": cut_tail(&printed.stdout, MAX_TAIL, StepPart::Stdout, &mut left_out),
            "stderr": cut_tail(&printed.stderr, MAX_TAIL, StepPart::Stderr, &mut left_out),
        });
    }
    let diff_files = long.diff.as_deref().map(diff_files).unwrap_or_default();
    let room = MAX_STEP.saturating_sub(view.to_string().len() + LEFT_OUT_ROOM);
    match changed_files(attempt.files_changed.as_ref(), &diff_files) {
        Some(files) => {
            let listed = files_view(&files, room);
            let more = files.len() - listed.len();
            view["filesChanged"] = json!(listed);
            if more > 0 {
                view["moreFilesChanged"] = json!(more);
                let all = mask_secrets(&files_text(&files));
                left_out.insert("files".into(), json!(all.len()));
            }
        }
        None => view["filesChanged"] = Json::Null,
    }
    // Whole or not at all: a diff or a prompt cut short would read as if it were all of it.
    let mut room = MAX_STEP.saturating_sub(view.to_string().len() + LEFT_OUT_ROOM);
    let mut whole = |key: &str, text: &str, view: &mut Json| {
        let size = key.len() + json_len(text) + 6;
        if size <= room {
            room -= size;
            view[key] = json!(text);
        } else {
            left_out.insert(key.into(), json!(text.len()));
        }
    };
    match &long.diff {
        Some(diff) => whole("diff", diff, &mut view),
        None => view["diff"] = Json::Null,
    }
    whole("prompt", &long.prompt, &mut view);
    if !left_out.is_empty() {
        view["leftOut"] = Json::Object(left_out);
        view["toRead"] = json!(TO_READ);
    }
    view
}

/// What an attempt answered, as text: its structured answer as Keel read it, laid out, or else
/// what it wrote. Masked.
fn answer_text(attempt: &Attempt) -> String {
    match &attempt.output {
        Some(output) => {
            serde_json::to_string_pretty(&mask_value(output.clone())).unwrap_or_default()
        }
        None => attempt
            .result_text
            .as_deref()
            .map(mask_secrets)
            .unwrap_or_default(),
    }
}

fn ran_a_command(attempt: &Attempt) -> bool {
    matches!(attempt.kind.as_str(), "setup" | "check")
}

/// A file an attempt changed, and its part of the diff when the diff has it.
type ChangedFile<'a> = (&'a str, Option<&'a DiffFile>);

/// The files an attempt changed: Keel's list of them, or else the diff's.
fn changed_files<'a>(
    listed: Option<&'a Json>,
    diff: &'a [DiffFile],
) -> Option<Vec<ChangedFile<'a>>> {
    let paths: Vec<&str> = match listed.and_then(Json::as_array) {
        Some(listed) => listed.iter().filter_map(Json::as_str).collect(),
        None if !diff.is_empty() => diff.iter().map(|file| file.path.as_str()).collect(),
        None => return None,
    };
    let by_path: HashMap<&str, &DiffFile> =
        diff.iter().map(|file| (file.path.as_str(), file)).collect();
    let files = paths
        .into_iter()
        .map(|path| (path, by_path.get(plain_name(path)).copied()))
        .collect();
    Some(files)
}

/// As many of the files as fit in `room`, and at most MAX_LISTED_FILES, each with the lines its
/// diff adds and removes.
fn files_view(files: &[ChangedFile], room: usize) -> Vec<Json> {
    let mut used = 2;
    let mut listed = Vec::new();
    for (path, diff) in files.iter().take(MAX_LISTED_FILES) {
        let mut entry = json!({ "path": path });
        if let Some(diff) = diff {
            entry["added"] = json!(diff.added);
            entry["removed"] = json!(diff.removed);
        }
        used += entry.to_string().len() + 1;
        if used > room {
            break;
        }
        listed.push(entry);
    }
    listed
}

/// Every file, a line each, as `git diff --numstat` writes them: the lines it adds, the lines it
/// removes, and its path, with `-` when the diff doesn't have it.
fn files_text(files: &[ChangedFile]) -> String {
    files
        .iter()
        .map(|(path, diff)| match diff {
            Some(diff) => format!("{}\t{}\t{path}\n", diff.added, diff.removed),
            None => format!("-\t-\t{path}\n"),
        })
        .collect()
}

/// A piece of one part of an attempt, from `from`, as long as fits in one answer. It says how much
/// is left and how to ask for the next piece, so any size reads in a few calls.
fn piece_view(
    attempt: &Attempt,
    part: StepPart,
    file: Option<&str>,
    text: &str,
    from: usize,
) -> Result<Json, ToolError> {
    if from > text.len() {
        return Err(ToolError::new(format!(
            "That part has {} bytes, so `from` goes up to {}.",
            text.len(),
            text.len()
        )));
    }
    let mut from = from;
    while !text.is_char_boundary(from) {
        from -= 1;
    }
    let end = piece_end(text, from, MAX_PIECE);
    let mut view = json!({
        "attemptId": attempt.id,
        "part": part.as_str(),
        "bytes": text.len(),
        "from": from,
        "text": &text[from..end],
        "left": text.len() - end,
    });
    if let Some(file) = file {
        view["file"] = json!(file);
    }
    if end < text.len() {
        let mut next = json!({ "attempt": attempt.id, "part": part.as_str(), "from": end });
        if let Some(file) = file {
            next["file"] = json!(file);
        }
        view["next"] = next;
    }
    Ok(view)
}

/// `text` whole if it's at most `room` long as JSON, or else its start, saying where it was cut,
/// and `part` named in `left_out`.
fn cut_head(
    text: &str,
    room: usize,
    part: StepPart,
    left_out: &mut serde_json::Map<String, Json>,
) -> String {
    if json_len(text) <= room {
        return text.to_owned();
    }
    left_out.insert(part.as_str().into(), json!(text.len()));
    let end = piece_end(text, 0, room.saturating_sub(CUT_NOTE_ROOM));
    format!(
        "{}\n… [cut here: get_step with part {} reads it all]",
        &text[..end],
        part.as_str()
    )
}

/// Like `cut_head` but keeping the end, where a command says what went wrong.
fn cut_tail(
    text: &str,
    room: usize,
    part: StepPart,
    left_out: &mut serde_json::Map<String, Json>,
) -> String {
    if json_len(text) <= room {
        return text.to_owned();
    }
    left_out.insert(part.as_str().into(), json!(text.len()));
    let start = tail_start(text, room.saturating_sub(CUT_NOTE_ROOM));
    format!(
        "… [cut here: get_step with part {} reads it all]\n{}",
        part.as_str(),
        &text[start..]
    )
}

/// How long `text` is once written as a JSON string, without its quotes: what a client reads.
fn json_len(text: &str) -> usize {
    text.chars().map(json_char_len).sum()
}

fn json_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Where a piece of `text` that starts at `from` ends, to be at most `room` long as JSON. It ends
/// after a whole line when one ends in its second half, so each piece reads on its own. It always
/// takes something, so reading piece after piece gets to the end.
fn piece_end(text: &str, from: usize, room: usize) -> usize {
    let mut used = 0;
    let mut line_end = None;
    for (at, c) in text[from..].char_indices() {
        used += json_char_len(c);
        if used > room {
            let cut = from + at;
            return match line_end {
                _ if cut == from => cut + c.len_utf8(),
                Some(end) if end - from > (cut - from) / 2 => end,
                _ => cut,
            };
        }
        if c == '\n' {
            line_end = Some(from + at + 1);
        }
    }
    text.len()
}

/// Where the end of `text` that's at most `room` long as JSON starts. It starts on a whole line
/// when one starts in its first half, since Keel keeps only the end of what a command prints and
/// the line a cut goes through reads as nonsense.
fn tail_start(text: &str, room: usize) -> usize {
    let mut used = 0;
    let mut start = text.len();
    for (at, c) in text.char_indices().rev() {
        used += json_char_len(c);
        if used > room {
            break;
        }
        start = at;
    }
    let kept = &text[start..];
    match kept.find('\n') {
        Some(newline) if start > 0 && newline < kept.len() / 2 => start + newline + 1,
        _ => start,
    }
}

/// One file's part of a diff: where it is in it, and the lines it adds and removes.
#[derive(Debug, PartialEq)]
struct DiffFile {
    path: String,
    start: usize,
    end: usize,
    added: usize,
    removed: usize,
}

/// The files of a diff as git writes it, in order. Each goes from its `diff --git` line to the
/// next one.
fn diff_files(diff: &str) -> Vec<DiffFile> {
    let mut starts = Vec::new();
    let mut at = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(at);
        }
        at += line.len();
    }
    let ends = starts.iter().skip(1).copied().chain([diff.len()]);
    starts
        .iter()
        .zip(ends)
        .map(|(&start, end)| diff_file(&diff[start..end], start, end))
        .collect()
}

fn diff_file(text: &str, start: usize, end: usize) -> DiffFile {
    let mut lines = text.split_inclusive('\n');
    let header = lines.next().unwrap_or_default();
    let (mut old, mut new, mut moved_to) = (None, None, None);
    let (mut added, mut removed, mut in_hunk) = (0, 0, false);
    for line in lines {
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => added += 1,
                Some(b'-') => removed += 1,
                _ => {}
            }
        } else if let Some(name) = line.strip_prefix("--- ") {
            old = Some(name);
        } else if let Some(name) = line.strip_prefix("+++ ") {
            new = Some(name);
        } else if let Some(name) = line.strip_prefix("rename to ") {
            moved_to = Some(name);
        }
    }
    // The file as it is after the change, or as it was if it's gone.
    let side = |name: Option<&str>, prefix: &str| {
        name.map(plain_name)
            .filter(|name| *name != "/dev/null")
            .and_then(|name| name.strip_prefix(prefix))
            .map(str::to_owned)
    };
    let path = side(new, "b/")
        .or_else(|| side(old, "a/"))
        .or_else(|| moved_to.map(|name| plain_name(name).to_owned()))
        .unwrap_or_else(|| header_name(header).to_owned());
    DiffFile {
        path,
        start,
        end,
        added,
        removed,
    }
}

/// A name as git writes it, without the line's end, the tab git adds after a name with spaces, or
/// the quotes around one with unusual characters.
fn plain_name(name: &str) -> &str {
    let name = name.trim_end_matches(['\n', '\r']);
    let name = name.strip_suffix('\t').unwrap_or(name);
    name.strip_prefix('"')
        .and_then(|name| name.strip_suffix('"'))
        .unwrap_or(name)
}

/// The name in a `diff --git a/name b/name` line, for a file whose diff names it nowhere else, like
/// one that's binary or only changed mode.
fn header_name(header: &str) -> &str {
    let names = header.strip_prefix("diff --git ").unwrap_or(header);
    let names = names.trim_end_matches(['\n', '\r']);
    names.split_once(" b/").map_or(names, |(_, new)| new)
}

/// One file's diff in a diff, by a path from filesChanged.
fn file_diff<'a>(diff: &'a str, file: &str) -> Result<&'a str, ToolError> {
    diff_files(diff)
        .into_iter()
        .find(|found| found.path == plain_name(file))
        .map(|found| &diff[found.start..found.end])
        .ok_or_else(|| {
            ToolError::new(format!(
                "This attempt's diff has no file {file:?}. Its filesChanged lists the files it changed."
            ))
        })
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

/// What a setup or check printed, masked whole: a cut can part a value from the name that gets it
/// masked.
fn printed(events: &[keel_store::Event]) -> Option<Printed> {
    let result = &events
        .iter()
        .find(|event| event.kind == "checkResult")?
        .data;
    let text = |name: &str| mask_secrets(result[name].as_str().unwrap_or_default());
    Some(Printed {
        exit_code: result["exitCode"].clone(),
        stdout: text("stdout"),
        stderr: text("stderr"),
    })
}

/// What a check printed, for a run's report, which is how a client learns why it failed.
fn command_output(events: &[keel_store::Event]) -> Option<Json> {
    let printed = printed(events)?;
    Some(json!({
        "exitCode": printed.exit_code,
        "stdout": capped_end(&printed.stdout, MAX_TEXT),
        "stderr": capped_end(&printed.stderr, MAX_TEXT),
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

/// Why start_run was refused, with nothing created.
fn start_run_error(error: Error, issue: Option<i64>) -> ToolError {
    match (error, issue) {
        (Error::InvalidState, Some(number)) => ToolError::new(format!(
            "Issue #{number} already has a run open on the board. list_runs shows it."
        )),
        (Error::Blocked(Reason::StartsOnTheBoard), Some(number)) => already_across_projects(number),
        (error, _) => engine_error(error),
    }
}

/// Why propose_change was refused, with nothing created.
fn propose_change_error(error: Error, issue: Option<i64>) -> ToolError {
    match (error, issue) {
        (Error::InvalidState, Some(number)) => ToolError::new(format!(
            "Issue #{number} already has a run open on the board. list_runs shows it."
        )),
        (Error::Blocked(Reason::StartsOnTheBoard), Some(number)) => already_across_projects(number),
        (error, _) => engine_error(error),
    }
}

/// The same answer from start_run and propose_change, since both refuse it. The person or another
/// client may have started it, so `mine` wouldn't find it.
fn already_across_projects(number: i64) -> ToolError {
    ToolError::new(format!(
        "Issue #{number} already is a change across several projects on the board, so nothing new was created. The person follows it in Keel and approves its agreement there. list_runs with this `project` shows its run once it has one, even if this client didn't start it."
    ))
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
    fn an_issue_that_already_is_a_change_across_projects_gets_the_same_answer_from_both_tools() {
        let refusal = start_run_error(Error::Blocked(Reason::StartsOnTheBoard), Some(47)).0;
        // propose_change refuses it too, so pointing there only costs a call.
        assert!(!refusal.contains("propose_change"), "{refusal}");
        assert!(refusal.contains("#47"), "{refusal}");
        assert!(refusal.contains("nothing new was created"), "{refusal}");
        // The person, or another client, may have started it, so `mine` wouldn't find it.
        assert!(
            refusal.contains("list_runs with this `project`"),
            "{refusal}"
        );
        assert!(!refusal.contains("`mine`"), "{refusal}");
        let again = propose_change_error(Error::Blocked(Reason::StartsOnTheBoard), Some(47));
        assert_eq!(again.0, refusal);
        let open = start_run_error(Error::InvalidState, Some(47));
        assert!(open.0.contains("already has a run open"), "{}", open.0);
    }

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
        let long = LongParts {
            prompt: String::new(),
            diff: None,
            printed: None,
        };
        let full = step_overview(&attempts[0], &long);
        assert_eq!(full["output"]["verdict"], "approved");
        assert!(full.get("result").is_none(), "{full}");
        assert_eq!(full["filesChanged"], json!([{ "path": "a.rs" }]));

        let (views, left_out) = steps_view(&attempts[..8]);
        assert_eq!((views.len(), left_out), (8, 0));
    }

    #[test]
    fn a_text_is_as_long_as_json_writes_it() {
        for text in [
            "",
            "plain",
            "ñandú 🦀",
            "a \"quoted\" \\ name\n\ttab\r",
            "\u{1}\u{1f}\u{7f}\u{8}\u{c}",
        ] {
            let written = Json::String(text.into()).to_string();
            assert_eq!(json_len(text) + 2, written.len(), "{text:?}");
        }
    }

    #[test]
    fn pieces_fit_end_after_whole_lines_and_together_are_the_text() {
        let text = format!("{}\n{}\n", "ñ\"🦀\\".repeat(500), "x".repeat(300)).repeat(20);
        for room in [7, 100, 1000, 5000] {
            let mut joined = String::new();
            let mut from = 0;
            while from < text.len() {
                let end = piece_end(&text, from, room);
                assert!(end > from);
                assert!(json_len(&text[from..end]) <= room, "{room}");
                joined.push_str(&text[from..end]);
                from = end;
            }
            assert_eq!(joined, text);
        }
        // After a whole line when one ends in its second half, and through one when it doesn't.
        let text = "one line\nand another one\n";
        assert_eq!(piece_end(text, 0, 16), "one line\n".len());
        assert_eq!(piece_end(text, 0, 22), "one line\nand another ".len());
        // A character too long for the room still goes, so reading goes on.
        assert_eq!(piece_end("🦀", 0, 1), "🦀".len());
    }

    #[test]
    fn the_end_of_long_output_starts_on_a_whole_line_and_a_character_boundary() {
        let text = format!("{}\nthe error\n", "x".repeat(100));
        assert_eq!(&text[tail_start(&text, 20)..], "the error\n");
        assert_eq!(tail_start("short", 20), 0);
        assert_eq!(&"ñññ"[tail_start("ñññ", 3)..], "ñ");
        // One long line keeps its end rather than nothing.
        let line = "y".repeat(100);
        assert_eq!(&line[tail_start(&line, 10)..], "y".repeat(10));
    }

    #[test]
    fn a_diff_splits_into_its_files_with_the_lines_each_adds_and_removes() {
        let diff = concat!(
            "diff --git a/src/a.rs b/src/b.rs\nsimilarity index 90%\nrename from src/a.rs\n",
            "rename to src/b.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/b.rs\n",
            "@@ -1,2 +1,2 @@\n-old\n+new\n+++ added, not a header\n context\n",
            "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n",
            "+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\\ No newline at end of file\n",
            "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n",
            "diff --git a/moved b/elsewhere\nsimilarity index 100%\nrename from moved\n",
            "rename to elsewhere\n",
            "diff --git a/my notes.md b/my notes.md\n--- a/my notes.md\t\n+++ b/my notes.md\t\n",
            "@@ -1 +1 @@\n-a\n+b\n",
            "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n",
            "--- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n-x\n+y\n",
        );
        let files = diff_files(diff);
        let summary: Vec<(&str, usize, usize)> = files
            .iter()
            .map(|file| (file.path.as_str(), file.added, file.removed))
            .collect();
        assert_eq!(
            summary,
            [
                ("src/b.rs", 2, 1),
                ("gone.txt", 0, 1),
                ("logo.png", 0, 0),
                ("elsewhere", 0, 0),
                ("my notes.md", 1, 1),
                ("caf\\303\\251.txt", 1, 1),
            ]
        );
        let joined: String = files
            .iter()
            .map(|file| &diff[file.start..file.end])
            .collect();
        assert_eq!(joined, diff);
        // A path as git lists it finds its file, quotes and all.
        let quoted = file_diff(diff, "\"caf\\303\\251.txt\"").unwrap();
        assert_eq!(quoted, &diff[files[5].start..]);
        assert_eq!(
            file_diff(diff, "gone.txt").unwrap(),
            &diff[files[1].start..files[1].end]
        );
        assert!(file_diff(diff, "nope").is_err());
        assert!(diff_files("").is_empty());
    }

    #[test]
    fn an_attempt_that_wrote_a_lot_still_comes_in_one_go_and_says_what_it_left_out() {
        let mut long = attempt("review", 1);
        let findings = vec!["a finding that goes on and on"; 3000];
        long.output = Some(json!({ "verdict": "changes", "findings": findings }));
        let deep = "deep/".repeat(20);
        let paths: Vec<String> = (0..3000)
            .map(|n| format!("src/{deep}file_{n}.rs"))
            .collect();
        long.files_changed = Some(json!(paths));
        let parts = LongParts {
            prompt: "p".repeat(100_000),
            diff: Some("+".repeat(300_000)),
            printed: Some(Printed {
                exit_code: json!(1),
                stdout: "o\n".repeat(50_000),
                stderr: "\u{1}".repeat(50_000),
            }),
        };
        let view = step_overview(&long, &parts);
        let size = view.to_string().len();
        assert!(size <= MAX_STEP, "{size}");
        let left_out = &view["leftOut"];
        assert_eq!(left_out["answer"], answer_text(&long).len());
        assert_eq!(left_out["diff"], 300_000);
        assert_eq!(left_out["prompt"], 100_000);
        assert_eq!(left_out["stdout"], 100_000);
        assert_eq!(left_out["stderr"], 50_000);
        let every_file: String = paths.iter().map(|path| format!("-\t-\t{path}\n")).collect();
        assert_eq!(left_out["files"], every_file.len());
        assert_eq!(view["toRead"], TO_READ);
        for gone in ["output", "result", "diff", "prompt"] {
            assert!(view.get(gone).is_none(), "{gone}");
        }
        let listed = view["filesChanged"].as_array().unwrap().len();
        let more = view["moreFilesChanged"].as_u64().unwrap() as usize;
        assert!(listed > 0 && listed + more == 3000, "{listed} {more}");
        let output = &view["commandOutput"];
        assert!(output["stdout"].as_str().unwrap().ends_with("o\no\n"));
        assert!(output["stderr"].as_str().unwrap().ends_with('\u{1}'));
    }
}
