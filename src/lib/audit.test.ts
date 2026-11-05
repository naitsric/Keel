import { describe, expect, it } from "vitest";
import type { AuditEvent } from "./api";
import { accountsOf, activityFrom, approvalOutcome, commandsFrom, explainingAttempts, filesChangedBy, focusAttempt, gist, latestOfEachStep, reviewsOfRound, toolSummary, touchesWorkflows } from "./audit";

const event = (seq: number, kind: string, data: Record<string, unknown>): AuditEvent => ({ seq, attemptId: "a", at: "2026-01-01T00:00:00Z", kind, data });

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

  it("focuses on what needs a person, ahead of what's working", () => {
    expect(focusAttempt([a("security", "running"), a("code", "failed"), a("ux", "running")])?.stepId).toBe("code");
    expect(focusAttempt([a("security", "succeeded"), a("ux", "running")])?.stepId).toBe("ux");
    expect(focusAttempt([a("builder", "succeeded"), a("human", "waitingApproval")])?.stepId).toBe("human");
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
