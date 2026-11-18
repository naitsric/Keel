import { invoke } from "@tauri-apps/api/core";
import type { MenuSpec } from "../menu/menuModel";

export type AttemptState =
  | "queued"
  | "preparing"
  | "running"
  | "stalled"
  | "succeeded"
  | "failed"
  | "stopped"
  | "interrupted"
  | "blocked"
  | "waitingApproval"
  | "waitingAnswers"
  | "rejected"
  | "returned"
  | "postponed"
  | "skipped"
  | "abandoned";

/**
 * What the app calls a state. `corrected` is a failed answer that Keel had fixed at once, and
 * `afterSetup` a step put off for a new setup to run first: see `shownState`.
 */
export type ShownState = AttemptState | "corrected" | "afterSetup";

export type ToolProfile = "readOnly" | "build";

/** What became of a pull request after Keel opened it, as of the last time it looked. */
export type PrState = "open" | "draft" | "merged" | "closed";

/** What a pull request's CI said about its latest commit, as of the last time Keel looked. */
export type CiState = "pending" | "passed" | "failed" | "noCi";

/** A code with its data. The backend sends no sentences; `reasonText` writes them. */
export type Reason = { code: string } & Record<string, unknown>;

export interface AuthStatus {
  loggedIn: boolean;
  method: string | null;
  provider: string | null;
  subscription: string | null;
  email: string | null;
  orgName: string | null;
  expiresAt: string | null;
}

export type VersionCheck =
  | { status: "supported"; found: string }
  | { status: "tooOld"; found: string; minimum: string }
  | { status: "untested"; found: string; below: string }
  | { status: "unknown" };

export interface Preflight {
  claudeFound: boolean;
  gitFound: boolean;
  ghFound: boolean;
  version: VersionCheck | null;
  auth: AuthStatus | null;
  ignoredEnv: string[];
  blocker: Reason | null;
}

export type Service =
  | { type: "bridge"; name: string; port: number; socket: string | null }
  | { type: "command"; name: string; start: string; socket: string | null };

export interface EnvVar {
  name: string;
  value: string;
}

export interface Project {
  id: string;
  name: string;
  path: string;
  githubAccount: string | null;
  githubRepo: string | null;
  setupCommand: string | null;
  checkCommand: string | null;
  allowedDomains: string[];
  allowedRead: string[];
  services: Service[];
  env: EnvVar[];
  /** What a board card runs. Null means the full built-in workflow. */
  defaultWorkflow: string | null;
  /** Too sensitive for a change to count as small, on top of what always is. */
  sensitivePaths: string[];
  /** Its tests start servers on localhost, so its check and its builder's commands may reach anything there. */
  allowLocalhost: boolean;
  /** Keel brings the repo's open issues with this label to the Backlog, and says on each when its run starts and when its pull request opens. Null: it watches none. */
  issueLabel: string | null;
}

/** Why Keel couldn't look at a project's issues, or something the person should know. */
export type IssueWatchProblem = { kind: "noRepo" } | { kind: "noAccount" } | { kind: "ghMissing" } | { kind: "labelMissing" } | { kind: "github"; detail: string };

/** What came of the last look at a project's issues since the app opened. */
export interface IssueWatch {
  label: string;
  at: string;
  /** Open issues with the label. */
  open: number;
  /** Of those, how many that look put in the Backlog. */
  brought: number;
  /** Cards it took off because their issue closed before they started. */
  removed: number;
  problem: IssueWatchProblem | null;
  /** The last comment on an issue that didn't work, until one does. */
  commentFailed: { number: number; detail: string } | null;
}

export interface ProjectSettings {
  githubAccount: string | null;
  setupCommand: string | null;
  checkCommand: string | null;
  allowedDomains: string[];
  allowedRead: string[];
  services: Service[];
  env: EnvVar[];
  defaultWorkflow: string | null;
  sensitivePaths: string[];
  allowLocalhost: boolean;
  issueLabel: string | null;
}

/** A folder in the home folder where programs a project's commands use are installed. */
export interface ToolFolder {
  /** As a person writes it: `~/.nvm`. */
  folder: string;
  programs: string[];
}

