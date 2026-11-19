//! Types shared by the store, the harness, the engine and the UI.
//!
//! Nothing here holds user-facing text. Reasons are codes with data, and the UI translates
//! them, so the backend stays language-neutral.

mod secrets;

use serde::{Deserialize, Serialize};

pub use secrets::{MASK, Masked, diff_adds_secret, mask_secrets, mask_value, serialize_masked};

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
    /// Works out what a request will change, and may ask a person what only they can decide. It
    /// can't edit anything.
    Specifier,
    /// Says, for each thing the specifier said has to be true, whether a test really proves it. The
    /// engine corrects what it answers, and it can't edit anything or run anything.
    Verifier,
}

impl StepRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Approver { .. } => "approver",
            Self::Mapper => "mapper",
            Self::Learner => "learner",
            Self::Specifier => "specifier",
            Self::Verifier => "verifier",
        }
    }

    fn is_worker(&self) -> bool {
        matches!(self, Self::Worker)
    }
}

/// Something that has to hold for a step to run. A closed list, since the engine has to be able to
/// work out each one from facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepCondition {
    /// Only if the change isn't small. See `WhyNotSmall` for what small takes.
    NotSmall,
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
    /// Runs only if this holds. Only agents can have one, so a person's approval and a check are
    /// never skipped this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<StepCondition>,
    /// Runs only if the specification names one of these risks. Empty means always.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<String>,
    /// The model it runs with instead of `model` when the change is risky. The engine decides that
    /// from the specification and the change, once both exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_when_risky: Option<String>,
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
    /// A step that asked a person something and waits for what they say. Nothing is running.
    WaitingAnswers,
    /// A person rejected it at an approval step. The run ends here.
    Rejected,
    /// A person sent the work back at an approval step, with what to change. The run goes on.
    Returned,
    /// A person asked for what was skipped to run first. They didn't answer: the run goes on, and
    /// the approval comes round again.
    Postponed,
    /// Not needed for this change. Its reason says why.
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
            Self::WaitingAnswers => "waitingAnswers",
            Self::Rejected => "rejected",
            Self::Returned => "returned",
            Self::Postponed => "postponed",
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
            "waitingAnswers" => Self::WaitingAnswers,
            "rejected" => Self::Rejected,
            "returned" => Self::Returned,
            "postponed" => Self::Postponed,
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
    /// `claude` started but didn't say who's signed in, so Keel doesn't know.
    ClaudeNotResponding,
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
    /// A project runs one step with commands at a time, and another one has the turn: `step`, in
    /// the run `run_id`, whose title is `title`. Each run still works in its own copy. A reason
    /// saved before Keel said who has the turn has none of them, and reads back with them empty.
    WaitingForProject {
        #[serde(default)]
        step: String,
        #[serde(default)]
        run_id: String,
        #[serde(default)]
        title: String,
    },
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
    /// A check command ended with this code. `localhost_refused` when what it printed says the
    /// sandbox refused its tests a server on localhost, which the project's settings can open.
    /// Keel sends the work back to the builder by itself when a check runs and fails. `failed_times`
    /// is there when it didn't because the run had already gone back as often as it may: how many
    /// times this check has failed in the run, this one counted. `unchanged` when the builder changed
    /// nothing after it failed, so it didn't run again, and `exit_code` is the one it failed with.
    CheckFailed {
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        localhost_refused: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_times: Option<u32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unchanged: bool,
    },
    SetupFailed {
        exit_code: Option<i32>,
    },
    /// Accepting this would go over the room for decisions and learnings. One of them has to be
    /// retired first.
    BrainFull {
        used: usize,
        limit: usize,
    },
    /// The step ended, but Keel couldn't read what it changed.
    SnapshotFailed,
    /// A reviewer, approver or verifier finished without the answer it was asked for, or with one
    /// Keel can't use.
    NoStructuredOutput,
    /// The approver kept sending work back, up to the limit. A person decides now.
    ReworkLimit {
        rounds: u32,
    },
    NoMatchingPaths {
        paths: Vec<String>,
    },
    /// The specification names none of the risks the step looks at.
    NoMatchingRisks {
        risks: Vec<String>,
    },
    /// The engine found the change small, going by what it could see for itself, and the step only
    /// runs for one that isn't. `lines` and `files` are the size of the change at that moment.
    TreatedAsSmall {
        lines: usize,
        files: usize,
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
    /// A change can't be rejected without saying why: the reason is what later runs learn from.
    RejectionNeedsReason,
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
    /// The workflow a project starts its cards with no longer exists.
    DefaultWorkflowMissing,
    /// A project can't be removed while a run of its hasn't finished. `run_id` and `title` are the
    /// oldest one, and `count` is how many there are.
    ProjectHasOpenRuns {
        count: usize,
        run_id: String,
        title: String,
    },
    /// The project was removed from Keel, so nothing new can start on it until it's added again.
    ProjectRemoved,
    /// A specifier's answer broke its limits, and so did the answer it was given a chance to fix.
    SpecNotUsable {
        problems: Vec<SpecProblem>,
    },
    /// Keel couldn't read what it needed to check a specifier's answer, such as what a change across
    /// projects agreed, so the answer doesn't count. Trying again asks the agent again.
    SpecNotChecked {
        detail: String,
    },
    /// A person's correction of what the parts of a change agree on exactly breaks the rules the
    /// agreement's contract kept, so the contract stays as it was.
    ContractNotValid {
        problems: Vec<SpecProblem>,
    },
    /// An answer to a step's questions doesn't belong to them. `problem` is a code the UI
    /// translates: `unknownQuestion`, `repeatedQuestion`, `unanswered`, `optionNotOffered` or
    /// `emptyText`. `question` is the one it's about.
    InvalidAnswer {
        problem: String,
        question: String,
    },
    /// Work can't be sent back without saying what to change.
    SendBackNeedsInstructions,
    /// This approval has nothing before it that could do the work again.
    NothingToSendBackTo,
    /// Nothing was skipped for being a small change, so there's nothing to run now.
    NothingSkippedToRun,
    /// A specifier went on asking something with no recommended option after this many rounds of
    /// questions, the extra one included. Keel doesn't answer that for a person, so it stops.
    KeptAsking {
        rounds: usize,
    },
    /// What was agreed across projects was approved, and its parts couldn't be put on the board.
    PartsNotOpened {
        detail: String,
    },
    /// This issue's card is where a change across projects is agreed, which only the board starts.
    StartsOnTheBoard,
    /// A run that's part of a change across projects reads a copy of another project, and Keel
    /// couldn't get one: not from where the project lives, and not from an earlier copy.
    ReadCopyFailed {
        project: String,
        detail: String,
    },
    /// Only a setup or a check takes the project's commands as they are now. Any other step, a
    /// delivery included, retries as it is.
    NewCommandsNeedSetupOrCheck,
    /// An assistant put the card back in Todo, and the person has since revoked it or taken away
    /// its leave to start runs, so the card is back with them. `client` is its name.
    AssistantMayNotRun {
        client: String,
    },
    /// An assistant put the card back in Todo to run its last run again, and the saved workflow
    /// that run used, called `workflow`, was deleted meanwhile.
    RerunWorkflowMissing {
        workflow: String,
    },
}

