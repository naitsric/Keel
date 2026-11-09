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
import { useSearchEverywhere, type SearchScope } from "../src/commands/SearchEverywhere";
import type { BrainNote, Attempt, AttemptState, AuditEvent, BoardCard, BoardColumn, Issue, Preflight, PrState, Project, ProjectAdded, Reason, Run, RunRow, WorkflowResults, WorkflowStep, WorkflowDraft } from "../src/lib/api";
import { useApp } from "../src/lib/store";

const params = new URLSearchParams(location.search);
const view = params.get("view") ?? "home";
// palette=all, actions or runs opens the search palette. q=<text> starts it on a search.
const palette = params.get("palette") as SearchScope | null;
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
// What the verifier found: verify=all|some|none|unmet with flow=approval|delivered, flow=verifying and
// flow=verifyUnreadable for the step itself, and verify=old to see one from an earlier round, which
// says nothing. node=verify opens what it found. long=1 has text at its limits.
const verifyMode = params.get("verify");
const verifyShown = verifyMode !== null || params.get("flow") === "verifying" || params.get("flow") === "verifyUnreadable";
const verifyStep: WorkflowStep = { ...agentStep("verify", "sonnet", "readOnly"), role: { kind: "verifier" } };
// The workflow that fits the change: flow=small is its approval card when the change was small, flow=asked
// the moment after pressing "Run them now", and flow=movedUp its approval card when it wasn't. why=all
// has every reason, with long=1 for long names. view=workflows shows it among the others, node=security
// its control, view=settings the field for a project's own list.
const fittingSteps: WorkflowStep[] = [
  { ...agentStep("spec", "opus", "readOnly"), role: { kind: "specifier" } },
  agentStep("builder", "sonnet", "build"),
  { type: "check", id: "check", run: "make test" },
  verifyStep,
  { type: "parallel", id: "reviews", steps: [{ ...reviewerStep("security", []), when: "notSmall" }, { ...reviewerStep("ux", ["**/*.tsx"]), when: "notSmall" }, { ...reviewerStep("ui", ["**/*.tsx"]), when: "notSmall" }, { ...reviewerStep("code", []), when: "notSmall" }] },
  { ...agentStep("approver", "opus", "readOnly"), role: { kind: "approver", reworkTo: "builder", maxRounds: 2 }, when: "notSmall" },
  { type: "approval", id: "approval" },
  { type: "deliver", id: "pull-request" },
];
const workflow: WorkflowStep[] = [
  { ...agentStep("spec", "opus", "readOnly"), ...(params.get("specifier") === "1" || params.get("flow") === "questions" || params.get("flow") === "corrected" ? { role: { kind: "specifier" as const } } : {}) },
  agentStep("builder", "sonnet", "build"),
  { type: "check", id: "check", run: "make test" },
  ...(verifyShown ? [verifyStep] : []),
  { type: "parallel", id: "reviews", steps: [reviewerStep("security", []), reviewerStep("ux", ["**/*.tsx"]), reviewerStep("ui", ["**/*.tsx"]), reviewerStep("code", [])] },
  { ...agentStep("approver", "opus", "readOnly"), role: { kind: "approver", reworkTo: "builder", maxRounds: 2 } },
  { type: "approval", id: "approval" },
  { type: "deliver", id: "pull-request" },
];
const delivered = params.get("flow") === "delivered";
const waiting = params.get("flow") === "approval" || params.get("flow") === "limit" || delivered;
// Variants of an approval: check=none, reviews=none, changes=none (the run changed nothing), long=1
// (text that would push the buttons away, what was asked included), gate=build (asks before building,
// where what was asked is already what the card shows), gate=end (never delivers). asked=none has no
// step that says what was asked. With flow=delivered, pr=draft|open|merged|closed is what became of the
// pull request, and pr=unknown is one Keel hasn't looked at yet. specifier=1 has what was asked come
// from a specifier's structured answer, with what it decided alone and a "Change" on each line, and
// with gate=build there's nothing yet to say about what it did. flow=questions is the card that asks,
// in the language of lang=, with long=1 for text at its limits, many=1 for three questions, none=1 for
// one with no recommended option, last=1 for the last round and extra=1 for the round after it.
// flow=corrected is a run stopped because it couldn't keep to the limits.
const noCheck = params.get("check") === "none";
const noReviews = params.get("reviews") === "none";
const noChanges = params.get("changes") === "none";
const long = params.get("long") === "1";
const gate = params.get("gate");
const LONG_TEXT = "I rebuilt how `add` handles its arguments in calc.py, because it subtracted instead of adding and every caller compensated for that in its own way, which meant changing the invoices, the reports and the export too. I kept the old signature so nothing else had to change.\n\n## Details\n\n" + "- Each caller now passes plain numbers, and the rounding moved into one helper that the tests cover directly.\n".repeat(30);
const LONG_FILES = Array.from({ length: 14 }, (_, i) => `src/modules/billing/invoices/generated/handlers/handler_${i}.py`);
const builtText = noChanges ? "I changed nothing. The bug in `add` is already fixed on main, in commit 8a3f21c, and `python3 test_calc.py` passes without touching the code.\n\nThere's no change to review. You can close this issue." : long ? LONG_TEXT : "I fixed `add` in calc.py: it subtracted instead of adding.\n\nTests: `make test` passes.";
const builtFiles = noChanges ? [] : [...(long ? LONG_FILES : ["calc.py"]), ...(params.get("workflows") === "1" ? [".github/workflows/ci.yml"] : [])];
const round = params.get("round") === "2" ? 1 : 0;
const askedLong = `The add function in calc.py has to return the sum of its two arguments, for whole numbers, negative numbers and decimals alike, because invoices, reports and the export all rely on it.\n\n${"Every caller keeps working as it does today, and the tests in test_calc.py prove each of these cases.\n".repeat(30)}`;
const spanish = params.get("lang") === "es";
const say = (en: string, es: string) => (spanish ? es : en);
const atLimit = (text: string, limit: number) => (long ? `${text} ${"x".repeat(limit)}`.slice(0, limit) : text);
const previewSpec = {
  summary: atLimit(say("`add` will return the sum of its two arguments, for whole numbers, negative numbers and decimals alike, and every caller keeps working as it does today without having to change.", "`add` va a devolver la suma de sus dos argumentos, para enteros, negativos y decimales por igual, y todos los que la llaman siguen funcionando como hoy sin tener que cambiar nada."), 200),
  criteria: [
    { id: "C1", when: say("add(1, 2) is called", "se llama add(1, 2)"), then: say("it returns 3", "devuelve 3") },
    { id: "C2", when: say("one argument is negative", "un argumento es negativo"), then: say("it still returns the sum", "igual devuelve la suma") },
  ],
  changes: [{ kind: "changes", text: say("add stops subtracting", "add deja de restar") }],
  limits: [say("Its signature stays the same", "Su firma sigue igual")],
  decided: [
    { text: say("Keeps the rounding as it is today, because nothing asked to change it.", "Deja el redondeo como está hoy, porque nada pidió cambiarlo.") },
    { text: say("If `whole` is 0, what should happen? Show an error, as today", "Si `whole` es 0, ¿qué debe pasar? Mostrar error, como hoy"), taken: "person" },
    ...(params.get("taken") === "1" ? [{ text: say("Should the total round up? Leave it as it is", "¿El total debe redondear hacia arriba? Dejarlo como está"), taken: "roundsLimit" }] : []),
  ],
  questions: [],
};
const useSpecifier = params.get("specifier") === "1";
const specAttempt: Attempt = { ...attempt, id: "a0", stepId: "spec", role: useSpecifier ? "specifier" : "worker", tools: "readOnly", state: "succeeded", reason: null, modelActual: "claude-opus-5-5", filesChanged: null, output: useSpecifier ? (previewSpec as unknown as Attempt["output"]) : null, resultText: useSpecifier ? "" : long ? askedLong : "1. add returns the sum.\n2. The tests pass.", finishedAt: at(2) };
const previewQuestions = [
  {
    id: "Q1",
    text: atLimit(say("If `whole` is 0, what should happen?", "Si `whole` es 0, ¿qué debe pasar con el total de la factura cuando el cliente ya pagó una parte del pedido?"), 140),
    options: [
      { label: atLimit(say("Show an error, as today", "Mostrar error, como hoy"), 60), recommended: true },
      { label: atLimit(say("Return 0", "Devolver 0"), 60), recommended: false },
    ],
  },
  ...(params.get("many") === "1" || long
    ? [
        { id: "Q2", text: atLimit(say("Who should get the warning when a payment fails?", "¿Quién debe recibir el aviso cuando un pago falla por segunda vez seguida en el mismo mes?"), 140), options: [{ label: atLimit(say("Only the account owner", "Solo la persona dueña de la cuenta"), 60), recommended: true }, { label: atLimit(say("Everyone on the account", "Todas las personas de la cuenta"), 60), recommended: false }, { label: atLimit(say("Nobody", "Nadie"), 60), recommended: false }] },
        { id: "Q3", text: atLimit(say("Should old invoices change too?", "¿Las facturas viejas también deben cambiar, o solo las que se emitan de ahora en adelante?"), 140), options: [{ label: atLimit(say("Only new ones", "Solo las nuevas"), 60), recommended: true }, { label: atLimit(say("All of them", "Todas"), 60), recommended: false }] },
      ]
    : []),
  ...(params.get("none") === "1" ? [{ id: "Q4", text: say("What discount do frequent customers get?", "¿Qué descuento reciben los clientes frecuentes?"), options: ["5 %", "10 %", "15 %"].map((label) => ({ label, recommended: false })) }] : []),
];
const specRound = (fields: Partial<Attempt>, output: object): Attempt => ({ ...attempt, id: "q1", stepId: "spec", role: "specifier", tools: "readOnly", reason: null, modelActual: "claude-opus-5-5", filesChanged: null, resultText: "", finishedAt: at(1), output: output as unknown as Attempt["output"], ...fields });
const answeredRounds = params.get("extra") === "1" ? 2 : params.get("last") === "1" ? 1 : 0;
const questionAttempts: Attempt[] = [
  ...Array.from({ length: answeredRounds }, (_, index) => specRound({ id: `q0-${index}`, number: index + 1, state: "succeeded" }, { ...previewSpec, questions: previewQuestions.slice(0, 1), answered: [{ question: "Q1", asked: previewQuestions[0].text, answer: previewQuestions[0].options[1].label, recommended: false, remember: false }] })),
  specRound({ number: answeredRounds + 1, state: "waitingAnswers" }, { ...previewSpec, decided: previewSpec.decided.slice(0, 1), questions: previewQuestions }),
];
const correctedAttempts: Attempt[] = [
  specRound({ id: "c1", number: 1, state: "failed", reason: { code: "specNotUsable", problems: [{ code: "summaryTooLong", chars: 240, limit: 200 }] } }, previewSpec),
  specRound({ id: "c2", number: 2, state: "blocked", reason: { code: "specNotUsable", problems: [{ code: "summaryTooLong", chars: 231, limit: 200 }, { code: "tooManyRecommended", id: "Q2", count: 2 }] } }, previewSpec),
];
const builtAttempt: Attempt = { ...attempt, state: "succeeded", reason: null, shaAfter: "28c696fd11223344556677889900aabbccddeeff", finishedAt: at(1), rawSha256: "9f2c", resultText: builtText, filesChanged: builtFiles, runFiles: builtFiles };
const checkAttempt: Attempt = { ...attempt, id: "a2", stepId: "check", kind: "check", tools: "check", round, state: "succeeded", reason: null, exitCode: 0, modelActual: null, modelRequested: "", prompt: "make test", filesChanged: null, resultText: null, finishedAt: at(1), rawSha256: "ab12" };
const approvalAttempt: Attempt = { ...attempt, id: "a3", stepId: "approval", kind: "approval", tools: "none", round, state: delivered ? "succeeded" : "waitingApproval", reason: null, modelRequested: "", modelActual: null, filesChanged: null, resultText: null, rawSha256: null, finishedAt: delivered ? at(0) : null };
const prAttempt: Attempt = { ...attempt, id: "a4", stepId: "pull-request", kind: "deliver", tools: "none", state: "succeeded", reason: null, modelRequested: "", modelActual: null, filesChanged: null, resultText: "https://github.com/acme/app/pull/12", rawSha256: null, finishedAt: at(0) };
const reworkAttempt: Attempt = { ...builtAttempt, id: "a1b", number: 2, round: 1, resultText: "I added the role check the security review asked for, in app/api.py.\n\nTests: `make test` passes." };
const reviewer = (id: string, verdict: "approved" | "changes", summary: string, findings: string[]): Attempt => ({ ...attempt, id: `r-${id}`, stepId: id, role: "reviewer", round, state: "succeeded", reason: null, tools: "readOnly", filesChanged: null, resultText: null, finishedAt: at(1), output: { verdict, findings, summary } as Attempt["output"] });
const skippedReviewer = (id: string): Attempt => ({ ...attempt, id: `r-${id}`, stepId: id, role: "reviewer", round, state: "skipped", reason: { code: "noMatchingPaths", paths: ["**/*.tsx"] }, tools: "readOnly", filesChanged: null, resultText: null, modelActual: null, finishedAt: at(1), output: null });
const approverAttempt = (decision: "approve" | "rework"): Attempt => ({ ...attempt, id: "r-approver", stepId: "approver", role: "approver", round, state: params.get("flow") === "limit" ? "blocked" : "succeeded", reason: params.get("flow") === "limit" ? { code: "reworkLimit", rounds: 2 } : null, tools: "readOnly", modelActual: "claude-opus-5-5", filesChanged: null, resultText: null, finishedAt: at(1), output: { decision, risk: params.get("risk") ?? "medium", reasons: long ? "The security finding is real: the new endpoint skips the role check, and the same pattern repeats in every handler. ".repeat(6) : "The security finding is real: the new endpoint skips the role check.", instructions: "Add the role check.", overriddenByPerson: params.get("override") === "1" } as Attempt["output"] });
const verifiedItems = () => {
  const longWords = (text: string, limit: number) => atLimit(text, limit);
  const item = (id: string, when: string, then: string, result: "met" | "unmet" | "unverified", evidence: string, corrected?: string) => ({ id, when: longWords(when, 100), then: longWords(then, 100), result, evidence: long ? `${evidence} ${"x".repeat(140)}`.slice(0, 140) : evidence, ...(corrected ? { corrected } : {}) });
  const C1 = [say("add(1, 2) is called", "se llama add(1, 2)"), say("it returns 3", "devuelve 3")] as const;
  const C2 = [say("one argument is negative", "un argumento es negativo"), say("it still returns the sum", "igual devuelve la suma")] as const;
  const C3 = [say("an argument is a decimal", "un argumento es decimal"), say("the rounding stays as it is today", "el redondeo sigue como hoy")] as const;
  const C4 = [say("the invoices page adds up a total", "la página de facturas suma un total"), say("the total is what it was", "el total es el mismo de antes")] as const;
  switch (verifyMode) {
    case "all":
      return [item("C1", ...C1, "met", "test_calc.py::test_add"), item("C2", ...C2, "met", "test_calc.py:14"), item("C3", ...C3, "met", "test_calc.py:21")];
    case "none":
      return [item("C1", ...C1, "unverified", ""), item("C2", ...C2, "unverified", "test_calc.py:14", "noCheckPassed"), item("C3", ...C3, "unverified", "", "noEvidence"), item("C4", ...C4, "unverified", "", "leftOut")];
    case "unmet":
      return [item("C1", ...C1, "met", "test_calc.py::test_add"), item("C2", ...C2, "unmet", "calc.py:3"), item("C3", ...C3, "unverified", ""), item("C4", ...C4, "unverified", "", "leftOut")];
    default:
      return [item("C1", ...C1, "met", "test_calc.py::test_add"), item("C2", ...C2, "met", "test_calc.py:14"), item("C3", ...C3, "met", "test_calc.py:21"), item("C4", ...C4, "unverified", "")];
  }
};
const verifierOutput = () => ({ criteria: verifiedItems(), summary: atLimit(say("Tests prove the sum works, and nothing covers the invoices page.", "Las pruebas demuestran que la suma funciona, y nada cubre la página de facturas."), 200) }) as unknown as Attempt["output"];
const verifierAttempt = (s: AttemptState, fields: Partial<Attempt> = {}): Attempt => ({ ...attempt, id: "v1", stepId: "verify", role: "verifier", round: verifyMode === "old" ? 0 : round, tools: "readOnly", state: s, reason: null, filesChanged: null, resultText: null, finishedAt: at(1), output: s === "succeeded" ? verifierOutput() : null, ...fields });
const verifiedAttempts = verifyMode !== null ? [verifierAttempt("succeeded")] : [];
const reviews: Attempt[] = [
  reviewer("security", params.get("findings") === "1" ? "changes" : "approved", params.get("findings") === "1" ? "The new endpoint skips the role check." : "No problems in authorization, logs or input handling.", params.get("findings") === "1" ? ["app/api.py:42 doesn't call require_role"] : []),
  skippedReviewer("ux"),
  skippedReviewer("ui"),
  reviewer("code", "approved", "It meets each criterion, and the tests prove it.", []),
  approverAttempt(params.get("findings") === "1" ? "rework" : "approve"),
];
const built = [builtAttempt, ...(round > 0 ? [reworkAttempt] : [])];
const gated = gate === "build" ? [specAttempt, approvalAttempt] : gate === "end" ? [specAttempt, builtAttempt, approvalAttempt] : null;
const asked = params.get("asked") === "none" ? [] : [specAttempt];
const attemptsList: Attempt[] = params.get("flow") === "limit" ? [specAttempt, builtAttempt, checkAttempt, ...reviews] : gated ?? (waiting ? [...asked, ...built, ...(noCheck ? [] : [checkAttempt]), ...verifiedAttempts, ...(noReviews ? [] : reviews), approvalAttempt, ...(delivered ? [prAttempt] : [])] : [attempt]);
const stepNamed = (id: string) => workflow.filter((step) => step.id === id);
const runWorkflow: WorkflowStep[] =
  gate === "build"
    ? [...stepNamed("spec"), ...stepNamed("approval"), ...stepNamed("builder"), ...stepNamed("pull-request")]
    : gate === "end"
      ? [...stepNamed("spec"), ...stepNamed("builder"), ...stepNamed("approval")]
      : workflow.filter((step) => !(noCheck && step.type === "check") && !(noReviews && (step.id === "reviews" || step.id === "approver")));
