import { describe, expect, it } from "vitest";
import { allIds, freshId, moved } from "./WorkflowsView";

describe("workflow editor helpers", () => {
  it("names a new step without clashing", () => {
    expect(freshId("check", [])).toBe("check");
    expect(freshId("check", ["check"])).toBe("check-2");
    expect(freshId("check", ["check", "check-2"])).toBe("check-3");
  });

  it("sees the names inside groups", () => {
    const parallel = { type: "parallel" as const, id: "g", steps: [{ id: "a", model: "m", effort: null, tools: "readOnly" as const, prompt: "p", maxTurns: null, maxBudgetUsd: null }] };
    expect(allIds([{ type: "approval", id: "human" }, parallel])).toEqual(["human", "g", "a"]);
  });

  it("moves steps, and stays put at the edges", () => {
    expect(moved(["a", "b", "c"], 1, -1)).toEqual(["b", "a", "c"]);
    expect(moved(["a", "b", "c"], 1, 1)).toEqual(["a", "c", "b"]);
    expect(moved(["a", "b"], 0, -1)).toEqual(["a", "b"]);
    expect(moved(["a", "b"], 1, 1)).toEqual(["a", "b"]);
  });
});