/// Why a run doesn't count as small. Each is a fact the engine checked for itself, and any one is
/// enough. The UI puts them in words, and the results count them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "code",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WhyNotSmall {
    /// What was asked was worked out as not small, or didn't say, or there's nothing structured
    /// that could say.
    SpecNotSmall,
    /// Keel stopped asking and took a recommended option itself, so nobody really decided it.
    KeelChoseAnAnswer,
    /// What was asked names these risks. Whatever they say counts, known or not.
    SpecNamesRisks { risks: Vec<String> },
    /// The workflow has no check, so nothing proves the change works.
    NoCheck,
    /// It has one, and it hasn't passed on the code as it is in this round.
    CheckNotPassed,
    /// It passed, but its test tools said they skipped this many tests, which proved nothing.
    CheckSkippedTests { count: u32 },
    /// The run came from an issue this person wrote, who isn't the project's GitHub account.
    IssueFromSomeoneElse { author: String },
    /// The run came from an issue, and it doesn't say who wrote it.
    IssueFromUnknownAuthor,
    /// The run came from an issue, and the project has no GitHub account to compare its author with.
    IssueWithNoAccount,
    /// An assistant started the run over MCP, so no person wrote or vouched for what it asks.
    StartedByAssistant { client: String },
    /// The change touches these files, out of `total`, which are too sensitive to count as small.
    SensitiveFiles { files: Vec<String>, total: usize },
    /// A pattern in the project's list can't be read, so it can't be said that nothing matched it.
    BadSensitivePattern { pattern: String },
    /// The change has this many changed lines, and small has to be under `limit`.
    TooManyLines { lines: usize, limit: usize },
    /// These files, out of `total`, have changes git won't count in lines, so the size isn't known.
    FilesWithoutLines { files: Vec<String>, total: usize },
    /// The change couldn't be read.
    DiffUnreadable,
    /// A verifier said this many things aren't so, in this round.
    VerifierFoundUnmet { count: usize },
    /// The workflow has a verifier that hasn't looked at the code as it is in this round.
    VerifierHasNotRun,
    /// A person asked for what was skipped to run.
    PersonAsked,
    /// It's one part of a change across projects, and the parts only work together, which nobody
    /// can prove from one project.
    PartOfJointChange,
}

