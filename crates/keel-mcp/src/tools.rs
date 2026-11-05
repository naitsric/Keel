//! What each tool does: plain methods of [`McpServer`], each leaving a row in the audit log. The
//! engine blocks, so they run on tokio's blocking threads.

use std::time::{Duration, Instant};

use keel_core::{WorkflowStep, mask_secrets, mask_value};
use keel_engine::{Error, StartRun, request_from_issue};
use keel_store::{Attempt, McpClient, NewMcpCall, Project};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::{McpServer, ToolError};

/// Runs a client may have open at once. A run waiting for the person counts, so an assistant can't
/// spend a person's usage faster than they're looking at it.
pub const MAX_OPEN_RUNS: i64 = 3;

const MAX_TEXT: usize = 16 * 1024;
const MAX_DIFF: usize = 64 * 1024;
const MAX_LOGGED_ARGS: usize = 4 * 1024;
const MAX_LOGGED_ERROR: usize = 4 * 1024;
const WAIT_DEFAULT_SECS: u64 = 30;
const WAIT_MAX_SECS: u64 = 50;
const WAIT_TICK: Duration = Duration::from_millis(300);

/// A person, not a client, moves a run on from these.
const NEEDS_PERSON: [&str; 6] = [
    "waitingApproval",
    "stalled",
    "blocked",
    "interrupted",
    "failed",
    "stopped",
];
/// What stop_run may stop. A step waiting for approval isn't working, and only a person answers it.
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
            Ok((json!({ "workflows": workflows }), None))
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
            let run = engine
                .start_run(StartRun {
                    project_id: project.id,
                    title: issue
                        .as_ref()
                        .map(|issue| format!("#{} {}", issue.number, issue.title)),
                    request,
                    workflow: workflow.steps,
                    language,
                    source_issue: issue,
                })
                .map_err(engine_error)?;
            store
                .set_run_started_by(&run, &client.name)
                .map_err(store_error)?;
            engine.announce(&run);
            Ok((server.run_view(&run)?, Some(run)))
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
            "language": run.language,
            "branch": run.branch,
            "prUrl": run.pr_url,
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
            "type": "agent", "id": spec.id, "model": spec.model, "tools": spec.tools, "role": spec.role,
        }),
        WorkflowStep::Setup { id, run } => json!({ "type": "setup", "id": id, "run": run }),
        WorkflowStep::Check { id, run } => json!({ "type": "check", "id": id, "run": run }),
        WorkflowStep::Parallel { id, steps } => json!({
            "type": "parallel",
            "id": id,
            "steps": steps.iter().map(|spec| json!({
                "id": spec.id, "model": spec.model, "tools": spec.tools, "paths": spec.paths,
            })).collect::<Vec<_>>(),
        }),
        WorkflowStep::Approval { id } => json!({ "type": "approval", "id": id }),
        WorkflowStep::Deliver { id } => json!({ "type": "deliver", "id": id }),
    }
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
