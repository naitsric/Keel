//! Runs workflows. Everything happens inside the app process, without a daemon, but each agent
//! is its own process writing to a file, so it outlives the app and can be picked up again.

mod board;
mod brain;
mod deliver;
mod detect;
mod diffs;
mod git;
mod joint;
mod project_proposals;
mod prompts;
mod pull_requests;
mod runner;
mod screenshots;
mod services;
mod sizing;
mod skipped_tests;
mod specs;
mod verification;
mod workflows;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub use board::{Board, BoardCard, BoardIssue};
pub use brain::{BrainView, DEFAULT_BRAIN_MODEL, DecisionView, NoteView};
pub use detect::Guess;
pub use git::{ChangeSize, Git, GitError, Snapshot};
pub use joint::{JointCard, JointChangeView, Named, Neighbour, PartOf, PartWarning, RunJoint};
use keel_core::{AttemptState, CardLane, Reason, StepCondition, StepRole, StepSpec, WorkflowStep};
use keel_harness::auth::{self, AuthStatus, VersionCheck};
use keel_harness::github::{self, Account, Gh, GhError, Issue, RepoInfo};
use keel_harness::process::{self, SIGINT, SIGTERM};
use keel_harness::{Tools, settings};
use keel_store::rusqlite_value::Value;
use keel_store::{
    Decision, NewAttempt, NewRun, Project, ProjectSettings, Store, WorkflowResults, now,
};
pub use project_proposals::{MAX_PROJECT_PROPOSALS, ProjectProposal};
pub use prompts::full_workflow;
pub use screenshots::{Screenshot, Screenshots};
use serde_json::json;
// What a reader outside the engine needs to tell what a run did and what it waits for, read the way
// the engine reads it.
pub use sizing::skipped_steps;
pub use specs::of_attempt as spec_of_attempt;
pub use specs::{Answer, AnswerChoice, Spec};
pub use verification::of_attempt as verification_of_attempt;
pub use verification::{Outcome, Verification};
pub use workflows::{Workflow, WorkflowDraft, can_send_back, files_changed};

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

/// What a project in this folder is called, until the person renames it.
fn folder_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn gh_blocked(error: GhError) -> Error {
    Error::Blocked(Reason::DeliveryFailed {
        kind: error.kind.to_owned(),
        detail: error.detail,
    })
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Where new runs keep their folder. Apart from `data_dir` so it can be a path without
    /// spaces: scripts that don't quote paths break in "Application Support".
    pub runs_dir: PathBuf,
    /// Quiet this long, with the process tree idle, counts as stalled.
    pub stall_after: Duration,
    /// Quiet this long counts as stalled even if something is using CPU.
    pub stall_hard_limit: Duration,
    pub tick: Duration,
    /// How long "wait" silences the stall warning.
    pub snooze: Duration,
    /// Time to answer SIGINT before Keel escalates.
    pub stop_grace: Duration,
    /// How many agents work at once, until a person chooses a number. Theirs is in the store.
    pub max_agents: usize,
    pub keep_awake: bool,
    /// How long `claude` gets to answer each question about the session.
    pub session_check_limit: Duration,
    /// Where git reaches GitHub. Tests point this at local repos.
    pub github_git_base: String,
    /// How long what Keel knows about a pull request is good for, in the background.
    pub pr_look_again: Duration,
    /// What Claude Code runs around each Bash command of a step that can edit, to tell the agent
    /// which project files changed (see `keel_harness::hook`). The app installs it. Without it, or
    /// without git, no hook.
    pub bash_hook: Option<PathBuf>,
}

impl Config {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            runs_dir: data_dir.join("runs"),
            data_dir,
            stall_after: Duration::from_secs(180),
            stall_hard_limit: Duration::from_secs(1800),
            tick: Duration::from_millis(250),
            snooze: Duration::from_secs(600),
            stop_grace: Duration::from_secs(10),
            max_agents: 2,
            keep_awake: true,
            session_check_limit: auth::CHECK_LIMIT,
            github_git_base: "https://github.com".to_owned(),
            pr_look_again: pull_requests::LOOK_AGAIN,
            bash_hook: None,
        }
    }
}

/// Which saved workflow a run's steps came from. Its copy of them doesn't say, and comparing
/// workflows needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct StartRun {
    pub project_id: String,
    /// Shown as the run's name. Taken from the request if absent.
    pub title: Option<String>,
    pub request: String,
    pub workflow: Vec<WorkflowStep>,
    /// None for steps put together on the spot, such as a single agent or trying a command.
    pub saved_workflow: Option<WorkflowRef>,
    /// Language for what agents write for the person.
    pub language: String,
    /// The issue this came from, kept as evidence of where the request started.
    pub source_issue: Option<Issue>,
    /// The assistant that asked for this over MCP. Written with the run, because whether a change
    /// can count as small depends on it.
    pub started_by: Option<String>,
}

impl StartRun {
    /// A run with a single agent step.
    pub fn one_step(project_id: String, step: StepSpec, language: String) -> Self {
        Self {
            project_id,
            title: None,
            request: step.prompt.clone(),
            workflow: vec![WorkflowStep::Agent(step)],
            saved_workflow: None,
            language,
            source_issue: None,
            started_by: None,
        }
    }
}

pub use prompts::{request_from_issue, title_from_issue};

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

/// What adding a folder did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AddOutcome {
    New,
    /// It was already in Keel.
    Existing,
    /// It had been removed, and it's back with its history.
    Restored,
}

/// Whether a new project's map really started.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MapStart {
    Started,
    NotStarted {
        #[serde(serialize_with = "keel_core::serialize_masked")]
        reason: Reason,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedProject {
    pub project: Project,
    pub outcome: AddOutcome,
    /// Only a new project starts one, so anything else says nothing about a map.
    pub map: Option<MapStart>,
}

/// What came of proposing a folder.
#[derive(Debug, Clone)]
pub enum ProposedProject {
    /// It waits for the person, whether this call left it or an earlier one did.
    Waiting(ProjectProposal),
    /// It's a project already, so there's nothing for the person to decide.
    InKeel(Box<Project>),
    /// As many as may wait are waiting.
    Full,
    /// Its path could hide from the person what it really is.
    UnreadablePath,
}

/// What the person needs to decide on a proposal, read from the folder as it is now.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectProposalReview {
    #[serde(flatten)]
    pub proposal: ProjectProposal,
    pub name: String,
    pub github_repo: Option<String>,
    /// It was in Keel and was removed, so it comes back with its history and starts no map.
    pub returning: bool,
    /// It isn't the root of a repo with commits anymore, so all that's left is to discard it.
    pub gone: bool,
}

/// What the setup dialog starts from.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSetup {
    pub project: Project,
    pub guess: Guess,
}

/// The step that marks a run as someone trying a command. See `Inner::close_failed_trial`.
const TRY_CHECK_STEP: &str = "try-check";

#[derive(Default)]
struct Slots {
    active: HashSet<String>,
    /// Projects with a step that can run commands, only one at a time per project, and which
    /// attempt has the turn.
    command_steps: HashMap<String, Turn>,
}

/// A project's turn to run commands.
struct Turn {
    /// Only this attempt can give it back.
    attempt_id: String,
    /// What a step waiting for the turn is told: which step has it, and in which run.
    waiting: Reason,
}

/// Continuing a session rather than starting one.
pub(crate) struct Resume {
    pub session: String,
    /// The attempt it continues from.
    pub from: String,
    /// What it's told, if not simply to go on.
    pub prompt: Option<String>,
}

