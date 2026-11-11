//! Runs one step and watches it until it ends.

use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, Reason, StepSpec, ToolProfile, WorkflowStep};
use keel_harness::auth::{self, AuthStatus};
use keel_harness::process::{self, SIGINT, SIGTERM, Tail};
use keel_harness::stream::ResultEvent;
use keel_harness::{Parsed, StepCommand, parse_line, settings};
use keel_store::rusqlite_value::Value;
use keel_store::{Attempt, now};
use serde_json::json;

use crate::git::{Git, RUN_CACHE};
use crate::services::{self, ServiceSet};
use crate::specs::{self, Verdict};
use crate::{
    Error, Inner, Result, Resume, Turn, brain, joint, locked, prompts, screenshots, skipped_tests,
    verification, workflows,
};

/// Tool output kept in the database. The raw file always has all of it.
const EVENT_TEXT_LIMIT: usize = 64 * 1024;

fn text_value(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
}

/// The running services, and the variables that point at them.
type StartedServices = (Option<ServiceSet>, Vec<(String, String)>);

pub(crate) struct Monitor {
    attempt_id: String,
    run_id: String,
    step_id: String,
    project_id: String,
    kind: String,
    holds_project: bool,
    counts_as_agent: bool,
    tools_expected: Vec<String>,
    raw_path: PathBuf,
    workspace: PathBuf,
    sha_before: Option<String>,
    resumed_from: Option<String>,
    root_pid: Option<i32>,
    child: Option<Child>,
    tail: Tail,
    /// A step that was asked for a structured answer and ends without it didn't do its job.
    requires_output: bool,
    role: String,
    /// Why this attempt must end blocked, once decided.
    block: Option<Reason>,
    result: Option<ResultEvent>,
    /// Stops the step's services when dropped.
    services: Option<ServiceSet>,
}

impl Monitor {
    fn new(attempt: &Attempt, project_id: String, child: Option<Child>, tail: Tail) -> Self {
        let tools = ToolProfile::parse(&attempt.tools);
        Self {
            attempt_id: attempt.id.clone(),
            run_id: attempt.run_id.clone(),
            step_id: attempt.step_id.clone(),
            project_id,
            kind: attempt.kind.clone(),
            holds_project: runs_commands(&attempt.kind, tools),
            counts_as_agent: attempt.kind == "agent",
            tools_expected: tools
                .map(|t| t.tools().iter().map(|x| (*x).to_owned()).collect())
                .unwrap_or_default(),
            raw_path: PathBuf::from(&attempt.raw_path),
            workspace: PathBuf::from(&attempt.workspace),
            sha_before: attempt.sha_before.clone(),
            resumed_from: attempt.resumed_from.clone(),
            root_pid: attempt.pid.map(|p| p as i32),
            child,
            tail,
            requires_output: attempt.role != "worker",
            role: attempt.role.clone(),
            block: None,
            result: None,
            services: None,
        }
    }
}

/// Whether one of the project's allowed domains is `host` or a subdomain of it.
fn allows_host(domains: &[String], host: &str) -> bool {
    domains.iter().any(|domain| {
        let domain = domain.trim().trim_start_matches("*.").to_ascii_lowercase();
        domain == host || domain.ends_with(&format!(".{host}"))
    })
}

