//! Runs workflows. Everything happens inside the app process, without a daemon, but each agent
//! is its own process writing to a file, so it outlives the app and can be picked up again.

mod brain;
mod deliver;
mod git;
mod prompts;
mod runner;
mod services;
mod workflows;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub use brain::{BrainView, DEFAULT_BRAIN_MODEL, DecisionView, NoteView};
pub use git::{Git, GitError, Snapshot};
use keel_core::{AttemptState, Reason, StepRole, StepSpec, WorkflowStep};
use keel_harness::auth::{self, AuthStatus, VersionCheck};
use keel_harness::github::{self, Account, Gh, GhError, Issue, RepoInfo};
use keel_harness::process::{self, SIGINT, SIGTERM};
use keel_harness::{Tools, settings};
use keel_store::rusqlite_value::Value;
use keel_store::{Decision, NewAttempt, NewRun, Project, ProjectSettings, Store, now};
pub use prompts::full_workflow;
use serde_json::json;
pub use workflows::Workflow;

const RESUME_PROMPT: &str = "Continue where you left off.";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] keel_store::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A precondition failed before anything started. Holds a code the UI translates.
    #[error("blocked: {0:?}")]
    Blocked(Reason),
    #[error("this attempt can't do that now")]
    InvalidState,
    #[error("{0} is not a git repository with commits")]
    NotARepo(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn gh_blocked(error: GhError) -> Error {
    Error::Blocked(Reason::DeliveryFailed {
        kind: error.kind.to_owned(),
        detail: error.detail,
    })
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Quiet this long, with the process tree idle, counts as stalled.
    pub stall_after: Duration,
    /// Quiet this long counts as stalled even if something is using CPU.
    pub stall_hard_limit: Duration,
    pub tick: Duration,
    /// How long "wait" silences the stall warning.
    pub snooze: Duration,
    /// Time to answer SIGINT before Keel escalates.
    pub stop_grace: Duration,
    pub max_agents: usize,
    pub keep_awake: bool,
    /// Where git reaches GitHub. Tests point this at local repos.
    pub github_git_base: String,
}

impl Config {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            stall_after: Duration::from_secs(180),
            stall_hard_limit: Duration::from_secs(1800),
            tick: Duration::from_millis(250),
            snooze: Duration::from_secs(600),
            stop_grace: Duration::from_secs(10),
            max_agents: 2,
            keep_awake: true,
            github_git_base: "https://github.com".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartRun {
    pub project_id: String,
    /// Shown as the run's name. Taken from the request if absent.
    pub title: Option<String>,
    pub request: String,
    pub workflow: Vec<WorkflowStep>,
    /// Language for what agents write for the person.
    pub language: String,
    /// The issue this came from, kept as evidence of where the request started.
    pub source_issue: Option<Issue>,
}

impl StartRun {
    /// A run with a single agent step.
    pub fn one_step(project_id: String, step: StepSpec, language: String) -> Self {
        Self {
            project_id,
            title: None,
            request: step.prompt.clone(),
            workflow: vec![WorkflowStep::Agent(step)],
            language,
            source_issue: None,
        }
    }
}

pub use prompts::request_from_issue;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RawCheck {
    /// The file still matches the hash taken when the attempt ended.
    Intact,
    Changed,
    Missing,
    /// The attempt hasn't ended, so there's no hash yet.
    Pending,
}

/// What the session looks like right now, for the header and for starting a run.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub claude_found: bool,
    pub git_found: bool,
    pub gh_found: bool,
    pub version: Option<VersionCheck>,
    pub auth: Option<AuthStatus>,
    /// Auth variables in the environment that Keel removes before starting an agent.
    pub ignored_env: Vec<String>,
    /// Why a run can't start, if it can't.
    pub blocker: Option<Reason>,
}

/// What a project's GitHub account can do, shown when choosing it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubStatus {
    pub repo: Option<String>,
    pub accounts: Vec<Account>,
    pub info: Option<RepoInfo>,
    /// Scopes beyond what Keel needs. It can't reduce them, so it says so.
    pub excess_scopes: Vec<String>,
    pub error: Option<Reason>,
}

/// `repo` covers everything Keel does. `read:org` and `gist` are what `gh` asks for at minimum.
const NEEDED_SCOPES: [&str; 3] = ["repo", "read:org", "gist"];

#[derive(Default)]
struct Slots {
    active: HashSet<String>,
    /// Projects with a step that can run commands. Only one at a time per project.
    command_steps: HashSet<String>,
}

