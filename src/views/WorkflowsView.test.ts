import { describe, expect, it } from "vitest";
import type { StepSpec, WorkflowStep } from "../lib/api";
import { allIds, freshId, insertedStep, memberAdded, moved, movedStep, pathExists, removedStep } from "./WorkflowsView";

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

describe("changes to the steps", () => {
  const member = (id: string): StepSpec => ({ id, model: "m", effort: null, tools: "readOnly", prompt: "p", maxTurns: null, maxBudgetUsd: null });
  const a: WorkflowStep = { type: "approval", id: "a" };
  const b: WorkflowStep = { type: "deliver", id: "b" };
  const group: WorkflowStep = { type: "parallel", id: "g", steps: [member("x"), member("y"), member("z")] };
  const members = (steps: WorkflowStep[], stage: number) => {
    const found = steps[stage];
    return found.type === "parallel" ? found.steps.map((s) => s.id) : [];
  };
  const ids = (steps: WorkflowStep[]) => steps.map((s) => s.id);

  it("inserts at any gap, and selects what was inserted", () => {
    expect(ids(insertedStep([a, b], 0, group).steps)).toEqual(["g", "a", "b"]);
    expect(ids(insertedStep([a, b], 1, group).steps)).toEqual(["a", "g", "b"]);
    const atEnd = insertedStep([a, b], 2, group);
    expect(ids(atEnd.steps)).toEqual(["a", "b", "g"]);
    expect(atEnd.selected).toEqual({ stage: 2, member: null });
    expect(insertedStep([], 0, a)).toEqual({ steps: [a], selected: { stage: 0, member: null } });
  });

  it("leaves what it was given alone", () => {
    const steps = [a, group];
    insertedStep(steps, 0, b);
    removedStep(steps, { stage: 0, member: null });
    removedStep(steps, { stage: 1, member: 0 });
    movedStep(steps, { stage: 1, member: 1 }, -1);
    memberAdded(steps, 1, member("n"));
    expect(ids(steps)).toEqual(["a", "g"]);
    expect(members(steps, 1)).toEqual(["x", "y", "z"]);
  });

  it("moves a stage and takes the selection with it", () => {
    expect(movedStep([a, b, group], { stage: 0, member: null }, 1)).toMatchObject({ selected: { stage: 1, member: null } });
    expect(ids(movedStep([a, b, group], { stage: 2, member: null }, -1).steps)).toEqual(["a", "g", "b"]);
  });

  it("stays put at the edges", () => {
    const steps = [a, b];
    expect(movedStep(steps, { stage: 0, member: null }, -1)).toEqual({ steps, selected: { stage: 0, member: null } });
    expect(movedStep(steps, { stage: 1, member: null }, 1)).toEqual({ steps, selected: { stage: 1, member: null } });
  });

  it("moves inside a group, and stays inside it", () => {
    const up = movedStep([a, group], { stage: 1, member: 1 }, -1);
    expect(members(up.steps, 1)).toEqual(["y", "x", "z"]);
    expect(up.selected).toEqual({ stage: 1, member: 0 });
    expect(movedStep([a, group], { stage: 1, member: 0 }, -1).selected).toEqual({ stage: 1, member: 0 });
    expect(movedStep([a, group], { stage: 1, member: 2 }, 1).selected).toEqual({ stage: 1, member: 2 });
  });

  it("selects what took a removed stage's place", () => {
    expect(removedStep([a, group, b], { stage: 1, member: null })).toEqual({ steps: [a, b], selected: { stage: 1, member: null } });
  });

  it("selects the one before when the last stage goes", () => {
    expect(removedStep([a, b], { stage: 1, member: null }).selected).toEqual({ stage: 0, member: null });
  });

  it("selects nothing when nothing's left", () => {
    expect(removedStep([a], { stage: 0, member: null })).toEqual({ steps: [], selected: null });
  });

  it("removes one member and selects the next, or the one before", () => {
    const first = removedStep([group], { stage: 0, member: 0 });
    expect(members(first.steps, 0)).toEqual(["y", "z"]);
    expect(first.selected).toEqual({ stage: 0, member: 0 });
    expect(removedStep([group], { stage: 0, member: 2 }).selected).toEqual({ stage: 0, member: 1 });
  });

  it("selects the group when its last member goes", () => {
    const only: WorkflowStep = { type: "parallel", id: "g", steps: [member("x")] };
    expect(removedStep([only], { stage: 0, member: 0 })).toEqual({ steps: [{ ...only, steps: [] }], selected: { stage: 0, member: null } });
  });

  it("adds to the end of a group and selects it", () => {
    const added = memberAdded([a, group], 1, member("n"));
    expect(members(added.steps, 1)).toEqual(["x", "y", "z", "n"]);
    expect(added.selected).toEqual({ stage: 1, member: 3 });
  });

  it("adds nothing to what isn't a group", () => {
    expect(memberAdded([a], 0, member("n")).steps).toEqual([a]);
  });

  it("knows when a path no longer leads anywhere", () => {
    expect(pathExists([a, group], { stage: 1, member: 2 })).toBe(true);
    expect(pathExists([a, group], { stage: 1, member: null })).toBe(true);
    expect(pathExists([a, group], { stage: 1, member: 3 })).toBe(false);
    expect(pathExists([a, group], { stage: 2, member: null })).toBe(false);
    expect(pathExists([a, group], { stage: 0, member: 0 })).toBe(false);
    expect(pathExists([a, group], null)).toBe(false);
  });
});