/** What a project's files give away. Nothing is saved until a person says so. */
export interface Guess {
  check: string | null;
  setup: string | null;
  from: string[];
  /** What else the setup command can be told to install, for a person to add. */
  setupOptions: string[];
  /** Other commands that check something here, for a person to add. */
  checkOptions: string[];
  /** It's what the project's CI runs, since nothing else here says how it installs and tests. */
  fromCi: boolean;
  /** Its check can't run with localhost closed: its tool talks to the processes it starts there. */
  localhost: boolean;
  /** What the check runs because the project's CI does, though nothing else here names it, as Keel runs it with no network. */
  ciChecks: string[];
  /** What else the CI checks, as it runs it, and why the check doesn't. */
  ciLeftOut: CiLeftOut[];
}

/** Why a command the CI runs isn't added to the check: Keel can't be sure it runs in its sandbox, with no network, as in CI. */
export type CiWhy = "offline" | "mayFail" | "env" | "sometimes" | "installs" | "template";

/** A command the CI runs, as it runs it, and why the check doesn't. */
export interface CiLeftOut {
  command: string;
  why: CiWhy;
}

/** What a project's CI checks and its saved check doesn't run. Keel never adds them by itself. */
export interface CiGap {
  /** As Keel runs them with no network, for a person to add. */
  commands: string[];
  /** As the CI runs them, and why a check doesn't. */
  leftOut: CiLeftOut[];
  /** The CI's files they're in. */
  from: string[];
}

export interface ProjectSetup {
  project: Project;
  guess: Guess;
}

export type MapStart = { status: "started" } | { status: "notStarted"; reason: Reason };

export interface ProjectAdded {
  project: Project;
  outcome: "new" | "existing" | "restored";
  /** Null when the folder was already in Keel, which starts nothing. */
  map: MapStart | null;
}

export interface GithubAccount {
  login: string;
  active: boolean;
  ok: boolean;
  scopes: string[];
}

export interface McpSettings {
  enabled: boolean;
  port: number;
}

export type McpStartError = { code: "portInUse"; port: number } | { code: "listen"; port: number; message: string };

export interface McpClient {
  id: string;
  name: string;
  tokenPrefix: string;
  canRun: boolean;
  /** May send work back and retry on the runs it started, and add a note to their cards while they wait. Only with `canRun`. */
  canSteer: boolean;
  /** How many of its runs may be open at once, counting those that wait for the person. */
  maxOpenRuns: number;
  createdAt: string;
  lastSeenAt: string | null;
  revokedAt: string | null;
}

export interface McpCall {
  seq: number;
  clientId: string;
  clientName: string;
  tool: string;
  args: string;
  ok: boolean;
  error: string | null;
  runId: string | null;
  at: string;
  durationMs: number;
}

export interface McpOverview {
  settings: McpSettings;
  status: { running: boolean; port: number | null; url: string | null; error: McpStartError | null };
  clients: McpClient[];
  calls: McpCall[];
}

export interface GithubStatus {
  repo: string | null;
  accounts: GithubAccount[];
  info: { defaultBranch: string; permission: string; private: boolean; defaultBranchProtected: boolean | null } | null;
  excessScopes: string[];
  error: Reason | null;
}

export interface Issue {
  number: number;
  title: string;
  author: string;
  url: string;
  body: string;
}

export interface Decision {
  seq: number;
  at: string;
  answer: "approve" | "reject" | "answered";
  text: string;
  /** What they were asked, when it's an answer they asked to have remembered. */
  question: string | null;
}

export type StepRole = { kind: "worker" } | { kind: "reviewer" } | { kind: "approver"; reworkTo: string; maxRounds: number } | { kind: "mapper" } | { kind: "learner" } | { kind: "specifier" } | { kind: "verifier" };

export type StepCondition = "notSmall";

export interface BrainNote {
  seq: number;
  projectId: string;
  kind: "map" | "learning";
  state: "proposed" | "active" | "dismissed" | "retired" | "replaced";
  text: string;
  evidence: string | null;
  paths: string[];
  commitSha: string | null;
  runId: string;
  attemptId: string;
  createdAt: string;
  decidedAt: string | null;
}

export interface NoteView extends BrainNote {
  stale: boolean;
  included: boolean;
}

export interface DecisionView extends Decision {
  included: boolean;
}

export interface BrainView {
  map: NoteView[];
  learnings: NoteView[];
  proposals: BrainNote[];
  decisions: DecisionView[];
  /** Decisions and learnings, and their room. */
  used: number;
  limit: number;
  /** The map has its own room. */
  mapUsed: number;
  mapLimit: number;
  prompt: string;
  mapRun: string | null;
}