#[derive(Default)]
struct Control {
    stop_requested: bool,
    snooze_until: Option<Instant>,
    pid: Option<i32>,
}

struct Inner {
    store: Arc<Store>,
    config: Config,
    tools: Mutex<Tools>,
    on_change: Box<dyn Fn(&str) + Send + Sync>,
    slots: Mutex<Slots>,
    slot_freed: Condvar,
    controls: Mutex<HashMap<String, Control>>,
    /// Starting a step reads what ran and then adds to it. One at a time, or a step could start twice.
    advancing: Mutex<()>,
    creating_workspace: Mutex<()>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A run's name: the request's first line, cut at a word so it still reads.
fn title_from(prompt: &str) -> String {
    const LIMIT: usize = 72;
    let line = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= LIMIT {
        return line.to_owned();
    }
    let cut: String = line.chars().take(LIMIT).collect();
    let kept = cut
        .rfind(' ')
        .filter(|i| *i > LIMIT / 2)
        .map_or(cut.as_str(), |i| &cut[..i]);
    format!("{}…", kept.trim_end())
}

impl Engine {
    pub fn new(
        store: Arc<Store>,
        config: Config,
        tools: Tools,
        on_change: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                config,
                tools: Mutex::new(tools),
                on_change: Box::new(on_change),
                slots: Mutex::default(),
                slot_freed: Condvar::new(),
                controls: Mutex::default(),
                advancing: Mutex::new(()),
                creating_workspace: Mutex::new(()),
            }),
        }
    }

    pub fn set_tools(&self, tools: Tools) {
        *locked(&self.inner.tools) = tools;
    }

    pub fn preflight(&self) -> Preflight {
        let tools = locked(&self.inner.tools).clone();
        let version_text = tools
            .claude
            .as_deref()
            .and_then(|c| auth::read_version(c, &tools.path_env));
        let version = version_text.as_deref().map(auth::check_version);
        let auth = tools
            .claude
            .as_deref()
            .and_then(|c| AuthStatus::read(c, &tools.path_env));
        let blocker = if tools.claude.is_none() {
            Some(Reason::ClaudeMissing)
        } else if tools.git.is_none() {
            Some(Reason::GitMissing)
        } else if let Some(VersionCheck::TooOld { found, minimum }) = &version {
            Some(Reason::CliVersionTooOld {
                found: found.clone(),
                minimum: minimum.clone(),
            })
        } else if let Some(VersionCheck::Untested { found, below }) = &version {
            Some(Reason::CliVersionUntested {
                found: found.clone(),
                below: below.clone(),
            })
        } else if !auth.as_ref().is_some_and(|a| a.logged_in) {
            Some(Reason::ClaudeLoggedOut)
        } else {
            None
        };
        Preflight {
            claude_found: tools.claude.is_some(),
            git_found: tools.git.is_some(),
            gh_found: tools.gh.is_some(),
            version,
            auth,
            ignored_env: keel_harness::command::ignored_auth_env()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            blocker,
        }
    }

    fn git(&self) -> Result<Git> {
        let bin = locked(&self.inner.tools)
            .git
            .clone()
            .ok_or(Error::Blocked(Reason::GitMissing))?;
        Ok(Git::new(bin))
    }

    fn gh(&self) -> Result<Gh> {
        self.inner.gh()
    }

    pub fn add_project(&self, path: &Path) -> Result<Project> {
        let git = self.git()?;
        let root = git
            .toplevel(path)
            .map_err(|_| Error::NotARepo(path.to_string_lossy().into_owned()))?;
        let root_text = root.to_string_lossy().into_owned();
        if let Some(existing) = self.inner.store.project_by_path(&root_text)? {
            return Ok(existing);
        }
        let name = root
            .file_name()
            .map_or_else(|| root_text.clone(), |n| n.to_string_lossy().into_owned());
        let project = Project {
            id: new_id(),
            name,
            path: root_text,
            created_at: now(),
            github_account: None,
            setup_command: None,
            github_repo: git
                .origin_url(&root)
                .and_then(|url| github::github_repo(&url)),
            check_command: None,
            allowed_domains: Vec::new(),
            allowed_read: Vec::new(),
            services: Vec::new(),
            env: Vec::new(),
        };
        self.inner.store.add_project(&project)?;
        Ok(project)
    }

    pub fn update_project(&self, id: &str, settings: &ProjectSettings) -> Result<Project> {
        services::validate(&settings.services, &settings.env).map_err(Error::Blocked)?;
        self.inner.store.update_project(id, settings)?;
        Ok(self.inner.store.project(id)?)
    }

    /// The accounts in `gh`, and what the project's account can do. Never changes the active one.
    pub fn github_status(&self, project_id: &str) -> Result<GithubStatus> {
        let project = self.inner.store.project(project_id)?;
        let mut status = GithubStatus {
            repo: project.github_repo.clone(),
            accounts: Vec::new(),
            info: None,
            excess_scopes: Vec::new(),
            error: None,
        };
        let gh = match self.gh() {
            Ok(gh) => gh,
            Err(_) => {
                status.error = Some(Reason::GhMissing);
                return Ok(status);
            }
        };
        status.accounts = gh.accounts().map_err(gh_blocked)?;
        let Some(login) = project.github_account else {
            return Ok(status);
        };
        if let Some(account) = status.accounts.iter().find(|a| a.login == login) {
            status.excess_scopes = account
                .scopes
                .iter()
                .filter(|s| !NEEDED_SCOPES.contains(&s.as_str()))
                .cloned()
                .collect();
        }
        let Some(repo) = project.github_repo else {
            status.error = Some(Reason::NoGithubRemote);
            return Ok(status);
        };
        let result = gh
            .token(&login)
            .and_then(|token| gh.repo_info(&token, &repo));
        match result {
            Ok(info) => status.info = Some(info),
            Err(error) => {
                status.error = Some(Reason::DeliveryFailed {
                    kind: error.kind.to_owned(),
                    detail: error.detail,
                })
            }
        }
        Ok(status)
    }

    /// Open issues of the project's repo, to import one by choosing it. Keel never polls.
    pub fn issues(&self, project_id: &str) -> Result<Vec<Issue>> {
        let project = self.inner.store.project(project_id)?;
        let repo = project
            .github_repo
            .ok_or(Error::Blocked(Reason::NoGithubRemote))?;
        let login = project
            .github_account
            .ok_or(Error::Blocked(Reason::GithubAccountMissing))?;
        let gh = self.gh()?;
        let token = gh.token(&login).map_err(gh_blocked)?;
        gh.issues(&token, &repo).map_err(gh_blocked)
    }

    /// Starts a run. Returns once its first step is on its way. Refuses if the session can't work,
    /// or if a later step could never succeed, rather than failing after the agents already ran.
    pub fn start_run(&self, request: StartRun) -> Result<String> {
        if let Some(reason) = self.preflight().blocker {
            return Err(Error::Blocked(reason));
        }
        workflows::validate(&request.workflow).map_err(Error::Blocked)?;
        let project = self.inner.store.project(&request.project_id)?;
        if request
            .workflow
            .iter()
            .any(|s| matches!(s, WorkflowStep::Deliver { .. }))
        {
            if project.github_repo.is_none() {
                return Err(Error::Blocked(Reason::NoGithubRemote));
            }
            if project.github_account.is_none() {
                return Err(Error::Blocked(Reason::GithubAccountMissing));
            }
            self.gh().map_err(|_| Error::Blocked(Reason::GhMissing))?;
        }
        // Preparation always comes first, and from what's published, before any agent touches it.
        let mut workflow = request.workflow;
        if let Some(run) = project
            .setup_command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            && !workflow
                .iter()
                .any(|s| matches!(s, WorkflowStep::Setup { .. }))
            // Only a step that runs something needs what this installs.
            && workflow.iter().any(|s| match s {
                WorkflowStep::Agent(spec) => spec.tools == keel_core::ToolProfile::Build,
                WorkflowStep::Check { .. } => true,
                _ => false,
            })
        {
            // A workflow can have a step of its own called that.
            let id = if workflows::find_step(&workflow, "setup").is_some() {
                "project-setup"
            } else {
                "setup"
            };
            workflow.insert(
                0,
                WorkflowStep::Setup {
                    id: id.into(),
                    run: run.to_owned(),
                },
            );
        }
        let run_id = new_id();
        self.inner.store.add_run(&NewRun {
            id: run_id.clone(),
            project_id: project.id,
            title: request.title.unwrap_or_else(|| title_from(&request.request)),
            request: request.request,
            workflow: serde_json::to_string(&workflow).unwrap_or_else(|_| "[]".into()),
            language: request.language,
            source_issue: request.source_issue.as_ref().map(|issue| {
                json!({ "number": issue.number, "title": issue.title, "author": issue.author, "url": issue.url }).to_string()
            }),
        })?;
        std::fs::create_dir_all(self.inner.run_dir(&run_id))?;
        restrict_dir(&self.inner.run_dir(&run_id));
        self.inner.advance(&run_id)?;
        Ok(run_id)
    }

    /// Answers an approval step. The text, if any, becomes a decision later runs of this project
    /// follow. Rejecting ends the run; approving lets it continue.
    pub fn answer_approval(&self, attempt_id: &str, approve: bool, text: &str) -> Result<()> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        if attempt.state != AttemptState::WaitingApproval.as_str() {
            return Err(Error::InvalidState);
        }
        let answer = if approve { "approve" } else { "reject" };
        let project_id = store.project_of_run(&attempt.run_id)?;
        if !text.trim().is_empty() {
            store.add_decision(
                &project_id,
                &attempt.run_id,
                attempt_id,
                answer,
                text.trim(),
            )?;
        }
        store.append_event(
            attempt_id,
            "decision",
            &json!({ "answer": answer, "text": text.trim() }),
        )?;
        store.patch_attempt(attempt_id, &[("finished_at", Value::Text(now()))])?;
        let state = if approve {
            AttemptState::Succeeded
        } else {
            AttemptState::Rejected
        };
        store.set_state(attempt_id, state, None)?;
        store.append_event(attempt_id, "finished", &json!({ "state": state.as_str() }))?;
        if approve {
            self.inner.advance(&attempt.run_id)?;
        } else {
            store.patch_run(&attempt.run_id, &[("finished_at", Value::Text(now()))])?;
        }
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    pub fn decisions(&self, project_id: &str) -> Result<Vec<Decision>> {
        Ok(self.inner.store.decisions(project_id)?)
    }

    pub fn retire_decision(&self, seq: i64) -> Result<()> {
        Ok(self.inner.store.retire_decision(seq)?)
    }

    /// Asks a step to stop. A queued step never starts; a running one gets SIGINT, and the
    /// monitor escalates if it doesn't answer.
    pub fn stop(&self, attempt_id: &str) -> Result<()> {
        let attempt = self.inner.store.attempt(attempt_id)?;
        let state = AttemptState::parse(&attempt.state).ok_or(Error::InvalidState)?;
        if !state.is_active() {
            return Err(Error::InvalidState);
        }
        let pid = {
            let mut controls = locked(&self.inner.controls);
            let control = controls.entry(attempt_id.to_owned()).or_default();
            control.stop_requested = true;
            control.pid
        };
        self.inner
            .store
            .append_event(attempt_id, "stopRequested", &json!({}))?;
        if let Some(pid) = pid {
            process::signal_group(pid, SIGINT);
        }
        self.inner.slot_freed.notify_all();
        Ok(())
    }

    /// "Wait": clears the stall warning and doesn't raise it again for a while.
    pub fn wait_longer(&self, attempt_id: &str) -> Result<()> {
        let attempt = self.inner.store.attempt(attempt_id)?;
        if attempt.state != AttemptState::Stalled.as_str() {
            return Err(Error::InvalidState);
        }
        {
            // See `check_stall`: the two have to happen together.
            let mut controls = locked(&self.inner.controls);
            controls
                .entry(attempt_id.to_owned())
                .or_default()
                .snooze_until = Some(Instant::now() + self.inner.config.snooze);
            self.inner
                .store
                .set_state(attempt_id, AttemptState::Running, None)?;
        }
        self.inner.store.append_event(
            attempt_id,
            "waitLonger",
            &json!({ "seconds": self.inner.config.snooze.as_secs() }),
        )?;
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// "Kill the command": ends the command a stalled step is stuck on. The agent keeps going
    /// and sees that it failed.
    pub fn kill_command(&self, attempt_id: &str) -> Result<()> {
        let attempt = self.inner.store.attempt(attempt_id)?;
        let root = locked(&self.inner.controls)
            .get(attempt_id)
            .and_then(|c| c.pid)
            .ok_or(Error::InvalidState)?;
        let activity = process::activity(root);
        let (Some(pid), Some(command)) = (activity.pid, activity.command) else {
            return Err(Error::InvalidState);
        };
        process::signal_process(pid, SIGTERM);
        self.inner.store.append_event(
            attempt_id,
            "commandKilled",
            &json!({ "pid": pid, "command": command }),
        )?;
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// Tries a step again in the same workspace. With `resume`, an agent continues its session
    /// instead of starting a new one. A failed delivery retries only the delivery.
    pub fn retry(&self, attempt_id: &str, resume: bool) -> Result<String> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        let state = AttemptState::parse(&attempt.state).ok_or(Error::InvalidState)?;
        if state.is_active() || state == AttemptState::WaitingApproval {
            return Err(Error::InvalidState);
        }
        let run = store.run(&attempt.run_id)?;
        let number = store
            .attempts_of_run(&attempt.run_id)?
            .iter()
            .filter(|a| a.step_id == attempt.step_id)
            .map(|a| a.number)
            .max()
            .unwrap_or(0)
            + 1;
        let steps: Vec<WorkflowStep> =
            serde_json::from_value(run.workflow.clone()).unwrap_or_default();
        let step = workflows::find_step(&steps, &attempt.step_id).ok_or(Error::InvalidState)?;
        let resume_session =
            (resume && attempt.kind == "agent").then(|| attempt.session_id.clone());
        let new_attempt = self.inner.start_step(
            &attempt.run_id,
            &step,
            number,
            resume_session.clone(),
            resume_session.is_some().then(|| attempt.id.clone()),
        )?;
        if run.finished_at.is_some() {
            store.patch_run(&attempt.run_id, &[("finished_at", Value::Null)])?;
        }
        Ok(new_attempt)
    }

    pub fn abandon(&self, attempt_id: &str) -> Result<()> {
        let attempt = self.inner.store.attempt(attempt_id)?;
        let state = AttemptState::parse(&attempt.state).ok_or(Error::InvalidState)?;
        if state.is_active() {
            return Err(Error::InvalidState);
        }
        self.inner
            .store
            .set_state(attempt_id, AttemptState::Abandoned, None)?;
        self.inner
            .store
            .append_event(attempt_id, "abandoned", &json!({}))?;
        self.inner
            .store
            .patch_run(&attempt.run_id, &[("finished_at", Value::Text(now()))])?;
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// Run once at startup. Reconnects to steps that outlived the app, closes out the rest, and
    /// continues any run that stopped between two steps. Approvals simply keep waiting.
    pub fn recover(&self) -> Result<()> {
        for attempt in self.inner.store.active_attempts()? {
            self.inner.recover_attempt(&attempt)?;
        }
        for run in self.inner.store.open_runs()? {
            self.inner.advance(&run.id)?;
        }
        Ok(())
    }

    /// Whether the raw stream still matches the hash taken when the attempt ended.
    pub fn verify_raw(&self, attempt_id: &str) -> Result<RawCheck> {
        let attempt = self.inner.store.attempt(attempt_id)?;
        let Some(expected) = attempt.raw_sha256 else {
            return Ok(RawCheck::Pending);
        };
        Ok(match process::sha256_file(Path::new(&attempt.raw_path)) {
            Ok(actual) if actual == expected => RawCheck::Intact,
            Ok(_) => RawCheck::Changed,
            Err(_) => RawCheck::Missing,
        })
    }

    /// Everything the run has changed so far, for the person deciding whether it goes out.
    pub fn run_diff(&self, run_id: &str) -> Result<String> {
        let attempts = self.inner.store.attempts_of_run(run_id)?;
        let Some(base) = attempts.iter().find_map(|a| a.sha_before.clone()) else {
            return Ok(String::new());
        };
        let workspace = self.inner.workspace(run_id);
        if !workspace.join(".git").exists() {
            return Ok(String::new());
        }
        Ok(self.git()?.diff_since(&workspace, &base)?)
    }

    /// A person's answer when the approver hit its limit: another round, or on to their approval.
    pub fn resolve_rework_limit(&self, attempt_id: &str, another_round: bool) -> Result<()> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        let is_limit = attempt.state == AttemptState::Blocked.as_str()
            && attempt.role == "approver"
            && attempt
                .reason
                .as_ref()
                .and_then(|r| r.get("code"))
                .and_then(|c| c.as_str())
                == Some("reworkLimit");
        if !is_limit {
            return Err(Error::InvalidState);
        }
        store.append_event(
            attempt_id,
            "reworkLimitAnswer",
            &json!({ "anotherRound": another_round }),
        )?;
        let run = store.run(&attempt.run_id)?;
        if another_round {
            store.patch_run(&attempt.run_id, &[("round", Value::Integer(run.round + 1))])?;
        } else {
            // The person takes it from here. The approver's own answer stays on record in its events.
            let mut output = attempt.output.clone().unwrap_or_else(|| json!({}));
            output["decision"] = json!("approve");
            output["overriddenByPerson"] = json!(true);
            store.patch_attempt(attempt_id, &[("output", Value::Text(output.to_string()))])?;
        }
        store.set_state(attempt_id, AttemptState::Succeeded, None)?;
        self.inner.advance(&attempt.run_id)?;
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// The workflows a project can use: the built-in ones, then its own.
    pub fn workflows(&self, project_id: &str, language: &str) -> Result<Vec<Workflow>> {
        let project = self.inner.store.project(project_id)?;
        let mut all = workflows::builtin(language, project.check_command.as_deref());
        all.extend(workflows::load_all(
            &self.inner.mirror(project_id).with_file_name("workflows"),
        )?);
        Ok(all)
    }

    pub fn save_workflow(&self, project_id: &str, workflow: &Workflow) -> Result<Workflow> {
        workflows::validate(&workflow.steps).map_err(Error::Blocked)?;
        Ok(workflows::save(
            &self.inner.mirror(project_id).with_file_name("workflows"),
            workflow,
        )?)
    }

    pub fn delete_workflow(&self, project_id: &str, workflow_id: &str) -> Result<()> {
        Ok(workflows::delete(
            &self.inner.mirror(project_id).with_file_name("workflows"),
            workflow_id,
        )?)
    }

    pub fn diff(&self, attempt_id: &str) -> Result<String> {
        let attempt = self.inner.store.attempt(attempt_id)?;
        Ok(match attempt.diff_path {
            Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
            None => String::new(),
        })
    }

    pub fn active_count(&self) -> Result<usize> {
        Ok(self.inner.store.active_attempts()?.len())
    }
}

fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// Helpers shared with `runner` and `deliver`.
impl Inner {
    fn changed(&self, run_id: &str) {
        (self.on_change)(run_id);
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.config.data_dir.join("runs").join(run_id)
    }

    fn workspace(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("workspace")
    }

    fn mirror(&self, project_id: &str) -> PathBuf {
        self.config
            .data_dir
            .join("projects")
            .join(project_id)
            .join("mirror.git")
    }

    fn gh(&self) -> Result<Gh> {
        let tools = locked(&self.tools);
        let bin = tools.gh.clone().ok_or(Error::Blocked(Reason::GhMissing))?;
        Ok(Gh {
            bin,
            path_env: tools.path_env.clone(),
        })
    }

    /// Records a step and sets it going.
    fn start_step(
        self: &Arc<Self>,
        run_id: &str,
        step: &WorkflowStep,
        number: i64,
        resume: Option<String>,
        resumed_from: Option<String>,
    ) -> Result<String> {
        let run = self.store.run(run_id)?;
        let project = self.store.project(&run.project_id)?;
        let workspace = self.workspace(run_id);
        let dir = self.run_dir(run_id);
        std::fs::create_dir_all(&dir)?;

        let stem = format!("{}-{number}", step.id());
        let id = new_id();
        let (model, effort, tools, turns, budget, prompt, settings_json, raw_name) = match step {
            WorkflowStep::Agent(spec) => {
                let text = if resume.is_some() {
                    RESUME_PROMPT.to_owned()
                } else {
                    self.render_prompt(&spec.prompt, &run)?
                };
                (
                    spec.model.clone(),
                    spec.effort.clone(),
                    spec.tools.as_str().to_owned(),
                    spec.max_turns,
                    spec.max_budget_usd,
                    text,
                    settings::step_settings(
                        &workspace,
                        &project.allowed_domains,
                        &project.allowed_read,
                        // Only steps that run commands get anywhere near a service.
                        &if spec.tools == keel_core::ToolProfile::Build {
                            services::socket_dirs(&services::root_for(&project.id), &project.services)
                        } else {
                            Vec::new()
                        },
                    )
                    .to_string(),
                    format!("{stem}.jsonl"),
                )
            }
            WorkflowStep::Setup { run: command, .. } => (
                String::new(),
                None,
                "setup".to_owned(),
                None,
                None,
                command.clone(),
                json!({ "network": "internet, nothing local", "read": project.allowed_read, "sockets": services::socket_dirs(&services::root_for(&project.id), &project.services) }).to_string(),
                format!("{stem}.log"),
            ),
            WorkflowStep::Check { run: command, .. } => (
                String::new(),
                None,
                "check".to_owned(),
                None,
                None,
                command.clone(),
                json!({ "network": "none", "read": project.allowed_read, "sockets": services::socket_dirs(&services::root_for(&project.id), &project.services) }).to_string(),
                format!("{stem}.log"),
            ),
            WorkflowStep::Parallel { .. } | WorkflowStep::Approval { .. } | WorkflowStep::Deliver { .. } => (
                String::new(),
                None,
                "none".to_owned(),
                None,
                None,
                String::new(),
                "{}".to_owned(),
                format!("{stem}.log"),
            ),
        };
        let session_id = resume.clone().unwrap_or_else(new_id);
        self.store.add_attempt(&NewAttempt {
            id: id.clone(),
            run_id: run_id.to_owned(),
            step_id: step.id().to_owned(),
            kind: step.kind().to_owned(),
            role: match step {
                WorkflowStep::Agent(spec) => spec.role.as_str().to_owned(),
                _ => "worker".to_owned(),
            },
            round: run.round,
            number,
            state: if matches!(step, WorkflowStep::Approval { .. }) {
                AttemptState::WaitingApproval
            } else {
                AttemptState::Queued
            },
            model_requested: model,
            effort,
            tools,
            max_turns: turns,
            max_budget_usd: budget,
            language: run.language.clone(),
            prompt,
            system_prompt_sources: "[]".to_owned(),
            settings: settings_json,
            session_id,
            resumed_from,
            workspace: workspace.to_string_lossy().into_owned(),
            raw_path: dir.join(raw_name).to_string_lossy().into_owned(),
            started_at: now(),
        })?;
        self.changed(run_id);
        if matches!(step, WorkflowStep::Approval { .. }) {
            self.store.append_event(&id, "waiting", &json!({}))?;
            return Ok(id);
        }
        locked(&self.controls).insert(id.clone(), Control::default());
        let inner = self.clone();
        let step = step.clone();
        let attempt_id = id.clone();
        let project_id = project.id.clone();
        std::thread::spawn(move || inner.execute(attempt_id, project_id, step, resume));
        Ok(id)
    }

    /// What a step's prompt becomes right now. The answers come from this run's latest round.
    fn render_prompt(&self, template: &str, run: &keel_store::Run) -> Result<String> {
        let attempts = self.store.attempts_of_run(&run.id)?;
        let workspace = self.workspace(&run.id);
        let base = attempts.iter().find_map(|a| a.sha_before.clone());
        let git = locked(&self.tools).git.clone().map(Git::new);
        Ok(prompts::render(template, |name| match name {
            "request" => Some(run.request.clone()),
            "diff" => {
                let diff = match (&git, &base) {
                    (Some(git), Some(base)) if workspace.join(".git").exists() => {
                        git.diff_since(&workspace, base).unwrap_or_default()
                    }
                    _ => String::new(),
                };
                Some(workflows::cap_diff(diff))
            }
            "rework" => Some(workflows::rework_text(&attempts, run.round, &run.language)),
            _ => {
                let id = name.strip_prefix("steps.")?.strip_suffix(".output")?;
                Some(workflows::output_of(&attempts, id))
            }
        }))
    }

    /// Moves the run on as far as it can. Anything that isn't a success needs a person, and an
    /// approver can send the work back, which reruns everything from the step it names.
    fn advance(self: &Arc<Self>, run_id: &str) -> Result<()> {
        let _guard = locked(&self.advancing);
        'round: loop {
            let run = self.store.run(run_id)?;
            if run.finished_at.is_some() {
                return Ok(());
            }
            let steps: Vec<WorkflowStep> =
                serde_json::from_value(run.workflow.clone()).unwrap_or_default();
            let attempts = self.store.attempts_of_run(run_id)?;
            let rework_from = workflows::rework_index(&steps);
            // Steps the approver can send work back to run again each round. Earlier ones don't.
            let latest = |id: &str, index: usize| {
                let round = if rework_from.is_some_and(|from| index >= from) {
                    run.round
                } else {
                    0
                };
                attempts
                    .iter()
                    .rev()
                    .find(|a| a.step_id == id && a.round >= round)
            };
            for (index, step) in steps.iter().enumerate() {
                if let WorkflowStep::Parallel { steps: group, .. } = step {
                    let changed = workflows::files_changed(&attempts);
                    let mut pending = false;
                    for spec in group {
                        match latest(&spec.id, index) {
                            None if !workflows::paths_match(&spec.paths, &changed) => {
                                self.record_skipped(run_id, spec, &attempts)?
                            }
                            None => {
                                let number = workflows::next_number(&attempts, &spec.id);
                                self.start_step(
                                    run_id,
                                    &WorkflowStep::Agent(spec.clone()),
                                    number,
                                    None,
                                    None,
                                )?;
                                pending = true;
                            }
                            Some(a)
                                if a.state == AttemptState::Succeeded.as_str()
                                    || a.state == AttemptState::Skipped.as_str() => {}
                            Some(_) => pending = true,
                        }
                    }
                    if pending {
                        return Ok(());
                    }
                    continue;
                }
                let Some(attempt) = latest(step.id(), index) else {
                    let number = workflows::next_number(&attempts, step.id());
                    self.start_step(run_id, step, number, None, None)?;
                    return Ok(());
                };
                if attempt.state != AttemptState::Succeeded.as_str() {
                    // Proposals are a bonus. If they fail, everything before them still stands.
                    if brain::optional_step_is_over(step, attempt) {
                        continue;
                    }
                    return Ok(());
                }
                if let WorkflowStep::Agent(StepSpec {
                    role: StepRole::Approver { max_rounds, .. },
                    ..
                }) = step
                    && attempt.round == run.round
                    && workflows::wants_rework(attempt)
                {
                    if run.round < i64::from(*max_rounds) {
                        self.store
                            .patch_run(run_id, &[("round", Value::Integer(run.round + 1))])?;
                        self.store.append_event(
                            &attempt.id,
                            "rework",
                            &json!({ "round": run.round + 1, "output": attempt.output }),
                        )?;
                        self.changed(run_id);
                        continue 'round;
                    }
                    let reason = Reason::ReworkLimit {
                        rounds: *max_rounds,
                    };
                    self.store
                        .set_state(&attempt.id, AttemptState::Blocked, Some(&reason))?;
                    self.store.append_event(
                        &attempt.id,
                        "reworkLimit",
                        &json!({ "rounds": max_rounds, "output": attempt.output }),
                    )?;
                    self.changed(run_id);
                    return Ok(());
                }
            }
            self.store
                .patch_run(run_id, &[("finished_at", Value::Text(now()))])?;
            self.changed(run_id);
            return Ok(());
        }
    }

    /// A reviewer that has nothing to look at says so, on the record, instead of disappearing.
    fn record_skipped(
        &self,
        run_id: &str,
        spec: &StepSpec,
        attempts: &[keel_store::Attempt],
    ) -> Result<()> {
        let run = self.store.run(run_id)?;
        let id = new_id();
        let reason = Reason::NoMatchingPaths {
            paths: spec.paths.clone(),
        };
        self.store.add_attempt(&NewAttempt {
            id: id.clone(),
            run_id: run_id.to_owned(),
            step_id: spec.id.clone(),
            kind: "agent".to_owned(),
            role: spec.role.as_str().to_owned(),
            round: run.round,
            number: workflows::next_number(attempts, &spec.id),
            state: AttemptState::Skipped,
            model_requested: spec.model.clone(),
            effort: spec.effort.clone(),
            tools: spec.tools.as_str().to_owned(),
            max_turns: spec.max_turns,
            max_budget_usd: spec.max_budget_usd,
            language: run.language.clone(),
            prompt: spec.prompt.clone(),
            system_prompt_sources: "[]".to_owned(),
            settings: "{}".to_owned(),
            session_id: new_id(),
            resumed_from: None,
            workspace: self.workspace(run_id).to_string_lossy().into_owned(),
            raw_path: String::new(),
            started_at: now(),
        })?;
        self.store
            .set_state(&id, AttemptState::Skipped, Some(&reason))?;
        self.store
            .patch_attempt(&id, &[("finished_at", Value::Text(now()))])?;
        self.store
            .append_event(&id, "skipped", &json!({ "paths": spec.paths }))?;
        self.changed(run_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::ToolProfile;

    #[test]
    fn long_requests_get_a_short_title() {
        assert_eq!(title_from("\n\n  Fix it\nmore"), "Fix it");
        assert_eq!(title_from(&"x".repeat(200)).chars().count(), 73);
        let long = "The add function in calc.py returns the wrong result. Fix it so the tests in test_calc.py pass.";
        assert_eq!(
            title_from(long),
            "The add function in calc.py returns the wrong result. Fix it so the…"
        );
    }

    #[test]
    fn step_kinds_serialize_with_a_type() {
        let step = WorkflowStep::Check {
            id: "ci".into(),
            run: "make test".into(),
        };
        let value = serde_json::to_value(&step).unwrap();
        assert_eq!(
            value,
            json!({ "type": "check", "id": "ci", "run": "make test" })
        );
        let back: WorkflowStep = serde_json::from_value(value).unwrap();
        assert_eq!(back, step);
        let agent = WorkflowStep::Agent(StepSpec {
            id: "a".into(),
            model: "opus".into(),
            effort: None,
            tools: ToolProfile::Build,
            prompt: "p".into(),
            max_turns: None,
            max_budget_usd: None,
            paths: vec![],
            role: Default::default(),
        });
        let back: WorkflowStep =
            serde_json::from_value(serde_json::to_value(&agent).unwrap()).unwrap();
        assert_eq!(back, agent);
    }
}
