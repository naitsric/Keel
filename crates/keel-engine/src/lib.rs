//! Runs steps. Everything happens inside the app process, without a daemon, but each agent is
//! its own process writing to a file, so it outlives the app and can be picked up again.

mod git;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub use git::{Git, GitError, Snapshot};
use keel_core::{AttemptState, Reason, StepSpec, ToolProfile};
use keel_harness::auth::{self, AuthStatus, VersionCheck};
use keel_harness::process::{self, SIGINT, SIGTERM, Tail};
use keel_harness::stream::ResultEvent;
use keel_harness::{Parsed, StepCommand, Tools, parse_line, settings};
use keel_store::rusqlite_value::Value;
use keel_store::{Attempt, NewAttempt, Project, Store, now};
use serde_json::json;

/// Tool output kept in the database. The raw file always has all of it.
const EVENT_TEXT_LIMIT: usize = 64 * 1024;
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartRun {
    pub project_id: String,
    pub step: StepSpec,
    /// Language for the agent's closing explanation.
    pub language: String,
    pub allowed_domains: Vec<String>,
}

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
    pub version: Option<VersionCheck>,
    pub auth: Option<AuthStatus>,
    /// Auth variables in the environment that Keel removes before starting an agent.
    pub ignored_env: Vec<String>,
    /// Why a run can't start, if it can't.
    pub blocker: Option<Reason>,
}

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

fn title_from(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > 90 {
        format!("{}…", line.chars().take(89).collect::<String>())
    } else {
        line.to_owned()
    }
}

fn language_name(code: &str) -> &'static str {
    if code.starts_with("es") {
        "Spanish"
    } else {
        "English"
    }
}

