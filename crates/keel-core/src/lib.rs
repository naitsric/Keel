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

/// One step as the user defined it, including its own model.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