/// What's wrong with a specifier's answer. The UI and the prompt asking for a fix each put it in
/// their own words, so these only hold the facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "code",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SpecProblem {
    /// It isn't in the structure it was asked for.
    Unreadable,
    NoSummary,
    SummaryTooLong {
        chars: usize,
        limit: usize,
    },
    NoCriteria,
    CriterionNeedsText {
        id: String,
    },
    QuestionNeedsText {
        id: String,
    },
    TooManyChanges {
        count: usize,
        limit: usize,
    },
    TooManyLimits {
        count: usize,
        limit: usize,
    },
    TooManyCriteria {
        count: usize,
        limit: usize,
    },
    CriterionTooLong {
        id: String,
        chars: usize,
        limit: usize,
    },
    CriterionNeedsId {
        position: usize,
    },
    RepeatedCriterion {
        id: String,
    },
    TooManyDecided {
        count: usize,
        limit: usize,
    },
    DecidedTooLong {
        position: usize,
        chars: usize,
        limit: usize,
    },
    TooManyQuestions {
        count: usize,
        limit: usize,
    },
    QuestionTooLong {
        id: String,
        chars: usize,
        limit: usize,
    },
    QuestionNeedsId {
        position: usize,
    },
    RepeatedQuestion {
        id: String,
    },
    WrongNumberOfOptions {
        id: String,
        count: usize,
        min: usize,
        max: usize,
    },
    OptionTooLong {
        id: String,
        chars: usize,
        limit: usize,
    },
    RepeatedOption {
        id: String,
    },
    /// A question can recommend one option, or none when nothing backs one up.
    TooManyRecommended {
        id: String,
        count: usize,
    },
    /// Something was left marked as still to be decided.
    LeftUnresolved,
    /// An agreement across projects that changes none of them.
    NoParts,
    TooManyParts {
        count: usize,
        limit: usize,
    },
    /// A part in a project that isn't one of those the change touches.
    UnknownProject {
        project: String,
    },
    /// Two parts in one project.
    RepeatedPart {
        project: String,
    },
    TooManyTogether {
        count: usize,
        limit: usize,
    },
    TooManyContract {
        count: usize,
        limit: usize,
    },
    /// A point of the contract between the parts with no `when` or no `then`. Keel gives each point
    /// its `id`, so the agent that wrote it knows it by its `position`, from 1.
    ContractNeedsText {
        id: String,
        position: usize,
    },
    /// A point of the contract whose `when` and `then` together go past the limit of a criterion.
    ContractTooLong {
        id: String,
        position: usize,
        chars: usize,
        limit: usize,
    },
    /// A point of the contract names a project that isn't one of those the change touches.
    ContractUnknownProject {
        id: String,
        position: usize,
        project: String,
    },
    /// A point of the contract that names no project with a part, so no part would check it.
    ContractWithoutPart {
        id: String,
        position: usize,
    },
}

