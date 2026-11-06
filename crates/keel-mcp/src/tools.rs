//! What each tool does: plain methods of [`McpServer`], each leaving a row in the audit log. The
//! engine blocks, so they run on tokio's blocking threads.

use std::time::{Duration, Instant};

use keel_core::{
    StepCondition, StepRole, StepSpec, ToolProfile, WorkflowStep, mask_secrets, mask_value,
};
use keel_engine::{Error, StartRun, request_from_issue};
use keel_store::{Attempt, McpClient, NewMcpCall, Project};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::{McpServer, ToolError};

/// Runs a client may have open at once. A run waiting for the person counts, so an assistant can't
/// spend a person's usage faster than they're looking at it.
pub const MAX_OPEN_RUNS: i64 = 3;

/// Drafts a project may have waiting, so a client can't bury the person in them.
pub const MAX_DRAFTS: usize = 20;
const MAX_DRAFT_NAME: usize = 80;
const MAX_MODEL_NAME: usize = 100;

const MAX_TEXT: usize = 16 * 1024;
const MAX_DIFF: usize = 64 * 1024;
const MAX_LOGGED_ARGS: usize = 4 * 1024;
const MAX_LOGGED_ERROR: usize = 4 * 1024;
const WAIT_DEFAULT_SECS: u64 = 30;
const WAIT_MAX_SECS: u64 = 50;
const WAIT_TICK: Duration = Duration::from_millis(300);

/// A person, not a client, moves a run on from these.
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
    pub run: String,
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
            Ok((json!({ "projects": projects }), None))
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
        self.call(client, "list_runs", args, |server, _, args| {
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
            let runs = server.0.store.runs(scan).map_err(store_error)?;
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
        self.call(client, "get_run", args, |server, _, args| {
            Ok((server.run_view(&args.run)?, Some(args.run)))
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
        self.call(client, "wait_for_run", args, |server, _, args| {
            let seconds = args.seconds.unwrap_or(WAIT_DEFAULT_SECS).min(WAIT_MAX_SECS);
            let deadline = Instant::now() + Duration::from_secs(seconds);
            loop {
                let view = server.run_view(&args.run)?;
                if view["state"] != "working" || Instant::now() >= deadline {
                    return Ok((view, Some(args.run)));
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
            let store = &server.0.store;
            let open = store
                .open_runs_of_mcp_client(&client.id)
                .map_err(store_error)?;
            if open >= MAX_OPEN_RUNS {
                return Err(ToolError::new(format!(
                    "This client already has {open} runs open, which is as many as it may have. They finish when the person decides in Keel."
                )));
            }
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
                })
                .map_err(|error| match (error, issue_number) {
                    (Error::InvalidState, Some(number)) => ToolError::new(format!(
                        "Issue #{number} already has a run open on the board. list_runs shows it."
                    )),
                    (error, _) => engine_error(error),
                })?;
            store
                .set_run_started_by(&run, &client.name)
                .map_err(store_error)?;
            engine.announce(&run);
            Ok((server.run_view(&run)?, Some(run)))
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
            Ok((server.run_view(&args.run)?, Some(args.run)))
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

    fn run_view(&self, run_id: &str) -> Result<Json, ToolError> {
        let attempts = self.attempts(run_id)?;
        let run = self.0.store.run(run_id).map_err(store_error)?;
        let waiting = needs_person(&attempts);
        let state = if run.finished_at.is_some() {
            "finished"
        } else if waiting.is_some() {
            "waitingForPerson"
        } else {
            "working"
        };
        let total_cost: f64 = attempts.iter().filter_map(|a| a.cost_usd).sum();
        Ok(json!({
            "id": run.id,
            "projectId": run.project_id,
            "title": run.title,
            "request": capped(&run.request, MAX_TEXT),
            "state": state,
            "waitingFor": waiting.filter(|_| run.finished_at.is_none()).map(|attempt| json!({
                "step": attempt.step_id,
                "attemptId": attempt.id,
                "state": attempt.state,
                "reason": attempt.reason,
                "who": "The person, in the Keel app. There is no tool for this.",
            })),
            "startedBy": run.started_by,
            "cardId": self.0.store.card_of_run(run_id).map_err(store_error)?,
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
            "steps": attempts.iter().map(attempt_view).collect::<Vec<_>>(),
        }))
    }
}

/// The step a person has to act on, if any: looking only at each step's latest attempt, and never
/// at a learner, since its failure doesn't stop a run.
fn needs_person(attempts: &[Attempt]) -> Option<&Attempt> {
    attempts.iter().enumerate().find_map(|(index, attempt)| {
        let superseded = attempts[index + 1..]
            .iter()
            .any(|later| later.step_id == attempt.step_id);
        (!superseded && attempt.role != "learner" && NEEDS_PERSON.contains(&attempt.state.as_str()))
            .then_some(attempt)
    })
}

fn attempt_view(attempt: &Attempt) -> Json {
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
        "account": attempt.account,
        "costUsd": attempt.cost_usd,
        "inputTokens": attempt.input_tokens,
        "outputTokens": attempt.output_tokens,
        "turns": attempt.turns,
        "durationMs": attempt.duration_ms,
        "filesChanged": attempt.files_changed,
        "result": attempt.result_text.as_deref().map(|text| capped(text, MAX_TEXT)),
        "output": attempt.output,
        "startedAt": attempt.started_at,
        "finishedAt": attempt.finished_at,
    })
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
    fn a_cut_leaves_no_half_token_that_masking_would_not_recognize() {
        let token = format!("{}{}", "ghp_", "x".repeat(36));
        let text = format!("the token is {token} and more");
        let cut = capped(&text, "the token is ghp_xxxx".len());
        assert_eq!(mask_secrets(&cut), cut);
        assert!(!cut.contains("ghp_"), "{cut}");
        assert!(cut.starts_with("the token is\n"), "{cut}");
    }
}