/// What a step's condition comes to, each time `advance` goes through. Reading the change takes a
/// moment, so it's worked out at most once, and only if a step asks.
#[derive(Default)]
struct SizeThisPass {
    decision: Option<sizing::Decision>,
    said: bool,
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
    /// Gets the scheduler to look at the board. See `board::Wake`.
    board_wake: Arc<board::Wake>,
    /// Whether the app is on its way out. Held from taking a card until its run has its first step,
    /// so whoever sets this knows that anything underway has finished and can be seen.
    board_stopped: Mutex<bool>,
    /// Why cards aren't starting, as of the scheduler's last look.
    board_paused: Mutex<Option<Reason>>,
    max_agents: AtomicUsize,
    slots: Mutex<Slots>,
    slot_freed: Condvar,
    controls: Mutex<HashMap<String, Control>>,
    /// Starting a step reads what ran and then adds to it. One at a time, or a step could start twice.
    advancing: Mutex<()>,
    creating_workspace: Mutex<()>,
    /// One lock for each project being fetched to read and each run making its copies. See
    /// `joint::prepare_read_copies`.
    read_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// One lock for each project starting its map. See `Engine::generate_map`.
    starting_map: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
        let max_agents = board::stored_max_agents(&store).unwrap_or(config.max_agents);
        let board_wake = Arc::new(board::Wake::default());
        let wake = board_wake.clone();
        Self {
            inner: Arc::new(Inner {
                store,
                config,
                tools: Mutex::new(tools),
                // This runs synchronously, per stream batch, sometimes holding `advancing` or
                // `slots`. All it may do for the board is leave a note for the scheduler's thread.
                on_change: Box::new(move |run_id| {
                    wake.notify();
                    on_change(run_id);
                }),
                board_wake,
                board_stopped: Mutex::new(false),
                board_paused: Mutex::new(None),
                max_agents: AtomicUsize::new(max_agents),
                slots: Mutex::default(),
                slot_freed: Condvar::new(),
                controls: Mutex::default(),
                advancing: Mutex::new(()),
                creating_workspace: Mutex::new(()),
                read_locks: Mutex::default(),
                starting_map: Mutex::default(),
            }),
        }
    }

    pub fn set_tools(&self, tools: Tools) {
        *locked(&self.inner.tools) = tools;
        // Cards wait in Todo while the session can't work.
        self.inner.board_wake.notify();
    }

    pub fn preflight(&self) -> Preflight {
        let tools = locked(&self.inner.tools).clone();
        let check = tools
            .claude
            .as_deref()
            .map(|c| auth::check_session(c, &tools.path_env, self.inner.config.session_check_limit))
            .unwrap_or_default();
        let version = check.version.as_deref().map(auth::check_version);
        let auth = check.auth;
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
        } else if check.not_responding {
            Some(Reason::ClaudeNotResponding)
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
        Ok(self.add_folder(path)?.0)
    }

    /// Adds a folder, and starts a new project's map. Without a Claude Code session the map can't
    /// start, which isn't a reason to refuse the project: the Brain screen offers it again.
    pub fn add_project_with_map(&self, path: &Path, language: &str) -> Result<AddedProject> {
        let (project, outcome) = self.add_folder(path)?;
        let map = if outcome != AddOutcome::New {
            None
        } else {
            match self.first_map(&project.id, DEFAULT_BRAIN_MODEL, language) {
                Ok(run) => run.map(|_| MapStart::Started),
                Err(Error::Blocked(reason)) => Some(MapStart::NotStarted { reason }),
                Err(other) => Some(MapStart::NotStarted {
                    reason: Reason::PrepareFailed {
                        detail: other.to_string(),
                    },
                }),
            }
        };
        Ok(AddedProject {
            project,
            outcome,
            map,
        })
    }

    /// However a folder got added, a proposal to add it has its answer.
    fn add_folder(&self, path: &Path) -> Result<(Project, AddOutcome)> {
        let added = self.add_or_restore(path)?;
        self.inner.settle_project_proposals(&added.0.path);
        Ok(added)
    }

    /// A folder already in Keel keeps its row, so a removed one comes back with its history.
    fn add_or_restore(&self, path: &Path) -> Result<(Project, AddOutcome)> {
        let git = self.git()?;
        let root = git
            .toplevel(path)
            .map_err(|_| Error::NotARepo(path.to_string_lossy().into_owned()))?;
        let root_text = root.to_string_lossy().into_owned();
        if let Some(existing) = self.inner.store.project_by_path(&root_text)? {
            let outcome = if self.inner.store.project_is_removed(&existing.id)? {
                self.inner.store.restore_project(&existing.id)?;
                AddOutcome::Restored
            } else {
                AddOutcome::Existing
            };
            let project = self.refresh_repo(&existing.id)?;
            if outcome == AddOutcome::Restored {
                // Cards it had in Todo were asked to start, and the board should look again.
                self.inner.changed("");
            }
            return Ok((project, outcome));
        }
        let project = Project {
            id: new_id(),
            name: folder_name(&root),
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
            default_workflow: None,
            sensitive_paths: Vec::new(),
        };
        self.inner.store.add_project(&project)?;
        Ok((project, AddOutcome::New))
    }

    /// A repo can be published long after it's added, or move, and only its folder knows. A folder
    /// that's gone says nothing about GitHub, so what was known stays.
    fn refresh_repo(&self, id: &str) -> Result<Project> {
        let project = self.inner.store.project(id)?;
        let root = Path::new(&project.path);
        let Ok(git) = self.git() else {
            return Ok(project);
        };
        if git.toplevel(root).is_err() {
            return Ok(project);
        }
        let repo = git
            .origin_url(root)
            .and_then(|url| github::github_repo(&url));
        if repo == project.github_repo {
            return Ok(project);
        }
        self.inner.store.set_project_repo(id, repo.as_deref())?;
        let project = self.inner.store.project(id)?;
        self.inner.compare_map_with_github(&project)?;
        Ok(project)
    }

    /// Where the setup dialog starts: the project as it is now, and a guess at its commands. Only
    /// reads the repo, and saves nothing but which GitHub repo it's on.
    pub fn project_setup(&self, id: &str) -> Result<ProjectSetup> {
        let project = self.refresh_repo(id)?;
        let guess = detect::guess(Path::new(&project.path));
        Ok(ProjectSetup { project, guess })
    }

    /// Tries commands before anyone saves them, in the real sandbox and with no model. Saved
    /// settings aren't touched, and what's being tried replaces them for this run.
    pub fn try_check(
        &self,
        project_id: &str,
        setup: &str,
        check: &str,
        language: &str,
    ) -> Result<String> {
        let (setup, check) = (setup.trim(), check.trim());
        if check.is_empty() {
            return Err(Error::Blocked(Reason::InvalidSetting {
                field: "checkCommand".to_owned(),
                value: String::new(),
            }));
        }
        // An emptied field means no setup, and `start_run` would put the saved one in front.
        let saved = self.inner.store.project(project_id)?.setup_command;
        let setup = match (setup.is_empty(), saved) {
            (false, _) => Some(setup),
            (true, Some(_)) => Some("true"),
            (true, None) => None,
        };
        let mut workflow = Vec::new();
        if let Some(run) = setup {
            workflow.push(WorkflowStep::Setup {
                id: "setup".to_owned(),
                run: run.to_owned(),
            });
        }
        workflow.push(WorkflowStep::Check {
            id: TRY_CHECK_STEP.to_owned(),
            run: check.to_owned(),
        });
        let what = if language.starts_with("es") {
            format!("Probar la verificación: {check}")
        } else {
            format!("Try the check: {check}")
        };
        self.start_run_in_session(
            StartRun {
                project_id: project_id.to_owned(),
                title: Some(title_from(&what)),
                request: what,
                workflow,
                language: language.to_owned(),
                source_issue: None,
                saved_workflow: None,
                started_by: None,
            },
            None,
        )
    }

    /// Hides a project from Keel. Deletes nothing, here or in its folder, and its cards stay where
    /// they are, starting nothing until it's added again. Not while a run of its hasn't finished,
    /// since that run would go on with no project anyone can see.
    pub fn remove_project(&self, id: &str) -> Result<()> {
        let store = &self.inner.store;
        store.project(id)?;
        if store.remove_project(id)? {
            return Ok(());
        }
        let open = store.open_runs_of_project(id)?;
        if let Some(run) = open.first() {
            return Err(Error::Blocked(Reason::ProjectHasOpenRuns {
                count: open.len(),
                run_id: run.id.clone(),
                title: run.title.clone(),
            }));
        }
        // A card being started right now has no run to point at yet.
        let starting = store
            .cards(id)?
            .into_iter()
            .find(|card| card.lane == CardLane::Started && card.run_id.is_none());
        match starting {
            Some(card) => Err(Error::Blocked(Reason::ProjectHasOpenRuns {
                count: 1,
                run_id: String::new(),
                title: card.title,
            })),
            None => Ok(()),
        }
    }

    pub fn update_project(&self, id: &str, settings: &ProjectSettings) -> Result<Project> {
        services::validate(&settings.services, &settings.env).map_err(Error::Blocked)?;
        sizing::check_patterns(&settings.sensitive_paths).map_err(Error::Blocked)?;
        let before = self.inner.store.project(id)?.github_account;
        self.inner.store.update_project(id, settings)?;
        let project = self.inner.store.project(id)?;
        if project.github_account != before {
            self.inner.compare_map_with_github(&project)?;
        }
        Ok(project)
    }

    /// The accounts signed in to `gh`. Never changes the active one.
    pub fn github_accounts(&self) -> Result<Vec<github::Account>> {
        self.gh()?.accounts().map_err(gh_blocked)
    }

    /// The accounts in `gh`, and what `account` can do in the project's repo. It takes the account
    /// rather than reading the saved one, so a person sees what an account can do before saving it.
    /// An account `gh` doesn't have counts as none. Never changes the active one.
    pub fn github_status(&self, project_id: &str, account: Option<&str>) -> Result<GithubStatus> {
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
        let Some(chosen) =
            account.and_then(|login| status.accounts.iter().find(|a| a.login == login))
        else {
            return Ok(status);
        };
        let login = chosen.login.clone();
        status.excess_scopes = chosen
            .scopes
            .iter()
            .filter(|s| !NEEDED_SCOPES.contains(&s.as_str()))
            .cloned()
            .collect();
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

    /// Open issues of the project's repo, to import one by choosing it. Keel never polls for these.
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
        self.start_run_in_session(request, None)
    }

    /// Whether a project could run a workflow, leaving aside the session. Run before it starts, and
    /// when a card asks to.
    fn check_can_run(&self, project: &Project, workflow: &[WorkflowStep]) -> Result<()> {
        workflows::validate(workflow).map_err(Error::Blocked)?;
        if self.inner.store.project_is_removed(&project.id)? {
            return Err(Error::Blocked(Reason::ProjectRemoved));
        }
        if workflow
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
        Ok(())
    }

    /// `start_run`, for a caller that has already looked at the session or doesn't need one. A run
    /// for a card is linked to it from the moment it exists.
    fn start_run_in_session(&self, request: StartRun, card: Option<&str>) -> Result<String> {
        let project = self.inner.store.project(&request.project_id)?;
        self.check_can_run(&project, &request.workflow)?;
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
        let run = NewRun {
            id: run_id.clone(),
            project_id: project.id,
            title: request.title.unwrap_or_else(|| title_from(&request.request)),
            request: request.request,
            workflow: serde_json::to_string(&workflow).unwrap_or_else(|_| "[]".into()),
            language: request.language,
            source_issue: request.source_issue.as_ref().map(|issue| {
                json!({ "number": issue.number, "title": issue.title, "author": issue.author, "url": issue.url }).to_string()
            }),
            workflow_id: request.saved_workflow.as_ref().map(|w| w.id.clone()),
            workflow_name: request.saved_workflow.map(|w| w.name),
            started_by: request.started_by,
        };
        match card {
            Some(card) => self.inner.store.add_run_for_card(&run, card)?,
            None => self.inner.store.add_run(&run)?,
        }
        if let Err(error) = self.begin_run(&run_id) {
            // Whoever asked hears that it failed. Left open with nothing recorded, it would hold a
            // place on the board, and the next launch would start it after all.
            let store = &self.inner.store;
            if store.attempts_of_run(&run_id).is_ok_and(|a| a.is_empty()) {
                let _ = store.patch_run(&run_id, &[("finished_at", Value::Text(now()))]);
            }
            return Err(error);
        }
        Ok(run_id)
    }

    fn begin_run(&self, run_id: &str) -> Result<()> {
        std::fs::create_dir_all(self.inner.run_dir(run_id))?;
        restrict_dir(&self.inner.run_dir(run_id));
        self.inner.advance(run_id)
    }

    /// Tells the app to look at this run again, for a change made outside the engine.
    pub fn announce(&self, run_id: &str) {
        self.inner.changed(run_id);
    }

    /// Answers an approval step. The text, if any, becomes a decision later runs of this project
    /// follow. Rejecting ends the run and takes a reason; approving lets it continue.
    pub fn answer_approval(&self, attempt_id: &str, approve: bool, text: &str) -> Result<()> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        if attempt.state != AttemptState::WaitingApproval.as_str() {
            return Err(Error::InvalidState);
        }
        if !approve && text.trim().is_empty() {
            let steps: Vec<WorkflowStep> =
                serde_json::from_value(store.run(&attempt.run_id)?.workflow).unwrap_or_default();
            let files = workflows::files_changed(&store.attempts_of_run(&attempt.run_id)?);
            // With no change and delivery next there's nothing to reject, only a run to close.
            if !files.is_empty() || !workflows::delivery_is_next(&steps, &attempt.step_id) {
                return Err(Error::Blocked(Reason::RejectionNeedsReason));
            }
        }
        let answer = if approve { "approve" } else { "reject" };
        let state = if approve {
            AttemptState::Succeeded
        } else {
            AttemptState::Rejected
        };
        // Whichever answer gets here first is the answer. See `send_back`.
        if !store.change_state(attempt_id, AttemptState::WaitingApproval, state)? {
            return Err(Error::InvalidState);
        }
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
        store.append_event(attempt_id, "finished", &json!({ "state": state.as_str() }))?;
        if approve {
            self.inner.advance(&attempt.run_id)?;
        } else {
            store.patch_run(&attempt.run_id, &[("finished_at", Value::Text(now()))])?;
            self.inner.free_builds(&attempt.run_id);
        }
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// Answers a step's questions. If all of them got the recommended option, no agent runs: they
    /// go in with what it decided alone, and the run moves on. Otherwise the same session resumes
    /// with the answers, so it doesn't reread the repo, and what it says then is judged again.
    /// Answers that don't belong to the questions are refused with nothing changed.
    pub fn answer_questions(&self, attempt_id: &str, answers: &[Answer]) -> Result<()> {
        let store = &self.inner.store;
        // Held from looking at the state until it has changed, so a second answer finds it changed
        // and nothing can move the run on in between.
        let guard = locked(&self.inner.advancing);
        let attempt = store.attempt(attempt_id)?;
        if attempt.state != AttemptState::WaitingAnswers.as_str() {
            return Err(Error::InvalidState);
        }
        let mut spec = specs::of_attempt(&attempt).ok_or(Error::InvalidState)?;
        let given = specs::check_answers(&spec.questions, answers).map_err(Error::Blocked)?;
        let matched = given.iter().filter(|answer| answer.recommended).count();
        let every_one_matched = matched == given.len();

        let project_id = store.project_of_run(&attempt.run_id)?;
        let keeping = |output: serde_json::Value| -> Result<()> {
            let mut output = output;
            output["answered"] = json!(given);
            store.patch_attempt(attempt_id, &[("output", Value::Text(output.to_string()))])?;
            Ok(())
        };
        if every_one_matched {
            spec.take_recommended(specs::Taken::Person);
            keeping(json!(spec))?;
        } else {
            // On the record before the agent starts, so it counts as a round when its result comes.
            let before = attempt.output.clone().unwrap_or_else(|| json!({}));
            keeping(before.clone())?;
            let started = store
                .run(&attempt.run_id)
                .map_err(Error::from)
                .and_then(|run| {
                    let steps: Vec<WorkflowStep> =
                        serde_json::from_value(run.workflow).unwrap_or_default();
                    let step = workflows::find_step(&steps, &attempt.step_id)
                        .ok_or(Error::InvalidState)?;
                    let attempts = store.attempts_of_run(&attempt.run_id)?;
                    self.inner.start_step(
                        &attempt.run_id,
                        &step,
                        workflows::next_number(&attempts, &attempt.step_id),
                        Some(Resume {
                            session: attempt.session_id.clone(),
                            from: attempt.id.clone(),
                            prompt: Some(specs::answered_prompt(&given, &attempt.language)),
                        }),
                    )
                });
            if let Err(error) = started {
                // Nothing went ahead, so nothing stays on the record, and it can be answered again.
                store.patch_attempt(attempt_id, &[("output", Value::Text(before.to_string()))])?;
                return Err(error);
            }
        }
        for answer in given.iter().filter(|answer| answer.remember) {
            let (question, text) = specs::decision_parts(answer);
            store.add_answered_decision(
                &project_id,
                &attempt.run_id,
                attempt_id,
                &question,
                &text,
            )?;
        }
        store.append_event(
            attempt_id,
            "answers",
            &json!({ "answers": given, "total": given.len(), "recommended": matched }),
        )?;
        store.set_state(attempt_id, AttemptState::Succeeded, None)?;
        store.append_event(
            attempt_id,
            "finished",
            &json!({ "state": AttemptState::Succeeded.as_str() }),
        )?;
        drop(guard);
        if every_one_matched {
            self.inner.advance(&attempt.run_id)?;
        }
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// A person sends the work back from an approval, with what they want changed. It doesn't start
    /// over: everything from the rework point runs again, the check included, and the approval comes
    /// round again after it. Their words stay in this run and don't become a decision, and the round
    /// doesn't count against the approver, since a person asked for it.
    pub fn send_back(&self, attempt_id: &str, instructions: &str) -> Result<()> {
        self.send_back_as(attempt_id, instructions, None)
    }

    /// `send_back` for a client over MCP. The attempt and its event name it, so the person sees
    /// who sent the work back.
    pub fn send_back_by(&self, attempt_id: &str, instructions: &str, by: &str) -> Result<()> {
        self.send_back_as(attempt_id, instructions, Some(by))
    }

    fn send_back_as(&self, attempt_id: &str, instructions: &str, by: Option<&str>) -> Result<()> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        if attempt.state != AttemptState::WaitingApproval.as_str() {
            return Err(Error::InvalidState);
        }
        let instructions = instructions.trim();
        if instructions.is_empty() {
            return Err(Error::Blocked(Reason::SendBackNeedsInstructions));
        }
        let steps: Vec<WorkflowStep> =
            serde_json::from_value(store.run(&attempt.run_id)?.workflow).unwrap_or_default();
        if !workflows::can_send_back(&steps, &attempt.step_id) {
            return Err(Error::Blocked(Reason::NothingToSendBackTo));
        }
        if !store.return_attempt(attempt_id, &attempt.run_id, instructions, by)? {
            return Err(Error::InvalidState);
        }
        self.inner.advance(&attempt.run_id)?;
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// A person at an approval asks for what was skipped for being a small change to run first. Nothing
    /// is built again and no round goes by, so this doesn't count against the approver. Their approval
    /// comes round again afterwards. If the approver then asks for rework, that takes the normal path.
    pub fn run_skipped(&self, attempt_id: &str) -> Result<()> {
        let store = &self.inner.store;
        let attempt = store.attempt(attempt_id)?;
        if attempt.state != AttemptState::WaitingApproval.as_str() {
            return Err(Error::InvalidState);
        }
        let skipped = sizing::skipped_steps(&store.attempts_of_run(&attempt.run_id)?);
        if skipped.is_empty() {
            return Err(Error::Blocked(Reason::NothingSkippedToRun));
        }
        if !store.postpone_attempt(attempt_id, &skipped)? {
            return Err(Error::InvalidState);
        }
        self.inner.advance(&attempt.run_id)?;
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
        self.retry_as(attempt_id, resume, None)
    }

    /// `retry` for a client over MCP. The step it retries says who asked, so the person sees it.
    pub fn retry_by(&self, attempt_id: &str, resume: bool, by: &str) -> Result<String> {
        self.retry_as(attempt_id, resume, Some(by))
    }

    fn retry_as(&self, attempt_id: &str, resume: bool, by: Option<&str>) -> Result<String> {
        let store = &self.inner.store;
        // Held from looking at the attempt until its next try is on the record, so two retries, or
        // a retry and a close, can't both go through. `start_step` never takes it: `advance` and
        // `answer_questions` call it while they hold it too.
        let _guard = locked(&self.inner.advancing);
        let attempt = store.attempt(attempt_id)?;
        let state = AttemptState::parse(&attempt.state).ok_or(Error::InvalidState)?;
        // Those that wait for a person get answered, and one sent back has already moved on.
        if state.is_active()
            || matches!(
                state,
                AttemptState::WaitingApproval
                    | AttemptState::WaitingAnswers
                    | AttemptState::Returned
                    | AttemptState::Postponed
            )
        {
            return Err(Error::InvalidState);
        }
        // A client only retries what went wrong, and never reopens a run the person, or a close,
        // has ended. The person can.
        if by.is_some()
            && !matches!(
                state,
                AttemptState::Failed | AttemptState::Interrupted | AttemptState::Stopped
            )
        {
            return Err(Error::InvalidState);
        }
        let run = store.run(&attempt.run_id)?;
        if by.is_some() && run.finished_at.is_some() {
            return Err(Error::InvalidState);
        }
        if store.project_is_removed(&run.project_id)? {
            return Err(Error::Blocked(Reason::ProjectRemoved));
        }
        // An approval answers what was agreed then. Something new agreed would go through on it, and
        // the parts may already be on the board.
        if state == AttemptState::Succeeded && self.inner.agreement(&attempt.run_id)?.is_some() {
            return Err(Error::InvalidState);
        }
        let attempts = store.attempts_of_run(&attempt.run_id)?;
        // One that was tried again already has its next try. Another would run the step twice.
        if !workflows::is_latest_of_its_step(&attempts, &attempt) {
            return Err(Error::InvalidState);
        }
        let number = workflows::next_number(&attempts, &attempt.step_id);
        let steps: Vec<WorkflowStep> =
            serde_json::from_value(run.workflow.clone()).unwrap_or_default();
        let step = workflows::find_step(&steps, &attempt.step_id).ok_or(Error::InvalidState)?;
        let resume = (resume && attempt.kind == "agent").then(|| Resume {
            session: attempt.session_id.clone(),
            from: attempt.id.clone(),
            prompt: resumed_prompt(&attempt),
        });
        // What really happens: a step that isn't an agent has no session to continue.
        if let Some(by) = by {
            store.append_event(
                attempt_id,
                "retryRequested",
                &json!({ "by": by, "resume": resume.is_some() }),
            )?;
        }
        // Before the step starts: a quick one can be over, and have closed the run, by the time
        // `start_step` returns.
        if run.finished_at.is_some() {
            store.patch_run(&attempt.run_id, &[("finished_at", Value::Null)])?;
        }
        self.inner
            .start_step(&attempt.run_id, &step, number, resume)
    }

    pub fn abandon(&self, attempt_id: &str) -> Result<()> {
        self.abandon_as(attempt_id, None)
    }

    /// `abandon` for a client over MCP. Its event names the client, so the person sees who closed
    /// the run.
    pub fn abandon_by(&self, attempt_id: &str, by: &str) -> Result<()> {
        self.abandon_as(attempt_id, Some(by))
    }

    fn abandon_as(&self, attempt_id: &str, by: Option<&str>) -> Result<()> {
        // So that cancelling and answering the same question can't both go through.
        let guard = locked(&self.inner.advancing);
        let attempt = self.inner.store.attempt(attempt_id)?;
        let state = AttemptState::parse(&attempt.state).ok_or(Error::InvalidState)?;
        // What worked has moved on, and leaving it would close the run under whatever came next.
        if state.is_active()
            || matches!(
                state,
                AttemptState::Succeeded | AttemptState::Returned | AttemptState::Postponed
            )
        {
            return Err(Error::InvalidState);
        }
        // A client closes only an open run that's at rest, and on where it stands now. Closing
        // doesn't stop anything, so a step still working would go on in a run that has ended.
        if by.is_some() {
            let store = &self.inner.store;
            let run = store.run(&attempt.run_id)?;
            let attempts = store.attempts_of_run(&attempt.run_id)?;
            let working = attempts
                .iter()
                .any(|a| AttemptState::parse(&a.state).is_some_and(AttemptState::is_active));
            if run.finished_at.is_some()
                || state == AttemptState::Skipped
                || working
                || !workflows::is_latest_of_its_step(&attempts, &attempt)
            {
                return Err(Error::InvalidState);
            }
        }
        // Approving doesn't wait for `advancing`, and one that lands first has to stand.
        if !self
            .inner
            .store
            .change_state(attempt_id, state, AttemptState::Abandoned)?
        {
            return Err(Error::InvalidState);
        }
        let data = match by {
            Some(by) => json!({ "by": by }),
            None => json!({}),
        };
        self.inner
            .store
            .append_event(attempt_id, "abandoned", &data)?;
        self.inner
            .store
            .patch_run(&attempt.run_id, &[("finished_at", Value::Text(now()))])?;
        self.inner.free_builds(&attempt.run_id);
        drop(guard);
        (self.inner.on_change)(&attempt.run_id);
        Ok(())
    }

    /// Run once at startup, once the tools are known. Reconnects to steps that outlived the app,
    /// closes out the rest, and continues any run that stopped between two steps. Approvals and
    /// questions simply keep waiting: nothing is running for them. Ended runs that still hold what
    /// their tools built are freed in the background.
    pub fn recover(&self) -> Result<()> {
        self.inner.store.release_claimed_cards()?;
        for attempt in self.inner.store.active_attempts()? {
            self.inner.recover_attempt(&attempt)?;
        }
        for run in self.inner.store.open_runs()? {
            self.inner.advance(&run.id)?;
        }
        self.inner.free_ended_runs();
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

    /// The pictures this round's steps left of how the change looks, for its approval, each with
    /// the step it came from and whether the code may have changed after it.
    pub fn run_screenshots(&self, run_id: &str) -> Result<Screenshots> {
        let store = &self.inner.store;
        let round = store.run(run_id)?.round;
        let attempts = store.attempts_of_run(run_id)?;
        let taken: Vec<(String, screenshots::Taken)> = store
            .run_events(run_id, screenshots::EVENT)?
            .into_iter()
            .filter_map(|event| Some((event.attempt_id, serde_json::from_value(event.data).ok()?)))
            .collect();
        Ok(screenshots::for_approval(
            &self.inner.run_dir(run_id),
            &attempts,
            round,
            &taken,
        ))
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

    /// One of the workflows a project can use, in this language.
    pub fn workflow(
        &self,
        project_id: &str,
        workflow_id: &str,
        language: &str,
    ) -> Result<Option<Workflow>> {
        Ok(self
            .workflows(project_id, language)?
            .into_iter()
            .find(|w| w.id == workflow_id))
    }

    pub fn save_workflow(&self, project_id: &str, workflow: &Workflow) -> Result<Workflow> {
        workflows::validate(&workflow.steps).map_err(Error::Blocked)?;
        Ok(workflows::save(
            &self.inner.mirror(project_id).with_file_name("workflows"),
            workflow,
        )?)
    }

    /// How each workflow's runs in a project have gone, the latest used first.
    pub fn workflow_results(&self, project_id: &str) -> Result<Vec<WorkflowResults>> {
        Ok(self.inner.store.workflow_results(project_id)?)
    }

    pub fn delete_workflow(&self, project_id: &str, workflow_id: &str) -> Result<()> {
        Ok(workflows::delete(
            &self.inner.mirror(project_id).with_file_name("workflows"),
            workflow_id,
        )?)
    }

    /// Leaves a folder for the person to review. Nothing is added and nothing runs until they say
    /// so, and a subfolder is proposed as its repo, as adding it would add. The same folder proposed
    /// again is the same proposal, so asking twice doesn't ask them twice.
    pub fn propose_project(&self, path: &Path, proposed_by: &str) -> Result<ProposedProject> {
        let root = self
            .git()?
            .toplevel(path)
            .map_err(|_| Error::NotARepo(path.to_string_lossy().into_owned()))?;
        let root = root.to_string_lossy().into_owned();
        if !project_proposals::readable_path(&root) {
            return Ok(ProposedProject::UnreadablePath);
        }
        let store = &self.inner.store;
        if let Some(project) = store.project_by_path(&root)?
            && !store.project_is_removed(&project.id)?
        {
            return Ok(ProposedProject::InKeel(Box::new(project)));
        }
        let dir = self.inner.project_proposals_dir();
        let waiting = project_proposals::load(&dir)?;
        if let Some(proposal) = waiting.iter().find(|proposal| proposal.path == root) {
            return Ok(ProposedProject::Waiting(proposal.clone()));
        }
        if waiting.len() >= MAX_PROJECT_PROPOSALS {
            return Ok(ProposedProject::Full);
        }
        let proposal = ProjectProposal {
            id: new_id(),
            path: root,
            proposed_by: proposed_by.to_owned(),
            created_at: now(),
        };
        project_proposals::save(&dir, &proposal)?;
        self.inner.changed("");
        Ok(ProposedProject::Waiting(proposal))
    }

    /// What's waiting, newest first. It reads nothing but its own files, so a page can ask often.
    pub fn project_proposals(&self) -> Result<Vec<ProjectProposal>> {
        Ok(project_proposals::load(
            &self.inner.project_proposals_dir(),
        )?)
    }

    /// What's waiting, with what the person needs to see of each folder.
    pub fn project_proposal_reviews(&self) -> Result<Vec<ProjectProposalReview>> {
        let git = self.git()?;
        let store = &self.inner.store;
        self.project_proposals()?
            .into_iter()
            .map(|proposal| {
                let root = PathBuf::from(&proposal.path);
                let returning = match store.project_by_path(&proposal.path)? {
                    Some(project) => store.project_is_removed(&project.id)?,
                    None => false,
                };
                Ok(ProjectProposalReview {
                    name: folder_name(&root),
                    github_repo: git
                        .origin_url(&root)
                        .and_then(|url| github::github_repo(&url)),
                    returning,
                    gone: git.toplevel(&root).ok().as_deref() != Some(root.as_path()),
                    proposal,
                })
            })
            .collect()
    }

    /// The person adds what they were shown: that folder, and not whatever's around it by now. From
    /// here on it's the same as adding it by hand, map included.
    pub fn accept_project_proposal(&self, id: &str, language: &str) -> Result<AddedProject> {
        let proposal = self
            .project_proposals()?
            .into_iter()
            .find(|proposal| proposal.id == id)
            .ok_or(Error::InvalidState)?;
        let root = PathBuf::from(&proposal.path);
        if self.git()?.toplevel(&root).ok().as_deref() != Some(root.as_path()) {
            return Err(Error::NotARepo(proposal.path));
        }
        self.add_project_with_map(&root, language)
    }

    pub fn discard_project_proposal(&self, id: &str) -> Result<()> {
        project_proposals::delete(&self.inner.project_proposals_dir(), id)?;
        self.inner.changed("");
        Ok(())
    }

    /// Leaves a proposed workflow for the person to review. It follows the same rules as any other,
    /// but [`Engine::workflows`] doesn't see it, so nothing can run it until they save it.
    pub fn draft_workflow(
        &self,
        project_id: &str,
        name: &str,
        steps: Vec<WorkflowStep>,
        proposed_by: &str,
    ) -> Result<WorkflowDraft> {
        self.inner.store.project(project_id)?;
        workflows::validate(&steps).map_err(Error::Blocked)?;
        let draft = WorkflowDraft {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: project_id.to_owned(),
            name: name.trim().to_owned(),
            steps,
            proposed_by: proposed_by.to_owned(),
            created_at: keel_store::now(),
        };
        workflows::save_draft(&self.inner.drafts_dir(project_id), &draft)?;
        self.inner.changed("");
        Ok(draft)
    }

    /// One project's drafts, newest first.
    pub fn workflow_drafts_of(&self, project_id: &str) -> Result<Vec<WorkflowDraft>> {
        Ok(workflows::load_drafts(
            &self.inner.drafts_dir(project_id),
            project_id,
        )?)
    }

    /// Every project's drafts, newest first, so none stays out of sight behind another project.
    pub fn workflow_drafts(&self) -> Result<Vec<WorkflowDraft>> {
        let mut all = Vec::new();
        for project in self.inner.store.projects()? {
            all.extend(self.workflow_drafts_of(&project.id)?);
        }
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(all)
    }

    /// The person saves a draft: what they have on screen, with their changes, becomes a new
    /// workflow. It never replaces one, whatever id comes along.
    pub fn save_workflow_draft(
        &self,
        project_id: &str,
        draft_id: &str,
        workflow: &Workflow,
    ) -> Result<Workflow> {
        let dir = self.inner.drafts_dir(project_id);
        if !workflows::draft_exists(&dir, draft_id)? {
            return Err(Error::InvalidState);
        }
        let saved = self.save_workflow(
            project_id,
            &Workflow {
                id: String::new(),
                builtin: false,
                ..workflow.clone()
            },
        )?;
        workflows::delete(&dir, draft_id)?;
        self.inner.changed("");
        Ok(saved)
    }

    pub fn discard_workflow_draft(&self, project_id: &str, draft_id: &str) -> Result<()> {
        workflows::delete(&self.inner.drafts_dir(project_id), draft_id)?;
        self.inner.changed("");
        Ok(())
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

/// What an attempt being resumed by hand is told. A limit it broke has to be said again, and answers
/// it may never have got shouldn't be lost to a bare "go on".
fn resumed_prompt(attempt: &keel_store::Attempt) -> Option<String> {
    if let Some(Reason::SpecNotUsable { problems }) = attempt
        .reason
        .clone()
        .and_then(|reason| serde_json::from_value(reason).ok())
    {
        return Some(specs::correction_prompt(&problems, &attempt.language));
    }
    (attempt.resumed_from.is_some() && attempt.prompt != RESUME_PROMPT)
        .then(|| attempt.prompt.clone())
}

/// A run made before `runs_dir` existed keeps its folder under the data folder; any other run
/// gets one in `runs_dir`.
fn run_dir_for(config: &Config, run_id: &str) -> PathBuf {
    let old = config.data_dir.join("runs").join(run_id);
    if old.exists() {
        old
    } else {
        config.runs_dir.join(run_id)
    }
}

fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// What a run's tools build in its copy and would build again: Rust's, Node's and Python's, and the
/// run's own copy of the caches. A run that compiles Rust leaves gigabytes.
const REBUILT: [&str; 4] = ["target", "node_modules", ".venv", git::RUN_CACHE];

/// What setup installs. A step tried again doesn't run setup, so it would find them gone.
const SETUP_INSTALLS: [&str; 2] = ["node_modules", ".venv"];

/// Where, in a run's folder, what's freed from its copy goes to be deleted. Outside the copy, so a
/// step tried there again finds a folder whole or not at all, never half deleted.
const TRASH: &str = "trash";

/// The folders right in `workspace` that tools build, named exactly as they are on disk. The Mac's
/// disk would also find `Target` as `target`, which is someone's own folder, not cargo's.
fn built_folders(workspace: &Path) -> Vec<String> {
    std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .flatten()
        // Not following links, so a link stays, and so does what it points to.
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| REBUILT.contains(&name.as_str()))
        .collect()
}

/// A run that is someone trying a command. See `Inner::close_failed_trial`.
fn is_trial(run: &keel_store::Run) -> bool {
    let steps: Vec<WorkflowStep> = serde_json::from_value(run.workflow.clone()).unwrap_or_default();
    steps.iter().any(|s| s.id() == TRY_CHECK_STEP)
}

/// Deletes a folder, even one a tool made read-only, as Go does with the modules it downloads.
fn remove_all(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_err() {
        let_owner_change(dir);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Deleting what's in a folder takes being able to change it. Links are left as they are, so
/// nothing outside `dir` changes.
fn let_owner_change(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode() | 0o700;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode));
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let_owner_change(&entry.path());
    }
}

/// Helpers shared with `runner` and `deliver`.
impl Inner {
    fn changed(&self, run_id: &str) {
        (self.on_change)(run_id);
    }

    /// Any other run that fails waits for a person to retry or abandon it. Someone trying a command
    /// has already seen the answer, and no agent was paid for. Left open, this run would stop the
    /// project from being removed. Retrying it from its page still works, and rebuilds what's freed.
    fn close_failed_trial(self: &Arc<Self>, run_id: &str, state: AttemptState) {
        if matches!(state, AttemptState::Succeeded | AttemptState::Skipped) {
            return;
        }
        let Ok(run) = self.store.run(run_id) else {
            return;
        };
        if run.finished_at.is_none() && is_trial(&run) {
            let _ = self
                .store
                .patch_run(run_id, &[("finished_at", Value::Text(now()))]);
            self.free_builds(run_id);
        }
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        run_dir_for(&self.config, run_id)
    }

    fn workspace(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("workspace")
    }

    /// `free_now`, on a thread of its own, since a build can take a while to delete.
    fn free_builds(self: &Arc<Self>, run_id: &str) {
        let inner = Arc::clone(self);
        let run_id = run_id.to_owned();
        std::thread::spawn(move || inner.free_now(&run_id));
    }

    /// `free_now` for every run that has ended, one after another on a thread of its own. Catches
    /// one that ended before git was found, and one whose freeing the app quit in the middle of.
    fn free_ended_runs(self: &Arc<Self>) {
        let Ok(ended) = self.store.ended_run_ids() else {
            return;
        };
        let inner = Arc::clone(self);
        std::thread::spawn(move || {
            for run_id in ended {
                inner.free_now(&run_id);
            }
        });
    }

    /// Frees what a run that has ended no longer needs: whatever its tools built. Its transcripts,
    /// diffs and logs are outside the workspace and stay, and so does the copy with its commits, so
    /// a step tried there again builds what it needs anew. A folder git keeps any file in is the
    /// project's own and stays too, and so does everything while git isn't found yet: `recover`
    /// comes back for it. Each folder is moved out of the copy at once and deleted after, and only
    /// while the run is still ended, so a run a person opens again stops being freed.
    fn free_now(&self, run_id: &str) {
        let Some(git) = locked(&self.tools).git.clone().map(Git::new) else {
            return;
        };
        let run_dir = self.run_dir(run_id);
        let (workspace, trash) = (run_dir.join("workspace"), run_dir.join(TRASH));
        if workspace.join(".git").exists() {
            for name in built_folders(&workspace) {
                if git.tracks(&workspace, &name) {
                    continue;
                }
                // What `retry` holds while it opens the run again and starts the step.
                let _guard = locked(&self.advancing);
                if !self.may_free(run_id, &name) {
                    continue;
                }
                let _ = std::fs::create_dir_all(&trash);
                let _ = std::fs::rename(workspace.join(&name), trash.join(new_id()));
            }
        }
        for entry in std::fs::read_dir(&trash).into_iter().flatten().flatten() {
            remove_all(&entry.path());
        }
        let _ = std::fs::remove_dir(&trash);
    }

    /// Whether `name` can go from the run's copy now. Not while the run is open. A trial that failed
    /// is closed to be tried again from its page, without setup, so what setup installed stays.
    fn may_free(&self, run_id: &str, name: &str) -> bool {
        let Ok(run) = self.store.run(run_id) else {
            return false;
        };
        if run.finished_at.is_none() {
            return false;
        }
        if !SETUP_INSTALLS.contains(&name) || !is_trial(&run) {
            return true;
        }
        let latest = self
            .store
            .attempts_of_run(run_id)
            .ok()
            .and_then(|attempts| attempts.last().map(|a| a.state.clone()));
        latest.is_some_and(|state| {
            matches!(
                AttemptState::parse(&state),
                Some(AttemptState::Succeeded | AttemptState::Skipped)
            )
        })
    }

    /// Where a change too big for a prompt is saved, one file per changed file, for an agent to
    /// read. It's outside the workspace, so nothing in it can end up in a commit, and no step that
    /// changes the workspace can change it.
    fn change_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("change")
    }

    /// What an agent can read outside its workspace and change nothing in: the other projects of a
    /// change across several, whose copies are made when the step starts, and the change itself if
    /// it was too big for a prompt.
    fn read_only_dirs(&self, run_id: &str, role: &StepRole) -> Result<Vec<String>> {
        let mut dirs: Vec<String> = if joint::reads_others(role) {
            self.read_copies(run_id)?
                .iter()
                .map(|copy| copy.path.to_string_lossy().into_owned())
                .collect()
        } else {
            Vec::new()
        };
        let change = self.change_dir(run_id);
        if change.is_dir() {
            dirs.push(change.to_string_lossy().into_owned());
        }
        Ok(dirs)
    }

    fn project_proposals_dir(&self) -> PathBuf {
        self.config.data_dir.join("project-proposals")
    }

    /// Not being able to tidy up doesn't undo adding the project, so this never fails. One left
    /// behind only adds what's already there.
    fn settle_project_proposals(&self, root: &str) {
        let dir = self.project_proposals_dir();
        let Ok(waiting) = project_proposals::load(&dir) else {
            return;
        };
        let mut settled = false;
        for proposal in waiting.iter().filter(|proposal| proposal.path == root) {
            settled |= project_proposals::delete(&dir, &proposal.id).is_ok();
        }
        if settled {
            self.changed("");
        }
    }

    fn drafts_dir(&self, project_id: &str) -> PathBuf {
        self.mirror(project_id).with_file_name("workflow-drafts")
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

    /// Records a step and sets it going. Every caller holds `advancing`, so this never takes it:
    /// the step itself runs on a thread of its own.
    fn start_step(
        self: &Arc<Self>,
        run_id: &str,
        step: &WorkflowStep,
        number: i64,
        resume: Option<Resume>,
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
                let text = match &resume {
                    Some(Resume { prompt: Some(prompt), .. }) => prompt.clone(),
                    Some(_) => RESUME_PROMPT.to_owned(),
                    None => self.agent_prompt(spec, &run)?,
                };
                let can_edit = spec.tools == keel_core::ToolProfile::Build;
                let mut agent_settings = settings::with_read_only(
                    settings::step_settings(
                        &workspace,
                        &project.allowed_domains,
                        &project.allowed_read,
                        // Only steps that run commands get anywhere near a service.
                        &if can_edit {
                            services::socket_dirs(&services::root_for(&project.id), &project.services)
                        } else {
                            Vec::new()
                        },
                    ),
                    &self.read_only_dirs(run_id, &spec.role)?,
                );
                // Builders changed files from Bash even when told to use Edit and Write, and an
                // edit there that matches nothing says nothing. Only a step that can edit has Bash.
                let git = locked(&self.tools).git.clone();
                if can_edit
                    && let (Some(program), Some(git)) = (&self.config.bash_hook, git)
                {
                    // Beside the workspace, where the sandbox can't write. The hook's git runs in
                    // the sandbox Keel's own git gets on a workspace, and without it, no hook.
                    let state = dir.join("hook");
                    let profile = state.join("git.sb");
                    let written = Git::new(git.clone())
                        .workspace_profile(&workspace)
                        .ok()
                        .and_then(|text| {
                            std::fs::create_dir_all(&state).ok()?;
                            std::fs::write(&profile, text).ok()
                        });
                    // Marked as used, so another Keel doesn't clear it away as an old build; and if
                    // one already did, the step goes without.
                    if written.is_some() && keel_harness::hook::touch(program) {
                        agent_settings = settings::with_bash_hook(
                            agent_settings,
                            &settings::BashHook {
                                program,
                                git: &git,
                                workspace: &workspace,
                                state: &state,
                                profile: &profile,
                            },
                        );
                    }
                }
                (
                    spec.model.clone(),
                    spec.effort.clone(),
                    spec.tools.as_str().to_owned(),
                    spec.max_turns,
                    spec.max_budget_usd,
                    text,
                    agent_settings.to_string(),
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
        let session_id = resume.as_ref().map_or_else(new_id, |r| r.session.clone());
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
            resumed_from: resume.as_ref().map(|r| r.from.clone()),
            workspace: workspace.to_string_lossy().into_owned(),
            raw_path: dir.join(raw_name).to_string_lossy().into_owned(),
            started_at: now(),
        })?;
        self.changed(run_id);
        if matches!(step, WorkflowStep::Approval { .. }) {
            self.store.append_event(&id, "waiting", &json!({}))?;
            self.look_at_earlier_parts(run_id);
            return Ok(id);
        }
        locked(&self.controls).insert(id.clone(), Control::default());
        let inner = self.clone();
        let step = step.clone();
        let attempt_id = id.clone();
        let project_id = project.id.clone();
        let resume = resume.map(|r| r.session);
        std::thread::spawn(move || inner.execute(attempt_id, project_id, step, resume));
        Ok(id)
    }

    /// An agent's prompt. What a person sent back has to reach whoever does the work, whatever a
    /// saved workflow's prompt leaves out.
    fn agent_prompt(&self, spec: &StepSpec, run: &keel_store::Run) -> Result<String> {
        let judging = matches!(
            spec.role,
            StepRole::Reviewer | StepRole::Verifier | StepRole::Approver { .. }
        );
        let mut text = self.render_prompt(&spec.prompt, run, judging)?;
        if spec.role == StepRole::Worker && spec.tools == keel_core::ToolProfile::Build {
            let attempts = self.store.attempts_of_run(&run.id)?;
            if workflows::returned_before(&attempts, run.round) {
                let rework = workflows::rework_text(&attempts, run.round, &run.language);
                if !text.contains(&rework) {
                    text.push_str(&format!("\n\n{rework}"));
                }
            }
        }
        // Every approver can say in which round something the change added appeared, so every one
        // is told which round this is, and what the builder said it added, whatever its prompt says.
        if matches!(spec.role, StepRole::Approver { .. }) {
            let attempts = self.store.attempts_of_run(&run.id)?;
            let builder = workflows::builder_text(&attempts, &run.language);
            let rounds = workflows::round_text(&attempts, run.round, &run.language);
            for part in [builder, rounds].iter().filter(|part| !part.is_empty()) {
                text.push_str(&format!("\n\n{part}"));
            }
        }
        Ok(text)
    }

    /// What a step's prompt becomes right now. The answers come from this run's latest round.
    /// `judging` is for an agent that decides on the change, which has to read all of it.
    fn render_prompt(
        &self,
        template: &str,
        run: &keel_store::Run,
        judging: bool,
    ) -> Result<String> {
        let attempts = self.store.attempts_of_run(&run.id)?;
        let steps: Vec<WorkflowStep> =
            serde_json::from_value(run.workflow.clone()).unwrap_or_default();
        let workspace = self.workspace(&run.id);
        let base = attempts.iter().find_map(|a| a.sha_before.clone());
        let git = locked(&self.tools).git.clone().map(Git::new);
        Ok(prompts::render(template, |name| match name {
            "request" => Some(run.request.clone()),
            "diff" => {
                let (diff, head) = match (&git, &base) {
                    (Some(git), Some(base)) if workspace.join(".git").exists() => (
                        git.diff_since(&workspace, base).unwrap_or_default(),
                        git.head(&workspace).unwrap_or_default(),
                    ),
                    _ => (String::new(), String::new()),
                };
                // Named after the commit it goes up to, so each round has its own.
                let folder = self
                    .change_dir(&run.id)
                    .join(head.get(..12).unwrap_or(&head));
                Some(diffs::for_prompt(diff, &folder, &run.language, judging))
            }
            "rework" => Some(workflows::rework_text(&attempts, run.round, &run.language)),
            "answers" => Some(workflows::answers_text(&attempts, &run.language)),
            "projects" => self
                .agreement(&run.id)
                .ok()
                .flatten()
                .map(|agreement| agreement.projects_text(&run.language)),
            "check" => Some(verification::check_text(
                &verification::checks_in_round(&steps, &attempts, run.round),
                &run.language,
            )),
            _ => {
                let id = name.strip_prefix("steps.")?.strip_suffix(".output")?;
                Some(workflows::output_of(&attempts, id))
            }
        }))
    }

    /// Moves the run on as far as it can. Anything that isn't a success needs a person, and an
    /// approver or a person can send the work back, which reruns everything from the rework point.
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
            // Steps that work can be sent back to run again each round. Earlier ones don't.
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
                    .filter(|a| !sizing::due_again(a, &attempts))
            };
            let mut size = SizeThisPass::default();
            for (index, step) in steps.iter().enumerate() {
                if let WorkflowStep::Parallel { steps: group, .. } = step {
                    let changed = workflows::files_changed(&attempts);
                    let mut pending = false;
                    for spec in group {
                        match latest(&spec.id, index) {
                            None => {
                                let small = self
                                    .treated_as_small(&run, &steps, &attempts, spec, &mut size)?;
                                let id = if let Some(reason) = small {
                                    self.record_skipped(run_id, spec, &attempts, reason)?
                                } else if !workflows::paths_match(&spec.paths, &changed) {
                                    let reason = Reason::NoMatchingPaths {
                                        paths: spec.paths.clone(),
                                    };
                                    self.record_skipped(run_id, spec, &attempts, reason)?
                                } else {
                                    pending = true;
                                    let number = workflows::next_number(&attempts, &spec.id);
                                    self.start_step(
                                        run_id,
                                        &WorkflowStep::Agent(spec.clone()),
                                        number,
                                        None,
                                    )?
                                };
                                self.say_size(spec, &id, &run, &attempts, &mut size)?;
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
                    let conditional = match step {
                        WorkflowStep::Agent(spec) if spec.when.is_some() => Some(spec),
                        _ => None,
                    };
                    if let Some(spec) = conditional
                        && let Some(reason) =
                            self.treated_as_small(&run, &steps, &attempts, spec, &mut size)?
                    {
                        let id = self.record_skipped(run_id, spec, &attempts, reason)?;
                        self.say_size(spec, &id, &run, &attempts, &mut size)?;
                        continue 'round;
                    }
                    let number = workflows::next_number(&attempts, step.id());
                    let id = self.start_step(run_id, step, number, None)?;
                    if let Some(spec) = conditional {
                        self.say_size(spec, &id, &run, &attempts, &mut size)?;
                    }
                    return Ok(());
                };
                // Only a condition gets a step here that stands alone. It holds nothing up, and it has
                // no answer to act on: an approver that didn't run sends nothing back.
                if attempt.state == AttemptState::Skipped.as_str() {
                    continue;
                }
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
                    // A round a person asked for isn't the approver's to count.
                    let its_rounds = run.round - workflows::rounds_asked_by_a_person(&attempts);
                    if its_rounds < i64::from(*max_rounds) {
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
            // An agreement across projects has its parts on the board before it's over, or it waits
            // for the person to try again. A run that never closed comes back through here.
            if !self.open_parts(run_id, &steps, &attempts)? {
                return Ok(());
            }
            self.store
                .patch_run(run_id, &[("finished_at", Value::Text(now()))])?;
            self.free_builds(run_id);
            self.changed(run_id);
            return Ok(());
        }
    }

    /// Whether a step's condition says to skip it, and why, for the record.
    fn treated_as_small(
        &self,
        run: &keel_store::Run,
        steps: &[WorkflowStep],
        attempts: &[keel_store::Attempt],
        spec: &StepSpec,
        size: &mut SizeThisPass,
    ) -> Result<Option<Reason>> {
        let Some(StepCondition::NotSmall) = spec.when else {
            return Ok(None);
        };
        if size.decision.is_none() {
            size.decision = Some(self.size_of_run(run, steps, attempts)?);
        }
        Ok(match size.decision {
            Some(sizing::Decision::Small { lines, files }) => {
                Some(Reason::TreatedAsSmall { lines, files })
            }
            _ => None,
        })
    }

    /// Goes by the attempts and nothing kept in memory, so it holds across a restart.
    fn size_of_run(
        &self,
        run: &keel_store::Run,
        steps: &[WorkflowStep],
        attempts: &[keel_store::Attempt],
    ) -> Result<sizing::Decision> {
        if sizing::already_up(steps, attempts) {
            return Ok(sizing::Decision::StaysUp);
        }
        if sizing::person_asked(attempts) {
            return Ok(sizing::Decision::NotSmall(vec![
                keel_core::WhyNotSmall::PersonAsked,
            ]));
        }
        let project = self.store.project(&run.project_id)?;
        let commands: Vec<String> = steps
            .iter()
            .filter_map(|step| match step {
                WorkflowStep::Check { run, .. } | WorkflowStep::Setup { run, .. } => {
                    Some(run.clone())
                }
                _ => None,
            })
            .chain(project.setup_command.clone())
            .chain(project.check_command.clone())
            .collect();
        Ok(sizing::decide(&sizing::Facts {
            steps,
            attempts,
            round: run.round,
            issue: run.source_issue.as_ref(),
            account: project.github_account.as_deref(),
            started_by: run.started_by.as_deref(),
            sensitive: &project.sensitive_paths,
            commands: &commands,
            change: self.change_of_run(&run.id, attempts),
            part_of_joint_change: self.is_part(&run.id)?,
        }))
    }

    /// `None` if it can't be read, whatever the reason.
    fn change_of_run(
        &self,
        run_id: &str,
        attempts: &[keel_store::Attempt],
    ) -> Option<git::ChangeSize> {
        let base = attempts.iter().find_map(|a| a.sha_before.clone())?;
        let workspace = self.workspace(run_id);
        if !workspace.join(".git").exists() {
            return None;
        }
        let git = Git::new(locked(&self.tools).git.clone()?);
        git.change_size(&workspace, &base).ok()
    }

    /// Puts what was decided on the record, on the first step it applied to. Results count from this.
    /// Being small again later in the same round isn't a new decision.
    fn say_size(
        &self,
        spec: &StepSpec,
        attempt_id: &str,
        run: &keel_store::Run,
        attempts: &[keel_store::Attempt],
        size: &mut SizeThisPass,
    ) -> Result<()> {
        if spec.when.is_none() || size.said {
            return Ok(());
        }
        size.said = true;
        let told = attempts
            .iter()
            .any(|a| a.round == run.round && sizing::skipped_as_small(a));
        let data = match &size.decision {
            Some(sizing::Decision::Small { .. }) if told => None,
            Some(decision) => decision.on_record(run.round),
            None => None,
        };
        if let Some(data) = data {
            self.store.append_event(attempt_id, "sizeDecided", &data)?;
        }
        Ok(())
    }

    /// A step that isn't needed says so and why, on the record, instead of disappearing. The workflow
    /// the run keeps a copy of stays exactly as it was.
    fn record_skipped(
        &self,
        run_id: &str,
        spec: &StepSpec,
        attempts: &[keel_store::Attempt],
        reason: Reason,
    ) -> Result<String> {
        let run = self.store.run(run_id)?;
        let id = new_id();
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
        self.store.append_event(
            &id,
            "skipped",
            &json!({ "paths": spec.paths, "reason": reason }),
        )?;
        self.changed(run_id);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::ToolProfile;

    #[test]
    fn new_runs_use_runs_dir_and_old_ones_keep_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new(dir.path().join("Application Support"));
        config.runs_dir = dir.path().join("keel-runs");
        assert_eq!(
            run_dir_for(&config, "new"),
            dir.path().join("keel-runs/new")
        );
        let old = config.data_dir.join("runs").join("old");
        std::fs::create_dir_all(&old).unwrap();
        assert_eq!(run_dir_for(&config, "old"), old);
        assert_eq!(
            run_dir_for(&config, "new"),
            dir.path().join("keel-runs/new")
        );
    }

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
            when: None,
            role: Default::default(),
        });
        let back: WorkflowStep =
            serde_json::from_value(serde_json::to_value(&agent).unwrap()).unwrap();
        assert_eq!(back, agent);
    }
}