export interface StepSpec {
  id: string;
  model: string;
  effort: string | null;
  tools: ToolProfile;
  prompt: string;
  maxTurns: number | null;
  maxBudgetUsd: number | null;
  paths?: string[];
  role?: StepRole;
  /** Runs only if this holds. Absent means always. */
  when?: StepCondition;
  /** Runs only if what was asked names one of these risks. Absent means always. */
  risks?: string[];
  /** The model it runs with instead when the change is risky. Absent keeps `model`. */
  modelWhenRisky?: string;
}

export type WorkflowStep =
  | ({ type: "agent" } & StepSpec)
  | { type: "setup"; id: string; run: string }
  | { type: "check"; id: string; run: string }
  | { type: "parallel"; id: string; steps: StepSpec[] }
  | { type: "approval"; id: string }
  | { type: "deliver"; id: string };

export interface Workflow {
  id: string;
  name: string;
  steps: WorkflowStep[];
  builtin: boolean;
}

/** A workflow an assistant proposed, which can't run until the person saves it. */
export interface WorkflowDraft {
  id: string;
  projectId: string;
  name: string;
  steps: WorkflowStep[];
  proposedBy: string;
  createdAt: string;
}

export interface SpecQuestion {
  id: string;
  text: string;
  options: { label: string; recommended: boolean }[];
}

export interface SpecDecided {
  text: string;
  /** Who settled it, if it was once a question: a person, or Keel when the rounds of them ran out. Or the request, in a list of what was already decided, which `text` is word for word. */
  taken?: "person" | "roundsLimit" | "request";
  /** The assistant that wrote that request for the person, when one did. */
  assistant?: string;
  /** Or the project's GitHub account, for an issue written with it, which isn't proven to be the person. */
  githubAccount?: string;
}

/** An answer to a question, as the engine keeps it. */
export interface GivenAnswer {
  question: string;
  asked: string;
  answer: string;
  recommended: boolean;
  remember: boolean;
}

/** A specifier's answer. */
export interface SpecOutput {
  /** Only ever a prediction. The engine decides from the change itself. */
  size?: "small" | "normal";
  risks?: string[];
  summary: string;
  criteria: { id: string; when: string; then: string }[];
  changes: { kind: "adds" | "changes" | "removes"; text: string }[];
  limits: string[];
  decided: SpecDecided[];
  questions: SpecQuestion[];
  /** What a person said to `questions`. Only the engine writes this. */
  answered?: GivenAnswer[];
  /** Only where a change across projects is agreed: what changes in each project, in order. */
  parts?: { project: string; text: string }[];
  /** Only there too: what only works with every part in place, for a person to try. */
  together?: string[];
  /** Only there too: what the parts agree on exactly. Keel numbers them K1, K2… and each part that a point names keeps it as one of its own. */
  contract?: { id: string; when: string; then: string; projects: string[] }[];
}

/** One thing the specifier said has to be true, in its words, and what became of it. */
export interface CheckedCriterion {
  id: string;
  when: string;
  then: string;
  result: "met" | "unmet" | "unverified";
  evidence: string;
  /** Why it isn't what the verifier said. Only the engine writes this. */
  corrected?: "noEvidence" | "noCheckPassed" | "leftOut";
  /** Why nothing proves it, as the verifier said. Answers from before it did don't say. */
  why?: "noTest" | "notRun";
}

/** A verifier's answer, once the engine has corrected it. */
export interface VerificationOutput {
  criteria: CheckedCriterion[];
  summary: string;
}

/** How a person answers one question. */
export interface QuestionAnswer {
  question: string;
  choice: { kind: "option"; label: string } | { kind: "own"; text: string };
  remember: boolean;
}

/** A folder an assistant proposed adding, as it is now. It isn't a project until the person adds it. */
export interface ProjectProposal {
  id: string;
  path: string;
  name: string;
  githubRepo: string | null;
  proposedBy: string;
  createdAt: string;
  /** It was in Keel and was removed, so it comes back with its history and starts no map. */
  returning: boolean;
  /** It isn't the root of a repo with commits anymore, so all that's left is to discard it. */
  gone: boolean;
}

