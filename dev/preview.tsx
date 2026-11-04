// Renders the real screens with made-up data, without the backend, to check how they look in
// every state and language. `pnpm dev`, then open /dev/preview.html?view=run&state=stalled&lang=es
// Not part of the app: only index.html is bundled.
import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "../src/styles/app.css";
import "../src/i18n";

import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "../src/App";
import i18next from "../src/i18n";
import type { Attempt, AttemptState, AuditEvent, Preflight, Reason, Run, RunRow, WorkflowStep } from "../src/lib/api";
import { useApp } from "../src/lib/store";

const params = new URLSearchParams(location.search);
const view = params.get("view") ?? "home";
const state = (params.get("state") ?? "succeeded") as AttemptState;
const theme = params.get("theme") ?? "dark";
document.documentElement.dataset.theme = theme;
void i18next.changeLanguage(params.get("lang") ?? "en");

const REASONS: Partial<Record<AttemptState, Reason>> = {
  stalled: { code: "stalled", command: "uv run pytest -q", pid: 4242, minutes: 11, cpuPercent: 0.2 },
  blocked: { code: "unexpectedAuth", source: "ANTHROPIC_API_KEY" },
  failed: { code: "resultError", subtype: "error_max_turns", detail: "Reached the turn limit." },
  interrupted: { code: "agentLost" },
  queued: { code: "waitingForProject" },
};
const finished = !["queued", "preparing", "running", "stalled"].includes(state);
const at = (minutes: number) => new Date(Date.now() - minutes * 60000).toISOString();

const attempt: Attempt = {
  id: "a1",
  runId: "r1",
  stepId: "builder",
  kind: "agent",
  role: "worker",
  round: 0,
  output: null,
  runFiles: null,
  exitCode: null,
  number: 1,
  state,
  reason: REASONS[state] ?? null,
  modelRequested: "sonnet",
  modelActual: "claude-sonnet-5-5",
  effort: "high",
  tools: "build",
  language: "en",
  cliVersion: "2.1.318 (Claude Code)",
  apiKeySource: state === "blocked" ? "ANTHROPIC_API_KEY" : "none",
  account: "you@example.com (max)",
  prompt: "The add function in calc.py is wrong. Fix it, then run the tests.",
  settings: { sandbox: { enabled: true, failIfUnavailable: true, allowUnsandboxedCommands: false, filesystem: { denyRead: ["~"], allowRead: ["<workspace>"] }, network: { allowedDomains: [] } } },
  sessionId: "5e882d0f-0000-4000-8000-000000000000",
  workspace: "/Users/you/Library/Application Support/io.github.naitsric.keel/runs/r1/workspace",
  rawPath: "/Users/you/Library/Application Support/io.github.naitsric.keel/runs/r1/builder-1.jsonl",
  rawSha256: finished ? "9f2c4e1ab7d04c55e6a1f0b3c28d7e6fa1b2c3d4e5f60718293a4b5c6d7e8f90" : null,
  shaBefore: "12ee7430aa11bb22cc33dd44ee55ff6600112233",
  shaAfter: state === "succeeded" ? "28c696fd11223344556677889900aabbccddeeff" : null,
  filesChanged: state === "succeeded" ? ["calc.py", "tests/test_calc.py"] : null,
  resultText: state === "succeeded" ? "I fixed `add` in calc.py: it subtracted instead of adding.\n\n- Tests: `python3 test_calc.py` prints ok.\n- Commit: 28c696f on the run's branch.\n\nNothing for you to decide." : null,
  costUsd: finished ? 0.0034 : null,
  inputTokens: finished ? 18432 : null,
  outputTokens: finished ? 412 : null,
  turns: finished ? 6 : null,
  durationMs: finished ? 41000 : null,
  startedAt: at(state === "stalled" ? 14 : 3),
  finishedAt: finished ? at(1) : null,
  lastActivityAt: at(state === "stalled" ? 11 : 1),
};

