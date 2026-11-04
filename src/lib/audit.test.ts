import { describe, expect, it } from "vitest";
import type { AuditEvent } from "./api";
import { activityFrom, commandsFrom, toolSummary } from "./audit";

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