/**
 * One problem a review found: where, what, and the id of the criterion it breaks, or `L<n>` for the
 * specification's limit number n. Older answers wrote only text.
 */
export type Finding = string | { file: string | null; line: number | null; text: string; breaks: string | null };

/** A reviewer's answer. */
export interface Verdict {
  verdict: "approved" | "changes";
  findings: Finding[];
  summary: string;
  /** Files of the change it couldn't read. Keel never counts such a review as an approval. Older answers don't have it. */
  unread?: string[];
  /** What it couldn't check by reading the code, such as how it looks on screen. Older answers don't have it. */
  unchecked?: string[];
}

/**
 * A line of what the specification decided on its own that a review questions, and why. Or else
 * something the change added that nothing asks for, which has no number and says its round.
 */
export interface Questioned {
  /** The line, quoted, maybe in other words. */
  decided: string;
  /** Its place in the list, from 1. Older answers don't have it, and an agent can get it wrong. */
  number?: number | null;
  /** For something the change added, the round it appeared in, from 1. Older answers don't have it. */
  round?: number | null;
  why: string;
}

/** The approver's answer. */
export interface Decision_ {
  decision: "approve" | "rework";
  risk: "low" | "medium" | "high";
  reasons: string;
  instructions: string;
  /** What the specification decided on its own, or the change added, that a review questions, for the person to settle. Older answers don't have it. */
  questioned?: Questioned[];
  overriddenByPerson?: boolean;
}

export interface Run {
  id: string;
  projectId: string;
  title: string;
  baseRef: string | null;
  request: string;
  workflow: WorkflowStep[];
  language: string;
  sourceIssue: { number: number; title: string; author: string; url: string } | null;
  branch: string | null;
  prUrl: string | null;
  prNumber: number | null;
  round: number;
  createdAt: string;
  finishedAt: string | null;
  /** The MCP client that started it. Null when a person did, in the app. */
  startedBy: string | null;
  /** The saved workflow its steps came from. Null if it had none, or is from before Keel kept this. */
  workflowId: string | null;
  workflowName: string | null;
  /** Null until Keel knows. */
  prState: PrState | null;
  /** When that happened, going by GitHub. */
  prStateAt: string | null;
  prCheckedAt: string | null;
  /** Null until Keel knows. */
  prCi: CiState | null;
  /** The checks that failed, by name. */
  prCiFailed: string[];
}

/** How a workflow's runs in a project have gone. */
export interface WorkflowResults {
  workflowId: string;
  /** What its latest run called it. */
  workflowName: string;
  runs: number;
  /** Those that ended, however they ended. */
  finished: number;
  merged: number;
  closed: number;
  /** Null if no pull request was merged. */
  costPerMergedUsd: number | null;
  /** Null if no run finished. */
  averageRounds: number | null;
  /** Runs that stopped to ask a person something. */
  stoppedToAsk: number;
  /** Questions a person answered, and how many of those got the recommended option. */
  answers: number;
  answersRecommended: number;
  /** How many times a person sent work back from what it decided alone. */
  sentBack: number;
  /** Of what had to be true, what the last verifier of each finished run left. */
  criteriaMet: number;
  criteriaUnmet: number;
  criteriaUnverified: number;
  /** Finished runs the engine last found small. */
  shortRoute: number;
  /** Runs it found weren't. */
  movedUp: number;
  /** Why they weren't, most common first. A run counts once per reason, and can have several. */
  movedUpBecause: { code: string; runs: number }[];
}

export type WorkflowChoice =
  | { mode: "saved"; workflowId: string }
  | { mode: "single"; stepId: string; model: string; effort: string | null; tools: ToolProfile; maxTurns: number | null };

export interface RunRow {
  id: string;
  title: string;
  startedBy: string | null;
  prUrl: string | null;
  projectId: string;
  projectName: string;
  stepId: string;
  attemptId: string;
  attemptNumber: number;
  state: AttemptState;
  reason: Reason | null;
  model: string;
  turns: number | null;
  startedAt: string;
  finishedAt: string | null;
  lastActivityAt: string | null;
}

export interface Totals {
  running: number;
  retrying: number;
  blocked: number;
  tokens: number;
  durationMs: number;
  costUsd: number;
}

/** A run's setup or check whose command the project changed after the run started. */
export interface CommandChange {
  stepId: string;
  kind: "setup" | "check";
  before: string;
  now: string;
}

