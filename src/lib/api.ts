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
  checkCommand: string | null;
  allowedDomains: string[];
  allowedRead: string[];
  services: Service[];
  env: EnvVar[];
}

export interface ProjectSettings {
  githubAccount: string | null;
  checkCommand: string | null;
  allowedDomains: string[];
  allowedRead: string[];
  services: Service[];
  env: EnvVar[];
}

export interface GithubAccount {
  login: string;
  active: boolean;
  ok: boolean;
  scopes: string[];
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

export type WorkflowStep =
  | { type: "agent"; id: string; model: string; tools: ToolProfile }
  | { type: "check"; id: string; run: string }
  | { type: "approval"; id: string }
  | { type: "deliver"; id: string };

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
  createdAt: string;
  finishedAt: string | null;
}

export type WorkflowChoice =
  | { mode: "full"; specModel: string; builderModel: string }
  | { mode: "single"; stepId: string; model: string; effort: string | null; tools: ToolProfile; maxTurns: number | null };

export interface RunRow {
  id: string;
  title: string;
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
}

export interface Attempt {
  id: string;
  runId: string;
  stepId: string;
  kind: "agent" | "check" | "approval" | "deliver";
  exitCode: number | null;
  number: number;
  state: AttemptState;
  reason: Reason | null;
  modelRequested: string;
  modelActual: string | null;
  effort: string | null;
  tools: ToolProfile | "check" | "none";
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

export const api = {
  preflight: () => invoke<Preflight>("preflight"),
  refreshSession: () => invoke<Preflight>("refresh_session"),
  overview: () => invoke<Overview>("overview"),
  projectAdd: (path: string) => invoke<Project>("project_add", { path }),
  runStart: (request: RunRequest) => invoke<string>("run_start", { request }),
  runGet: (runId: string) => invoke<Run>("run_get", { runId }),
  runAttempts: (runId: string) => invoke<Attempt[]>("run_attempts", { runId }),
  projectUpdate: (projectId: string, settings: ProjectSettings) => invoke<Project>("project_update", { projectId, settings }),
  githubStatus: (projectId: string) => invoke<GithubStatus>("github_status", { projectId }),
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
};

export const ACTIVE_STATES: AttemptState[] = ["queued", "preparing", "running", "stalled"];

export const isActive = (state: AttemptState) => ACTIVE_STATES.includes(state);
