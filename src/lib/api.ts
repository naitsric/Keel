import { invoke } from "@tauri-apps/api/core";

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
  | "rejected"
  | "skipped"
  | "abandoned";

export type ToolProfile = "readOnly" | "build";

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
  answer: "approve" | "reject";
  text: string;
}

export type StepRole = { kind: "worker" } | { kind: "reviewer" } | { kind: "approver"; reworkTo: string; maxRounds: number } | { kind: "mapper" } | { kind: "learner" };

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
  used: number;
  limit: number;
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

/** A reviewer's answer. */
export interface Verdict {
  verdict: "approved" | "changes";
  findings: string[];
  summary: string;
}

/** The approver's answer. */
export interface Decision_ {
  decision: "approve" | "rework";
  risk: "low" | "medium" | "high";
  reasons: string;
  instructions: string;
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

export interface Overview {
  runs: RunRow[];
  totals: Totals;
  projects: Project[];
  pendingProposals: number;
}

export interface Attempt {
  id: string;
  runId: string;
  stepId: string;
  kind: "agent" | "setup" | "check" | "approval" | "deliver";
  role: "worker" | "reviewer" | "approver" | "mapper" | "learner";
  round: number;
  output: (Verdict & Decision_ & { lines?: unknown[] }) | null;
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
  /** What the person wrote when rejecting. It may be empty. */
  rejection: string | null;
  startBlocked: Reason | null;
  createdAt: string;
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
  projectAdd: (path: string, language: string) => invoke<Project>("project_add", { path, language }),
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
  runDiff: (runId: string) => invoke<string>("run_diff", { runId }),
  runAddsSecret: (runId: string) => invoke<boolean>("run_adds_secret", { runId }),
  workflowsList: (projectId: string, language: string) => invoke<Workflow[]>("workflows_list", { projectId, language }),
  workflowSave: (projectId: string, workflow: Workflow) => invoke<Workflow>("workflow_save", { projectId, workflow }),
  workflowDelete: (projectId: string, workflowId: string) => invoke<void>("workflow_delete", { projectId, workflowId }),
  reworkLimitAnswer: (attemptId: string, anotherRound: boolean) => invoke<void>("rework_limit_answer", { attemptId, anotherRound }),
  runAttempts: (runId: string) => invoke<Attempt[]>("run_attempts", { runId }),
  projectUpdate: (projectId: string, settings: ProjectSettings) => invoke<Project>("project_update", { projectId, settings }),
  githubStatus: (projectId: string, account: string | null) => invoke<GithubStatus>("github_status", { projectId, account }),
  mcpOverview: () => invoke<McpOverview>("mcp_overview"),
  mcpSettingsSave: (settings: McpSettings) => invoke<McpOverview>("mcp_settings_save", { settings }),
  mcpClientCreate: (name: string, canRun: boolean) => invoke<{ client: McpClient; token: string }>("mcp_client_create", { name, canRun }),
  mcpClientSetCanRun: (id: string, canRun: boolean) => invoke<void>("mcp_client_set_can_run", { id, canRun }),
  mcpClientRevoke: (id: string) => invoke<void>("mcp_client_revoke", { id }),
  mcpConnectClaude: (canRun: boolean) => invoke<McpOverview>("mcp_connect_claude", { canRun }),
  issuesList: (projectId: string) => invoke<Issue[]>("issues_list", { projectId }),
  decisionsList: (projectId: string) => invoke<Decision[]>("decisions_list", { projectId }),
  decisionRetire: (seq: number) => invoke<void>("decision_retire", { seq }),
  approvalAnswer: (attemptId: string, approve: boolean, text: string) => invoke<void>("approval_answer", { attemptId, approve, text }),
  openGithubUrl: (url: string) => invoke<void>("open_github_url", { url }),
  attemptEvents: (attemptId: string, afterSeq: number) => invoke<AuditEvent[]>("attempt_events", { attemptId, afterSeq }),
  attemptDiff: (attemptId: string) => invoke<string>("attempt_diff", { attemptId }),
  attemptRawCheck: (attemptId: string) => invoke<RawCheck>("attempt_raw_check", { attemptId }),
  attemptStop: (attemptId: string) => invoke<void>("attempt_stop", { attemptId }),
  attemptWait: (attemptId: string) => invoke<void>("attempt_wait", { attemptId }),
  attemptKillCommand: (attemptId: string) => invoke<void>("attempt_kill_command", { attemptId }),
  attemptRetry: (attemptId: string, resume: boolean) => invoke<string>("attempt_retry", { attemptId, resume }),
  attemptAbandon: (attemptId: string) => invoke<void>("attempt_abandon", { attemptId }),
  languageGet: () => invoke<string | null>("language_get"),
  languageSet: (language: string) => invoke<void>("language_set", { language }),
  openClaudeLogin: () => invoke<void>("open_claude_login"),
  revealRaw: (attemptId: string) => invoke<void>("reveal_raw", { attemptId }),
  appQuit: (stopRuns: boolean) => invoke<void>("app_quit", { stopRuns }),
  menuSet: (labels: Record<string, string>) => invoke<void>("menu_set", { labels }),
  boardGet: (projectId: string) => invoke<Board>("board_get", { projectId }),
  cardCreate: (projectId: string, title: string, body: string) => invoke<string>("card_create", { projectId, title, body }),
  cardImportIssue: (projectId: string, issue: Issue) => invoke<string>("card_import_issue", { projectId, issue }),
  cardUpdate: (cardId: string, title: string, body: string) => invoke<void>("card_update", { cardId, title, body }),
  cardMove: (cardId: string, lane: "backlog" | "todo", index: number, language: string) => invoke<void>("card_move", { cardId, lane, index, language }),
  cardDelete: (cardId: string) => invoke<void>("card_delete", { cardId }),
  maxAgentsGet: () => invoke<number>("max_agents_get"),
  maxAgentsSet: (value: number) => invoke<void>("max_agents_set", { value }),
};

export const ACTIVE_STATES: AttemptState[] = ["queued", "preparing", "running", "stalled"];

export const isActive = (state: AttemptState) => ACTIVE_STATES.includes(state);