fn text_value(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
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
        Ok(Git { bin })
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
        };
        self.inner.store.add_project(&project)?;
        Ok(project)
    }

    /// Starts a run with one step. Returns once it's queued. Refuses if there's no usable session.
    pub fn start_run(&self, request: StartRun) -> Result<String> {
        if let Some(reason) = self.preflight().blocker {
            return Err(Error::Blocked(reason));
        }
        let project = self.inner.store.project(&request.project_id)?;
        let run_id = new_id();
        self.inner.store.add_run(
            &run_id,
            &project.id,
            &title_from(&request.step.prompt),
            None,
        )?;
        let dir = self.inner.config.data_dir.join("runs").join(&run_id);
        let workspace = dir.join("workspace");
        let attempt_id = self.insert_attempt(
            &run_id,
            1,
            &request.step,
            &request.language,
            &workspace,
            new_id(),
            None,
            &request.allowed_domains,
        )?;
        self.launch(Launch {
            attempt_id,
            run_id: run_id.clone(),
            project_id: project.id,
            step: request.step,
            source: Some(PathBuf::from(project.path)),
            resume: None,
        });
        Ok(run_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_attempt(
        &self,
        run_id: &str,
        number: i64,
        step: &StepSpec,
        language: &str,
        workspace: &Path,
        session_id: String,
        resumed_from: Option<String>,
        domains: &[String],
    ) -> Result<String> {
        let id = new_id();
        let dir = workspace.parent().unwrap_or(workspace).to_path_buf();
        std::fs::create_dir_all(&dir)?;
        restrict_dir(&dir);
        let raw_path = dir.join(format!("{}-{number}.jsonl", step.id));
        self.inner.store.add_attempt(&NewAttempt {
            id: id.clone(),
            run_id: run_id.to_owned(),
            step_id: step.id.clone(),
            number,
            state: AttemptState::Queued,
            model_requested: step.model.clone(),
            effort: step.effort.clone(),
            tools: step.tools.as_str().to_owned(),
            max_turns: step.max_turns,
            max_budget_usd: step.max_budget_usd,
            language: language.to_owned(),
            prompt: step.prompt.clone(),
            system_prompt_sources: "[]".to_owned(),
            settings: settings::step_settings(workspace, domains, &[]).to_string(),
            session_id,
            resumed_from,
            workspace: workspace.to_string_lossy().into_owned(),
            raw_path: raw_path.to_string_lossy().into_owned(),
            started_at: now(),
        })?;
        Ok(id)
    }

    fn launch(&self, launch: Launch) {
        locked(&self.inner.controls).insert(launch.attempt_id.clone(), Control::default());
        let inner = self.inner.clone();
        (inner.on_change)(&launch.run_id);
        std::thread::spawn(move || inner.execute(launch));
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
        locked(&self.inner.controls)
            .entry(attempt_id.to_owned())
            .or_default()
            .snooze_until = Some(Instant::now() + self.inner.config.snooze);
        self.inner
            .store
            .set_state(attempt_id, AttemptState::Running, None)?;
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

    /// Tries again in the same workspace. With `resume`, it continues the agent's session
    /// instead of starting a new one from the prompt.
    pub fn retry(&self, attempt_id: &str, resume: bool) -> Result<String> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        let state = AttemptState::parse(&attempt.state).ok_or(Error::InvalidState)?;
        if state.is_active() {
            return Err(Error::InvalidState);
        }
        let latest = store
            .attempts_of_run(&attempt.run_id)?
            .last()
            .map_or(0, |a| a.number);
        let step = StepSpec {
            id: attempt.step_id.clone(),
            model: attempt.model_requested.clone(),
            effort: attempt.effort.clone(),
            tools: ToolProfile::parse(&attempt.tools).unwrap_or(ToolProfile::ReadOnly),
            prompt: if resume {
                RESUME_PROMPT.to_owned()
            } else {
                attempt.prompt.clone()
            },
            max_turns: attempt.max_turns.and_then(|t| u32::try_from(t).ok()),
            max_budget_usd: attempt.max_budget_usd,
        };
        let session = if resume {
            attempt.session_id.clone()
        } else {
            new_id()
        };
        let domains: Vec<String> = attempt
            .settings
            .pointer("/sandbox/network/allowedDomains")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default();
        let workspace = PathBuf::from(&attempt.workspace);
        let new_attempt = self.insert_attempt(
            &attempt.run_id,
            latest + 1,
            &step,
            &attempt.language,
            &workspace,
            session,
            resume.then(|| attempt.id.clone()),
            &domains,
        )?;
        let project_id = store.project_of_run(&attempt.run_id)?;
        let source = (!workspace.join(".git").exists())
            .then(|| store.project(&project_id).map(|p| PathBuf::from(p.path)))
            .transpose()?;
        self.launch(Launch {
            attempt_id: new_attempt.clone(),
            run_id: attempt.run_id,
            project_id,
            step,
            source,
            resume: resume.then_some(attempt.session_id),
        });
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
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// Run once at startup. Reconnects to steps that outlived the app, and closes out the rest.
    pub fn recover(&self) -> Result<()> {
        for attempt in self.inner.store.active_attempts()? {
            let alive = match (attempt.pid, &attempt.pid_started) {
                (Some(pid), Some(started)) => process::is_same_process(pid as i32, started),
                _ => false,
            };
            let store = &self.inner.store;
            store.append_event(&attempt.id, "recovered", &json!({ "processAlive": alive }))?;
            let project_id = store.project_of_run(&attempt.run_id)?;
            locked(&self.inner.controls).insert(
                attempt.id.clone(),
                Control {
                    pid: attempt.pid.map(|p| p as i32),
                    ..Control::default()
                },
            );
            let inner = self.inner.clone();
            let is_command_step = attempt.tools == ToolProfile::Build.as_str();
            {
                let mut slots = locked(&inner.slots);
                slots.active.insert(attempt.id.clone());
                if is_command_step {
                    slots.command_steps.insert(project_id.clone());
                }
            }
            let tail = Tail {
                offset: attempt.raw_offset as u64,
            };
            let context = Monitor::new(&attempt, project_id, is_command_step, None, tail);
            if alive {
                std::thread::spawn(move || inner.monitor(context));
            } else {
                inner.finish(context, None, true);
            }
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

struct Launch {
    attempt_id: String,
    run_id: String,
    project_id: String,
    step: StepSpec,
    /// The user's repo, when the workspace has to be created.
    source: Option<PathBuf>,
    resume: Option<String>,
}

struct Monitor {
    attempt_id: String,
    run_id: String,
    step_id: String,
    project_id: String,
    is_command_step: bool,
    tools_expected: Vec<String>,
    raw_path: PathBuf,
    workspace: PathBuf,
    sha_before: Option<String>,
    resumed_from: Option<String>,
    root_pid: Option<i32>,
    child: Option<Child>,
    tail: Tail,
    /// Why this attempt must end blocked, once decided.
    block: Option<Reason>,
    result: Option<ResultEvent>,
}

impl Monitor {
    fn new(
        attempt: &Attempt,
        project_id: String,
        is_command_step: bool,
        child: Option<Child>,
        tail: Tail,
    ) -> Self {
        let tools = ToolProfile::parse(&attempt.tools).unwrap_or(ToolProfile::ReadOnly);
        Self {
            attempt_id: attempt.id.clone(),
            run_id: attempt.run_id.clone(),
            step_id: attempt.step_id.clone(),
            project_id,
            is_command_step,
            tools_expected: tools.tools().iter().map(|t| (*t).to_owned()).collect(),
            raw_path: PathBuf::from(&attempt.raw_path),
            workspace: PathBuf::from(&attempt.workspace),
            sha_before: attempt.sha_before.clone(),
            resumed_from: attempt.resumed_from.clone(),
            root_pid: attempt.pid.map(|p| p as i32),
            child,
            tail,
            block: None,
            result: None,
        }
    }
}

impl Inner {
    fn changed(&self, run_id: &str) {
        (self.on_change)(run_id);
    }

    fn stop_requested(&self, attempt_id: &str) -> bool {
        locked(&self.controls)
            .get(attempt_id)
            .is_some_and(|c| c.stop_requested)
    }

    /// Waits for room. Runs are limited overall, and a project runs one command step at a
    /// time, because tests often share resources such as a database.
    fn acquire_slot(&self, launch: &Launch) -> bool {
        let needs_project = launch.step.tools == ToolProfile::Build;
        let mut last_reason: Option<Reason> = None;
        let mut slots = locked(&self.slots);
        loop {
            if self.stop_requested(&launch.attempt_id) {
                return false;
            }
            let reason = if needs_project && slots.command_steps.contains(&launch.project_id) {
                Some(Reason::WaitingForProject)
            } else if slots.active.len() >= self.config.max_agents {
                Some(Reason::WaitingForCapacity)
            } else {
                None
            };
            let Some(reason) = reason else {
                slots.active.insert(launch.attempt_id.clone());
                if needs_project {
                    slots.command_steps.insert(launch.project_id.clone());
                }
                return true;
            };
            if last_reason.as_ref() != Some(&reason) {
                let _ =
                    self.store
                        .set_state(&launch.attempt_id, AttemptState::Queued, Some(&reason));
                self.changed(&launch.run_id);
                last_reason = Some(reason);
            }
            slots = self
                .slot_freed
                .wait_timeout(slots, Duration::from_millis(200))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    fn release_slot(&self, attempt_id: &str, project_id: &str, is_command_step: bool) {
        let mut slots = locked(&self.slots);
        slots.active.remove(attempt_id);
        if is_command_step {
            slots.command_steps.remove(project_id);
        }
        drop(slots);
        self.slot_freed.notify_all();
    }

    fn end_early(&self, launch: &Launch, state: AttemptState, reason: Option<Reason>) {
        let _ = self
            .store
            .set_state(&launch.attempt_id, state, reason.as_ref());
        let _ = self
            .store
            .patch_attempt(&launch.attempt_id, &[("finished_at", Value::Text(now()))]);
        let _ = self.store.append_event(
            &launch.attempt_id,
            "state",
            &json!({ "state": state.as_str(), "reason": reason }),
        );
        self.changed(&launch.run_id);
    }

    fn execute(self: Arc<Self>, launch: Launch) {
        if !self.acquire_slot(&launch) {
            self.end_early(&launch, AttemptState::Stopped, None);
            return;
        }
        let is_command_step = launch.step.tools == ToolProfile::Build;
        let started = self.prepare_and_spawn(&launch);
        match started {
            Ok(Some(monitor)) => self.monitor(monitor),
            Ok(None) => {
                self.release_slot(&launch.attempt_id, &launch.project_id, is_command_step);
                self.end_early(&launch, AttemptState::Stopped, None);
            }
            Err(error) => {
                self.release_slot(&launch.attempt_id, &launch.project_id, is_command_step);
                let reason = match error {
                    Error::Blocked(reason) => reason,
                    other => Reason::PrepareFailed {
                        detail: other.to_string(),
                    },
                };
                self.end_early(&launch, AttemptState::Failed, Some(reason));
            }
        }
    }

    fn prepare_and_spawn(&self, launch: &Launch) -> Result<Option<Monitor>> {
        let store = &self.store;
        let attempt = store.attempt(&launch.attempt_id)?;
        store.set_state(&attempt.id, AttemptState::Preparing, None)?;
        self.changed(&attempt.run_id);

        let tools = locked(&self.tools).clone();
        let claude = tools
            .claude
            .clone()
            .ok_or(Error::Blocked(Reason::ClaudeMissing))?;
        let git = Git {
            bin: tools
                .git
                .clone()
                .ok_or(Error::Blocked(Reason::GitMissing))?,
        };
        let workspace = PathBuf::from(&attempt.workspace);
        let dir = workspace.parent().unwrap_or(&workspace).to_path_buf();

        let mut base_ref = None;
        if let Some(source) = &launch.source {
            let mirror = self
                .config
                .data_dir
                .join("projects")
                .join(&launch.project_id)
                .join("mirror.git");
            base_ref = Some(git.update_mirror(source, &mirror)?);
            let identity = git.identity(source);
            let short: String = attempt.run_id.chars().take(8).collect();
            git.create_workspace(&mirror, &workspace, &format!("keel/{short}"), &identity)?;
        }
        let sha_before = git.head(&workspace)?;

        let stem = format!("{}-{}", attempt.step_id, attempt.number);
        let settings_file = dir.join(format!("{stem}.settings.json"));
        std::fs::write(&settings_file, attempt.settings.to_string())?;
        let (system_prompt, sources) =
            system_prompt(&workspace, &attempt.language, launch.step.tools);
        let system_prompt_file = dir.join(format!("{stem}.system.md"));
        std::fs::write(&system_prompt_file, &system_prompt)?;
        let prompt_file = dir.join(format!("{stem}.prompt.txt"));
        std::fs::write(&prompt_file, &launch.step.prompt)?;

        let command = StepCommand {
            claude: claude.clone(),
            path_env: tools.path_env.clone(),
            workspace: workspace.clone(),
            settings_file,
            system_prompt_file,
            session_id: attempt.session_id.clone(),
            resume: launch.resume.clone(),
        };
        let args = command.args(&launch.step);
        let version = auth::read_version(&claude, &tools.path_env);
        let account = AuthStatus::read(&claude, &tools.path_env).and_then(|a| a.account_label());
        store.patch_attempt(
            &attempt.id,
            &[
                ("sha_before", Value::Text(sha_before.clone())),
                ("system_prompt_sources", Value::Text(sources.to_string())),
                ("cli_version", text_value(version.clone())),
                ("account", text_value(account.clone())),
            ],
        )?;
        store.append_event(
            &attempt.id,
            "started",
            &json!({
                "args": args,
                "envNames": keel_harness::clean_env(&tools.path_env).into_iter().map(|(k, _)| k).collect::<Vec<_>>(),
                "cliVersion": version,
                "account": account,
                "shaBefore": sha_before,
                "baseRef": base_ref,
                "systemPrompt": system_prompt,
                "systemPromptSources": sources,
                "resumedFrom": attempt.resumed_from,
            }),
        )?;
        if self.stop_requested(&attempt.id) {
            return Ok(None);
        }

        let raw_path = PathBuf::from(&attempt.raw_path);
        let stderr_path = dir.join(format!("{stem}.stderr.txt"));
        let child = process::spawn_step(
            command.command(&launch.step),
            &prompt_file,
            &raw_path,
            &stderr_path,
        )?;
        let pid = child.id() as i32;
        let pid_started = process::process_start(pid);
        store.patch_attempt(
            &attempt.id,
            &[
                ("pid", Value::Integer(i64::from(pid))),
                ("pid_started", text_value(pid_started)),
            ],
        )?;
        locked(&self.controls)
            .entry(attempt.id.clone())
            .or_default()
            .pid = Some(pid);
        store.set_state(&attempt.id, AttemptState::Running, None)?;
        self.changed(&attempt.run_id);
        if self.config.keep_awake {
            // Ends by itself when the step does.
            let _ = std::process::Command::new("/usr/bin/caffeinate")
                .args(["-i", "-w", &pid.to_string()])
                .spawn()
                .map(reap_in_background);
        }

        let attempt = store.attempt(&attempt.id)?;
        let is_command_step = launch.step.tools == ToolProfile::Build;
        Ok(Some(Monitor::new(
            &attempt,
            launch.project_id.clone(),
            is_command_step,
            Some(child),
            Tail { offset: 0 },
        )))
    }

    fn monitor(self: Arc<Self>, mut m: Monitor) {
        // Instant doesn't advance while the Mac sleeps, so sleep never looks like a stall.
        let mut last_progress = Instant::now();
        let mut last_stall_check = Instant::now();
        let mut stop_sent_at: Option<Instant> = None;
        let mut term_sent = false;
        loop {
            match m.tail.read_new(&m.raw_path) {
                Ok(lines) if !lines.is_empty() => {
                    last_progress = Instant::now();
                    self.handle_lines(&mut m, &lines);
                    self.clear_stall(&m);
                    self.changed(&m.run_id);
                }
                _ => {}
            }

            let exit = match m.child.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => Some(status.code()),
                    Ok(None) => None,
                    Err(_) => Some(None),
                },
                None => {
                    let alive = m.root_pid.is_some_and(|pid| unsafe { libc_kill_zero(pid) });
                    if alive { None } else { Some(None) }
                }
            };
            if let Some(code) = exit {
                self.finish(m, code, false);
                return;
            }

            if self.stop_requested(&m.attempt_id) || m.block.is_some() {
                let sent = *stop_sent_at.get_or_insert_with(|| {
                    if let Some(pid) = m.root_pid {
                        process::signal_group(pid, SIGINT);
                    }
                    Instant::now()
                });
                if !term_sent && sent.elapsed() > self.config.stop_grace {
                    term_sent = true;
                    if let Some(pid) = m.root_pid {
                        process::signal_group(pid, SIGTERM);
                    }
                }
                if sent.elapsed() > self.config.stop_grace * 2
                    && let Some(pid) = m.root_pid
                {
                    process::signal_group(pid, 9);
                }
            } else if last_stall_check.elapsed() >= self.config.tick * 4 {
                last_stall_check = Instant::now();
                self.check_stall(&m, last_progress.elapsed());
            }
            std::thread::sleep(self.config.tick);
        }
    }

    fn clear_stall(&self, m: &Monitor) {
        if let Ok(attempt) = self.store.attempt(&m.attempt_id)
            && attempt.state == AttemptState::Stalled.as_str()
        {
            let _ = self
                .store
                .set_state(&m.attempt_id, AttemptState::Running, None);
            let _ = self
                .store
                .append_event(&m.attempt_id, "stallCleared", &json!({}));
        }
    }

    /// Reports a stall. Never acts on it: the person decides what happens next.
    fn check_stall(&self, m: &Monitor, quiet: Duration) {
        if quiet < self.config.stall_after {
            return;
        }
        if locked(&self.controls)
            .get(&m.attempt_id)
            .and_then(|c| c.snooze_until)
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let Some(root) = m.root_pid else { return };
        let activity = process::activity(root);
        if activity.cpu_percent >= 5.0 && quiet < self.config.stall_hard_limit {
            return; // Quiet, but working. A long test suite looks like this.
        }
        let reason = Reason::Stalled {
            command: activity.command.clone(),
            pid: activity.pid,
            minutes: quiet.as_secs() / 60,
            cpu_percent: activity.cpu_percent,
        };
        let Ok(attempt) = self.store.attempt(&m.attempt_id) else {
            return;
        };
        let already = attempt.state == AttemptState::Stalled.as_str();
        let _ = self
            .store
            .set_state(&m.attempt_id, AttemptState::Stalled, Some(&reason));
        if !already {
            let _ = self
                .store
                .append_event(&m.attempt_id, "stalled", &json!({ "reason": reason }));
        }
        self.changed(&m.run_id);
    }

    fn handle_lines(&self, m: &mut Monitor, lines: &[String]) {
        let store = &self.store;
        for line in lines {
            for parsed in parse_line(line) {
                match parsed {
                    Parsed::Init {
                        session_id,
                        model,
                        tools,
                        api_key_source,
                        permission_mode,
                        cli_version,
                    } => {
                        let _ = store.patch_attempt(
                            &m.attempt_id,
                            &[
                                ("model_actual", text_value(model.clone())),
                                ("api_key_source", text_value(api_key_source.clone())),
                            ],
                        );
                        let _ = store.append_event(
                            &m.attempt_id,
                            "init",
                            &json!({ "sessionId": session_id, "model": model, "tools": tools, "apiKeySource": api_key_source, "permissionMode": permission_mode, "cliVersion": cli_version }),
                        );
                        // Keel only accepts the user's own login, and exactly the tools it asked
                        // for. Anything else means the launch didn't behave as tested.
                        if api_key_source
                            .as_deref()
                            .is_some_and(|source| source != "none")
                        {
                            m.block = Some(Reason::UnexpectedAuth {
                                source: api_key_source.unwrap_or_default(),
                            });
                        } else {
                            let mut found = tools.clone();
                            let mut expected = m.tools_expected.clone();
                            found.sort();
                            expected.sort();
                            if found != expected {
                                m.block = Some(Reason::UnexpectedTools { expected, found });
                            }
                        }
                    }
                    Parsed::AssistantText { text } => {
                        let _ = store.append_event(&m.attempt_id, "text", &json!({ "text": text }));
                    }
                    Parsed::ToolCall {
                        id,
                        name,
                        input,
                        parent,
                    } => {
                        let _ = store.append_event(
                            &m.attempt_id,
                            "toolCall",
                            &json!({ "id": id, "name": name, "input": input, "parent": parent }),
                        );
                    }
                    Parsed::ToolResult {
                        id,
                        is_error,
                        text,
                        exit_code,
                    } => {
                        let truncated = text.len() > EVENT_TEXT_LIMIT;
                        let kept: String = if truncated {
                            text.chars().take(EVENT_TEXT_LIMIT).collect()
                        } else {
                            text
                        };
                        let _ = store.append_event(&m.attempt_id, "toolResult", &json!({ "id": id, "isError": is_error, "text": kept, "truncated": truncated, "exitCode": exit_code }));
                    }
                    Parsed::ApiRetry { attempt, error } => {
                        let _ = store.append_event(
                            &m.attempt_id,
                            "apiRetry",
                            &json!({ "attempt": attempt, "error": error }),
                        );
                    }
                    Parsed::PermissionDenied { tool, detail } => {
                        let _ = store.append_event(
                            &m.attempt_id,
                            "permissionDenied",
                            &json!({ "tool": tool, "detail": detail }),
                        );
                    }
                    Parsed::RateLimit {
                        status,
                        resets_at,
                        kind,
                    } => {
                        if status.as_deref() == Some("rejected") {
                            m.block = Some(Reason::RateLimited { resets_at });
                        }
                        let _ = store.append_event(
                            &m.attempt_id,
                            "rateLimit",
                            &json!({ "status": status, "resetsAt": resets_at, "kind": kind }),
                        );
                    }
                    Parsed::Result(result) => {
                        let _ = store.append_event(
                            &m.attempt_id,
                            "result",
                            &json!({
                                "subtype": result.subtype, "isError": result.is_error, "text": result.text, "errors": result.errors,
                                "turns": result.turns, "durationMs": result.duration_ms, "costUsd": result.cost_usd,
                                "inputTokens": result.input_tokens, "outputTokens": result.output_tokens, "models": result.models,
                                "denials": result.denials, "structuredOutput": result.structured_output,
                            }),
                        );
                        m.result = Some(result);
                    }
                    Parsed::Other { .. } => {}
                }
            }
        }
        let _ = store.patch_attempt(
            &m.attempt_id,
            &[
                ("raw_offset", Value::Integer(m.tail.offset as i64)),
                ("last_activity_at", Value::Text(now())),
            ],
        );
    }

    /// Closes an attempt: keeps everything it left, then decides what state that means.
    fn finish(&self, mut m: Monitor, exit_code: Option<i32>, process_lost: bool) {
        if let Ok(lines) = m.tail.read_new(&m.raw_path)
            && !lines.is_empty()
        {
            self.handle_lines(&mut m, &lines);
        }
        let store = &self.store;
        let stop_requested = self.stop_requested(&m.attempt_id);

        let mut fields: Vec<(&'static str, Value)> = vec![("finished_at", Value::Text(now()))];
        let raw_sha = process::sha256_file(&m.raw_path).ok();
        fields.push(("raw_sha256", text_value(raw_sha.clone())));

        let mut files: Vec<String> = Vec::new();
        let mut sha_after = None;
        if let (Some(before), Some(bin)) = (&m.sha_before, locked(&self.tools).git.clone()) {
            match (Git { bin }).snapshot(&m.workspace, before, &m.step_id) {
                Ok(snapshot) => {
                    let diff_path = m.raw_path.with_extension("diff");
                    if process::create_private(&diff_path).is_ok()
                        && std::fs::write(&diff_path, &snapshot.diff).is_ok()
                    {
                        fields.push((
                            "diff_path",
                            Value::Text(diff_path.to_string_lossy().into_owned()),
                        ));
                    }
                    files = snapshot.files;
                    sha_after = Some(snapshot.sha.clone());
                    fields.push(("sha_after", Value::Text(snapshot.sha)));
                    fields.push(("files_changed", Value::Text(json!(files).to_string())));
                }
                Err(error) => {
                    let _ = store.append_event(
                        &m.attempt_id,
                        "snapshotFailed",
                        &json!({ "error": error.to_string() }),
                    );
                }
            }
        }

        if let Some(result) = &m.result {
            // A resumed session reports totals for the whole session, so count only what's new.
            let earlier = m
                .resumed_from
                .as_deref()
                .and_then(|id| store.attempt(id).ok());
            let delta_cost = |value: Option<f64>, before: Option<f64>| {
                value.map(|v| (v - before.unwrap_or(0.0)).max(0.0))
            };
            let delta = |value: Option<i64>, before: Option<i64>| {
                value.map(|v| (v - before.unwrap_or(0)).max(0))
            };
            let optional_real = |v: Option<f64>| v.map_or(Value::Null, Value::Real);
            let optional_int = |v: Option<i64>| v.map_or(Value::Null, Value::Integer);
            fields.push((
                "cost_usd",
                optional_real(delta_cost(
                    result.cost_usd,
                    earlier.as_ref().and_then(|a| a.cost_usd),
                )),
            ));
            fields.push((
                "input_tokens",
                optional_int(delta(
                    result.input_tokens,
                    earlier.as_ref().and_then(|a| a.input_tokens),
                )),
            ));
            fields.push((
                "output_tokens",
                optional_int(delta(
                    result.output_tokens,
                    earlier.as_ref().and_then(|a| a.output_tokens),
                )),
            ));
            fields.push(("turns", optional_int(result.turns)));
            fields.push(("duration_ms", optional_int(result.duration_ms)));
            fields.push(("result_text", text_value(result.text.clone())));
            if !result.models.is_empty() {
                fields.push(("model_actual", Value::Text(result.models.join(", "))));
            }
        }
        let _ = store.patch_attempt(&m.attempt_id, &fields);

        let (state, reason) = classify(&m, exit_code, stop_requested, process_lost);
        let _ = store.set_state(&m.attempt_id, state, reason.as_ref());
        let _ = store.append_event(
            &m.attempt_id,
            "finished",
            &json!({ "state": state.as_str(), "reason": reason, "exitCode": exit_code, "rawSha256": raw_sha, "shaAfter": sha_after, "files": files }),
        );
        self.release_slot(&m.attempt_id, &m.project_id, m.is_command_step);
        locked(&self.controls).remove(&m.attempt_id);
        self.changed(&m.run_id);
    }
}

/// What an attempt's ending means. `is_error` decides, not the subtype or the exit code:
/// M0 saw `subtype: success` on a rejected key, and exit 0 after SIGINT.
fn classify(
    m: &Monitor,
    exit_code: Option<i32>,
    stop_requested: bool,
    process_lost: bool,
) -> (AttemptState, Option<Reason>) {
    if let Some(block) = &m.block {
        return (AttemptState::Blocked, Some(block.clone()));
    }
    if stop_requested {
        return (AttemptState::Stopped, None);
    }
    match &m.result {
        Some(result) if !result.is_error => (AttemptState::Succeeded, None),
        Some(result) => {
            let detail = result
                .text
                .clone()
                .or_else(|| result.errors.first().cloned())
                .unwrap_or_default();
            let lower = detail.to_lowercase();
            if [
                "invalid api key",
                "authentication",
                "/login",
                "not logged in",
                "oauth token",
            ]
            .iter()
            .any(|needle| lower.contains(needle))
            {
                (
                    AttemptState::Blocked,
                    Some(Reason::SessionRejected { detail }),
                )
            } else if lower.contains("rate limit") || lower.contains("usage limit") {
                (
                    AttemptState::Blocked,
                    Some(Reason::RateLimited { resets_at: None }),
                )
            } else {
                (
                    AttemptState::Failed,
                    Some(Reason::ResultError {
                        subtype: result.subtype.clone(),
                        detail,
                    }),
                )
            }
        }
        None if process_lost || (m.child.is_none() && exit_code.is_none()) => {
            (AttemptState::Interrupted, Some(Reason::AgentLost))
        }
        None => (
            AttemptState::Failed,
            Some(Reason::ExitedWithoutResult { exit_code }),
        ),
    }
}

/// What every step is told, plus the project's own instructions, which `--restricted` skips.
fn system_prompt(
    workspace: &Path,
    language: &str,
    tools: ToolProfile,
) -> (String, serde_json::Value) {
    let mut text = String::from(
        "# Working inside Keel\n\n\
         You are running one step of a software workflow, in a private clone of the project.\n\
         There is no GitHub access and no credentials here. Do not try to push or open a pull request.\n\
         A person reviews what you produce.\n",
    );
    if tools == ToolProfile::Build {
        text.push_str(
            "Commit your work on the current branch before you finish.\n\
             Never run two test commands at the same time: they can share a database and block each other.\n",
        );
    }
    text.push_str(&format!(
        "\nEnd with a short message for the person, in {}: what you did, what changed, how you checked it, and anything they need to decide.\n",
        language_name(language),
    ));
    let mut sources = Vec::new();
    if let Ok(claude_md) = std::fs::read_to_string(workspace.join("CLAUDE.md")) {
        text.push_str("\n# Project instructions (CLAUDE.md)\n\n");
        text.push_str(&claude_md);
        sources.push(json!({ "source": "CLAUDE.md", "bytes": claude_md.len() }));
    }
    (text, json!(sources))
}

/// Without this, `Child` would stay a zombie until the app exits.
fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// `kill(pid, 0)`: whether a process we didn't start is still there.
unsafe fn libc_kill_zero(pid: i32) -> bool {
    process::signal_process(pid, 0)
}