const agentStep = (id: string, model: string, tools: "readOnly" | "build") => ({ type: "agent" as const, id, model, tools, effort: null, prompt: "Implement {{steps.spec.output}}", maxTurns: null, maxBudgetUsd: null });
const reviewerStep = (id: string, paths: string[]) => ({ id, model: "sonnet", tools: "readOnly" as const, effort: null, prompt: "Review {{diff}}", maxTurns: null, maxBudgetUsd: null, paths, role: { kind: "reviewer" as const } });
const workflow: WorkflowStep[] = [
  agentStep("spec", "opus", "readOnly"),
  agentStep("builder", "sonnet", "build"),
  { type: "check", id: "check", run: "make test" },
  { type: "parallel", id: "reviews", steps: [reviewerStep("security", []), reviewerStep("ux", ["**/*.tsx"]), reviewerStep("ui", ["**/*.tsx"]), reviewerStep("code", [])] },
  { ...agentStep("approver", "opus", "readOnly"), role: { kind: "approver", reworkTo: "builder", maxRounds: 2 } },
  { type: "approval", id: "approval" },
  { type: "deliver", id: "pull-request" },
];
const delivered = params.get("flow") === "delivered";
const waiting = params.get("flow") === "approval" || params.get("flow") === "limit" || delivered;
const specAttempt: Attempt = { ...attempt, id: "a0", stepId: "spec", tools: "readOnly", state: "succeeded", reason: null, modelActual: "claude-opus-5-5", filesChanged: null, resultText: "1. add returns the sum.\n2. The tests pass.", finishedAt: at(2) };
const builtAttempt: Attempt = { ...attempt, state: "succeeded", reason: null, shaAfter: "28c696fd11223344556677889900aabbccddeeff", finishedAt: at(1), rawSha256: "9f2c", resultText: "I fixed `add` in calc.py: it subtracted instead of adding.\n\nTests: `make test` passes.", filesChanged: params.get("workflows") === "1" ? ["calc.py", ".github/workflows/ci.yml"] : ["calc.py"] };
const checkAttempt: Attempt = { ...attempt, id: "a2", stepId: "check", kind: "check", tools: "check", state: "succeeded", reason: null, exitCode: 0, modelActual: null, modelRequested: "", prompt: "make test", filesChanged: null, resultText: null, finishedAt: at(1), rawSha256: "ab12" };
const approvalAttempt: Attempt = { ...attempt, id: "a3", stepId: "approval", kind: "approval", tools: "none", state: delivered ? "succeeded" : "waitingApproval", reason: null, modelRequested: "", modelActual: null, filesChanged: null, resultText: null, rawSha256: null, finishedAt: delivered ? at(0) : null };
const prAttempt: Attempt = { ...attempt, id: "a4", stepId: "pull-request", kind: "deliver", tools: "none", state: "succeeded", reason: null, modelRequested: "", modelActual: null, filesChanged: null, resultText: "https://github.com/acme/app/pull/12", rawSha256: null, finishedAt: at(0) };
const round = params.get("round") === "2" ? 1 : 0;
const reviewer = (id: string, verdict: "approved" | "changes", summary: string, findings: string[]): Attempt => ({ ...attempt, id: `r-${id}`, stepId: id, role: "reviewer", round, state: "succeeded", reason: null, tools: "readOnly", filesChanged: null, resultText: null, finishedAt: at(1), output: { verdict, findings, summary } as Attempt["output"] });
const skippedReviewer = (id: string): Attempt => ({ ...attempt, id: `r-${id}`, stepId: id, role: "reviewer", round, state: "skipped", reason: { code: "noMatchingPaths", paths: ["**/*.tsx"] }, tools: "readOnly", filesChanged: null, resultText: null, modelActual: null, finishedAt: at(1), output: null });
const approverAttempt = (decision: "approve" | "rework"): Attempt => ({ ...attempt, id: "r-approver", stepId: "approver", role: "approver", round, state: params.get("flow") === "limit" ? "blocked" : "succeeded", reason: params.get("flow") === "limit" ? { code: "reworkLimit", rounds: 2 } : null, tools: "readOnly", modelActual: "claude-opus-5-5", filesChanged: null, resultText: null, finishedAt: at(1), output: { decision, risk: "medium", reasons: "The security finding is real: the new endpoint skips the role check.", instructions: "Add the role check." } as Attempt["output"] });
const reviews: Attempt[] = [
  reviewer("security", params.get("findings") === "1" ? "changes" : "approved", params.get("findings") === "1" ? "The new endpoint skips the role check." : "No problems in authorization, logs or input handling.", params.get("findings") === "1" ? ["app/api.py:42 doesn't call require_role"] : []),
  skippedReviewer("ux"),
  skippedReviewer("ui"),
  reviewer("code", "approved", "It meets each criterion, and the tests prove it.", []),
  approverAttempt(params.get("findings") === "1" ? "rework" : "approve"),
];
const attemptsList: Attempt[] = params.get("flow") === "limit" ? [specAttempt, builtAttempt, checkAttempt, ...reviews] : waiting ? [specAttempt, builtAttempt, checkAttempt, ...reviews, approvalAttempt, ...(delivered ? [prAttempt] : [])] : [attempt];
const run: Run = {
  id: "r1",
  projectId: "p1",
  title: "#7 The add function in calc.py is wrong",
  baseRef: "main",
  request: "Fix add",
  workflow: waiting ? workflow : [agentStep("builder", "sonnet", "build")],
  language: "en",
  sourceIssue: waiting ? { number: 7, title: "add is wrong", author: "someone", url: "https://github.com/acme/app/issues/7" } : null,
  branch: "keel/5e882d0f",
  prUrl: delivered ? "https://github.com/acme/app/pull/12" : null,
  prNumber: delivered ? 12 : null,
  round: params.get("round") === "2" ? 1 : 0,
  createdAt: at(4),
  finishedAt: delivered ? at(0) : null,
};