/// Points the usual tools at a cache a sandboxed command can write to, since theirs live in the
/// home folder. Cargo and Go keep what they download in `tool_homes`: for setup that's the
/// project's own cache, which only setup writes, and for any other step it's the run's copy of it.
/// `domains` are those the step may reach, or `None` when it has the whole network (setup). With no
/// way to crates.io cargo is told to work offline, and Go has no proxy, so a missing dependency is
/// an error at once and not minutes of retries. A project that does allow them is left as it was.
fn cache_env(
    cache: &std::path::Path,
    tool_homes: &std::path::Path,
    domains: Option<&[String]>,
) -> Vec<(String, String)> {
    let homes = tool_homes.to_string_lossy().into_owned();
    let cache = cache.to_string_lossy().into_owned();
    let mut env = vec![
        ("XDG_CACHE_HOME".to_owned(), cache.clone()),
        ("UV_CACHE_DIR".to_owned(), format!("{cache}/uv")),
        ("UV_LINK_MODE".to_owned(), "copy".to_owned()),
        ("PIP_CACHE_DIR".to_owned(), format!("{cache}/pip")),
        ("npm_config_cache".to_owned(), format!("{cache}/npm")),
        ("CARGO_HOME".to_owned(), format!("{homes}/cargo")),
        ("GOPATH".to_owned(), format!("{homes}/go")),
        ("GOCACHE".to_owned(), format!("{cache}/go-build")),
        ("GOTOOLCHAIN".to_owned(), "local".to_owned()),
    ];
    if let Some(domains) = domains {
        if !allows_host(domains, "crates.io") {
            env.push(("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()));
        }
        if !allows_host(domains, "golang.org") {
            env.push(("GOPROXY".to_owned(), "off".to_owned()));
        }
    }
    env
}

/// Gives a run its own copy of what setup downloaded for cargo and Go, once. It's a clone on APFS,
/// which costs almost nothing, and a copy where that's not possible. It's never a link: whatever
/// an agent changes stays in the run and can't reach the project's cache or the next run.
fn copy_tool_caches(shared: &std::path::Path, run_cache: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(run_cache)?;
    for tool in ["cargo", "go"] {
        let (from, to) = (shared.join(tool), run_cache.join(tool));
        if to.exists() {
            continue;
        }
        if !from.is_dir() {
            std::fs::create_dir_all(&to)?;
            continue;
        }
        let copy = |flags: &str| {
            std::process::Command::new("/bin/cp")
                .arg(flags)
                .arg(&from)
                .arg(&to)
                .status()
                .is_ok_and(|status| status.success())
        };
        if !copy("-cR") {
            let _ = std::fs::remove_dir_all(&to);
            if !copy("-R") {
                let _ = std::fs::remove_dir_all(&to);
                return Err(Error::Io(std::io::Error::other(format!(
                    "could not copy {}",
                    from.display()
                ))));
            }
        }
    }
    Ok(())
}

fn runs_commands(kind: &str, tools: Option<ToolProfile>) -> bool {
    kind == "check" || kind == "setup" || tools == Some(ToolProfile::Build)
}

impl Inner {
    fn stop_requested(&self, attempt_id: &str) -> bool {
        locked(&self.controls)
            .get(attempt_id)
            .is_some_and(|c| c.stop_requested)
    }

    /// What a step waiting for the project's turn is told while `attempt` has it.
    fn waiting_for(&self, attempt: &Attempt) -> Reason {
        Reason::WaitingForProject {
            step: attempt.step_id.clone(),
            run_id: attempt.run_id.clone(),
            title: self
                .store
                .run(&attempt.run_id)
                .map(|run| run.title)
                .unwrap_or_default(),
        }
    }

    /// The project's turn, held by `attempt`.
    fn turn_of(&self, attempt: &Attempt) -> Turn {
        Turn {
            attempt_id: attempt.id.clone(),
            waiting: self.waiting_for(attempt),
        }
    }

    /// Waits for room. Agents are limited overall, and a project runs one step with commands at
    /// a time, because tests often share resources such as a database. A step waiting for the
    /// project says which one has the turn, and says it again when the turn passes on.
    fn acquire_slot(
        &self,
        attempt: &Attempt,
        project_id: &str,
        holds_project: bool,
        counts_as_agent: bool,
    ) -> bool {
        let (attempt_id, run_id) = (attempt.id.as_str(), attempt.run_id.as_str());
        let turn = holds_project.then(|| self.turn_of(attempt));
        let mut last_reason: Option<Reason> = None;
        let mut slots = locked(&self.slots);
        loop {
            if self.stop_requested(attempt_id) {
                return false;
            }
            let reason = if holds_project && let Some(taken) = slots.command_steps.get(project_id) {
                Some(taken.waiting.clone())
            } else if counts_as_agent
                && slots.active.len() >= self.max_agents.load(Ordering::SeqCst)
            {
                Some(Reason::WaitingForCapacity)
            } else {
                None
            };
            let Some(reason) = reason else {
                if counts_as_agent {
                    slots.active.insert(attempt_id.to_owned());
                }
                if let Some(turn) = turn {
                    slots.command_steps.insert(project_id.to_owned(), turn);
                }
                return true;
            };
            if last_reason.as_ref() != Some(&reason) {
                let _ = self
                    .store
                    .set_state(attempt_id, AttemptState::Queued, Some(&reason));
                self.changed(run_id);
                last_reason = Some(reason);
            }
            slots = self
                .slot_freed
                .wait_timeout(slots, Duration::from_millis(200))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    fn release_slot(
        &self,
        attempt_id: &str,
        project_id: &str,
        holds_project: bool,
        counts_as_agent: bool,
    ) {
        let mut slots = locked(&self.slots);
        if counts_as_agent {
            slots.active.remove(attempt_id);
        }
        // A step recovered without the turn, because another had it, mustn't take it away.
        if holds_project
            && slots
                .command_steps
                .get(project_id)
                .is_some_and(|turn| turn.attempt_id == attempt_id)
        {
            slots.command_steps.remove(project_id);
        }
        drop(slots);
        self.slot_freed.notify_all();
    }

    pub(crate) fn end_early(
        self: &Arc<Self>,
        attempt_id: &str,
        run_id: &str,
        state: AttemptState,
        reason: Option<Reason>,
    ) {
        let _ = self.store.set_state(attempt_id, state, reason.as_ref());
        let _ = self
            .store
            .patch_attempt(attempt_id, &[("finished_at", Value::Text(now()))]);
        let _ = self.store.append_event(
            attempt_id,
            "finished",
            &json!({ "state": state.as_str(), "reason": reason }),
        );
        locked(&self.controls).remove(attempt_id);
        self.close_failed_trial(run_id, state);
        self.changed(run_id);
    }

    pub(crate) fn execute(
        self: Arc<Self>,
        attempt_id: String,
        project_id: String,
        step: WorkflowStep,
        resume: Option<String>,
    ) {
        let Ok(attempt) = self.store.attempt(&attempt_id) else {
            return;
        };
        let run_id = attempt.run_id.clone();
        let holds_project = runs_commands(&attempt.kind, ToolProfile::parse(&attempt.tools));
        let counts_as_agent = attempt.kind == "agent";
        if !self.acquire_slot(&attempt, &project_id, holds_project, counts_as_agent) {
            self.end_early(&attempt_id, &run_id, AttemptState::Stopped, None);
            return;
        }
        let started = match &step {
            WorkflowStep::Agent(spec) => self.spawn_agent(&attempt, &project_id, spec, resume),
            WorkflowStep::Setup { run, .. } => self.spawn_command(&attempt, &project_id, run, true),
            WorkflowStep::Check { run, .. } => {
                self.spawn_command(&attempt, &project_id, run, false)
            }
            WorkflowStep::Deliver { .. } => {
                self.release_slot(&attempt_id, &project_id, holds_project, counts_as_agent);
                self.deliver(&attempt);
                return;
            }
            WorkflowStep::Approval { .. } | WorkflowStep::Parallel { .. } => Ok(None),
        };
        match started {
            Ok(Some(monitor)) => self.monitor(monitor),
            Ok(None) => {
                self.release_slot(&attempt_id, &project_id, holds_project, counts_as_agent);
                self.end_early(&attempt_id, &run_id, AttemptState::Stopped, None);
            }
            Err(error) => {
                self.release_slot(&attempt_id, &project_id, holds_project, counts_as_agent);
                let (state, reason) = match error {
                    Error::Blocked(reason) => (AttemptState::Blocked, reason),
                    other => (
                        AttemptState::Failed,
                        Reason::PrepareFailed {
                            detail: other.to_string(),
                        },
                    ),
                };
                self.end_early(&attempt_id, &run_id, state, Some(reason));
            }
        }
    }

    /// What setup downloads for a project, shared by its runs. Only setup writes there.
    fn shared_cache(&self, project_id: &str) -> PathBuf {
        self.mirror(project_id).with_file_name("cache")
    }

    /// Makes sure the run has its private clone, and returns the commit the step starts from.
    fn prepare_workspace(&self, attempt: &Attempt, project_id: &str) -> Result<(Git, String)> {
        let tools = locked(&self.tools).clone();
        let git = Git::new(
            tools
                .git
                .clone()
                .ok_or(Error::Blocked(Reason::GitMissing))?,
        );
        let workspace = PathBuf::from(&attempt.workspace);
        // A workflow can open with a group, and those steps must not all create it at once.
        let _creating = locked(&self.creating_workspace);
        if !workspace.join(".git").exists() {
            let project = self.store.project(project_id)?;
            let mirror = self.mirror(project_id);
            let source = PathBuf::from(&project.path);
            let base = match (
                &project.github_account,
                &project.github_repo,
                tools.gh.as_ref(),
            ) {
                (Some(account), Some(repo), Some(_)) => {
                    self.update_mirror_from_github(&git, &mirror, account, repo)?
                }
                _ => git.update_mirror_from_local(&source, &mirror)?,
            };
            let identity = git.identity(&source);
            let short: String = attempt.run_id.chars().take(8).collect();
            let branch = format!("keel/{short}");
            git.create_workspace(&mirror, &workspace, &branch, &identity)?;
            self.store.patch_run(
                &attempt.run_id,
                &[
                    ("base_ref", Value::Text(base)),
                    ("branch", Value::Text(branch)),
                ],
            )?;
        }
        let sha = git.head(&workspace)?;
        self.store
            .patch_attempt(&attempt.id, &[("sha_before", Value::Text(sha.clone()))])?;
        Ok((git, sha))
    }

    /// Starts the project's services for a step that runs commands, and works out the variables
    /// that point at them. All of it goes in the audit log.
    fn start_services(
        &self,
        attempt: &Attempt,
        project: &keel_store::Project,
        path_env: &str,
    ) -> Result<StartedServices> {
        if project.services.is_empty() && project.env.is_empty() {
            return Ok((None, Vec::new()));
        }
        let root = services::root_for(&project.id);
        let env = services::resolve_env(&project.env, &project.services, &root);
        let set = ServiceSet::start(
            &project.services,
            &root,
            &self.run_dir(&attempt.run_id),
            path_env,
        )
        .map_err(Error::Blocked)?;
        self.store.append_event(
            &attempt.id,
            "services",
            &json!({
                "services": project.services,
                "sockets": project.services.iter().map(|s| services::socket_path(&root, s)).collect::<Vec<_>>(),
                "env": env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
            }),
        )?;
        Ok((Some(set), env))
    }

    fn spawn_agent(
        &self,
        attempt: &Attempt,
        project_id: &str,
        spec: &StepSpec,
        resume: Option<String>,
    ) -> Result<Option<Monitor>> {
        let store = &self.store;
        store.set_state(&attempt.id, AttemptState::Preparing, None)?;
        self.changed(&attempt.run_id);

        let tools = locked(&self.tools).clone();
        let claude = tools
            .claude
            .clone()
            .ok_or(Error::Blocked(Reason::ClaudeMissing))?;
        let (_git, sha_before) = self.prepare_workspace(attempt, project_id)?;
        let workspace = PathBuf::from(&attempt.workspace);
        let dir = self.run_dir(&attempt.run_id);
        // The other projects of a change across several, which it reads and can't change. Only
        // what its settings already close to changes, since `--add-dir` opens a folder to writes.
        let (copies, joint) = if joint::reads_others(&spec.role) {
            let wanted: Vec<_> = self
                .read_copies(&attempt.run_id)?
                .into_iter()
                .filter(|copy| {
                    settings::is_read_only(&attempt.settings, &copy.path.to_string_lossy())
                })
                .collect();
            let copies = self.prepare_read_copies(attempt, &wanted)?;
            let joint = self.joint_text(&attempt.run_id, &copies)?;
            (copies, joint)
        } else {
            (Vec::new(), String::new())
        };

        let stem = format!("{}-{}", attempt.step_id, attempt.number);
        let settings_file = dir.join(format!("{stem}.settings.json"));
        std::fs::write(&settings_file, attempt.settings.to_string())?;
        // A map should come from the code, not from an earlier map.
        let brain = if spec.role == keel_core::StepRole::Mapper {
            brain::Block::default()
        } else {
            self.brain_block(project_id)?
        };
        let (system_prompt, sources) =
            prompts::system_prompt(&workspace, &attempt.language, spec.tools, &brain, &joint);
        let system_prompt_file = dir.join(format!("{stem}.system.md"));
        std::fs::write(&system_prompt_file, &system_prompt)?;
        let prompt_file = dir.join(format!("{stem}.prompt.txt"));
        std::fs::write(&prompt_file, &attempt.prompt)?;

        let command = StepCommand {
            claude: claude.clone(),
            path_env: tools.path_env.clone(),
            workspace: workspace.clone(),
            settings_file,
            system_prompt_file,
            session_id: attempt.session_id.clone(),
            resume,
            json_schema: if spec.role == keel_core::StepRole::Specifier
                && self.agreement(&attempt.run_id)?.is_some()
            {
                Some(specs::plan_schema().to_owned())
            } else {
                workflows::schema_for(&spec.role).map(str::to_owned)
            },
            add_dirs: copies
                .iter()
                .map(|ready| ready.copy.path.clone())
                .chain(Some(self.change_dir(&attempt.run_id)).filter(|dir| {
                    dir.is_dir()
                        && settings::is_read_only(&attempt.settings, &dir.to_string_lossy())
                }))
                .collect(),
        };
        let args = command.args(spec);
        let version = auth::read_version(&claude, &tools.path_env);
        let account = AuthStatus::read(&claude, &tools.path_env).and_then(|a| a.account_label());
        store.patch_attempt(
            &attempt.id,
            &[
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
        // Only steps that can run commands get services, or a folder for screenshots. A read-only
        // step has no way to use them.
        let can_write = spec.tools == ToolProfile::Build;
        let (service_set, service_env, project) = if can_write {
            screenshots::empty(&workspace, &dir)?;
            let project = store.project(project_id)?;
            let (set, env) = self.start_services(attempt, &project, &tools.path_env)?;
            (set, env, Some(project))
        } else {
            (None, Vec::new(), None)
        };
        let mut process_command = command.command(spec);
        if let Some(project) = project {
            let run_cache = workspace.join(RUN_CACHE);
            copy_tool_caches(&self.shared_cache(project_id), &run_cache)?;
            process_command.envs(cache_env(
                &run_cache,
                &run_cache,
                Some(&project.allowed_domains),
            ));
        }
        process_command.envs(service_env);
        // After the project's own variables, as for a check, so the folder is always Keel's.
        if can_write {
            process_command.env(screenshots::VARIABLE, screenshots::folder(&workspace));
        }
        let child = process::spawn_step(process_command, &prompt_file, &raw_path, &stderr_path)?;
        let mut monitor = self.track_process(attempt, child, project_id)?;
        monitor.services = service_set;
        Ok(Some(monitor))
    }

    /// A check is Keel running a command, with no model. It's sandboxed like an agent's commands,
    /// because tests and Makefiles are files an agent may have just edited.
    fn spawn_command(
        &self,
        attempt: &Attempt,
        project_id: &str,
        command_text: &str,
        is_setup: bool,
    ) -> Result<Option<Monitor>> {
        let store = &self.store;
        store.set_state(&attempt.id, AttemptState::Preparing, None)?;
        self.changed(&attempt.run_id);
        let (_git, sha_before) = self.prepare_workspace(attempt, project_id)?;
        let project = store.project(project_id)?;
        let tools = locked(&self.tools).clone();
        let workspace = PathBuf::from(&attempt.workspace);
        let dir = self.run_dir(&attempt.run_id);
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let sockets = services::socket_dirs(&services::root_for(project_id), &project.services);
        // Setup fills a cache shared by the project's runs. Everything after it gets one of its
        // own, inside the private copy, so nothing an agent wrote can reach the next run.
        let shared_cache = self.shared_cache(project_id);
        let (profile, cache) = if is_setup {
            std::fs::create_dir_all(&shared_cache)?;
            (
                settings::setup_profile(
                    &workspace,
                    &home,
                    &project.allowed_read,
                    &sockets,
                    &shared_cache,
                ),
                shared_cache.clone(),
            )
        } else {
            (
                settings::check_profile(&workspace, &home, &project.allowed_read, &sockets),
                workspace.join(RUN_CACHE),
            )
        };
        let stem = format!("{}-{}", attempt.step_id, attempt.number);
        let prompt_file = dir.join(format!("{stem}.stdin"));
        let raw_path = PathBuf::from(&attempt.raw_path);
        let stderr_path = dir.join(format!("{stem}.stderr.txt"));
        let profile = settings::with_own_output(profile, &[&prompt_file, &raw_path, &stderr_path]);

        store.append_event(
            &attempt.id,
            "started",
            &json!({ "command": command_text, "shaBefore": sha_before, "profile": profile }),
        )?;
        if self.stop_requested(&attempt.id) {
            return Ok(None);
        }
        std::fs::write(&prompt_file, "")?;
        let mut command = std::process::Command::new("/usr/bin/sandbox-exec");
        command
            .args(["-p", &profile, "/bin/sh", "-c", command_text])
            .current_dir(&workspace)
            .env_clear()
            .envs(
                keel_harness::clean_env(&tools.path_env)
                    .into_iter()
                    .filter(|(name, _)| !name.starts_with("CLAUDE_") && !name.starts_with("BASH_")),
            );
        screenshots::empty(&workspace, &dir)?;
        let (service_set, service_env) = self.start_services(attempt, &project, &tools.path_env)?;
        // Setup writes to the project's cache. Any other step works on the run's own copy of it.
        let cache_vars = if is_setup {
            cache_env(&cache, &cache, None)
        } else {
            copy_tool_caches(&shared_cache, &cache)?;
            cache_env(&cache, &cache, Some(&project.allowed_domains))
        };
        // After the project's own variables, as for an agent, so the folder is always Keel's.
        command
            .envs(cache_vars)
            .envs(service_env)
            .env(screenshots::VARIABLE, screenshots::folder(&workspace));
        let child = process::spawn_step(command, &prompt_file, &raw_path, &stderr_path)?;
        let mut monitor = self.track_process(attempt, child, project_id)?;
        monitor.services = service_set;
        Ok(Some(monitor))
    }

    fn track_process(&self, attempt: &Attempt, child: Child, project_id: &str) -> Result<Monitor> {
        let store = &self.store;
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
                .map(|mut c| {
                    std::thread::spawn(move || {
                        let _ = c.wait();
                    })
                });
        }
        let attempt = store.attempt(&attempt.id)?;
        Ok(Monitor::new(
            &attempt,
            project_id.to_owned(),
            Some(child),
            Tail { offset: 0 },
        ))
    }

    pub(crate) fn recover_attempt(self: &Arc<Self>, attempt: &Attempt) -> Result<()> {
        let alive = match (attempt.pid, &attempt.pid_started) {
            (Some(pid), Some(started)) => process::is_same_process(pid as i32, started),
            _ => false,
        };
        self.store
            .append_event(&attempt.id, "recovered", &json!({ "processAlive": alive }))?;
        if attempt.kind == "deliver" {
            self.end_early(
                &attempt.id,
                &attempt.run_id,
                AttemptState::Interrupted,
                Some(Reason::AgentLost),
            );
            return Ok(());
        }
        let project_id = self.store.project_of_run(&attempt.run_id)?;
        locked(&self.controls).insert(
            attempt.id.clone(),
            crate::Control {
                pid: attempt.pid.map(|p| p as i32),
                ..Default::default()
            },
        );
        let context = Monitor::new(
            attempt,
            project_id.clone(),
            None,
            Tail {
                offset: attempt.raw_offset as u64,
            },
        );
        {
            let turn = context.holds_project.then(|| self.turn_of(attempt));
            let mut slots = locked(&self.slots);
            if context.counts_as_agent {
                slots.active.insert(attempt.id.clone());
            }
            // A step still running from before keeps the turn. Steps whose process is gone end
            // right here, one by one, so one already taken belongs to a step that's alive.
            if let Some(turn) = turn {
                slots.command_steps.entry(project_id).or_insert(turn);
            }
        }
        if alive {
            let mut context = context;
            if context.holds_project {
                // The bridges died with the app. The step may still need them.
                let project = self.store.project(&context.project_id)?;
                let path_env = locked(&self.tools).path_env.clone();
                match self.start_services(attempt, &project, &path_env) {
                    Ok((set, _)) => context.services = set,
                    Err(error) => {
                        let _ = self.store.append_event(
                            &attempt.id,
                            "servicesLost",
                            &json!({ "error": error.to_string() }),
                        );
                    }
                }
            }
            let inner = self.clone();
            std::thread::spawn(move || inner.monitor(context));
        } else {
            self.finish(context, None, true);
        }
        Ok(())
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
                    let progressed = self.handle_lines(&mut m, &lines);
                    if progressed {
                        last_progress = Instant::now();
                        self.clear_stall(&m);
                    }
                    self.changed(&m.run_id);
                }
                _ => {}
            }

            let exit = match m.child.as_mut() {
                Some(child) => {
                    let pid = child.id() as i32;
                    match process::has_ended(pid) {
                        // Until it's collected its group is only the step's, so whatever the step
                        // left running ends with it, and can't change the copy once it's over.
                        // Services have groups of their own, and so does every other step.
                        Ok(true) => {
                            process::signal_group(pid, process::SIGKILL);
                            Some(child.wait().ok().and_then(|status| status.code()))
                        }
                        Ok(false) => None,
                        Err(_) => match child.try_wait() {
                            Ok(Some(status)) => Some(status.code()),
                            Ok(None) => None,
                            Err(_) => Some(None),
                        },
                    }
                }
                // Not Keel's child: after a restart, its pid may already be someone else's, so
                // its group is left alone.
                None => {
                    let alive = m
                        .root_pid
                        .is_some_and(|pid| process::signal_process(pid, 0));
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
        let snoozed = |controls: &std::collections::HashMap<String, crate::Control>| {
            controls
                .get(&m.attempt_id)
                .and_then(|c| c.snooze_until)
                .is_some_and(|until| Instant::now() < until)
        };
        if snoozed(&locked(&self.controls)) {
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
        let already = {
            // Looking at the process takes a while, and a person may have said to wait meanwhile.
            // Holding this until the state is written means their answer can't be undone.
            let controls = locked(&self.controls);
            if snoozed(&controls) {
                return;
            }
            let Ok(attempt) = self.store.attempt(&m.attempt_id) else {
                return;
            };
            let already = attempt.state == AttemptState::Stalled.as_str();
            let _ = self
                .store
                .set_state(&m.attempt_id, AttemptState::Stalled, Some(&reason));
            already
        };
        if !already {
            let _ = self
                .store
                .append_event(&m.attempt_id, "stalled", &json!({ "reason": reason }));
        }
        self.changed(&m.run_id);
    }

    /// Returns whether the agent produced something. Claude Code's `tool_progress` heartbeats
    /// arrive every 30 s while a command runs, so they are read and saved but are not progress.
    /// Without this, a hung command could never be reported as a stall.
    fn handle_lines(&self, m: &mut Monitor, lines: &[String]) -> bool {
        let store = &self.store;
        let mut progressed = m.kind != "agent";
        if m.kind == "agent" {
            for line in lines {
                let parsed = parse_line(line);
                let heartbeat = !parsed.is_empty()
                    && parsed
                        .iter()
                        .all(|p| matches!(p, Parsed::Other { kind } if kind == "tool_progress"));
                progressed |= !heartbeat;
                for parsed in parsed {
                    self.handle_parsed(m, parsed);
                }
            }
        }
        let mut fields = vec![("raw_offset", Value::Integer(m.tail.offset as i64))];
        if progressed {
            fields.push(("last_activity_at", Value::Text(now())));
        }
        let _ = store.patch_attempt(&m.attempt_id, &fields);
        progressed
    }

    fn handle_parsed(&self, m: &mut Monitor, parsed: Parsed) {
        let store = &self.store;
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
                    // Claude Code adds this one itself when asked for a structured answer.
                    if m.requires_output {
                        expected.push("StructuredOutput".to_owned());
                    }
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

    /// Closes an attempt: keeps everything it left, decides what state that means, and moves the
    /// run on if it can.
    pub(crate) fn finish(
        self: &Arc<Self>,
        mut m: Monitor,
        exit_code: Option<i32>,
        process_lost: bool,
    ) {
        // First of all, so that "finished" means nothing is left running, and so the next step
        // can open the same sockets.
        drop(m.services.take());
        if let Ok(lines) = m.tail.read_new(&m.raw_path)
            && !lines.is_empty()
        {
            self.handle_lines(&mut m, &lines);
        }
        let store = &self.store;
        let stop_requested = self.stop_requested(&m.attempt_id);
        if m.holds_project {
            self.take_screenshots(&m);
        }

        let mut fields: Vec<(&'static str, Value)> = vec![("finished_at", Value::Text(now()))];
        let raw_sha = process::sha256_file(&m.raw_path).ok();
        fields.push(("raw_sha256", text_value(raw_sha.clone())));
        if let Some(code) = exit_code {
            fields.push(("exit_code", Value::Integer(i64::from(code))));
        }

        let mut files: Vec<String> = Vec::new();
        let mut sha_after = None;
        let mut snapshot_failed = false;
        let git = locked(&self.tools).git.clone().map(Git::new);
        if m.kind == "agent"
            && let (Some(before), Some(git)) = (&m.sha_before, &git)
        {
            match git.snapshot(&m.workspace, before, &m.step_id) {
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
                    let base = store
                        .attempts_of_run(&m.run_id)
                        .ok()
                        .and_then(|all| all.iter().find_map(|a| a.sha_before.clone()));
                    if let Some(net) = base.and_then(|b| git.files_since(&m.workspace, &b).ok()) {
                        fields.push(("run_files", Value::Text(json!(net).to_string())));
                    }
                }
                Err(error) => {
                    snapshot_failed = true;
                    let _ = store.append_event(
                        &m.attempt_id,
                        "snapshotFailed",
                        &json!({ "error": error.to_string() }),
                    );
                }
            }
        }

        if m.kind == "check" || m.kind == "setup" {
            let stderr_path = m.raw_path.with_file_name(format!(
                "{}.stderr.txt",
                m.raw_path
                    .file_stem()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default()
            ));
            let output = std::fs::read(&m.raw_path).unwrap_or_default();
            let stderr = std::fs::read(&stderr_path).unwrap_or_default();
            // The exit code says whether it passed. What its test tools printed says whether it
            // skipped tests on the way.
            if m.kind == "check" {
                let skipped = skipped_tests::in_files(&[&m.raw_path, &stderr_path]);
                fields.push((
                    "skipped_tests",
                    skipped.map_or(Value::Null, |count| Value::Integer(i64::from(count))),
                ));
            }
            let tail = |bytes: &[u8]| {
                String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(EVENT_TEXT_LIMIT)..])
                    .into_owned()
            };
            let dirty = git.as_ref().is_some_and(|g| g.is_dirty(&m.workspace));
            let _ = store.append_event(
                &m.attempt_id,
                "checkResult",
                &json!({ "kind": m.kind, "exitCode": exit_code, "stdout": tail(&output), "stderr": tail(&stderr), "truncated": output.len() > EVENT_TEXT_LIMIT, "leftWorkspaceDirty": dirty }),
            );
        }

        if let Some(result) = &m.result {
            // A resumed session reports totals for the whole session, so count only what's new. Each
            // attempt before this one only kept what it added, so it takes all of them.
            let earlier = self.earlier_in_session(m.resumed_from.as_deref());
            let delta_cost = |value: Option<f64>, before: f64| value.map(|v| (v - before).max(0.0));
            let delta = |value: Option<i64>, before: i64| value.map(|v| (v - before).max(0));
            let optional_real = |v: Option<f64>| v.map_or(Value::Null, Value::Real);
            let optional_int = |v: Option<i64>| v.map_or(Value::Null, Value::Integer);
            fields.push((
                "cost_usd",
                optional_real(delta_cost(
                    result.cost_usd,
                    earlier.iter().filter_map(|a| a.cost_usd).sum(),
                )),
            ));
            fields.push((
                "input_tokens",
                optional_int(delta(
                    result.input_tokens,
                    earlier.iter().filter_map(|a| a.input_tokens).sum(),
                )),
            ));
            fields.push((
                "output_tokens",
                optional_int(delta(
                    result.output_tokens,
                    earlier.iter().filter_map(|a| a.output_tokens).sum(),
                )),
            ));
            fields.push(("turns", optional_int(result.turns)));
            fields.push(("duration_ms", optional_int(result.duration_ms)));
            fields.push(("result_text", text_value(result.text.clone())));
            if let Some(output) = &result.structured_output {
                fields.push(("output", Value::Text(output.to_string())));
            }
            if !result.models.is_empty() {
                fields.push(("model_actual", Value::Text(result.models.join(", "))));
            }
        }
        let _ = store.patch_attempt(&m.attempt_id, &fields);

        let (mut state, mut reason) = classify(&m, exit_code, stop_requested, process_lost);
        // If Keel can't tell what the step changed, it can't say the step went well.
        if snapshot_failed && state == AttemptState::Succeeded {
            state = AttemptState::Failed;
            reason = Some(Reason::SnapshotFailed);
        }
        // A specifier's result only counts if it keeps to its limits, and it may have questions.
        let mut correction = None;
        if state == AttemptState::Succeeded && m.role == "specifier" {
            match self.judge_spec(&m) {
                Verdict::Ready(output) => self.keep_output(&m, &output),
                Verdict::Ask { output, questions } => {
                    self.keep_output(&m, &output);
                    state = AttemptState::WaitingAnswers;
                    let _ = store.append_event(
                        &m.attempt_id,
                        "questionsAsked",
                        &json!({ "questions": questions }),
                    );
                }
                Verdict::TookRecommended {
                    output,
                    settled,
                    rounds,
                } => {
                    self.keep_output(&m, &output);
                    let _ = store.append_event(
                        &m.attempt_id,
                        "questionsTaken",
                        &json!({ "settled": settled, "rounds": rounds }),
                    );
                }
                Verdict::Correct(problems) => {
                    state = AttemptState::Failed;
                    reason = Some(Reason::SpecNotUsable {
                        problems: problems.clone(),
                    });
                    correction = Some(problems);
                }
                Verdict::Stop(problems) => {
                    state = AttemptState::Blocked;
                    reason = Some(Reason::SpecNotUsable { problems });
                }
                Verdict::KeptAsking { output, rounds } => {
                    self.keep_output(&m, &output);
                    state = AttemptState::Blocked;
                    reason = Some(Reason::KeptAsking { rounds });
                }
            }
        }
        // Anything that makes it succeed comes through here, a resumed or retried attempt included,
        // and what can't be corrected has to reach a person rather than pass.
        if state == AttemptState::Succeeded && m.role == "verifier" && !self.correct_verifier(&m) {
            state = AttemptState::Failed;
            reason = Some(Reason::NoStructuredOutput);
        }
        if state == AttemptState::Succeeded && m.role == "reviewer" {
            self.correct_review(&m);
        }
        let _ = store.set_state(&m.attempt_id, state, reason.as_ref());
        let _ = store.append_event(
            &m.attempt_id,
            "finished",
            &json!({ "state": state.as_str(), "reason": reason, "exitCode": exit_code, "rawSha256": raw_sha, "shaAfter": sha_after, "files": files }),
        );
        self.release_slot(
            &m.attempt_id,
            &m.project_id,
            m.holds_project,
            m.counts_as_agent,
        );
        locked(&self.controls).remove(&m.attempt_id);
        self.close_failed_trial(&m.run_id, state);
        // Going straight on to a fix: that it needed one isn't news for the board.
        if correction.is_none() {
            self.changed(&m.run_id);
        }
        if state == AttemptState::Succeeded
            && let Ok(attempt) = store.attempt(&m.attempt_id)
            && let Err(error) = self.record_brain_output(&attempt)
        {
            let _ = store.append_event(
                &m.attempt_id,
                "brainFailed",
                &json!({ "error": error.to_string() }),
            );
        }
        if let Some(problems) = correction {
            if let Err(error) = self.ask_for_correction(&m.attempt_id, &problems) {
                // Nobody was told this finished, because the correction was meant to follow it.
                let _ = store.append_event(
                    &m.attempt_id,
                    "correctionFailed",
                    &json!({ "error": error.to_string() }),
                );
                self.changed(&m.run_id);
            }
            return;
        }
        if state == AttemptState::Succeeded || m.role == "learner" {
            let _ = self.advance(&m.run_id);
        }
    }

    /// Keeps what the step saved for its approval, and says so in its events, which is what tells
    /// later which step each picture came from.
    fn take_screenshots(&self, m: &Monitor) {
        let run_dir = self.run_dir(&m.run_id);
        let (kind, data) = match screenshots::take(&m.workspace, &run_dir, &m.attempt_id) {
            Ok(None) => return,
            Ok(Some(taken)) => (screenshots::EVENT, json!(taken)),
            Err(error) => ("screenshotsFailed", json!({ "error": error.to_string() })),
        };
        let _ = self.store.append_event(&m.attempt_id, kind, &data);
    }

    /// The attempts before this one in its session, nearest first.
    fn earlier_in_session(&self, resumed_from: Option<&str>) -> Vec<Attempt> {
        let mut found: Vec<Attempt> = Vec::new();
        let mut next = resumed_from.map(str::to_owned);
        while let Some(id) = next.take() {
            // It can't come round, but a bad row shouldn't hang the app.
            if found.iter().any(|attempt| attempt.id == id) {
                break;
            }
            let Ok(attempt) = self.store.attempt(&id) else {
                break;
            };
            next.clone_from(&attempt.resumed_from);
            found.push(attempt);
        }
        found
    }

    fn judge_spec(&self, m: &Monitor) -> Verdict {
        let attempts = self.store.attempts_of_run(&m.run_id).unwrap_or_default();
        let this = attempts.iter().find(|a| a.id == m.attempt_id);
        let fixing_a_limit = m.resumed_from.as_deref().is_some_and(|from| {
            attempts.iter().any(|a| {
                a.id == from
                    && a.reason.as_ref().and_then(|r| r.get("code"))
                        == Some(&json!("specNotUsable"))
            })
        });
        // What a person has answered. Fixing a limit isn't a round of questions.
        let rounds_answered = attempts
            .iter()
            .filter(|a| {
                a.step_id == m.step_id
                    && this.is_some_and(|this| a.round == this.round)
                    && !specs::answered_on(a).is_empty()
            })
            .count();
        let output = this.and_then(|attempt| attempt.output.as_ref());
        match self.agreement(&m.run_id).ok().flatten() {
            Some(agreement) => {
                specs::judge_agreement(output, fixing_a_limit, rounds_answered, &agreement.names())
            }
            None => specs::judge(output, fixing_a_limit, rounds_answered),
        }
    }

    /// What it answered, without anything an agent added beyond the structure. The whole answer as
    /// it came stays on the record, in the `result` event.
    fn keep_output(&self, m: &Monitor, output: &serde_json::Value) {
        let _ = self.store.patch_attempt(
            &m.attempt_id,
            &[("output", Value::Text(output.to_string()))],
        );
    }

    /// Replaces what a verifier answered with what the engine can stand behind. Whether a check
    /// passed comes from what Keel ran in this attempt's round, and the list from the latest spec.
    /// False if there's no answer to correct, or nothing to correct it against.
    fn correct_verifier(&self, m: &Monitor) -> bool {
        let Ok(attempts) = self.store.attempts_of_run(&m.run_id) else {
            return false;
        };
        let Some(this) = attempts.iter().find(|a| a.id == m.attempt_id) else {
            return false;
        };
        let Ok(run) = self.store.run(&m.run_id) else {
            return false;
        };
        let steps: Vec<WorkflowStep> = serde_json::from_value(run.workflow).unwrap_or_default();
        let check = verification::checks_in_round(&steps, &attempts, this.round);
        let corrected = workflows::specifying_attempt(&attempts)
            .and_then(specs::of_attempt)
            .zip(this.output.as_ref())
            .and_then(|(spec, output)| verification::correct(output, &spec, check.passed()));
        let Some(corrected) = corrected else {
            return false;
        };
        self.keep_output(m, &json!(corrected.verification));
        let changed: Vec<_> = corrected
            .verification
            .criteria
            .iter()
            .filter(|checked| checked.corrected.is_some())
            .collect();
        if !changed.is_empty() || !corrected.dropped.is_empty() {
            let _ = self.store.append_event(
                &m.attempt_id,
                "verificationCorrected",
                &json!({ "corrected": changed, "dropped": corrected.dropped, "check": check }),
            );
        }
        true
    }

    /// A review that couldn't read part of the change, or that says it breaks a criterion or a
    /// limit, isn't an approval of it, and the record says why. What it answered as it came stays
    /// in the `result` event.
    fn correct_review(&self, m: &Monitor) {
        let Ok(attempts) = self.store.attempts_of_run(&m.run_id) else {
            return;
        };
        let Some((kept, why)) = attempts
            .iter()
            .find(|a| a.id == m.attempt_id)
            .and_then(|attempt| attempt.output.as_ref())
            .and_then(|output| {
                workflows::review_as_kept(output, &workflows::breakable_ids(&attempts))
            })
        else {
            return;
        };
        self.keep_output(m, &kept);
        if !why.unread.is_empty() {
            let _ = self.store.append_event(
                &m.attempt_id,
                "reviewIncomplete",
                &json!({ "unread": why.unread }),
            );
        }
        if !why.breaks.is_empty() {
            let _ = self.store.append_event(
                &m.attempt_id,
                "reviewBreaks",
                &json!({ "breaks": why.breaks }),
            );
        }
    }

    /// Resumes the same session once, saying exactly what to fix.
    fn ask_for_correction(
        self: &Arc<Self>,
        attempt_id: &str,
        problems: &[keel_core::SpecProblem],
    ) -> Result<()> {
        let _guard = locked(&self.advancing);
        let attempt = self.store.attempt(attempt_id)?;
        let run = self.store.run(&attempt.run_id)?;
        if run.finished_at.is_some() {
            return Ok(());
        }
        let steps: Vec<WorkflowStep> = serde_json::from_value(run.workflow).unwrap_or_default();
        let step = workflows::find_step(&steps, &attempt.step_id).ok_or(Error::InvalidState)?;
        let attempts = self.store.attempts_of_run(&attempt.run_id)?;
        self.store.append_event(
            attempt_id,
            "specCorrected",
            &json!({ "problems": problems }),
        )?;
        self.start_step(
            &attempt.run_id,
            &step,
            workflows::next_number(&attempts, &attempt.step_id),
            Some(Resume {
                session: attempt.session_id.clone(),
                from: attempt.id.clone(),
                prompt: Some(specs::correction_prompt(problems, &attempt.language)),
            }),
        )?;
        Ok(())
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
    if m.kind == "check" || m.kind == "setup" {
        let failed = |exit_code| {
            if m.kind == "setup" {
                Reason::SetupFailed { exit_code }
            } else {
                Reason::CheckFailed { exit_code }
            }
        };
        return match exit_code {
            Some(0) => (AttemptState::Succeeded, None),
            Some(code) => (AttemptState::Failed, Some(failed(Some(code)))),
            None if process_lost || m.child.is_none() => {
                (AttemptState::Interrupted, Some(Reason::AgentLost))
            }
            None => (AttemptState::Failed, Some(failed(None))),
        };
    }
    match &m.result {
        Some(result)
            if !result.is_error && m.requires_output && result.structured_output.is_none() =>
        {
            (AttemptState::Failed, Some(Reason::NoStructuredOutput))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn without_crates_io_cargo_and_go_work_offline() {
        let env = cache_env(Path::new("/c"), Path::new("/h"), Some(&[]));
        assert_eq!(var(&env, "CARGO_HOME"), Some("/h/cargo"));
        assert_eq!(var(&env, "GOPATH"), Some("/h/go"));
        assert_eq!(var(&env, "CARGO_NET_OFFLINE"), Some("true"));
        assert_eq!(var(&env, "GOPROXY"), Some("off"));
    }

    #[test]
    fn a_project_that_allows_them_is_not_forced_offline() {
        let domains = ["Index.crates.io".to_owned(), "proxy.golang.org".to_owned()];
        let env = cache_env(Path::new("/c"), Path::new("/h"), Some(&domains));
        assert_eq!(var(&env, "CARGO_NET_OFFLINE"), None);
        assert_eq!(var(&env, "GOPROXY"), None);
        // A domain that merely ends in the same letters isn't it.
        let env = cache_env(
            Path::new("/c"),
            Path::new("/h"),
            Some(&["notcrates.io".to_owned()]),
        );
        assert_eq!(var(&env, "CARGO_NET_OFFLINE"), Some("true"));
    }

    #[test]
    fn setup_has_the_network_and_is_left_alone() {
        let env = cache_env(Path::new("/c"), Path::new("/c"), None);
        assert_eq!(var(&env, "CARGO_NET_OFFLINE"), None);
        assert_eq!(var(&env, "GOPROXY"), None);
    }

    #[test]
    fn a_run_copies_the_cache_once_and_never_writes_to_the_shared_one() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(shared.join("cargo/registry")).unwrap();
        std::fs::write(shared.join("cargo/registry/dep"), "x").unwrap();
        let run = dir.path().join("run");
        copy_tool_caches(&shared, &run).unwrap();
        assert!(run.join("cargo/registry/dep").exists());
        assert!(run.join("go").is_dir(), "an empty one when setup left none");
        std::fs::write(run.join("cargo/registry/mine"), "y").unwrap();
        std::fs::write(run.join("cargo/registry/dep"), "changed").unwrap();
        // Another step of the same run keeps what the run has.
        copy_tool_caches(&shared, &run).unwrap();
        assert!(run.join("cargo/registry/mine").exists());
        assert!(!shared.join("cargo/registry/mine").exists());
        assert_eq!(
            std::fs::read_to_string(shared.join("cargo/registry/dep")).unwrap(),
            "x"
        );
    }
}