export interface Overview {
  runs: RunRow[];
  totals: Totals;
  projects: Project[];
  pendingProposals: number;
  /** How many of them wait in each project that has any. */
  proposalsByProject: Record<string, number>;
  workflowDrafts: number;
  projectProposals: number;
}

export interface Attempt {
  id: string;
  runId: string;
  stepId: string;
  kind: "agent" | "setup" | "check" | "approval" | "deliver";
  role: "worker" | "reviewer" | "approver" | "mapper" | "learner" | "specifier" | "verifier";
  round: number;
  /** A returned approval has the `instructions` it went back with, and `by` if an assistant sent it. */
  output: (Verdict & Decision_ & { lines?: unknown[]; by?: string }) | null;
  runFiles: string[] | null;
  exitCode: number | null;
  number: number;
  state: AttemptState;
  reason: Reason | null;
  modelRequested: string;
  modelActual: string | null;
  effort: string | null;
  tools: ToolProfile | "setup" | "check" | "none";
  language: string;
  cliVersion: string | null;
  apiKeySource: string | null;
  account: string | null;
  prompt: string;
  settings: unknown;
  sessionId: string;
  /** The attempt whose session this one went on with. */
  resumedFrom: string | null;
  workspace: string;
  rawPath: string;
  rawSha256: string | null;
  shaBefore: string | null;
  shaAfter: string | null;
  filesChanged: string[] | null;
  resultText: string | null;
  costUsd: number | null;
  inputTokens: number | null;
  outputTokens: number | null;
  turns: number | null;
  durationMs: number | null;
  /** How many tests a check skipped, as its test tools said, though it may have passed. Null when Keel didn't recognise how they say it, and for anything that isn't a check. */
  skippedTests: number | null;
  startedAt: string;
  finishedAt: string | null;
  lastActivityAt: string | null;
}

export interface AuditEvent {
  seq: number;
  attemptId: string;
  at: string;
  kind: string;
  // Each kind has its own shape, as recorded by the engine.
  data: Record<string, any>;
}

export type RawCheck = "intact" | "changed" | "missing" | "pending";

export interface CommandError {
  code: string;
  reason: Reason | null;
  detail: string | null;
}

export interface RunRequest {
  projectId: string;
  prompt: string;
  issue: Issue | null;
  workflow: WorkflowChoice;
  language: string;
}

export type BoardColumn = "backlog" | "todo" | "inProgress" | "humanReview" | "done";

export interface BoardCard {
  id: string;
  projectId: string;
  title: string;
  body: string;
  issue: { number: number; title: string; author: string; url: string } | null;
  column: BoardColumn;
  position: number;
  runId: string | null;
  /** Of the latest attempt. Null if there's no run. */
  runState: AttemptState | null;
  /** Of the latest attempt. */
  reason: Reason | null;
  prUrl: string | null;
  prState: PrState | null;
  prStateAt: string | null;
  prCi: CiState | null;
  prCiFailed: string[];
  /** What the person wrote when rejecting. It may be empty. */
  rejection: string | null;
  startBlocked: Reason | null;
  createdAt: string;
  /** The change across several projects it's in, as the card where it's agreed or as a part. */
  jointChangeId: string | null;
  /** On the card where it's agreed: the ids of the other projects its request touches. */
  alsoTouches: string[];
  part: PartOf | null;
}

/** Something about an earlier part a person should know before approving this one. */
export interface PartWarning {
  kind: "closed" | "changed";
  position: number;
  project: string;
}

/** Where a card stands as one part of a change across projects. */
export interface PartOf {
  changeId: string;
  /** From 1. */
  position: number;
  total: number;
  /** The part it starts after, while it still waits for that one's pull request. */
  waitsFor: { cardId: string; position: number | null; project: string } | null;
  warnings: PartWarning[];
  /** The points of the contract it was specified with as they no longer are: corrected, added or removed since. Empty until it's specified. */
  contractChanged: string[];
}

export interface JointCard {
  cardId: string;
  projectId: string;
  project: string;
  title: string;
  column: BoardColumn;
  runId: string | null;
  runState: AttemptState | null;
  prUrl: string | null;
  prState: PrState | null;
  /** Null on the card where it's agreed. */
  part: PartOf | null;
}

