import { describe, expect, it } from "vitest";
import type { AuditEvent } from "./api";
import { activityFrom, commandsFrom, explainingAttempts, filesChangedBy, flatSteps, focusAttempt, latestOfEachStep, toolSummary, touchesWorkflows } from "./audit";

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

  it("opens up parallel groups", () => {
    expect(flatSteps([{ type: "agent", id: "a" }, { type: "parallel", id: "g", steps: [{ id: "b" }, { id: "c" }] }]).map((s) => s.id)).toEqual(["a", "b", "c"]);
  });
});