// The flow diagram's scenes, as flow=running|rework|returned|skipped|failed|many|unreadable|try. The others that
// show it: flow=approval|limit|delivered, and no flow at all for a single step. They all work on
// view=run, and flow=many also on view=workflows. width=900 is the narrowest the window gets.
// node=<a step's name> selects it once the page has loaded, and saveProblem=noPrompt:builder makes
// saving fail like that (with click=Save a copy to press it). Add theme=light to any of them.
/** `draft=late` has work happening after the person's approval, which the review has to call out. */
const draftsWaiting: WorkflowDraft[] = [
  {
    id: "d1",
    projectId: "p1",
    name: i18next.language === "es" ? "Arreglo con pruebas" : "Fix with tests",
    proposedBy: "Claude Code",
    createdAt: at(4),
    steps:
      params.get("draft") === "late"
        ? [{ type: "approval", id: "human" }, { ...agentStep("builder", "sonnet", "build"), prompt: "{{request}}" }, { type: "check", id: "tests", run: "make test" }, { type: "deliver", id: "pull-request" }]
        : [{ ...agentStep("builder", "sonnet", "build"), prompt: "{{request}}" }, { type: "check", id: "tests", run: "make test && make lint" }, { ...agentStep("review", "opus", "readOnly"), role: { kind: "reviewer" }, prompt: "Review this change:\n{{diff}}" }, { type: "approval", id: "human" }, { type: "deliver", id: "pull-request" }],
  },
  { id: "d2", projectId: "p2", name: i18next.language === "es" ? "Solo leer" : "Read only", proposedBy: "Claude Code", createdAt: at(90), steps: [{ ...agentStep("reader", "haiku", "readOnly"), prompt: "{{request}}" }] },
];
const learnerStep: WorkflowStep = { ...agentStep("learnings", "sonnet", "readOnly"), role: { kind: "learner" } };
const setupStep: WorkflowStep = { type: "setup", id: "setup", run: "uv sync --frozen" };
const fullWorkflow: WorkflowStep[] = [setupStep, ...workflow, learnerStep];
const ran = (stepId: string, s: AttemptState, fields: Partial<Attempt> = {}): Attempt => ({ ...attempt, id: `${stepId}-${fields.round ?? 0}-${fields.number ?? 1}`, stepId, state: s, reason: null, tools: "readOnly", filesChanged: null, resultText: null, modelActual: s === "skipped" ? null : "claude-sonnet-5-5", ...fields });
const setupRan = ran("setup", "succeeded", { kind: "setup", tools: "setup", modelRequested: "", modelActual: null });
const specRan = ran("spec", "succeeded", { modelRequested: "opus", modelActual: "claude-opus-5-5", resultText: "1. add returns the sum.\n2. The tests pass." });
const builderRan = (fields: Partial<Attempt> = {}) => ran("builder", "succeeded", { tools: "build", filesChanged: ["calc.py"], runFiles: ["calc.py"], resultText: "I fixed add in calc.py.", ...fields });
const checkRan = (s: AttemptState = "succeeded", fields: Partial<Attempt> = {}) => ran("check", s, { kind: "check", tools: "check", modelRequested: "", modelActual: null, prompt: "make test", exitCode: s === "succeeded" ? 0 : 1, reason: s === "failed" ? { code: "checkFailed", exitCode: 1 } : null, ...fields });
const reviewRan = (id: string, s: AttemptState, fields: Partial<Attempt> = {}) => ran(id, s, { role: "reviewer", reason: s === "skipped" ? { code: "noMatchingPaths", paths: ["**/*.tsx"] } : null, ...fields });
const verdict = (v: "approved" | "changes") => ({ output: { verdict: v, findings: [], summary: v === "approved" ? "No problems." : "The endpoint skips the role check." } as unknown as Attempt["output"] });
const decided = (decision: "approve" | "rework") => ran("approver", "succeeded", { role: "approver", modelRequested: "opus", modelActual: "claude-opus-5-5", output: { decision, risk: "medium", reasons: "The finding is real.", instructions: "Add the role check." } as unknown as Attempt["output"] });
const humanWaits = ran("approval", "waitingApproval", { kind: "approval", tools: "none", modelRequested: "", modelActual: null });
const manySteps: WorkflowStep[] = [
  setupStep,
  ...Array.from({ length: 6 }, (_, i) => agentStep(`step-${i + 1}`, i % 2 ? "sonnet" : "opus", "build")),
  agentStep("a-step-whose-name-is-much-too-long-to-fit-in-any-node-at-all", "claude-a-model-with-a-very-long-name-indeed", "build"),
  { type: "check", id: "check", run: "make test" },
  { type: "parallel", id: "reviews-with-a-rather-long-name-too", steps: [reviewerStep("security", []), reviewerStep("ux", ["**/*.tsx"]), reviewerStep("ui", ["**/*.tsx"]), reviewerStep("code", []), reviewerStep("a-reviewer-with-a-very-long-name-as-well", [])] },
  { ...agentStep("approver", "opus", "readOnly"), role: { kind: "approver", reworkTo: "step-6", maxRounds: 2 } },
  agentStep("after-the-reviews", "sonnet", "build"),
  { type: "check", id: "check-again", run: "make test" },
  { type: "approval", id: "approval" },
  { type: "deliver", id: "pull-request" },
  learnerStep,
];
const asSmall = { reason: { code: "treatedAsSmall", lines: 12, files: 2 } as Reason };
const skippedAsSmallRan = (id: string, role: Attempt["role"] = "reviewer") => ran(id, "skipped", { role, ...asSmall });
const longName = (name: string) => (long ? `src/modules/billing/invoices/generated/handlers/${name}` : name);
const WHY_MOVED_UP: Reason[] =
  params.get("why") === "all"
    ? [
        { code: "specNotSmall" },
        { code: "specNamesRisks", risks: ["security", "money", "personalData", "irreversibleData", "sharedContracts", "concurrency", "environment", "noRegressionTest", "somethingNew"] },
        { code: "keelChoseAnAnswer" },
        { code: "noCheck" },
        { code: "checkNotPassed" },
        { code: "issueFromSomeoneElse", author: "someone" },
        { code: "issueFromUnknownAuthor" },
        { code: "issueWithNoAccount" },
        { code: "startedByAssistant", client: "Claude Code" },
        { code: "sensitiveFiles", files: [".github/workflows/ci.yml", longName("auth.py"), longName("schema.sql")], total: 9 },
        { code: "badSensitivePattern", pattern: "billing[" },
        { code: "tooManyLines", lines: 512, limit: 400 },
        { code: "filesWithoutLines", files: [longName("logo.png")], total: 1 },
        { code: "diffUnreadable" },
        { code: "verifierFoundUnmet", count: 1 },
        { code: "verifierHasNotRun" },
        { code: "personAsked" },
      ]
    : [{ code: "sensitiveFiles", files: [".github/workflows/ci.yml"], total: 1 }, { code: "tooManyLines", lines: 512, limit: 400 }];