export interface JointCheck {
  seq: number;
  text: string;
  /** When a person said it works. */
  confirmedAt: string | null;
}

/** A change across several projects, in one place. */
export interface JointChange {
  id: string;
  /** Null once that card is deleted. */
  agreedIn: JointCard | null;
  projects: { id: string; name: string }[];
  /** What a person approved. Null until then. */
  agreed: SpecOutput | null;
  parts: JointCard[];
  /** What it touches that no part changes. */
  readOnly: { id: string; name: string }[];
  checks: JointCheck[];
  /** What the parts agree on exactly, point by point, as a person corrected it since, if they did. Empty until it's agreed, and for an agreement without any. */
  contract: ContractPoint[];
  /** The number the next new point takes: K{contractNext}. Null until it's agreed. */
  contractNext: number | null;
}

/** A point the parts agree on exactly, and what each part that keeps it found. */
export interface ContractPoint {
  id: string;
  when: string;
  then: string;
  /** The projects it names, by the names they go by in what was agreed. */
  projects: string[];
  /** First to last. */
  parts: {
    cardId: string;
    position: number;
    project: string;
    /** What its verifier said of it in its run's latest round. Null until it said something. */
    result: CheckedCriterion["result"] | null;
    /** False once its run shows a workflow with no verifier, which never will. */
    verifies: boolean;
    /** Its part was specified with this point as it was before a person corrected it, or before it was added. Then `result` is null. */
    earlier: boolean;
  }[];
}

/** A point of the contract as a person corrects it: one already there by its id, a new one without. */
export interface ContractEdit {
  id: string | null;
  when: string;
  then: string;
  projects: string[];
}

/** What a run's page says about the change across projects it's in. */
export interface RunJoint {
  changeId: string;
  /** Whether this run is where it's agreed. */
  agrees: boolean;
  part: PartOf | null;
}

/** A picture a step saved of how the change looks. */
export interface Screenshot {
  name: string;
  /** What its bytes say it is: PNG, JPEG, GIF or WebP. */
  mime: string;
  /** Base64. */
  data: string;
  /** The step it was saved during. Keel took it from the folder when that step ended. */
  step: string;
  /** A step that changed the code ended with that one or after it, so it may not show what's being approved. */
  beforeLastChange: boolean;
}

/** This round's pictures. An earlier round's were for its own approval. */
export interface Screenshots {
  /** The latest step's first. */
  shown: Screenshot[];
  /** What else this round's steps saved and isn't shown: not one of those pictures, too big, a link, or past the limit. */
  leftOut: string[];
  /** How many more were left out than `leftOut` names. */
  moreLeftOut: number;
}

export interface Board {
  /** Ordered by position within each column. */
  cards: BoardCard[];
  /** Runs working now, across the whole app. */
  working: number;
  maxAgents: number;
  defaultWorkflow: string | null;
  /** Why nothing starts from Todo right now: a Claude session problem, or a failure every run would hit. */
  paused: Reason | null;
}