const event = (seq: number, kind: string, data: Record<string, unknown>): AuditEvent => ({ seq, attemptId: "a1", at: at(3), kind, data });
const events: AuditEvent[] = [
  event(1, "started", { args: ["-p", "--output-format", "stream-json", "--verbose", "--restricted", "--strict-mcp-config", "--tools", "Read,Grep,Glob,Edit,Write,Bash", "--permission-mode", "dontAsk", "--model", "sonnet"], systemPrompt: "# Working inside Keel\n\nYou are running one step of a software workflow, in a private clone of the project." }),
  event(2, "init", { model: "claude-sonnet-5-5", tools: ["Read", "Bash"], apiKeySource: "none" }),
  event(3, "text", { text: "I'll read the function and its test first." }),
  event(4, "toolCall", { id: "t1", name: "Read", input: { file_path: "calc.py" } }),
  event(5, "toolResult", { id: "t1", isError: false, text: "1\tdef add(a, b):\n2\t    return a - b" }),
  event(6, "toolCall", { id: "t2", name: "Bash", input: { command: "python3 test_calc.py" } }),
  event(7, "toolResult", { id: "t2", isError: true, text: "Exit code 1\nAssertionError", exitCode: 1 }),
  event(8, "toolCall", { id: "t3", name: "Edit", input: { file_path: "calc.py" } }),
  event(9, "toolResult", { id: "t3", isError: false, text: "The file was updated." }),
  event(10, "toolCall", { id: "t4", name: "Bash", input: { command: state === "stalled" ? "uv run pytest -q" : "python3 test_calc.py" } }),
  ...(state === "stalled" || state === "running" ? [] : [event(11, "toolResult", { id: "t4", isError: false, text: "ok" })]),
  ...(state === "stalled" ? [event(12, "stalled", {})] : []),
  ...(finished ? [event(13, "result", {}), event(14, "finished", { state })] : []),
];

const DIFF = `diff --git a/calc.py b/calc.py
index 12ee743..4693ad3 100644
--- a/calc.py
+++ b/calc.py
@@ -1,2 +1,2 @@
 def add(a, b):
-    return a - b
+    return a + b`;

const preflight: Preflight = {
  claudeFound: true,
  gitFound: true,
  version: { status: "supported", found: "2.1.318" },
  auth: params.get("session") === "out" ? { loggedIn: false, method: null, provider: null, subscription: null, email: null, orgName: null, expiresAt: null } : { loggedIn: true, method: "claude.ai", provider: "firstParty", subscription: "max", email: "you@example.com", orgName: null, expiresAt: null },
  ignoredEnv: params.get("env") === "key" ? ["ANTHROPIC_API_KEY"] : [],
  blocker: params.get("session") === "out" ? { code: "claudeLoggedOut" } : null,
  ghFound: true,
};

