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
  version: VersionCheck | null;
  auth: AuthStatus | null;
  ignoredEnv: string[];
  blocker: Reason | null;
}

export interface Project {
  id: string;
  name: string;
  path: string;
}

export interface RunRow {
  id: string;
  title: string;
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
  number: number;
  state: AttemptState;
  reason: Reason | null;
  modelRequested: string;
  modelActual: string | null;
  effort: string | null;
  tools: ToolProfile;
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
  stepId: string;
  prompt: string;
  model: string;
  effort: string | null;
  tools: ToolProfile;
  maxTurns: number | null;
  language: string;
}

export const api = {
  preflight: () => invoke<Preflight>("preflight"),
  refreshSession: () => invoke<Preflight>("refresh_session"),
  overview: () => invoke<Overview>("overview"),
  projectAdd: (path: string) => invoke<Project>("project_add", { path }),
  runStart: (request: RunRequest) => invoke<string>("run_start", { request }),
  runAttempts: (runId: string) => invoke<Attempt[]>("run_attempts", { runId }),
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