const FLOW_SCENES: Record<string, { steps: WorkflowStep[] | null; attempts: Attempt[]; round: number }> = {
  small: { steps: fittingSteps, round: 0, attempts: [specRan, builderRan(), checkRan(), verifierAttempt("succeeded"), skippedAsSmallRan("security"), skippedAsSmallRan("ux"), skippedAsSmallRan("ui"), skippedAsSmallRan("code"), skippedAsSmallRan("approver", "approver"), humanWaits] },
  asked: { steps: fittingSteps, round: 0, attempts: [specRan, builderRan(), checkRan(), verifierAttempt("succeeded"), skippedAsSmallRan("security"), skippedAsSmallRan("ux"), skippedAsSmallRan("ui"), skippedAsSmallRan("code"), skippedAsSmallRan("approver", "approver"), ran("approval", "postponed", { kind: "approval", tools: "none", modelRequested: "", modelActual: null }), reviewRan("security", "running", { number: 2, modelActual: "claude-sonnet-5-5", finishedAt: null }), reviewRan("ux", "skipped", { number: 2 }), reviewRan("ui", "skipped", { number: 2 }), reviewRan("code", "running", { number: 2, modelActual: "claude-sonnet-5-5", finishedAt: null })] },
  movedUp: { steps: fittingSteps, round: 0, attempts: [specRan, builderRan({ runFiles: [".github/workflows/ci.yml", "calc.py"], filesChanged: [".github/workflows/ci.yml", "calc.py"] }), checkRan(), verifierAttempt("succeeded"), reviewRan("security", "succeeded", verdict("approved")), reviewRan("ux", "skipped"), reviewRan("ui", "skipped"), reviewRan("code", "succeeded", verdict("approved")), decided("approve"), humanWaits] },
  running: { steps: fullWorkflow, round: 0, attempts: [setupRan, specRan, builderRan(), checkRan(), reviewRan("security", "running", { modelActual: "claude-sonnet-5-5" }), reviewRan("ux", "skipped"), reviewRan("ui", "skipped"), reviewRan("code", "running")] },
  // Back at the builder: what ran after it is from round 1, so it doesn't count as done.
  rework: { steps: fullWorkflow, round: 1, attempts: [setupRan, specRan, builderRan(), checkRan(), reviewRan("security", "succeeded", verdict("changes")), reviewRan("ux", "skipped"), reviewRan("ui", "skipped"), reviewRan("code", "succeeded", verdict("approved")), decided("rework"), builderRan({ round: 1, number: 2, state: "running", finishedAt: null })] },
  skipped: { steps: fullWorkflow, round: 0, attempts: [setupRan, specRan, builderRan(), checkRan(), reviewRan("security", "skipped"), reviewRan("ux", "skipped"), reviewRan("ui", "skipped"), reviewRan("code", "skipped"), decided("approve"), humanWaits] },
  failed: { steps: fullWorkflow, round: 0, attempts: [setupRan, specRan, builderRan(), checkRan("failed"), checkRan("failed", { number: 2 })] },
  many: { steps: manySteps, round: 0, attempts: [setupRan, ...Array.from({ length: 6 }, (_, i) => ran(`step-${i + 1}`, "succeeded", { modelRequested: i % 2 ? "sonnet" : "opus", modelActual: i % 2 ? "claude-sonnet-5-5" : "claude-opus-5-5" })), ran("a-step-whose-name-is-much-too-long-to-fit-in-any-node-at-all", "succeeded", { modelRequested: "claude-a-model-with-a-very-long-name-indeed", modelActual: "claude-a-model-with-a-very-long-name-indeed" }), checkRan(), reviewRan("security", "succeeded", verdict("approved")), reviewRan("ux", "skipped"), reviewRan("ui", "skipped"), reviewRan("code", "succeeded", verdict("approved")), reviewRan("a-reviewer-with-a-very-long-name-as-well", "succeeded", verdict("approved")), decided("approve"), ran("after-the-reviews", "succeeded", { filesChanged: ["calc.py"], runFiles: ["calc.py"] }), checkRan("succeeded", { id: "check-again-0-1", stepId: "check-again" }), humanWaits] },
  unreadable: { steps: null, round: 0, attempts: [setupRan, specRan, builderRan(), reviewRan("security", "succeeded", verdict("approved")), checkRan("failed")] },
  try: { steps: [setupStep, { type: "check", id: "try-check", run: "pnpm test" }], round: 0, attempts: [setupRan, checkRan("failed", { id: "try-check-0-1", stepId: "try-check", prompt: "pnpm test" })] },
  verifying: { steps: fullWorkflow, round: 0, attempts: [setupRan, specRan, builderRan(), checkRan(), verifierAttempt("running", { finishedAt: null })] },
  // The answer wasn't in the structure it was asked for, so it stops here for a person.
  verifyUnreadable: { steps: fullWorkflow, round: 0, attempts: [setupRan, specRan, builderRan(), checkRan(), verifierAttempt("failed", { reason: { code: "noStructuredOutput" }, output: { verdict: "approved" } as unknown as Attempt["output"] })] },
  questions: { steps: fullWorkflow, round: 0, attempts: [setupRan, ...questionAttempts] },
  corrected: { steps: fullWorkflow, round: 0, attempts: [setupRan, ...correctedAttempts] },
  // A person sent it back: the approval they answered says so, and the run is on its next round.
  returned: { steps: fullWorkflow, round: 1, attempts: [setupRan, specRan, builderRan(), checkRan(), reviewRan("security", "skipped"), reviewRan("ux", "skipped"), reviewRan("ui", "skipped"), reviewRan("code", "skipped"), decided("approve"), ran("approval", "returned", { kind: "approval", tools: "none", modelRequested: "", modelActual: null, output: { instructions: "Return 0 instead" } as unknown as Attempt["output"] }), builderRan({ round: 1, number: 2, state: "running", finishedAt: null })] },
};
const flowScene = FLOW_SCENES[params.get("flow") ?? ""];
const note = (seq: number, text: string, paths: string[]): BrainNote => ({ seq, projectId: "p1", kind: "map", state: "active", text, evidence: null, paths, commitSha: "4f4c706aa11", runId: "r1", attemptId: "a1", createdAt: at(4000), decidedAt: null });
const mapNotes = [
  { ...note(1, "The API lives in app/api, one file per resource.", ["app/api"]), stale: false, included: true },
  { ...note(2, "Run the tests with make test. They need the database socket.", ["Makefile", "tests/conftest.py"]), stale: params.get("stale") === "1", included: true },
  { ...note(3, "Screens are in web/src/views and share web/src/ui/primitives.tsx.", ["web/src/views", "web/src/ui/primitives.tsx"]), stale: false, included: params.get("full") !== "1" },
];
const proposals: BrainNote[] = [{ ...note(7, "Migrations must be added in order: the app refuses to start if one is missing.", ["db/migrations"]), kind: "learning", state: "proposed", evidence: "The check failed with 'migration 12 missing' until the builder renamed the file." }];
const prShown = params.get("pr") ?? "draft";
const prState: PrState | null = delivered && prShown !== "unknown" ? (prShown as PrState) : null;
const run: Run = {
  id: "r1",
  startedBy: params.get("mcp") === "on" ? "Claude Code" : null,
  projectId: "p1",
  title: "#7 The add function in calc.py is wrong",
  baseRef: "main",
  request: "Fix add",
  workflow: flowScene ? (flowScene.steps as WorkflowStep[]) : waiting ? runWorkflow : [agentStep("builder", "sonnet", "build")],
  language: "en",
  sourceIssue: waiting ? { number: 7, title: "add is wrong", author: "someone", url: "https://github.com/acme/app/issues/7" } : null,
  branch: "keel/5e882d0f",
  prUrl: delivered ? "https://github.com/acme/app/pull/12" : null,
  prNumber: delivered ? 12 : null,
  workflowId: "builtin:full",
  workflowName: "Full, with reviews",
  prState,
  prStateAt: prState ? at(30) : null,
  prCheckedAt: prState ? at(2) : null,
  round: flowScene?.round ?? (params.get("round") === "2" ? 1 : 0),
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

// expires=<days from now, or anything else to send as it is>, for the session's warning.
const expires = params.get("expires");
const expiresAt = expires === null ? null : Number.isNaN(Number(expires)) ? expires : new Date(Date.now() + Number(expires) * 86400000).toISOString();

// The status bar: session=checking, session=out, claude=missing, and email=long to see it cut. With
// runs=quiet nothing runs or waits, and runs=many has more of both than fit. mcp=on|port for MCP.
const longEmail = "someone.with.a.very.long.address@a-company-with-a-very-long-name.example.com";
const preflight: Preflight = {
  claudeFound: params.get("claude") !== "missing",
  gitFound: true,
  version: params.get("claude") === "missing" ? null : { status: "supported", found: "2.1.318" },
  auth: params.get("claude") === "missing" ? null : params.get("session") === "out" ? { loggedIn: false, method: null, provider: null, subscription: null, email: null, orgName: null, expiresAt: null } : { loggedIn: true, method: "claude.ai", provider: "firstParty", subscription: "max", email: params.get("email") === "long" ? longEmail : "you@example.com", orgName: null, expiresAt },
  ignoredEnv: params.get("env") === "key" ? ["ANTHROPIC_API_KEY"] : [],
  blocker: params.get("session") === "out" ? { code: "claudeLoggedOut" } : null,
  ghFound: params.get("gh") !== "missing",
};

const row = (id: string, title: string, s: AttemptState, reason: Reason | null, model: string, turns: number | null, minutes: number): RunRow => ({
  id,
  title,
  startedBy: params.get("mcp") === "on" && id === "r1" ? "Claude Code" : null,
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
    : params.get("runs") === "quiet"
      ? [row("r1", "The add function in calc.py is wrong. Fix it, then run the tests.", "succeeded", null, "claude-haiku-5-5", 6, 90)]
      : params.get("runs") === "many"
        ? Array.from({ length: 150 }, (_, i) => row(`m${i}`, `Run number ${i + 1}`, i % 3 === 0 ? "running" : "waitingApproval", null, "claude-sonnet-5-5", 3, 10 + i))
        : [
        row("r2", "Add a retry to the payment webhook", "running", null, "claude-opus-5-5", 12, 6),
        row("r3", "The calendar repeats a holiday in 2030", "stalled", REASONS.stalled ?? null, "claude-sonnet-5-5", 4, 14),
        row("r4", "Review the login form", "queued", REASONS.queued ?? null, "haiku", null, 1),
        row("r5", "Rotate the staging keys", "blocked", REASONS.blocked ?? null, "claude-sonnet-5-5", 0, 40),
        row("r1", "The add function in calc.py is wrong. Fix it, then run the tests.", "succeeded", null, "claude-haiku-5-5", 6, 90),
        ...(palette ? [row("r6", "Holiday repeated in 2030", "waitingApproval", null, "claude-sonnet-5-5", 3, 20)] : []),
      ];

const mcpOverview = () => {
  const mcp = params.get("mcp");
  const on = mcp === "on" || mcp === "port";
  return {
    settings: { enabled: on, port: 7413 },
    status: mcp === "on" ? { running: true, port: 7413, url: "http://127.0.0.1:7413/mcp", error: null } : { running: false, port: null, url: null, error: mcp === "port" ? { code: "portInUse", port: 7413 } : null },
    clients: mcp === "on" ? [{ id: "c1", name: "Claude Code", tokenPrefix: "keel_Ab3xYz", canRun: true, createdAt: at(4000), lastSeenAt: at(20), revokedAt: null }, { id: "c2", name: "Cursor", tokenPrefix: "keel_Qr8Lmn", canRun: false, createdAt: at(90000), lastSeenAt: null, revokedAt: at(4000) }] : [],
    calls: mcp === "on"
      ? [
          { seq: 3, clientId: "c1", clientName: "Claude Code", tool: "wait_for_run", args: '{"run":"r1"}', ok: true, error: null, runId: "r1", at: at(20), durationMs: 4000 },
          { seq: 2, clientId: "c1", clientName: "Claude Code", tool: "start_run", args: '{"project":"demo"}', ok: true, error: null, runId: "r1", at: at(90), durationMs: 300 },
          { seq: 1, clientId: "c2", clientName: "Cursor", tool: "start_run", args: '{"project":"demo"}', ok: false, error: "This client may only read. The person can let it start runs in Keel's settings.", runId: null, at: at(600), durationMs: 2 },
        ]
      : [],
  };
};

let defaultWorkflow: string | null = params.get("defaultWorkflow");
const demoProject: Project = { sensitivePaths: params.get("sensitive") === "1" ? ["billing/", "*.proto"] : [],  id: "p1", name: "demo", path: "/Users/you/demo", githubAccount: params.get("account") === "none" ? null : "you-work", githubRepo: "acme/app", setupCommand: "uv sync --frozen", checkCommand: noCheck ? null : "make test", allowedDomains: ["pypi.org"], allowedRead: ["~/.nvm"], services: [{ type: "bridge", name: "db", port: 5432, socket: ".s.PGSQL.5432" }, { type: "command", name: "cache", start: "redis-server --unixsocket $KEEL_SOCKET --port 0", socket: null }], env: [{ name: "DATABASE_URL", value: "postgresql://postgres@/test?host={{socketDir:db}}" }], defaultWorkflow };
// A second project on the board, with one card, to try the selector and coming back from a run.
const otherProject: Project = { ...demoProject, id: "p2", name: "payments", path: "/Users/you/payments", services: [], env: [], defaultWorkflow: null };
const projects = view === "board" || palette || params.get("drafts") === "1" ? [demoProject, otherProject] : [demoProject];

// Adding a project. dialog=setup opens its dialog right away, and click=Add project… gets there the
// long way. setup=pnpm|npm|yarn|uv|cargo|go|make|unknown is what the folder gives away.
// accounts=one|several|readonly|none, gh=missing and repo=none are what GitHub has to say.
// added=new|existing|restored says how it got here (with session=out or map=failed for a map that
// didn't start), and leaving it out is reopening it from "Finish setup".
// try=pass|fail|setupfail|notfound|running is how Try it ends, with click=Try it to press it.
// projects=mixed puts a project that needs setup beside one that doesn't. remove=refused refuses
// removing, addError=1 refuses the folder, picker=cancel closes the picker, and history=1 with empty=1
// keeps the runs of projects that are all gone.
const freshProject: Project = { ...otherProject, id: "p3", name: "shop", path: "/Users/you/www/shop", githubAccount: null, githubRepo: params.get("repo") === "none" ? null : "acme/shop", setupCommand: null, checkCommand: null, allowedDomains: ["pypi.org"], allowedRead: [], services: [{ type: "bridge", name: "db", port: 5432, socket: null }], env: [{ name: "DB_HOST", value: "{{socketDir:db}}" }] };
let listedProjects: Project[] = params.get("empty") === "1" ? [] : [...projects, ...(params.get("projects") === "mixed" || params.get("dialog") === "setup" ? [freshProject] : [])];
const GUESSES: Record<string, { check: string | null; setup: string | null; from: string[] }> = {
  pnpm: { check: "pnpm test", setup: "pnpm install --frozen-lockfile", from: ["package.json", "pnpm-lock.yaml"] },
  npm: { check: "npm test", setup: "npm ci", from: ["package.json", "package-lock.json"] },
  yarn: { check: "yarn test", setup: "yarn install --frozen-lockfile", from: ["package.json", "yarn.lock"] },
  uv: { check: "uv run --frozen pytest", setup: "uv sync --frozen", from: ["uv.lock"] },
  cargo: { check: "cargo test", setup: "cargo fetch", from: ["Cargo.toml"] },
  go: { check: "go test ./...", setup: "go mod download", from: ["go.mod"] },
  make: { check: "make test", setup: null, from: ["Makefile"] },
  unknown: { check: null, setup: null, from: [] },
};
const addedAnswer = (): { project: Project; outcome: string; map: unknown } => {
  const outcome = params.get("added") ?? "new";
  if (outcome !== "new") return { project: freshProject, outcome, map: null };
  const map = params.get("map") === "failed" ? { status: "notStarted", reason: { code: "gitMissing" } } : params.get("session") === "out" ? { status: "notStarted", reason: { code: "claudeLoggedOut" } } : { status: "started" };
  return { project: freshProject, outcome, map };
};
const tryOutput = { pass: { stdout: "> shop@1.0.0 test\n> vitest run\n\n ✓ 42 tests passed\n", stderr: "" }, fail: { stdout: " ✗ 1 test failed: checkout > applies the discount\n", stderr: "" }, setupfail: { stdout: "", stderr: "ERR_PNPM_OUTDATED_LOCKFILE  Cannot install with \"frozen-lockfile\"\n" }, notfound: { stdout: "", stderr: "/bin/sh: pnpm: command not found\n" }, running: { stdout: "", stderr: "" } } as const;
const tryAttempt = (id: string, kind: "setup" | "check", s: AttemptState, reason: Reason | null, exitCode: number | null): Attempt => ({ ...attempt, id, runId: "try1", stepId: kind === "check" ? "try-check" : "setup", kind, tools: kind, state: s, reason, exitCode, modelRequested: "", modelActual: null, filesChanged: null, resultText: null, finishedAt: s === "running" ? null : at(0) });
const tryAttempts = (): Attempt[] => {
  const setup = tryAttempt("try-setup", "setup", "succeeded", null, 0);
  switch (params.get("try")) {
    case "fail":
      return [setup, tryAttempt("try-check", "check", "failed", { code: "checkFailed", exitCode: 1 }, 1)];
    case "setupfail":
      return [tryAttempt("try-setup", "setup", "failed", { code: "setupFailed", exitCode: 1 }, 1)];
    case "notfound":
      return [setup, tryAttempt("try-check", "check", "failed", { code: "checkFailed", exitCode: 127 }, 127)];
    case "running":
      return [tryAttempt("try-setup", "setup", "running", null, null)];
    default:
      return [setup, tryAttempt("try-check", "check", "succeeded", null, 0)];
  }
};
const setupAccounts = (account: string | null | undefined) => {
  const scene = params.get("accounts") ?? "one";
  const repo = freshProject.githubRepo;
  const accounts = [
    { login: "you-personal", active: true, ok: true, scopes: ["repo"] },
    { login: "you-work", active: false, ok: true, scopes: ["repo"] },
  ];
  if (params.get("gh") === "missing") return { repo, accounts: [], info: null, excessScopes: [], error: { code: "ghMissing" } };
  if (scene === "none") return { repo, accounts: [], info: null, excessScopes: [], error: null };
  if (!repo || !account) return { repo, accounts, info: null, excessScopes: [], error: null };
  const canPush = scene === "several" || (scene === "one" && account === "you-work");
  return { repo, accounts, info: { defaultBranch: "main", permission: canPush ? "WRITE" : "READ", private: true, defaultBranchProtected: true }, excessScopes: [], error: null };
};

const PR = "https://github.com/acme/app/pull/12";
const issueOf = (number: number, title: string) => ({ number, title, author: "someone", url: `https://github.com/acme/app/issues/${number}` });
const boardCard = (id: string, title: string, column: BoardColumn, extra: Partial<BoardCard> = {}): BoardCard => ({ id, projectId: "p1", title, body: "", issue: null, column, position: 0, runId: null, runState: null, reason: null, prUrl: null, prState: null, prStateAt: null, rejection: null, startBlocked: null, createdAt: at(100), ...extra });
const inRun = (runState: AttemptState, reason: Reason | null = null) => ({ runId: "r1", runState, reason });
let boardCards: BoardCard[] =
  params.get("cards") === "none"
    ? []
    : [
        boardCard("c1", "Add a dark mode to the invoices page", "backlog", { body: "It should follow the system setting." }),
        boardCard("c2", "The add function in calc.py is wrong", "backlog", { issue: issueOf(7, "The add function in calc.py is wrong") }),
        boardCard("c3", "Export the report as a CSV", "backlog", { startBlocked: { code: "githubAccountMissing" } }),
        boardCard("c4", "Rename the billing module", "backlog", { startBlocked: { code: "defaultWorkflowMissing" } }),
        boardCard("c5", "Move the cron jobs to the queue", "backlog", { ...inRun("rejected"), rejection: "Use the existing retry helper, not a new one." }),
        boardCard("c6", "Update the onboarding copy", "backlog", { ...inRun("rejected"), rejection: "" }),
        boardCard("c7", "Try a new charts library", "backlog", inRun("abandoned")),
        boardCard("c8", "Log failed sign-ins", "todo"),
        boardCard("c9", "Fix the time zone in the weekly email", "todo", { issue: issueOf(21, "Fix the time zone in the weekly email") }),
        boardCard("c10", "Paginate the orders list", "todo"),
        boardCard("c11", "Add a retry to the payment webhook", "inProgress", inRun("running")),
        boardCard("c12", "Review the login form", "inProgress", inRun("queued", { code: "waitingForCapacity" })),
        boardCard("c13", "Clean up the old feature flags", "inProgress", { runId: "r1" }),
        boardCard("c14", "Holiday repeated in 2030", "humanReview", { ...inRun("waitingApproval"), issue: issueOf(9, "Holiday repeated in 2030") }),
        boardCard("c27", "What to do with empty invoices", "humanReview", inRun("waitingAnswers")),
        boardCard("c15", "Speed up the search index", "humanReview", inRun("failed", { code: "checkFailed", exitCode: 1 })),
        boardCard("c16", "Remove the legacy API routes", "humanReview", inRun("blocked", { code: "reworkLimit", rounds: 2 })),
        boardCard("c17", "Upgrade the database driver", "humanReview", inRun("stalled", REASONS.stalled ?? null)),
        boardCard("c18", "Add tests to the importer", "humanReview", inRun("interrupted", { code: "agentLost" })),
        boardCard("c19", "Split the settings page", "humanReview", inRun("stopped")),
        boardCard("c20", "Show the invoice number on the receipt", "done", { ...inRun("succeeded"), prUrl: PR, prState: "merged", prStateAt: at(600) }),
        boardCard("c23", "Add a filter to the orders list", "done", { ...inRun("succeeded"), prUrl: PR, prState: "closed", prStateAt: at(2000) }),
        boardCard("c24", "Show the plan name in the header", "done", { ...inRun("succeeded"), prUrl: PR, prState: "open", prStateAt: at(90) }),
        boardCard("c25", "Trim the weekly email", "done", { ...inRun("succeeded"), prUrl: PR, prState: "draft", prStateAt: at(20) }),
        boardCard("c26", "A pull request Keel hasn't looked at yet", "done", { ...inRun("succeeded"), prUrl: PR }),
        boardCard("c21", "Write the map of the project", "done", inRun("succeeded")),
        boardCard("c22", "Rotate the payments key", "done", { ...inRun("succeeded"), projectId: "p2", prUrl: PR }),
      ];
// ?paused=rateLimited, or any other reason's code. ?paused=rateLimited&resets=none has no known time.
// ?paused=none with ?session=out is a session that was bad at startup and isn't any more.
const boardPaused = (): Reason | null => {
  const paused = params.get("paused");
  if (paused === "none") return null;
  if (paused === "rateLimited") return { code: paused, resetsAt: params.get("resets") === "none" ? null : Math.floor(Date.now() / 1000) + 3 * 3600 };
  return paused ? { code: paused } : preflight.blocker;
};
let maxAgents = 2;
const issues: Issue[] = [
  { ...issueOf(7, "The add function in calc.py is wrong"), body: "b" },
  { ...issueOf(9, "Holiday repeated in 2030"), body: "b" },
  { ...issueOf(30, "The dashboard takes ten seconds to load"), body: "b" },
  { ...issueOf(31, "Typo in the password reset email"), body: "b" },
];
const blockedBy = (code: string) => ({ code: "blocked", reason: { code }, detail: null });

// The results on the Workflows page. results=none has no runs, and results=few has too few to average.
const noAsking = { stoppedToAsk: 0, answers: 0, answersRecommended: 0, sentBack: 0 };
const noneChecked = { criteriaMet: 0, criteriaUnmet: 0, criteriaUnverified: 0 };
const noneFitted = { shortRoute: 0, movedUp: 0, movedUpBecause: [] };
const workflowResults: WorkflowResults[] =
  params.get("results") === "none"
    ? []
    : params.get("results") === "few"
      ? [{ workflowId: "builtin:full", workflowName: "Full, with reviews", runs: 2, finished: 1, merged: 1, closed: 0, costPerMergedUsd: 2.4, averageRounds: 1, stoppedToAsk: 1, answers: 2, answersRecommended: 1, sentBack: 0, criteriaMet: 1, criteriaUnmet: 0, criteriaUnverified: 2, ...noneFitted }]
      : [
          { workflowId: "builtin:full", workflowName: "Full, with reviews", runs: 24, finished: 22, merged: 15, closed: 3, costPerMergedUsd: 3.12, averageRounds: 0.7, stoppedToAsk: 6, answers: 9, answersRecommended: 7, sentBack: 2, criteriaMet: 41, criteriaUnmet: 3, criteriaUnverified: 28, ...noneFitted },
          { workflowId: "builtin:organic", workflowName: "Fits the change", runs: 17, finished: 16, merged: 12, closed: 1, costPerMergedUsd: 1.04, averageRounds: 0.2, stoppedToAsk: 2, answers: 2, answersRecommended: 2, sentBack: 1, criteriaMet: 30, criteriaUnmet: 1, criteriaUnverified: 9, shortRoute: 9, movedUp: 7, movedUpBecause: [{ code: "sensitiveFiles", runs: 3 }, { code: "specNotSmall", runs: 3 }, { code: "tooManyLines", runs: 1 }, { code: "personAsked", runs: 1 }] },
          { workflowId: "mine", workflowName: "Reviews only", runs: 6, finished: 5, merged: 1, closed: 2, costPerMergedUsd: 1.8, averageRounds: 0, ...noAsking, ...noneChecked, ...noneFitted },
          { workflowId: "gone", workflowName: "A workflow since deleted", runs: 9, finished: 9, merged: 4, closed: 1, costPerMergedUsd: 0.86, averageRounds: 0.25, ...noAsking, ...noneChecked, ...noneFitted },
        ];

// Same as the backend: the card lands at that index of the lane, and the rest close up around it.
function moveBoardCard(cardId: string, lane: "backlog" | "todo", index: number) {
  const card = boardCards.find((c) => c.id === cardId);
  if (!card) return;
  const rest = boardCards.filter((c) => c.id !== cardId);
  const inLane = rest.filter((c) => c.column === lane);
  const moved = { ...card, column: lane, runId: lane === "todo" ? null : card.runId, runState: lane === "todo" ? null : card.runState, reason: lane === "todo" ? null : card.reason, rejection: lane === "todo" ? null : card.rejection, startBlocked: lane === "todo" ? null : card.startBlocked };
  inLane.splice(index, 0, moved);
  boardCards = [...rest.filter((c) => c.column !== lane), ...inLane];
}

mockWindows("main");
mockIPC((command, args) => {
  switch (command) {
    case "preflight":
    case "refresh_session":
      // A new object each time, as the real answer is.
      return { ...preflight };
    case "overview":
      return params.get("empty") === "1" ? { pendingProposals: 0, workflowDrafts: 0, runs: params.get("history") === "1" ? runs : [], projects: listedProjects, totals: { running: 0, retrying: 0, blocked: 0, tokens: 0, durationMs: 0, costUsd: 0 } } : { pendingProposals: params.get("proposals") === "1" ? 1 : 0, workflowDrafts: params.get("drafts") === "1" ? draftsWaiting.length : 0, runs, projects: listedProjects.map((p) => (p.id === "p1" ? { ...p, defaultWorkflow } : p)), totals: { running: 1, retrying: 1, blocked: 2, tokens: 128400, durationMs: 1260000, costUsd: 1.87 } };
    case "plugin:dialog|open":
      return params.get("picker") === "cancel" ? null : freshProject.path;
    case "project_add":
      if (params.get("addError") === "1") throw { code: "notARepo", reason: null, detail: "/Users/you/Downloads" };
      if (!listedProjects.some((p) => p.id === freshProject.id)) listedProjects = [...listedProjects, freshProject];
      return addedAnswer();
    case "project_setup":
      return { project: listedProjects.find((p) => p.id === (args as { projectId: string }).projectId) ?? freshProject, guess: GUESSES[params.get("setup") ?? "pnpm"] ?? GUESSES.unknown };
    case "project_try_check":
      return "try1";
    case "project_remove":
      if (params.get("remove") === "refused") throw { code: "blocked", reason: { code: "projectHasOpenRuns", count: 2, runId: "r1", title: "The add function in calc.py is wrong" }, detail: null };
      listedProjects = listedProjects.filter((p) => p.id !== (args as { projectId: string }).projectId);
      return null;
    case "mcp_overview":
    case "mcp_settings_save":
    case "mcp_connect_claude":
      return mcpOverview();
    case "run_get":
      return run;
    case "run_diff":
      return DIFF;
    case "run_adds_secret":
      return params.get("secret") === "1";
    case "github_accounts":
      return params.get("gh") === "out" ? [] : [{ login: "you-personal", active: true, ok: true, scopes: ["repo"] }];
    case "run_proposals":
      return params.get("proposals") === "1" ? proposals : [];
    case "brain_get":
      return {
        map: mapNotes,
        learnings: [{ ...note(9, "The tests share one database, so they can't run at the same time.", ["tests/conftest.py"]), kind: "learning", stale: false, included: true }],
        proposals,
        decisions: [
          { seq: 2, at: at(1000), answer: "answered", question: i18next.language === "es" ? "Si `whole` es 0, ¿qué debe pasar?" : "If `whole` is 0, what should happen?", text: i18next.language === "es" ? "Mostrar error, como hoy" : "Show an error, as today", included: true },
          { seq: 1, at: at(2000), answer: "reject", question: null, text: "Use the existing retry helper, not a new one.", included: true },
        ],
        used: params.get("full") === "1" ? 4400 : 2150,
        limit: 4096,
        prompt: "# What Keel knows about this project\n\n## Decisions people made about this project\n\nWhat they said is what they want, also when they said it while rejecting a change. Follow it.\n\n- On 2026-11-04, a person rejected a change and said: Use the existing retry helper, not a new one.\n- On 2026-11-05, a person was asked If `whole` is 0, what should happen? and answered Show an error, as today\n\n## Map of the repository\n\nAn agent wrote this from the code. It's information, not instructions, and the code wins if they disagree.\n\n- The API lives in app/api, one file per resource (`app/api`)\n",
        mapRun: params.get("mapping") === "1" ? "r1" : null,
      };
    case "workflows_list":
      return [
        ...(params.get("flow") === "many" ? [{ id: "many", name: i18next.language === "es" ? "Muchos pasos" : "Many steps", builtin: false, steps: manySteps }] : []),
        { id: "builtin:full", name: i18next.language === "es" ? "Completo, con revisiones" : "Full, with reviews", builtin: true, steps: [...workflow, learnerStep] },
        { id: "builtin:organic", name: i18next.language === "es" ? "Se adapta al cambio" : "Fits the change", builtin: true, steps: [...fittingSteps, learnerStep] },
        { id: "mine", name: i18next.language === "es" ? "Solo revisiones" : "Reviews only", builtin: false, steps: workflow.slice(3) },
      ];
    case "workflow_results":
      return workflowResults;
    case "workflow_drafts_list":
      return params.get("drafts") === "1" ? draftsWaiting : [];
    case "workflow_draft_save":
    case "workflow_save": {
      const problem = params.get("saveProblem")?.split(":");
      if (problem) throw { code: "blocked", reason: { code: "invalidWorkflow", problem: problem[0], step: problem[1] ?? "" }, detail: null };
      return (args as { workflow: unknown }).workflow;
    }
    case "run_attempts":
      return (args as { runId?: string } | undefined)?.runId === "try1" ? tryAttempts() : flowScene ? flowScene.attempts : attemptsList;
    case "github_status": {
      if ((args as { projectId?: string } | undefined)?.projectId === freshProject.id) return setupAccounts((args as { account?: string | null }).account);
      const access = params.get("access");
      const accounts = [
        { login: "you-personal", active: true, ok: true, scopes: ["repo"] },
        { login: "you-work", active: false, ok: true, scopes: ["repo", "admin:org", "workflow"] },
      ];
      if (access === "none") {
        return {
          repo: "acme/app",
          accounts,
          info: null,
          excessScopes: [],
          error: { code: "deliveryFailed", kind: "noPermission", detail: "GraphQL: Could not resolve to a Repository with the name 'acme/app'. (repository)" },
        };
      }
      return {
        repo: "acme/app",
        accounts,
        info: { defaultBranch: "main", permission: access === "read" ? "READ" : "ADMIN", private: true, defaultBranchProtected: false },
        excessScopes: (args as { account?: string | null } | undefined)?.account === "you-work" ? ["admin:org", "workflow"] : [],
        error: null,
      };
    }
    case "decisions_list":
      return [{ seq: 1, at: at(2000), answer: "reject", question: null, text: "Use the existing retry helper, not a new one." }];
    case "issues_list":
      // ?issues=none, ?issues=account or ?issues=repo for the states without a list.
      if (params.get("issues") === "account") throw blockedBy("githubAccountMissing");
      if (params.get("issues") === "repo") throw blockedBy("noGithubRemote");
      return params.get("issues") === "none" ? [] : issues;
    case "board_get": {
      const { projectId } = args as { projectId: string };
      return { cards: boardCards.filter((c) => c.projectId === projectId), working: Number(params.get("working") ?? 2), maxAgents, defaultWorkflow, paused: boardPaused() };
    }
    case "card_create": {
      const { projectId, title, body } = args as { projectId: string; title: string; body: string };
      const id = `new-${Date.now()}`;
      boardCards = [...boardCards, boardCard(id, title, "backlog", { projectId, body })];
      return id;
    }
    case "card_import_issue": {
      const { projectId, issue } = args as { projectId: string; issue: Issue };
      const existing = boardCards.find((c) => c.projectId === projectId && c.issue?.number === issue.number);
      if (existing) return existing.id;
      const id = `issue-${issue.number}`;
      boardCards = [...boardCards, boardCard(id, issue.title, "backlog", { projectId, issue: issueOf(issue.number, issue.title) })];
      return id;
    }
    case "card_update": {
      const { cardId, title, body } = args as { cardId: string; title: string; body: string };
      boardCards = boardCards.map((c) => (c.id === cardId ? { ...c, title, body } : c));
      return null;
    }
    case "card_move": {
      // ?boardError=1 makes every move fail, as a project with no GitHub account would.
      if (params.get("boardError") === "1") throw blockedBy("githubAccountMissing");
      const { cardId, lane, index } = args as { cardId: string; lane: "backlog" | "todo"; index: number };
      moveBoardCard(cardId, lane, index);
      return null;
    }
    case "card_delete":
      boardCards = boardCards.filter((c) => c.id !== (args as { cardId: string }).cardId);
      return null;
    case "max_agents_get":
      return maxAgents;
    case "max_agents_set":
      maxAgents = (args as { value: number }).value;
      return null;
    case "project_update": {
      const { projectId, settings } = args as { projectId: string; settings: Partial<Project> & { defaultWorkflow: string | null } };
      if (projectId === "p1") defaultWorkflow = settings.defaultWorkflow;
      listedProjects = listedProjects.map((p) => (p.id === projectId ? { ...p, ...settings } : p));
      return null;
    }
    case "attempt_events":
      if ((args as { attemptId?: string } | undefined)?.attemptId?.startsWith("try-")) {
        return [{ seq: 1, attemptId: "try-check", at: at(0), kind: "checkResult", data: tryOutput[(params.get("try") ?? "pass") as keyof typeof tryOutput] ?? tryOutput.pass }];
      }
      if ((args as { attemptId?: string } | undefined)?.attemptId === "security-0-1") {
        if (params.get("flow") === "small" || params.get("flow") === "asked") return [{ seq: 100, attemptId: "security-0-1", at: at(1), kind: "skipped", data: { reason: asSmall.reason } }, { seq: 101, attemptId: "security-0-1", at: at(1), kind: "sizeDecided", data: { small: true, lines: 12, files: 2, round: 0 } }];
        if (params.get("flow") === "movedUp") return [{ seq: 100, attemptId: "security-0-1", at: at(1), kind: "sizeDecided", data: { small: false, because: WHY_MOVED_UP, round: 0 } }];
      }
      // Only one step has a log, or the run's commands would count once per step.
      return (args as { attemptId?: string } | undefined)?.attemptId === attempt.id ? events : [];
    case "attempt_diff":
      return state === "succeeded" ? DIFF : "";
    case "attempt_raw_check":
      return params.get("raw") ?? (finished ? "intact" : "pending");
    default:
      return null;
  }
});

void useApp.getState().refreshOverview();
void useApp.getState().refreshMcp();
useApp.setState({ preflight: params.get("session") === "checking" ? null : preflight });
// from=board or from=brain shows a run opened on that page: it stays the current one in the top bar.
const from = params.get("from");
if (view === "run") useApp.setState({ view: { name: "run", runId: "r1", backTo: from === "board" ? { name: "board", projectId: "p1" } : from === "brain" ? { name: "brain" } : undefined } });
if (view === "settings") useApp.setState({ view: { name: "settings" } });
if (view === "workflows") useApp.setState({ view: { name: "workflows" } });
if (view === "brain") useApp.setState({ view: { name: "brain" } });
if (view === "board") useApp.setState({ view: { name: "board" } });
if (params.get("dialog") === "newRun") useApp.setState({ newRunOpen: true });
if (params.get("dialog") === "quit") useApp.setState({ quitOpen: true });
if (params.get("dialog") === "setup") useApp.setState({ projectSetup: { projectId: freshProject.id, added: params.get("added") ? (addedAnswer() as ProjectAdded) : null, returnToNewRun: false } });
if (palette) useSearchEverywhere.setState({ open: true, scope: palette, query: params.get("q") ?? "" });

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

const width = params.get("width");
const page = document.getElementById("root");
if (width && page) page.style.width = `${width}px`;
const flowNode = params.get("node");
if (flowNode) {
  setTimeout(() => document.querySelector<HTMLElement>(`[data-flow-id="${flowNode}"]`)?.click(), 1000);
}

// details=open opens whatever opens in place once the page has loaded.
if (params.get("details") === "open") {
  setTimeout(() => document.querySelectorAll("details").forEach((details) => (details.open = true)), 1500);
}

// click=<a button's text> presses it once the page has loaded, to see what it opens. then=<another>
// presses the last one with that text a moment later, which is the one in a dialog it opened.
const click = params.get("click");
if (click) {
  setTimeout(() => [...document.querySelectorAll("button")].find((button) => button.textContent === click)?.click(), 1500);
}
/** For a button whose text changes, like one that says how long ago something happened. */
const clickStart = params.get("clickStart");
if (clickStart) {
  setTimeout(() => [...document.querySelectorAll("button")].find((button) => button.textContent?.startsWith(clickStart))?.click(), 1500);
}
const then = params.get("then");
if (then) {
  setTimeout(() => [...document.querySelectorAll("button")].filter((button) => button.textContent === then).pop()?.click(), 2200);
}
