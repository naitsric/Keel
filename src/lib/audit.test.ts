import { describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { Attempt, AuditEvent, CheckedCriterion, VerificationOutput } from "./api";
import { accountsOf, activityFrom, answersIn, approvalOutcome, askedText, closedByAssistant, commandEnd, commandsFrom, explainingAttempts, failureMarkIn, filesChangedBy, focusAttempt, gist, latestOfEachStep, pipeHidesExitCode, reviewsOfRound, roundsAnswered, sentBackByAssistant, shownState, specifyingAttempt, specOf, stillGoing, toolSummary, verificationCounts, verificationInRound, verificationLine, verificationOf, touchesWorkflows } from "./audit";

const event = (seq: number, kind: string, data: Record<string, unknown>): AuditEvent => ({ seq, attemptId: "a", at: "2026-01-01T00:00:00Z", kind, data });

describe("what an attempt's state reads as", () => {
  const didNotFit = { code: "specNotUsable", problems: [{ code: "decidedTooLong", position: 1, chars: 141, limit: 140 }] };
  const first = (fields: Partial<Attempt> = {}) => ({ id: "s1", state: "failed", reason: didNotFit, resumedFrom: null, ...fields }) as Attempt;
  const fix = { id: "s2", state: "succeeded", reason: null, resumedFrom: "s1" } as Attempt;

  it("doesn't call an answer Keel had fixed in the same session a failure", () => {
    expect(shownState(first(), [first(), fix])).toBe("corrected");
  });

  it("still says failed while nothing went on to fix it", () => {
    expect(shownState(first(), [first()])).toBe("failed");
  });

  it("only means an answer that didn't fit, and only once", () => {
    expect(shownState(first({ reason: { code: "noStructuredOutput" } }), [first(), fix])).toBe("failed");
    // The fix that didn't fit either stops the step for a person.
    expect(shownState(first({ state: "blocked" }), [first(), fix])).toBe("blocked");
    expect(shownState(fix, [first(), fix])).toBe("succeeded");
  });
});

describe("what a verifier found", () => {
  const item = (result: CheckedCriterion["result"], fields: Partial<CheckedCriterion> = {}): CheckedCriterion => ({ id: "C1", when: "add(1, 2)", then: "it returns 3", result, evidence: result === "met" ? "test_calc.py:1" : "", ...fields });
  const found = (...criteria: CheckedCriterion[]): VerificationOutput => ({ criteria, summary: "What it found" });
  const verifier = (output: unknown, fields: { round?: number; state?: string; role?: string } = {}) => ({ role: "verifier", state: "succeeded", round: 0, output, ...fields });

  it("is read only from a verifier that succeeded, since only that one has been corrected", () => {
    const output = found(item("met"));
    expect(verificationOf(verifier(output))).toEqual(output);
    for (const state of ["failed", "stopped", "blocked", "interrupted", "running"]) {
      expect(verificationOf(verifier(output, { state }))).toBeNull();
    }
    expect(verificationOf(verifier(output, { role: "reviewer" }))).toBeNull();
    expect(verificationOf(verifier({ verdict: "approved" }))).toBeNull();
    expect(verificationOf(verifier(null))).toBeNull();
  });

  it("only counts in its own round", () => {
    const first = found(item("unmet"));
    const second = found(item("met"));
    const attempts = [verifier(first), verifier(second, { round: 1 }), verifier(found(item("unmet")), { round: 1, state: "failed" })];
    expect(verificationInRound(attempts, 0)).toEqual(first);
    expect(verificationInRound(attempts, 1)).toEqual(second);
    expect(verificationInRound(attempts, 2)).toBeNull();
  });

  it("counts each thing once", () => {
    expect(verificationCounts(found(item("met"), item("met"), item("unmet"), item("unverified")))).toEqual({ met: 2, unmet: 1, unverified: 1, total: 4 });
  });

  const line = (language: string, ...criteria: CheckedCriterion[]) => verificationLine(i18next.getFixedT(language), found(...criteria));

  it("sums it up in a line, in either language", () => {
    expect(line("en", item("met"), item("met"))).toBe("2 of 2 met");
    expect(line("es", item("met"), item("met"))).toBe("2 de 2 se cumplen");
    expect(line("en", item("met"), item("met"), item("met"), item("unverified"))).toBe("3 of 4 met, 1 not verified");
    expect(line("es", item("met"), item("met"), item("met"), item("unverified"))).toBe("3 de 4 se cumplen, 1 sin verificar");
    expect(line("en", item("met"), item("unmet"), item("unverified"), item("unverified"))).toBe("1 of 4 met, 1 not met, 2 not verified");
    expect(line("es", item("met"), item("unmet"), item("unverified"), item("unverified"))).toBe("1 de 4 se cumple, 1 no se cumple, 2 sin verificar");
    expect(line("en", item("unmet"), item("unmet"))).toBe("0 of 2 met, 2 not met");
    expect(line("es", item("unmet"), item("unmet"))).toBe("0 de 2 se cumplen, 2 no se cumplen");
  });

  it("says plainly when none of it could be verified", () => {
    expect(line("en", item("unverified"), item("unverified"))).toBe("None of the 2 could be verified");
    expect(line("es", item("unverified"), item("unverified"))).toBe("No se pudo verificar ninguno de los 2");
    expect(line("en", item("unverified"))).toBe("It couldn't be verified");
    expect(line("es", item("unverified"))).toBe("No se pudo verificar");
  });
});

describe("commandsFrom", () => {
  const events = [
    event(1, "toolCall", { id: "1", name: "Bash", input: { command: "make test" } }),
    event(2, "toolResult", { id: "1", isError: true, text: "Exit code 3", exitCode: 3 }),
    event(3, "toolCall", { id: "2", name: "Read", input: { file_path: "a.py" } }),
    event(4, "toolResult", { id: "2", isError: false, text: "content" }),
    event(5, "toolCall", { id: "3", name: "Bash", input: { command: "echo ok" } }),
    event(6, "toolResult", { id: "3", isError: false, text: "ok" }),
    event(7, "toolCall", { id: "4", name: "Bash", input: { command: "sleep 9" } }),
  ];

  it("keeps only commands, with how each one ended", () => {
    expect(commandsFrom(events).map((c) => [c.command, c.status, c.exitCode])).toEqual([
      ["make test", "failed", 3],
      ["echo ok", "ok", null],
      ["sleep 9", "running", null],
    ]);
  });

  it("doesn't call tests that failed behind a pipe a success", () => {
    // What the builder ran in run 40c0fe47: head ended with 0, so Claude Code saw no error.
    const command = 'cargo test --workspace 2>&1 | grep -E "^test result|FAILED|failed|panicked|error" | head -30';
    const [piped] = commandsFrom([
      event(1, "toolCall", { id: "1", name: "Bash", input: { command } }),
      event(2, "toolResult", { id: "1", isError: false, text: "test prompts::it_works ... FAILED\ntest result: FAILED. 120 passed; 15 failed" }),
    ]);
    expect(piped).toMatchObject({ status: "unclear", piped: true, failureMark: "FAILED" });
  });

  it("still says a plain command that failed failed, with its code", () => {
    const [plain] = commandsFrom([
      event(1, "toolCall", { id: "1", name: "Bash", input: { command: "cargo test --workspace" } }),
      event(2, "toolResult", { id: "1", isError: true, text: "Exit code 101\ntest result: FAILED. 120 passed; 15 failed", exitCode: 101 }),
    ]);
    // That it failed already says it: the word in the output adds nothing.
    expect(plain).toMatchObject({ status: "failed", exitCode: 101, piped: false, failureMark: null });
  });
});

describe("how a command ended", () => {
  const result = (fields: Record<string, unknown>) => event(2, "toolResult", { id: "1", isError: false, text: "", ...fields });

  it("is a success only when nothing can hide a failure", () => {
    expect(commandEnd("cargo test", result({ text: "test result: ok. 3 passed; 0 failed" }))).toEqual({ status: "ok", exitCode: null, piped: false, failureMark: null });
    expect(commandEnd("cargo test | tail -5", result({ text: "test result: ok. 3 passed; 0 failed" }))).toEqual({ status: "unclear", exitCode: null, piped: true, failureMark: null });
    expect(commandEnd("cargo test || true", result({ text: "test result: FAILED. 2 passed; 1 failed" }))).toEqual({ status: "unclear", exitCode: null, piped: false, failureMark: "FAILED" });
  });

  it("is still going without a result", () => {
    expect(commandEnd("cargo test | head", undefined)).toEqual({ status: "running", exitCode: null, piped: false, failureMark: null });
  });

  it("keeps the pipe when a piped command fails, since its code may be the last one's", () => {
    expect(commandEnd("cargo test | grep nothing", result({ isError: true, text: "Exit code 1", exitCode: 1 }))).toEqual({ status: "failed", exitCode: 1, piped: true, failureMark: null });
  });

  it("knows a pipe from an or, a pipe in quotes, and one that keeps the failure", () => {
    expect(pipeHidesExitCode("cargo test 2>&1 | head -30")).toBe(true);
    expect(pipeHidesExitCode("cargo test |& tee log")).toBe(true);
    expect(pipeHidesExitCode("cargo test||echo broke")).toBe(false);
    expect(pipeHidesExitCode("make test || true")).toBe(false);
    expect(pipeHidesExitCode('grep -E "a|b" src/main.rs')).toBe(false);
    expect(pipeHidesExitCode("grep -E 'a|b' src/main.rs")).toBe(false);
    expect(pipeHidesExitCode("grep a\\|b src/main.rs")).toBe(false);
    expect(pipeHidesExitCode('echo "say \\"|\\"" | wc -c')).toBe(true);
    expect(pipeHidesExitCode("set -o pipefail; cargo test 2>&1 | tail -20")).toBe(false);
    expect(pipeHidesExitCode("set -euo pipefail && pnpm test | tail")).toBe(false);
    expect(pipeHidesExitCode("cargo test")).toBe(false);
  });

  it("finds what failing tests and builds print, and not every mention of a failure", () => {
    expect(failureMarkIn("test calc::adds ... FAILED")).toBe("FAILED");
    expect(failureMarkIn("thread 'calc::adds' panicked at src/calc.rs:3:5")).toBe("panicked");
    expect(failureMarkIn("error: could not compile `calc`")).toBe("error");
    expect(failureMarkIn("error[E0308]: mismatched types")).toBe("error");
    expect(failureMarkIn("test result: ok. 3 passed; 0 failed; 0 ignored")).toBeNull();
    expect(failureMarkIn("fn reports_error() {}\nlet my_error: u8 = 0;")).toBeNull();
    expect(failureMarkIn("")).toBeNull();
  });
});

describe("activityFrom", () => {
  it("shows a tool call and its result as one thing", () => {
    const items = activityFrom([event(1, "toolCall", { id: "1", name: "Bash" }), event(2, "text", { text: "hi" }), event(3, "toolResult", { id: "1", text: "out" })]);
    expect(items.map((i) => i.kind)).toEqual(["toolCall", "text"]);
    expect(items[0].result?.data.text).toBe("out");
  });
});

describe("toolSummary", () => {
  it("picks the useful field", () => {
    expect(toolSummary("Bash", { command: "ls" })).toBe("ls");
    expect(toolSummary("Read", { file_path: "a.py" })).toBe("a.py");
    expect(toolSummary("Grep", { pattern: "x" })).toBe("x");
    expect(toolSummary("Read", null)).toBe("");
  });
});

describe("run summary", () => {
  it("lists each changed file once", () => {
    expect(filesChangedBy([{ filesChanged: ["a", "b"] }, { filesChanged: null }, { filesChanged: ["b", "c"] }])).toEqual(["a", "b", "c"]);
  });

  it("flags workflow files, which can run with the repo's secrets", () => {
    expect(touchesWorkflows(["src/a.ts"])).toBe(false);
    expect(touchesWorkflows(["src/a.ts", ".github/workflows/ci.yml"])).toBe(true);
    expect(touchesWorkflows([".github/CODEOWNERS"])).toBe(false);
  });

  it("lets the builder explain the run rather than the spec writer", () => {
    const worker = { kind: "agent", role: "worker", state: "succeeded" };
    const spec = { ...worker, tools: "readOnly", resultText: "spec" };
    const builder = { ...worker, tools: "build", resultText: "built" };
    const check = { kind: "check", tools: "check", role: "worker", state: "succeeded", resultText: null };
    const reviewer = { ...spec, role: "reviewer", resultText: "looks fine" };
    expect(explainingAttempts([spec, builder, reviewer])).toEqual([builder]);
    expect(explainingAttempts([spec, builder, check])).toEqual([builder]);
    expect(explainingAttempts([spec, check])).toEqual([spec]);
    expect(explainingAttempts([check])).toEqual([]);
  });

  it("tells every round of the story, without the tries that failed", () => {
    const worker = { kind: "agent", role: "worker", tools: "build" };
    const failed = { ...worker, state: "failed", resultText: "gave up" };
    const first = { ...worker, state: "succeeded", resultText: "fixed it" };
    const second = { ...worker, state: "succeeded", resultText: "fixed what came back" };
    expect(explainingAttempts([failed, first, second])).toEqual([first, second]);
    expect(explainingAttempts([failed])).toEqual([failed]);
  });

  it("lists what the run changed in the end, not everything it touched on the way", () => {
    const added = { filesChanged: ["a.py", "b.py"], runFiles: ["a.py", "b.py"] };
    const removed = { filesChanged: ["b.py"], runFiles: ["a.py"] };
    const reviewed = { filesChanged: null, runFiles: null };
    expect(filesChangedBy([added, removed, reviewed])).toEqual(["a.py"]);
    expect(filesChangedBy([{ filesChanged: ["old.py"] }])).toEqual(["old.py"]);
  });
});

describe("the gist of what an agent wrote", () => {
  it("keeps a short message whole", () => {
    expect(gist("I fixed `add` in calc.py: it subtracted instead of adding.")).toEqual({ text: "I fixed `add` in calc.py: it subtracted instead of adding.", cut: false });
  });

  it("takes two sentences of the first paragraph, and says it left the rest out", () => {
    const text = "I fixed add in calc.py, which subtracted. I added a test. I also renamed a variable.\n\nTests: make test passes.";
    expect(gist(text)).toEqual({ text: "I fixed add in calc.py, which subtracted. I added a test.", cut: true });
  });

  it("doesn't take a file name or a version for the end of a sentence", () => {
    expect(gist("Moved calc.py to v1.2 of the helper, and fixed a bug there. Done.").text).toBe("Moved calc.py to v1.2 of the helper, and fixed a bug there. Done.");
  });

  it("skips headings", () => {
    expect(gist("## Summary\n\nI fixed the login form so it keeps the email.").text).toBe("I fixed the login form so it keeps the email.");
    expect(gist("**Resumen**\nCorregí el formulario para que conserve el correo.").text).toBe("Corregí el formulario para que conserve el correo.");
  });

  it("reads past an opening that says nothing on its own", () => {
    expect(gist("Listo.\n\nCorregí el formulario para que conserve el correo. Y algo más. Y otra cosa.").text).toBe("Listo. Corregí el formulario para que conserve el correo.");
    expect(gist("Here's what I did, as you asked for it today:\n\n- Fixed the form\n- Added a test").text).toBe("Here's what I did, as you asked for it today: Fixed the form Added a test");
  });

  it("cuts text with no sentences at a word, within the limit", () => {
    const { text, cut } = gist("word ".repeat(100));
    expect(text.length).toBeLessThanOrEqual(241);
    expect(text.endsWith("word…")).toBe(true);
    expect(cut).toBe(true);
  });

  it("doesn't come back empty from a message that's only a heading", () => {
    expect(gist("## Done").text).not.toBe("");
  });
});

describe("what was asked", () => {
  const agent = (id: string, fields: { stepId?: string; role?: string; state?: string; kind?: string } = {}) => ({ id, stepId: id, kind: "agent", role: "worker", state: "succeeded", ...fields });

  it("in a workflow saved before the role, is in the step called spec", () => {
    expect(specifyingAttempt([agent("spec"), agent("builder")])?.id).toBe("spec");
  });

  it("goes by the specifier's role before any name", () => {
    expect(specifyingAttempt([agent("plan", { role: "specifier" }), agent("spec")])?.id).toBe("plan");
  });

  it("is the latest attempt that worked", () => {
    const attempts = [agent("first", { stepId: "spec" }), agent("second", { stepId: "spec" }), agent("third", { stepId: "spec", state: "failed" })];
    expect(specifyingAttempt(attempts)?.id).toBe("second");
  });

  it("is nowhere without an agent that wrote it", () => {
    expect(specifyingAttempt([agent("builder")])).toBeUndefined();
    expect(specifyingAttempt([agent("spec", { kind: "check" })])).toBeUndefined();
  });

  it("reads a specifier's structured answer by the sentence that sums it up, not as JSON", () => {
    const output = { summary: " add returns the sum ", criteria: [{ id: "C1", when: "add(1, 2)", then: "it returns 3" }] };
    expect(askedText({ role: "specifier", output, resultText: "" })).toBe("add returns the sum");
    expect(specOf({ role: "specifier", output })).toMatchObject({ criteria: output.criteria, decided: [], questions: [], limits: [], changes: [] });
  });

  it("doesn't take anything else for a structured answer", () => {
    expect(specOf({ role: "reviewer", output: { summary: "fine", verdict: "approved" } })).toBeNull();
    expect(specOf({ role: "specifier", output: { nothing: "useful" } })).toBeNull();
    expect(specOf({ role: "specifier", output: null })).toBeNull();
    expect(askedText({ role: "specifier", output: { nothing: "useful" }, resultText: "What it wrote" })).toBe("What it wrote");
  });

  it("has no text if the agent wrote none", () => {
    expect(askedText({ resultText: "  add returns the sum\n" })).toBe("add returns the sum");
    expect(askedText({ resultText: " \n" })).toBeNull();
    expect(askedText({ resultText: null })).toBeNull();
  });
});

describe("what a person answered", () => {
  const answered = (id: string) => ({ summary: "s", answered: [{ question: id, asked: "Why?", answer: "Because", recommended: false, remember: false }] });
  const step = (output: unknown, round = 0, stepId = "spec") => ({ stepId, round, role: "specifier", output });

  it("is kept in order, and only a specifier has any", () => {
    expect(answersIn([step(answered("Q1")), step({ summary: "s" }), { role: "worker", output: answered("Q9") }, step(answered("Q2"))]).map((a) => a.question)).toEqual(["Q1", "Q2"]);
  });

  it("counts as a round for that step, in that round of work", () => {
    const attempts = [step(answered("Q1")), step({ summary: "s" }), step(answered("Q2"), 1), step(answered("Q3"), 0, "other")];
    expect(roundsAnswered(attempts, { stepId: "spec", round: 0 })).toBe(1);
  });
});

describe("the reviews of a round", () => {
  const a = (role: string, round: number, output: unknown = null) => ({ role, round, output });

  it("only speaks for the round it's asked about", () => {
    const oldReview = a("reviewer", 0, {});
    const newReview = a("reviewer", 1, {});
    const oldApprover = a("approver", 0, { decision: "rework" });
    const { reviewers, approver } = reviewsOfRound([oldReview, oldApprover, newReview, a("worker", 1)], 1);
    expect(reviewers).toEqual([newReview]);
    expect(approver).toBeUndefined();
  });

  it("takes the approver's last answer, not a try that gave none", () => {
    const answered = a("approver", 0, { decision: "approve" });
    expect(reviewsOfRound([answered, a("approver", 0)], 0).approver).toBe(answered);
  });
});

describe("work an assistant sent back", () => {
  const returned = (round: number, output: unknown) => ({ state: "returned", round, output });

  it("is what started this round, with who and what they asked", () => {
    const attempts = [returned(0, { instructions: "Return 0", by: "Claude Code" }), { state: "waitingApproval", round: 1, output: null }];
    expect(sentBackByAssistant(attempts, 1)).toEqual({ by: "Claude Code", instructions: "Return 0" });
  });

  it("is nothing when a person sent it back, or nobody did", () => {
    expect(sentBackByAssistant([returned(0, { instructions: "Return 0" })], 1)).toBeNull();
    expect(sentBackByAssistant([{ state: "succeeded", round: 0, output: { by: "Claude Code" } }], 1)).toBeNull();
    expect(sentBackByAssistant([], 0)).toBeNull();
  });

  it("is nothing once something else started a later round", () => {
    expect(sentBackByAssistant([returned(0, { instructions: "Return 0", by: "Claude Code" })], 2)).toBeNull();
  });

  it("finds the right one among several", () => {
    const attempts = [returned(0, { instructions: "First", by: "Claude Code" }), returned(1, { instructions: "Second" }), returned(2, { instructions: "Third", by: "Cursor" })];
    expect(sentBackByAssistant(attempts, 2)).toBeNull();
    expect(sentBackByAssistant(attempts, 3)).toEqual({ by: "Cursor", instructions: "Third" });
  });
});

describe("an assistant that closed the run", () => {
  const ended = { finishedAt: "2026-11-09T10:00:00Z" };
  const step = (id: string, stepId: string, state: string) => ({ id, stepId, state });
  const closed = (attemptId: string, seq: number, by?: string): AuditEvent => ({ seq, attemptId, at: "2026-11-09T10:00:00Z", kind: "abandoned", data: by ? { by } : {} });
  // Two reviews failed, and the assistant closed the run on the first one.
  const reviews = [step("b", "builder", "succeeded"), step("s", "security", "abandoned"), step("u", "ux", "failed")];

  it("is found on whichever step it closed", () => {
    expect(closedByAssistant(ended, reviews, { s: [closed("s", 7, "Claude Code")] })).toBe("Claude Code");
  });

  it("is nothing while the run is open, or when the person closed it", () => {
    expect(closedByAssistant({ finishedAt: null }, reviews, { s: [closed("s", 7, "Claude Code")] })).toBeNull();
    expect(closedByAssistant(ended, reviews, { s: [closed("s", 7)] })).toBeNull();
    expect(closedByAssistant(ended, reviews, {})).toBeNull();
  });

  it("goes by the last close, on steps as they stand", () => {
    const both = [step("s", "security", "abandoned"), step("u", "ux", "abandoned")];
    expect(closedByAssistant(ended, both, { s: [closed("s", 7, "Claude Code")], u: [closed("u", 9)] })).toBeNull();
    expect(closedByAssistant(ended, both, { s: [closed("s", 9, "Claude Code")], u: [closed("u", 7)] })).toBe("Claude Code");
    // The person tried that step again, and the run ended some other way.
    const again = [step("s", "security", "abandoned"), step("s2", "security", "succeeded"), step("h", "human", "rejected")];
    expect(closedByAssistant(ended, again, { s: [closed("s", 7, "Claude Code")] })).toBeNull();
  });
});

describe("what approving leads to", () => {
  const spec = { type: "agent", id: "spec" };
  const human = { type: "approval", id: "human" };
  const pr = { type: "deliver", id: "pr" };
  const learn = { type: "agent", id: "learn", role: { kind: "learner" } };

  it("opens a pull request when delivery comes next", () => {
    expect(approvalOutcome([spec, human, pr], "human", ["a.py"])).toBe("delivers");
    expect(approvalOutcome([spec, human, pr, learn], "human", ["a.py"])).toBe("delivers");
  });

  it("has nothing to deliver when the run changed nothing", () => {
    expect(approvalOutcome([spec, human, pr], "human", [])).toBe("nothingToDeliver");
    expect(approvalOutcome([spec, human, pr, learn], "human", [])).toBe("nothingToDeliver");
  });

  it("isn't empty yet when the approval comes before the building", () => {
    const builder = { type: "agent", id: "builder" };
    expect(approvalOutcome([spec, human, builder, pr], "human", [])).toBe("deliversLater");
    expect(approvalOutcome([spec, human, builder, pr], "human", ["a.py"])).toBe("deliversLater");
  });

  it("pushes nothing in a workflow that never delivers", () => {
    expect(approvalOutcome([spec, human], "human", [])).toBe("noDelivery");
    expect(approvalOutcome([spec, human], "human", ["a.py"])).toBe("noDelivery");
    expect(approvalOutcome([spec, human, learn], "human", ["a.py"])).toBe("noDelivery");
    expect(approvalOutcome([spec, human, { type: "check", id: "check" }], "human", ["a.py"])).toBe("noDelivery");
  });
});

describe("the accounts a run used", () => {
  it("says which Claude Code and which GitHub account did the work", () => {
    const attempts = [
      { kind: "check", account: null },
      { kind: "agent", account: "you@example.com" },
      { kind: "deliver", account: "you-work" },
    ];
    expect(accountsOf(attempts)).toEqual({ claude: "you@example.com", github: "you-work" });
    expect(accountsOf([{ kind: "agent", account: null }])).toEqual({ claude: null, github: null });
  });
});

describe("a run with several steps", () => {
  const a = (stepId: string, state: string) => ({ stepId, state });

  it("counts only the latest try of each step", () => {
    const failed = a("code", "failed");
    const retried = a("code", "succeeded");
    expect(latestOfEachStep([failed, a("ux", "succeeded"), retried]).map((x) => x.state)).toEqual(["succeeded", "succeeded"]);
  });

  it("doesn't take an approval put off for a person's attention, since what they asked for is going", () => {
    const attempts = [a("human", "postponed"), a("security", "running")];
    expect(focusAttempt(attempts)?.stepId).toBe("security");
    expect(stillGoing({ finishedAt: null }, attempts)).toBe(true);
    expect(stillGoing({ finishedAt: null }, [a("human", "postponed")])).toBe(true);
    expect(focusAttempt([a("human", "postponed"), a("human", "waitingApproval")])?.state).toBe("waitingApproval");
  });

  it("focuses on what needs a person, ahead of what's working", () => {
    expect(focusAttempt([a("security", "running"), a("code", "failed"), a("ux", "running")])?.stepId).toBe("code");
    expect(focusAttempt([a("security", "succeeded"), a("ux", "running")])?.stepId).toBe("ux");
    expect(focusAttempt([a("builder", "succeeded"), a("human", "waitingApproval")])?.stepId).toBe("human");
    expect(focusAttempt([a("setup", "succeeded"), a("spec", "waitingAnswers")])?.stepId).toBe("spec");
    expect(focusAttempt([a("pr", "succeeded")])?.stepId).toBe("pr");
  });

  it("doesn't dwell on a failure that was already retried", () => {
    expect(focusAttempt([a("code", "failed"), a("ux", "running"), a("code", "running")])?.state).toBe("running");
    expect(focusAttempt([a("code", "failed"), a("code", "succeeded")])?.state).toBe("succeeded");
  });

  it("isn't about a proposal step that failed, unless it's still working", () => {
    const delivered = { stepId: "pr", state: "succeeded", role: "worker" };
    expect(focusAttempt([delivered, { stepId: "learn", state: "failed", role: "learner" }])).toBe(delivered);
    expect(focusAttempt([delivered, { stepId: "learn", state: "running", role: "learner" }])?.stepId).toBe("learn");
    const alone = { stepId: "learn", state: "failed", role: "learner" };
    expect(focusAttempt([alone])).toBe(alone);
  });
});

describe("stillGoing", () => {
  const open = { finishedAt: null };

  it("is true while something works, between two steps, and through a stall", () => {
    expect(stillGoing(open, [{ stepId: "a", state: "running" }])).toBe(true);
    expect(stillGoing(open, [{ stepId: "a", state: "waitingAnswers" }])).toBe(false);
    expect(stillGoing(open, [{ stepId: "a", state: "returned" }])).toBe(true);
    expect(stillGoing(open, [{ stepId: "a", state: "succeeded" }])).toBe(true);
    expect(stillGoing(open, [{ stepId: "a", state: "stalled" }])).toBe(true);
  });

  it("is false once a person has to answer, or the run ended", () => {
    for (const state of ["waitingApproval", "blocked", "interrupted", "failed", "stopped", "rejected"]) {
      expect(stillGoing(open, [{ stepId: "a", state }])).toBe(false);
    }
    expect(stillGoing({ finishedAt: "2026-01-01T00:00:00Z" }, [{ stepId: "a", state: "running" }])).toBe(false);
  });

  it("looks at each step's latest attempt, so a retry that's running counts as going", () => {
    expect(stillGoing(open, [{ stepId: "a", state: "failed" }, { stepId: "a", state: "running" }])).toBe(true);
  });

  it("isn't held by a reviewer's sibling finishing, but is by one that fails", () => {
    expect(stillGoing(open, [{ stepId: "a", state: "succeeded" }, { stepId: "b", state: "running" }])).toBe(true);
    expect(stillGoing(open, [{ stepId: "a", state: "failed" }, { stepId: "b", state: "running" }])).toBe(false);
  });

  it("doesn't count proposals that failed: they're a bonus", () => {
    expect(stillGoing(open, [{ stepId: "learn", state: "failed", role: "learner" }])).toBe(true);
  });
});