impl Reason {
    /// Whether a run blocked on this means any other card would end up the same way, so that
    /// starting more would only spend the queue. `now` is in seconds, like `resets_at`. A failure
    /// that belongs to one run or one project answers no: other cards can still get through.
    ///
    /// There's deliberately no catch-all, so a new reason has to be decided here.
    pub fn stops_every_card(&self, now: i64) -> bool {
        match self {
            // The usage is the account's, whichever project is asking.
            Self::RateLimited { resets_at } => resets_at.is_none_or(|at| at > now),
            // The session itself. The preflight can't see these: it reads what's on this Mac, which
            // can say signed in while the server refuses it.
            Self::SessionRejected { .. } => true,
            // Keel locks each launch down to its login and its exact tools, so getting anything else
            // comes from the CLI or the machine, not from this run.
            Self::UnexpectedAuth { .. } | Self::UnexpectedTools { .. } => true,
            // A lost GitHub session stops every run at preparation. The other kinds are about one
            // repo or one push.
            Self::DeliveryFailed { kind, .. } => kind == "notSignedIn",
            // Shared too, but the preflight looks at these live and lifts by itself once they're
            // fixed. An old attempt saying so must not outlast the fix.
            Self::ClaudeMissing
            | Self::GitMissing
            | Self::ClaudeLoggedOut
            | Self::ClaudeNotResponding
            | Self::CliVersionTooOld { .. }
            | Self::CliVersionUntested { .. } => false,
            // Starting refuses this itself, and those cards go to the backlog, where they can be
            // dragged again. It also only matters to workflows that deliver.
            Self::GhMissing => false,
            // One project: the next card of another project is fine.
            Self::NoGithubRemote
            | Self::GithubAccountMissing
            | Self::ServiceFailed { .. }
            | Self::InvalidSetting { .. }
            | Self::InvalidWorkflow { .. }
            | Self::DefaultWorkflowMissing
            | Self::ProjectHasOpenRuns { .. }
            | Self::ProjectRemoved => false,
            // One run.
            Self::Stalled { .. }
            | Self::WaitingForProject { .. }
            | Self::WaitingForCapacity
            | Self::ResultError { .. }
            | Self::ExitedWithoutResult { .. }
            | Self::PrepareFailed { .. }
            | Self::AgentLost
            | Self::CheckFailed { .. }
            | Self::SetupFailed { .. }
            | Self::BrainFull { .. }
            | Self::SnapshotFailed
            | Self::NoStructuredOutput
            | Self::ReworkLimit { .. }
            | Self::NoMatchingPaths { .. }
            | Self::NoMatchingRisks { .. }
            | Self::NothingToDeliver
            | Self::RejectionNeedsReason
            | Self::UnsafeBranch { .. }
            | Self::SpecNotUsable { .. }
            | Self::SpecNotChecked { .. }
            | Self::ContractNotValid { .. }
            | Self::InvalidAnswer { .. }
            | Self::SendBackNeedsInstructions
            | Self::NothingToSendBackTo
            | Self::TreatedAsSmall { .. }
            | Self::NothingSkippedToRun
            | Self::KeptAsking { .. }
            | Self::PartsNotOpened { .. }
            | Self::StartsOnTheBoard
            | Self::AssistantMayNotRun { .. }
            | Self::RerunWorkflowMissing { .. } => false,
            // One project that's only read. Its account being signed out must not hold up the rest.
            Self::ReadCopyFailed { .. } => false,
            Self::NewCommandsNeedSetupOrCheck => false,
        }
    }
}

/// What became of a pull request after Keel opened it, as GitHub tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrState {
    Open,
    Draft,
    Merged,
    /// Closed without merging. It can still be reopened.
    Closed,
}

impl PrState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Draft => "draft",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "draft" => Some(Self::Draft),
            "merged" => Some(Self::Merged),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

/// What a pull request's CI says about its latest commit, as GitHub tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CiState {
    /// Something hasn't finished, and nothing has failed yet.
    Pending,
    Passed,
    /// At least one check failed.
    Failed,
    /// The pull request has no checks.
    NoCi,
}

impl CiState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::NoCi => "noCi",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Pending, Self::Passed, Self::Failed, Self::NoCi]
            .into_iter()
            .find(|state| state.as_str() == value)
    }
}

/// Where a card is kept. A started card's column comes from its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CardLane {
    Backlog,
    Todo,
    Started,
}

impl CardLane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::Started => "started",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "backlog" => Some(Self::Backlog),
            "todo" => Some(Self::Todo),
            "started" => Some(Self::Started),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BoardColumn {
    Backlog,
    Todo,
    InProgress,
    HumanReview,
    Done,
}