const row = (id: string, title: string, s: AttemptState, reason: Reason | null, model: string, turns: number | null, minutes: number): RunRow => ({
  id,
  title,
  prUrl: s === "succeeded" ? "https://github.com/acme/app/pull/12" : null,
  projectId: "p1",
  projectName: "demo",
  stepId: "builder",
  attemptId: id,
  attemptNumber: s === "running" ? 2 : 1,
  state: s,
  reason,
  model,
  turns,
  startedAt: at(minutes),
  finishedAt: ["running", "stalled", "queued"].includes(s) ? null : at(minutes - 1),
  lastActivityAt: at(Math.max(minutes - 3, 0)),
});
const runs: RunRow[] =
  params.get("empty") === "1"
    ? []
    : [
        row("r2", "Add a retry to the payment webhook", "running", null, "claude-opus-5-5", 12, 6),
        row("r3", "The calendar repeats a holiday in 2030", "stalled", REASONS.stalled ?? null, "claude-sonnet-5-5", 4, 14),
        row("r4", "Review the login form", "queued", REASONS.queued ?? null, "haiku", null, 1),
        row("r5", "Rotate the staging keys", "blocked", REASONS.blocked ?? null, "claude-sonnet-5-5", 0, 40),
        row("r1", "The add function in calc.py is wrong. Fix it, then run the tests.", "succeeded", null, "claude-haiku-5-5", 6, 90),
      ];

mockWindows("main");
mockIPC((command) => {
  switch (command) {
    case "preflight":
    case "refresh_session":
      return preflight;
    case "overview":
      return { runs, projects: [{ id: "p1", name: "demo", path: "/Users/you/demo", githubAccount: params.get("account") === "none" ? null : "you-work", githubRepo: "acme/app", setupCommand: "uv sync --frozen", checkCommand: "make test", allowedDomains: ["pypi.org"], allowedRead: ["~/.cargo"], services: [{ type: "bridge", name: "db", port: 5432, socket: ".s.PGSQL.5432" }, { type: "command", name: "cache", start: "redis-server --unixsocket $KEEL_SOCKET --port 0", socket: null }], env: [{ name: "DATABASE_URL", value: "postgresql://postgres@/test?host={{socketDir:db}}" }] }], totals: { running: 1, retrying: 1, blocked: 2, tokens: 128400, durationMs: 1260000, costUsd: 1.87 } };
    case "run_get":
      return run;
    case "run_diff":
      return DIFF;
    case "workflows_list":
      return [
        { id: "builtin:full", name: i18next.language === "es" ? "Completo, con revisiones" : "Full, with reviews", builtin: true, steps: workflow },
        { id: "mine", name: i18next.language === "es" ? "Solo revisiones" : "Reviews only", builtin: false, steps: workflow.slice(3) },
      ];
    case "run_attempts":
      return attemptsList;
    case "github_status":
      return {
        repo: "acme/app",
        accounts: [
          { login: "you-personal", active: true, ok: true, scopes: ["repo"] },
          { login: "you-work", active: false, ok: true, scopes: ["repo", "admin:org", "workflow"] },
        ],
        info: { defaultBranch: "main", permission: "ADMIN", private: true, defaultBranchProtected: false },
        excessScopes: ["admin:org", "workflow"],
        error: null,
      };
    case "decisions_list":
      return [{ seq: 1, at: at(2000), answer: "reject", text: "Use the existing retry helper, not a new one." }];
    case "issues_list":
      return [
        { number: 7, title: "The add function in calc.py is wrong", author: "someone", url: "u", body: "b" },
        { number: 9, title: "Holiday repeated in 2030", author: "other", url: "u", body: "b" },
      ];
    case "attempt_events":
      return events;
    case "attempt_diff":
      return state === "succeeded" ? DIFF : "";
    case "attempt_raw_check":
      return params.get("raw") ?? (finished ? "intact" : "pending");
    default:
      return null;
  }
});

void useApp.getState().refreshOverview();
useApp.setState({ preflight });
if (view === "run") useApp.setState({ view: { name: "run", runId: "r1" } });
if (view === "settings") useApp.setState({ view: { name: "settings" } });
if (view === "workflows") useApp.setState({ view: { name: "workflows" } });
if (params.get("dialog") === "newRun") useApp.setState({ newRunOpen: true });
if (params.get("dialog") === "quit") useApp.setState({ quitOpen: true });

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
