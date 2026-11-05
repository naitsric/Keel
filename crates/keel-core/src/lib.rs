//! Types shared by the store, the harness, the engine and the UI.
//!
//! Nothing here holds user-facing text. Reasons are codes with data, and the UI translates
//! them, so the backend stays language-neutral.

use serde::{Deserialize, Serialize};

/// What a step may do. Maps to the exact tools Claude Code gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolProfile {
    /// Reads code only. No shell, so it can't run anything.
    ReadOnly,
    /// Edits files and runs commands inside the sandbox.
    Build,
}

impl ToolProfile {
    pub fn tools(self) -> &'static [&'static str] {
        match self {
            Self::ReadOnly => &["Read", "Grep", "Glob"],
            Self::Build => &["Read", "Grep", "Glob", "Edit", "Write", "Bash"],
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "readOnly",
            Self::Build => "build",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "readOnly" => Some(Self::ReadOnly),
            "build" => Some(Self::Build),
            _ => None,
        }
    }
}

/// What an agent step is for. Reviewers and approvers return a structured answer Keel can act on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StepRole {
    #[default]
    Worker,
    /// Reads the change and returns a verdict with findings. It can't edit anything.
    Reviewer,
    /// Checks the reviewers' findings and decides. Sending work back reruns everything from
    /// `rework_to`, at most `max_rounds` times. Then a person decides.
    Approver { rework_to: String, max_rounds: u32 },
    /// Reads the repo and writes the project's map for the Brain.
    Mapper,
    /// Says what this run taught. Nothing it proposes reaches an agent until a person accepts it.
    Learner,
}

impl StepRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Approver { .. } => "approver",
            Self::Mapper => "mapper",
            Self::Learner => "learner",
        }
    }

    fn is_worker(&self) -> bool {
        matches!(self, Self::Worker)
    }
}

/// One agent step as the user defined it, including its own model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepSpec {
    pub id: String,
    pub model: String,
    pub effort: Option<String>,
    pub tools: ToolProfile,
    pub prompt: String,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    /// Runs only if the run changed files matching one of these globs. Empty means always.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "StepRole::is_worker")]
    pub role: StepRole,
}

/// Something a project's commands need, such as a test database. Keel runs these outside the
/// sandbox and gives steps one Unix socket per service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Service {
    /// Already listening on a local port, for example in Docker. Keel opens a socket that
    /// forwards to that port and nowhere else.
    Bridge {
        name: String,
        port: u16,
        socket: Option<String>,
    },
    /// A command Keel starts for the step. It must listen on `$KEEL_SOCKET`.
    Command {
        name: String,
        start: String,
        socket: Option<String>,
    },
}

impl Service {
    pub fn name(&self) -> &str {
        match self {
            Self::Bridge { name, .. } | Self::Command { name, .. } => name,
        }
    }

    /// The socket's file name, for clients that require one, like Postgres's `.s.PGSQL.5432`.
    pub fn socket_file(&self) -> Option<&str> {
        match self {
            Self::Bridge { socket, .. } | Self::Command { socket, .. } => socket.as_deref(),
        }
    }
}

/// A variable for a project's commands. These are saved in plain text and recorded in the audit
/// log, so they're for socket paths and test settings, not secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// One step of a workflow. There are few kinds on purpose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WorkflowStep {
    /// An agent working in the run's private copy.
    Agent(StepSpec),
    /// Installs what the project needs, once, before any agent runs: the code is still what was
    /// published. The one step with network, and still sandboxed.
    Setup { id: String, run: String },
    /// A command Keel runs itself, in a sandbox, with no model involved. Exit 0 passes.
    Check { id: String, run: String },
    /// Agents that run at the same time. Only read-only ones, since they share a workspace.
    Parallel { id: String, steps: Vec<StepSpec> },
    /// Waits for a person, for as long as it takes.
    Approval { id: String },
    /// Keel, not an agent, pushes the branch and opens a draft pull request.
    Deliver { id: String },
}

