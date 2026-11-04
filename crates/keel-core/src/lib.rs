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
            | Self::Approval { id }
            | Self::Deliver { id } => id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Agent(_) => "agent",
            Self::Setup { .. } => "setup",
            Self::Check { .. } => "check",
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