export const api = {
  preflight: () => invoke<Preflight>("preflight"),
  refreshSession: () => invoke<Preflight>("refresh_session"),
  overview: () => invoke<Overview>("overview"),
  projectAdd: (path: string, language: string) => invoke<ProjectAdded>("project_add", { path, language }),
  projectProposalsList: () => invoke<ProjectProposal[]>("project_proposals_list"),
  projectProposalAccept: (proposalId: string, language: string) => invoke<ProjectAdded>("project_proposal_accept", { proposalId, language }),
  projectProposalDiscard: (proposalId: string) => invoke<void>("project_proposal_discard", { proposalId }),
  projectSetup: (projectId: string) => invoke<ProjectSetup>("project_setup", { projectId }),
  /** Only reads the repo. */
  projectCiGap: (projectId: string) => invoke<CiGap>("project_ci_gap", { projectId }),
  projectToolFolders: (projectId: string, setup: string, check: string) => invoke<ToolFolder[]>("project_tool_folders", { projectId, setup, check }),
  /** `read` is the folders the commands may read on top of the project's, for this run only. */
  projectTryCheck: (projectId: string, setup: string, check: string, read: string[], localhost: boolean, language: string) => invoke<string>("project_try_check", { projectId, setup, check, read, localhost, language }),
  projectRemove: (projectId: string) => invoke<void>("project_remove", { projectId }),
  githubAccounts: () => invoke<GithubAccount[]>("github_accounts"),
  openGithubLogin: () => invoke<void>("open_github_login"),
  openHelp: (topic: "claude" | "git" | "gh") => invoke<void>("open_help", { topic }),
  brainGet: (projectId: string) => invoke<BrainView>("brain_get", { projectId }),
  brainGenerateMap: (projectId: string, model: string, language: string) => invoke<string>("brain_generate_map", { projectId, model, language }),
  brainAccept: (seq: number) => invoke<void>("brain_accept", { seq }),
  brainDismiss: (seq: number) => invoke<void>("brain_dismiss", { seq }),
  brainRetire: (seq: number) => invoke<void>("brain_retire", { seq }),
  runProposals: (runId: string) => invoke<BrainNote[]>("run_proposals", { runId }),
  runStart: (request: RunRequest) => invoke<string>("run_start", { request }),
  runGet: (runId: string) => invoke<Run>("run_get", { runId }),
  /** Asks Keel to see what became of the run's pull request. What it finds arrives as a change. */
  runLookAtPr: (runId: string) => invoke<void>("run_look_at_pr", { runId }),
  runDiff: (runId: string) => invoke<string>("run_diff", { runId }),
  runAddsSecret: (runId: string) => invoke<boolean>("run_adds_secret", { runId }),
  /** What a step saved of how the change looks, in `$KEEL_SCREENSHOTS`. Keel doesn't take them. */
  runScreenshots: (runId: string) => invoke<Screenshots>("run_screenshots", { runId }),
  workflowsList: (projectId: string, language: string) => invoke<Workflow[]>("workflows_list", { projectId, language }),
  workflowSave: (projectId: string, workflow: Workflow) => invoke<Workflow>("workflow_save", { projectId, workflow }),
  workflowResults: (projectId: string) => invoke<WorkflowResults[]>("workflow_results", { projectId }),
  workflowDelete: (projectId: string, workflowId: string) => invoke<void>("workflow_delete", { projectId, workflowId }),
  workflowDraftsList: () => invoke<WorkflowDraft[]>("workflow_drafts_list"),
  workflowDraftSave: (projectId: string, draftId: string, workflow: Workflow) => invoke<Workflow>("workflow_draft_save", { projectId, draftId, workflow }),
  workflowDraftDiscard: (projectId: string, draftId: string) => invoke<void>("workflow_draft_discard", { projectId, draftId }),
  reworkLimitAnswer: (attemptId: string, anotherRound: boolean) => invoke<void>("rework_limit_answer", { attemptId, anotherRound }),
  runAttempts: (runId: string) => invoke<Attempt[]>("run_attempts", { runId }),
  projectUpdate: (projectId: string, settings: ProjectSettings) => invoke<Project>("project_update", { projectId, settings }),
  githubStatus: (projectId: string, account: string | null) => invoke<GithubStatus>("github_status", { projectId, account }),
  mcpOverview: () => invoke<McpOverview>("mcp_overview"),
  mcpSettingsSave: (settings: McpSettings) => invoke<McpOverview>("mcp_settings_save", { settings }),
  mcpClientCreate: (name: string, canRun: boolean) => invoke<{ client: McpClient; token: string }>("mcp_client_create", { name, canRun }),
  mcpClientSetCanRun: (id: string, canRun: boolean) => invoke<void>("mcp_client_set_can_run", { id, canRun }),
  mcpClientSetCanSteer: (id: string, canSteer: boolean) => invoke<void>("mcp_client_set_can_steer", { id, canSteer }),
  mcpClientSetMaxOpenRuns: (id: string, max: number) => invoke<void>("mcp_client_set_max_open_runs", { id, max }),
  mcpClientRevoke: (id: string) => invoke<void>("mcp_client_revoke", { id }),
  mcpConnectClaude: (canRun: boolean) => invoke<McpOverview>("mcp_connect_claude", { canRun }),
  issuesList: (projectId: string) => invoke<Issue[]>("issues_list", { projectId }),
  issuesWatch: (projectId: string) => invoke<IssueWatch | null>("issues_watch", { projectId }),
  /** Looks at the project's issues now, as the background does every 2 minutes. */
  issuesLook: (projectId: string) => invoke<IssueWatch | null>("issues_look", { projectId }),
  decisionsList: (projectId: string) => invoke<Decision[]>("decisions_list", { projectId }),
  decisionRetire: (seq: number) => invoke<void>("decision_retire", { seq }),
  approvalAnswer: (attemptId: string, approve: boolean, text: string) => invoke<void>("approval_answer", { attemptId, approve, text }),
  approvalSendBack: (attemptId: string, instructions: string) => invoke<void>("approval_send_back", { attemptId, instructions }),
  /** Runs what was skipped for being a small change. The approval comes round again afterwards. */
  approvalRunSkipped: (attemptId: string) => invoke<void>("approval_run_skipped", { attemptId }),
  questionsAnswer: (attemptId: string, answers: QuestionAnswer[]) => invoke<void>("questions_answer", { attemptId, answers }),
  openGithubUrl: (url: string) => invoke<void>("open_github_url", { url }),
  attemptEvents: (attemptId: string, afterSeq: number) => invoke<AuditEvent[]>("attempt_events", { attemptId, afterSeq }),
  attemptDiff: (attemptId: string) => invoke<string>("attempt_diff", { attemptId }),
  attemptRawCheck: (attemptId: string) => invoke<RawCheck>("attempt_raw_check", { attemptId }),
  attemptStop: (attemptId: string) => invoke<void>("attempt_stop", { attemptId }),
  attemptWait: (attemptId: string) => invoke<void>("attempt_wait", { attemptId }),
  attemptKillCommand: (attemptId: string) => invoke<void>("attempt_kill_command", { attemptId }),
  /** With `currentCommands`, the run takes the project's setup and check as they are now. */
  attemptRetry: (attemptId: string, resume: boolean, currentCommands = false) => invoke<string>("attempt_retry", { attemptId, resume, currentCommands }),
  runCommandChanges: (runId: string) => invoke<CommandChange[]>("run_command_changes", { runId }),
  attemptAbandon: (attemptId: string) => invoke<void>("attempt_abandon", { attemptId }),
  languageGet: () => invoke<string | null>("language_get"),
  languageSet: (language: string) => invoke<void>("language_set", { language }),
  openClaudeLogin: () => invoke<void>("open_claude_login"),
  revealRaw: (attemptId: string) => invoke<void>("reveal_raw", { attemptId }),
  appQuit: (stopRuns: boolean) => invoke<void>("app_quit", { stopRuns }),
  menuSet: (menus: MenuSpec[]) => invoke<void>("menu_set", { menus }),
  /** Null for every project's cards on one board. */
  boardGet: (projectId: string | null) => invoke<Board>("board_get", { projectId }),
  cardCreate: (projectId: string, title: string, body: string) => invoke<string>("card_create", { projectId, title, body }),
  cardImportIssue: (projectId: string, issue: Issue) => invoke<string>("card_import_issue", { projectId, issue }),
  cardUpdate: (cardId: string, title: string, body: string) => invoke<void>("card_update", { cardId, title, body }),
  cardMove: (cardId: string, lane: "backlog" | "todo", index: number, language: string) => invoke<void>("card_move", { cardId, lane, index, language }),
  cardDelete: (cardId: string) => invoke<void>("card_delete", { cardId }),
  /** The other projects a card's request touches. None makes it an ordinary card again. */
  cardSetProjects: (cardId: string, projects: string[]) => invoke<void>("card_set_projects", { cardId, projects }),
  /** Lets a part start without the pull request of the one before it. */
  cardStopWaiting: (cardId: string) => invoke<void>("card_stop_waiting", { cardId }),
  jointChangeGet: (changeId: string) => invoke<JointChange>("joint_change_get", { changeId }),
  /** Only a person, from here, says what only works with every part in place works. */
  jointCheckSet: (changeId: string, seq: number, works: boolean) => invoke<void>("joint_check_set", { changeId, seq, works }),
  jointContractSet: (changeId: string, points: ContractEdit[]) => invoke<void>("joint_contract_set", { changeId, points }),
  runJoint: (runId: string) => invoke<RunJoint | null>("run_joint", { runId }),
  maxAgentsGet: () => invoke<number>("max_agents_get"),
  maxAgentsSet: (value: number) => invoke<void>("max_agents_set", { value }),
};

export const ACTIVE_STATES: AttemptState[] = ["queued", "preparing", "running", "stalled"];

export const isActive = (state: AttemptState) => ACTIVE_STATES.includes(state);