/// The column a card shows in. `state` is where its run stands, and is `None` until the run has
/// an attempt. A card that has just been taken from Todo has no run yet, and is already working.
///
/// A run left open at a rejected or abandoned attempt won't move again: `advance` waits at anything
/// that didn't succeed. That takes a sibling's retry, or the app dying in the middle of ending the
/// run. A person has to look at it, and it mustn't hold a place in the queue forever.
pub fn card_column(lane: CardLane, run_finished: bool, state: Option<AttemptState>) -> BoardColumn {
    use AttemptState::{
        Abandoned, Blocked, Failed, Interrupted, Rejected, Stalled, Stopped, WaitingAnswers,
        WaitingApproval,
    };
    match lane {
        CardLane::Backlog => BoardColumn::Backlog,
        CardLane::Todo => BoardColumn::Todo,
        CardLane::Started => match (run_finished, state) {
            (true, Some(Rejected | Abandoned)) => BoardColumn::Backlog,
            (true, _) => BoardColumn::Done,
            (
                false,
                Some(
                    WaitingApproval | WaitingAnswers | Blocked | Stalled | Failed | Stopped
                    | Interrupted | Rejected | Abandoned,
                ),
            ) => BoardColumn::HumanReview,
            (false, _) => BoardColumn::InProgress,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AttemptState::*;
    use BoardColumn::{Backlog as InBacklog, Done, HumanReview, InProgress, Todo as InTodo};

    /// Every state, with the run open and then finished.
    const STARTED: [(AttemptState, BoardColumn, BoardColumn); 16] = [
        (Queued, InProgress, Done),
        (Preparing, InProgress, Done),
        (Running, InProgress, Done),
        (Stalled, HumanReview, Done),
        (Succeeded, InProgress, Done),
        (Failed, HumanReview, Done),
        (Stopped, HumanReview, Done),
        (Interrupted, HumanReview, Done),
        (Blocked, HumanReview, Done),
        (WaitingApproval, HumanReview, Done),
        (WaitingAnswers, HumanReview, Done),
        (Rejected, HumanReview, InBacklog),
        // The work went back, and the next step is on its way.
        (Returned, InProgress, Done),
        // What was skipped is on its way, and the approval comes round again after it.
        (Postponed, InProgress, Done),
        (Skipped, InProgress, Done),
        (Abandoned, HumanReview, InBacklog),
    ];

    #[test]
    fn a_started_card_follows_its_run() {
        for (state, open, finished) in STARTED {
            assert_eq!(
                card_column(CardLane::Started, false, Some(state)),
                open,
                "{state:?} with the run open"
            );
            assert_eq!(
                card_column(CardLane::Started, true, Some(state)),
                finished,
                "{state:?} with the run finished"
            );
        }
    }

    #[test]
    fn the_table_above_has_every_state() {
        let states: Vec<&str> = STARTED.iter().map(|(s, _, _)| s.as_str()).collect();
        for state in &states {
            let parsed = AttemptState::parse(state).unwrap();
            // Adding a state stops this compiling until it's in the table.
            match parsed {
                Queued | Preparing | Running | Stalled | Succeeded | Failed | Stopped
                | Interrupted | Blocked | WaitingApproval | WaitingAnswers | Rejected
                | Returned | Postponed | Skipped | Abandoned => {}
            }
        }
        let mut unique = states.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), STARTED.len());
    }

    #[test]
    fn a_started_card_without_a_run_or_an_attempt_is_working() {
        assert_eq!(card_column(CardLane::Started, false, None), InProgress);
        assert_eq!(card_column(CardLane::Started, true, None), Done);
    }

    #[test]
    fn a_card_that_has_not_started_ignores_any_old_run() {
        for state in [None, Some(Running), Some(Failed), Some(Rejected)] {
            for finished in [false, true] {
                assert_eq!(card_column(CardLane::Backlog, finished, state), InBacklog);
                assert_eq!(card_column(CardLane::Todo, finished, state), InTodo);
            }
        }
    }

    fn delivery(kind: &str) -> Reason {
        Reason::DeliveryFailed {
            kind: kind.into(),
            detail: String::new(),
        }
    }

    /// Every reason, with whether it stops every card. `NOW` is well before the reset below.
    fn stopping_table() -> Vec<(Reason, bool)> {
        let text = String::new;
        vec![
            (Reason::ClaudeMissing, false),
            (Reason::GitMissing, false),
            (Reason::ClaudeLoggedOut, false),
            (
                Reason::CliVersionTooOld {
                    found: text(),
                    minimum: text(),
                },
                false,
            ),
            (
                Reason::CliVersionUntested {
                    found: text(),
                    below: text(),
                },
                false,
            ),
            (Reason::UnexpectedAuth { source: text() }, true),
            (
                Reason::UnexpectedTools {
                    expected: vec![],
                    found: vec![],
                },
                true,
            ),
            (Reason::SessionRejected { detail: text() }, true),
            (Reason::RateLimited { resets_at: None }, true),
            (
                Reason::Stalled {
                    command: None,
                    pid: None,
                    minutes: 1,
                    cpu_percent: 0.0,
                },
                false,
            ),
            (
                Reason::WaitingForProject {
                    step: text(),
                    run_id: text(),
                    title: text(),
                },
                false,
            ),
            (Reason::WaitingForCapacity, false),
            (
                Reason::ResultError {
                    subtype: text(),
                    detail: text(),
                },
                false,
            ),
            (Reason::ExitedWithoutResult { exit_code: None }, false),
            (Reason::PrepareFailed { detail: text() }, false),
            (Reason::AgentLost, false),
            (
                Reason::CheckFailed {
                    exit_code: None,
                    localhost_refused: true,
                    failed_times: None,
                    unchanged: false,
                },
                false,
            ),
            (Reason::SetupFailed { exit_code: None }, false),
            (Reason::BrainFull { used: 1, limit: 1 }, false),
            (Reason::SnapshotFailed, false),
            (Reason::NoStructuredOutput, false),
            (Reason::ReworkLimit { rounds: 1 }, false),
            (Reason::NoMatchingPaths { paths: vec![] }, false),
            (Reason::NoMatchingRisks { risks: vec![] }, false),
            (
                Reason::InvalidWorkflow {
                    problem: text(),
                    step: text(),
                },
                false,
            ),
            (Reason::NoGithubRemote, false),
            (Reason::GithubAccountMissing, false),
            (Reason::GhMissing, false),
            (delivery("notSignedIn"), true),
            (delivery("noPermission"), false),
            (delivery("sso"), false),
            (delivery("repoRules"), false),
            (delivery("signedCommits"), false),
            (delivery("workflowScope"), false),
            (delivery("other"), false),
            (Reason::NothingToDeliver, false),
            (Reason::RejectionNeedsReason, false),
            (Reason::UnsafeBranch { branch: text() }, false),
            (
                Reason::ServiceFailed {
                    name: text(),
                    detail: text(),
                },
                false,
            ),
            (
                Reason::InvalidSetting {
                    field: text(),
                    value: text(),
                },
                false,
            ),
            (Reason::DefaultWorkflowMissing, false),
            (Reason::ClaudeNotResponding, false),
            (
                Reason::ProjectHasOpenRuns {
                    count: 1,
                    run_id: text(),
                    title: text(),
                },
                false,
            ),
            (Reason::ProjectRemoved, false),
            (Reason::SpecNotUsable { problems: vec![] }, false),
            (Reason::SpecNotChecked { detail: text() }, false),
            (
                Reason::InvalidAnswer {
                    problem: text(),
                    question: text(),
                },
                false,
            ),
            (Reason::SendBackNeedsInstructions, false),
            (Reason::NothingToSendBackTo, false),
            (Reason::TreatedAsSmall { lines: 1, files: 1 }, false),
            (Reason::NothingSkippedToRun, false),
            (Reason::KeptAsking { rounds: 3 }, false),
            (Reason::PartsNotOpened { detail: text() }, false),
            (Reason::StartsOnTheBoard, false),
            (
                Reason::ReadCopyFailed {
                    project: text(),
                    detail: text(),
                },
                false,
            ),
            (Reason::NewCommandsNeedSetupOrCheck, false),
            (Reason::AssistantMayNotRun { client: text() }, false),
            (Reason::RerunWorkflowMissing { workflow: text() }, false),
            (Reason::ContractNotValid { problems: vec![] }, false),
        ]
    }

    /// Its place in the table. Adding a reason stops this compiling until it has a row.
    fn stopping_row(reason: &Reason) -> usize {
        match reason {
            Reason::ClaudeMissing => 0,
            Reason::GitMissing => 1,
            Reason::ClaudeLoggedOut => 2,
            Reason::CliVersionTooOld { .. } => 3,
            Reason::CliVersionUntested { .. } => 4,
            Reason::UnexpectedAuth { .. } => 5,
            Reason::UnexpectedTools { .. } => 6,
            Reason::SessionRejected { .. } => 7,
            Reason::RateLimited { .. } => 8,
            Reason::Stalled { .. } => 9,
            Reason::WaitingForProject { .. } => 10,
            Reason::WaitingForCapacity => 11,
            Reason::ResultError { .. } => 12,
            Reason::ExitedWithoutResult { .. } => 13,
            Reason::PrepareFailed { .. } => 14,
            Reason::AgentLost => 15,
            Reason::CheckFailed { .. } => 16,
            Reason::SetupFailed { .. } => 17,
            Reason::BrainFull { .. } => 18,
            Reason::SnapshotFailed => 19,
            Reason::NoStructuredOutput => 20,
            Reason::ReworkLimit { .. } => 21,
            Reason::NoMatchingPaths { .. } => 22,
            Reason::InvalidWorkflow { .. } => 23,
            Reason::NoGithubRemote => 24,
            Reason::GithubAccountMissing => 25,
            Reason::GhMissing => 26,
            Reason::DeliveryFailed { .. } => 27,
            Reason::NothingToDeliver => 28,
            Reason::RejectionNeedsReason => 29,
            Reason::UnsafeBranch { .. } => 30,
            Reason::ServiceFailed { .. } => 31,
            Reason::InvalidSetting { .. } => 32,
            Reason::DefaultWorkflowMissing => 33,
            Reason::ClaudeNotResponding => 34,
            Reason::ProjectHasOpenRuns { .. } => 35,
            Reason::ProjectRemoved => 36,
            Reason::SpecNotUsable { .. } => 37,
            Reason::InvalidAnswer { .. } => 38,
            Reason::SendBackNeedsInstructions => 39,
            Reason::NothingToSendBackTo => 40,
            Reason::TreatedAsSmall { .. } => 41,
            Reason::NothingSkippedToRun => 42,
            Reason::KeptAsking { .. } => 43,
            Reason::PartsNotOpened { .. } => 44,
            Reason::StartsOnTheBoard => 45,
            Reason::ReadCopyFailed { .. } => 46,
            Reason::SpecNotChecked { .. } => 47,
            Reason::NewCommandsNeedSetupOrCheck => 48,
            Reason::AssistantMayNotRun { .. } => 49,
            Reason::RerunWorkflowMissing { .. } => 50,
            Reason::NoMatchingRisks { .. } => 51,
            Reason::ContractNotValid { .. } => 52,
        }
    }

    #[test]
    fn every_reason_has_decided_whether_it_stops_every_card() {
        const NOW: i64 = 1_000;
        let table = stopping_table();
        for (reason, stops) in &table {
            assert_eq!(reason.stops_every_card(NOW), *stops, "{reason:?}");
        }
        let mut rows: Vec<usize> = table.iter().map(|(r, _)| stopping_row(r)).collect();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(rows, (0..=52).collect::<Vec<_>>(), "a row for each reason");
    }

    #[test]
    fn a_usage_limit_with_a_known_reset_stops_cards_until_then() {
        let limit = |resets_at| Reason::RateLimited { resets_at };
        assert!(limit(None).stops_every_card(1_000), "nobody knows when");
        assert!(limit(Some(1_001)).stops_every_card(1_000));
        assert!(!limit(Some(1_000)).stops_every_card(1_000));
        assert!(!limit(Some(999)).stops_every_card(1_000));
    }

    /// What the app and MCP clients read about who has a project's turn.
    #[test]
    fn waiting_for_the_project_names_the_step_and_run_that_have_the_turn() {
        let waiting = Reason::WaitingForProject {
            step: "builder".into(),
            run_id: "r44".into(),
            title: "Fix the calendar".into(),
        };
        assert_eq!(
            serde_json::to_value(&waiting).unwrap(),
            serde_json::json!({
                "code": "waitingForProject",
                "step": "builder",
                "runId": "r44",
                "title": "Fix the calendar",
            })
        );
    }

    /// #123: a check whose tests were refused localhost says so, for the app to point at the setting
    /// that opens it. Any other failed check reads as it always did, one saved before included.
    #[test]
    fn a_check_refused_localhost_says_so_and_any_other_reads_as_before() {
        let refused = Reason::CheckFailed {
            exit_code: Some(1),
            localhost_refused: true,
            failed_times: None,
            unchanged: false,
        };
        let value = serde_json::to_value(&refused).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "code": "checkFailed", "exitCode": 1, "localhostRefused": true })
        );
        assert_eq!(serde_json::from_value::<Reason>(value).unwrap(), refused);
        let plain = Reason::CheckFailed {
            exit_code: Some(1),
            localhost_refused: false,
            failed_times: None,
            unchanged: false,
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({ "code": "checkFailed", "exitCode": 1 })
        );
        let saved: Reason = serde_json::from_str(r#"{"code":"checkFailed","exitCode":3}"#).unwrap();
        assert_eq!(
            saved,
            Reason::CheckFailed {
                exit_code: Some(3),
                localhost_refused: false,
                failed_times: None,
                unchanged: false,
            }
        );
    }

    /// A failed check Keel left to the person says why, and one that says neither reads as before.
    #[test]
    fn a_failed_check_keel_left_to_the_person_says_why() {
        let at_the_limit = Reason::CheckFailed {
            exit_code: Some(1),
            localhost_refused: false,
            failed_times: Some(3),
            unchanged: false,
        };
        let value = serde_json::to_value(&at_the_limit).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "code": "checkFailed", "exitCode": 1, "failedTimes": 3 })
        );
        assert_eq!(
            serde_json::from_value::<Reason>(value).unwrap(),
            at_the_limit
        );
        let unchanged = Reason::CheckFailed {
            exit_code: Some(1),
            localhost_refused: false,
            failed_times: None,
            unchanged: true,
        };
        let value = serde_json::to_value(&unchanged).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "code": "checkFailed", "exitCode": 1, "unchanged": true })
        );
        assert_eq!(serde_json::from_value::<Reason>(value).unwrap(), unchanged);
    }

    /// A reason saved before Keel said who has the turn still reads, so its card says why it waits.
    #[test]
    fn waiting_for_the_project_saved_without_who_has_the_turn_still_reads() {
        let saved: Reason = serde_json::from_str(r#"{"code":"waitingForProject"}"#).unwrap();
        assert_eq!(
            saved,
            Reason::WaitingForProject {
                step: String::new(),
                run_id: String::new(),
                title: String::new(),
            }
        );
    }

    #[test]
    fn a_pull_request_state_reads_back_as_itself() {
        for state in [
            PrState::Open,
            PrState::Draft,
            PrState::Merged,
            PrState::Closed,
        ] {
            assert_eq!(PrState::parse(state.as_str()), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
        assert_eq!(PrState::parse("MERGED"), None);
    }

    #[test]
    fn a_ci_state_reads_back_as_itself() {
        for state in [
            CiState::Pending,
            CiState::Passed,
            CiState::Failed,
            CiState::NoCi,
        ] {
            assert_eq!(CiState::parse(state.as_str()), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
        assert_eq!(CiState::parse("FAILURE"), None);
    }

    #[test]
    fn a_specifier_is_a_role_of_its_own_and_states_read_back_as_themselves() {
        assert_eq!(StepRole::Specifier.as_str(), "specifier");
        assert_eq!(StepRole::Verifier.as_str(), "verifier");
        for state in [
            AttemptState::WaitingAnswers,
            AttemptState::Returned,
            AttemptState::Postponed,
        ] {
            assert_eq!(AttemptState::parse(state.as_str()), Some(state));
            assert!(!state.is_active(), "nothing is running");
        }
    }

    fn a_step() -> StepSpec {
        StepSpec {
            id: "a".into(),
            model: "opus".into(),
            effort: None,
            tools: ToolProfile::ReadOnly,
            prompt: "p".into(),
            max_turns: None,
            max_budget_usd: None,
            paths: vec![],
            role: StepRole::Reviewer,
            when: None,
            risks: vec![],
            model_when_risky: None,
        }
    }

    #[test]
    fn a_condition_only_shows_up_on_a_step_that_has_one() {
        let without = serde_json::to_value(a_step()).unwrap();
        assert_eq!(
            without.get("when"),
            None,
            "what was saved before stays as it was"
        );
        let with = StepSpec {
            when: Some(StepCondition::NotSmall),
            ..a_step()
        };
        let value = serde_json::to_value(&with).unwrap();
        assert_eq!(value["when"], "notSmall");
        assert_eq!(serde_json::from_value::<StepSpec>(value).unwrap(), with);
        assert_eq!(
            serde_json::from_value::<StepSpec>(without).unwrap(),
            a_step()
        );
    }

    #[test]
    fn risks_and_a_model_for_a_risky_change_only_show_up_on_a_step_that_has_them() {
        let without = serde_json::to_value(a_step()).unwrap();
        assert_eq!(without.get("risks"), None, "what was saved before stays");
        assert_eq!(without.get("modelWhenRisky"), None);
        let with = StepSpec {
            risks: vec!["concurrency".into()],
            model_when_risky: Some("opus".into()),
            ..a_step()
        };
        let value = serde_json::to_value(&with).unwrap();
        assert_eq!(value["risks"], serde_json::json!(["concurrency"]));
        assert_eq!(value["modelWhenRisky"], "opus");
        assert_eq!(serde_json::from_value::<StepSpec>(value).unwrap(), with);
    }

    #[test]
    fn a_condition_nobody_knows_is_refused_rather_than_taken_as_no_condition() {
        let mut value = serde_json::to_value(a_step()).unwrap();
        value["when"] = serde_json::json!("whenever");
        assert!(serde_json::from_value::<StepSpec>(value).is_err());
    }

    #[test]
    fn why_a_run_is_not_small_reads_as_a_code_with_its_data() {
        let why = WhyNotSmall::TooManyLines {
            lines: 512,
            limit: 400,
        };
        let value = serde_json::to_value(&why).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "code": "tooManyLines", "lines": 512, "limit": 400 })
        );
        assert_eq!(serde_json::from_value::<WhyNotSmall>(value).unwrap(), why);
    }

    #[test]
    fn a_lane_reads_back_as_itself() {
        for lane in [CardLane::Backlog, CardLane::Todo, CardLane::Started] {
            assert_eq!(CardLane::parse(lane.as_str()), Some(lane));
        }
        assert_eq!(CardLane::parse("inProgress"), None);
    }
}