impl WorkflowStep {
    pub fn id(&self) -> &str {
        match self {
            Self::Agent(step) => &step.id,
            Self::Setup { id, .. }
            | Self::Check { id, .. }
            | Self::Parallel { id, .. }
            | Self::Approval { id }
            | Self::Deliver { id } => id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Agent(_) => "agent",
            Self::Setup { .. } => "setup",
            Self::Check { .. } => "check",
            Self::Parallel { .. } => "parallel",
            Self::Approval { .. } => "approval",
            Self::Deliver { .. } => "deliver",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttemptState {
    /// Waiting for the project or for a free agent slot.
    Queued,
    Preparing,
    Running,
    /// No progress for a while. Not final: Keel never decides this alone.
    Stalled,
    Succeeded,
    Failed,
    Stopped,
    /// Keel lost the process, for example because the app or the Mac went down.
    Interrupted,
    /// Can't continue until a person acts.
    Blocked,
    /// An approval step waiting for an answer. Nothing is running.
    WaitingApproval,
    /// A person rejected it at an approval step. The run ends here.
    Rejected,
    /// Not needed for this change: nothing it covers was touched.
    Skipped,
    Abandoned,
}

impl AttemptState {
    pub const ACTIVE: [Self; 4] = [Self::Queued, Self::Preparing, Self::Running, Self::Stalled];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Stalled => "stalled",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Interrupted => "interrupted",
            Self::Blocked => "blocked",
            Self::WaitingApproval => "waitingApproval",
            Self::Rejected => "rejected",
            Self::Skipped => "skipped",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "preparing" => Self::Preparing,
            "running" => Self::Running,
            "stalled" => Self::Stalled,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            "interrupted" => Self::Interrupted,
            "blocked" => Self::Blocked,
            "waitingApproval" => Self::WaitingApproval,
            "rejected" => Self::Rejected,
            "skipped" => Self::Skipped,
            "abandoned" => Self::Abandoned,
            _ => return None,
        })
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }
}

/// Why an attempt is in its state. The UI turns each code into a sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "code",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Reason {
    ClaudeMissing,
    GitMissing,
    ClaudeLoggedOut,
    CliVersionTooOld {
        found: String,
        minimum: String,
    },
    CliVersionUntested {
        found: String,
        below: String,
    },
    UnexpectedAuth {
        source: String,
    },
    UnexpectedTools {
        expected: Vec<String>,
        found: Vec<String>,
    },
    SessionRejected {
        detail: String,
    },
    RateLimited {
        resets_at: Option<i64>,
    },
    Stalled {
        command: Option<String>,
        pid: Option<i32>,
        minutes: u64,
        cpu_percent: f32,
    },
    WaitingForProject,
    WaitingForCapacity,
    ResultError {
        subtype: String,
        detail: String,
    },
    ExitedWithoutResult {
        exit_code: Option<i32>,
    },
    PrepareFailed {
        detail: String,
    },
    AgentLost,
    /// A check command ended with this code.
    CheckFailed {
        exit_code: Option<i32>,
    },
    SetupFailed {
        exit_code: Option<i32>,
    },
    /// Accepting this would go over what the Brain can send. Something has to be retired first.
    BrainFull {
        used: usize,
        limit: usize,
    },
    /// The step ended, but Keel couldn't read what it changed.
    SnapshotFailed,
    /// A reviewer or approver finished without the answer it was asked for.
    NoStructuredOutput,
    /// The approver kept sending work back, up to the limit. A person decides now.
    ReworkLimit {
        rounds: u32,
    },
    NoMatchingPaths {
        paths: Vec<String>,
    },
    /// `problem` is a code the UI translates, and `step` is the step it's about.
    InvalidWorkflow {
        problem: String,
        step: String,
    },
    NoGithubRemote,
    GithubAccountMissing,
    GhMissing,
    /// `kind` says what to fix: `notSignedIn`, `noPermission`, `sso`, `repoRules`,
    /// `signedCommits`, `workflowScope` or `other`.
    DeliveryFailed {
        kind: String,
        detail: String,
    },
    NothingToDeliver,
    /// Keel refuses to push to anything but its own branches.
    UnsafeBranch {
        branch: String,
    },
    ServiceFailed {
        name: String,
        detail: String,
    },
    /// A value in a project's settings isn't usable. `field` says which.
    InvalidSetting {
        field: String,
        value: String,
    },
}
