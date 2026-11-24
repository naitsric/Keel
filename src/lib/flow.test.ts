import { describe, expect, it } from "vitest";
import type { AttemptState, StepRole, StepSpec, WorkflowStep } from "./api";
import { allNodes, attemptChoices, type FlowAttempt, flowOf, layoutFlow, nextItem, nodeOfAttempt, nodeWithKey, parseSteps, pathKey, pathOfKey, problemKeys, scrollToShow, canSendBack, canSendBackFromCheck, reworkIndex, isReadOnlyTask } from "./flow";

const spec = (id: string, model = "sonnet", role?: StepRole): StepSpec => ({ id, model, effort: null, tools: "readOnly", prompt: "p", maxTurns: null, maxBudgetUsd: null, ...(role ? { role } : {}) });
const agent = (id: string, model = "sonnet", role?: StepRole): WorkflowStep => ({ type: "agent", ...spec(id, model, role) });
const approver = (id: string, reworkTo: string, maxRounds = 2): WorkflowStep => agent(id, "opus", { kind: "approver", reworkTo, maxRounds });
const group = (id: string, ...members: string[]): WorkflowStep => ({ type: "parallel", id, steps: members.map((m) => spec(m, "sonnet", { kind: "reviewer" })) });

let serial = 0;
const attempt = (stepId: string, state: AttemptState, fields: Partial<FlowAttempt> = {}): FlowAttempt => ({ id: `a${serial++}`, stepId, kind: "agent", role: "worker", round: 0, number: 1, state, modelRequested: "sonnet", modelActual: null, output: null, ...fields });

const FULL: WorkflowStep[] = [agent("spec", "opus"), agent("builder"), { type: "check", id: "check", run: "make test" }, group("reviews", "security", "ux"), approver("approver", "builder"), { type: "approval", id: "human" }, { type: "deliver", id: "pr" }, agent("learn", "sonnet", { kind: "learner" })];

/** Every attempt given must be somewhere, and only once. */
function expectAllReachable(steps: WorkflowStep[], attempts: FlowAttempt[]) {
  const reached = allNodes(flowOf(steps, attempts)).flatMap((node) => node.attempts);
  expect([...reached].sort((a, b) => a.id.localeCompare(b.id))).toEqual([...attempts].sort((a, b) => a.id.localeCompare(b.id)));
}

describe("the stages of a workflow", () => {
  it("keeps the order, with a group as one stage", () => {
    const flow = flowOf(FULL);
    expect(flow.stages.map((stage) => (stage.type === "parallel" ? [stage.id, stage.nodes.map((n) => n.id)] : stage.node.id))).toEqual(["spec", "builder", "check", ["reviews", ["security", "ux"]], "approver", "human", "pr", "learn"]);
  });

  it("says what each node is", () => {
    const nodes = allNodes(flowOf(FULL));
    const of = (id: string) => nodes.find((n) => n.id === id);
    expect(of("spec")).toMatchObject({ kind: "agent", role: "worker", model: "opus" });
    expect(of("security")).toMatchObject({ kind: "agent", role: "reviewer", model: "sonnet" });
    expect(of("approver")).toMatchObject({ kind: "agent", role: "approver", model: "opus" });
    expect(of("check")).toMatchObject({ kind: "check", role: null, model: null });
    expect(of("human")).toMatchObject({ kind: "approval", role: null, model: null });
    expect(of("pr")).toMatchObject({ kind: "deliver", role: null, model: null });
  });

  it("reads a worker with no role, since the backend leaves it out", () => {
    const flow = flowOf([{ type: "agent", id: "a", model: "opus" } as WorkflowStep]);
    expect(allNodes(flow)[0]).toMatchObject({ role: "worker", minor: false });
  });

  it("has nothing to say about state without attempts", () => {
    const flow = flowOf(FULL);
    expect(flow.live).toBe(false);
    expect(allNodes(flow).every((n) => n.state === "pending" && n.attempt === null && n.attempts.length === 0 && n.round === 0 && !n.outdated)).toBe(true);
    expect(flow.extras).toEqual([]);
  });

  it("is live with attempts, even an empty list", () => {
    expect(flowOf(FULL, []).live).toBe(true);
  });

  it("shows the setup Keel put in front as the first step, with nothing to say about models", () => {
    const flow = flowOf([{ type: "setup", id: "setup", run: "pnpm install" }, ...FULL], [attempt("setup", "succeeded", { kind: "setup", modelRequested: "" })]);
    const first = flow.stages[0];
    expect(first.type === "step" && first.node).toMatchObject({ id: "setup", kind: "setup", role: null, model: null, state: "succeeded", minor: false });
    expect(flow.extras).toEqual([]);
  });

  it("makes only the learner minor", () => {
    expect(
      allNodes(flowOf(FULL))
        .filter((n) => n.minor)
        .map((n) => n.id),
    ).toEqual(["learn"]);
  });

  it("gives every node a key that tells two steps with one name apart", () => {
    const flow = flowOf([agent("a"), agent("a"), group("g", "b", "b")]);
    expect(allNodes(flow).map((n) => n.key)).toEqual(["0", "1", "2.0", "2.1"]);
  });
});

describe("what attempts say about a node", () => {
  it("is pending until it starts", () => {
    const flow = flowOf(FULL, [attempt("spec", "running")]);
    expect(nodeWithKey(flow, "0")).toMatchObject({ state: "running" });
    expect(nodeWithKey(flow, "1")).toMatchObject({ state: "pending", attempt: null, attempts: [] });
  });

  it("takes the state of the latest attempt, of every state the app has", () => {
    const states: AttemptState[] = ["queued", "preparing", "running", "stalled", "succeeded", "failed", "stopped", "interrupted", "blocked", "waitingApproval", "waitingAnswers", "rejected", "returned", "skipped", "abandoned"];
    for (const state of states) {
      expect(nodeWithKey(flowOf(FULL, [attempt("spec", "failed"), attempt("spec", state, { number: 2 })]), "0")?.state).toBe(state);
    }
  });

  it("maps to the attempt it shows, and keeps the others in order", () => {
    const first = attempt("builder", "failed");
    const second = attempt("builder", "succeeded", { number: 2 });
    const node = nodeWithKey(flowOf(FULL, [first, attempt("spec", "succeeded"), second]), "1");
    expect(node?.attempt).toBe(second);
    expect(node?.attempts).toEqual([first, second]);
  });

  it("counts rounds as the engine does, from 0", () => {
    const node = nodeWithKey(flowOf(FULL, [attempt("builder", "succeeded"), attempt("builder", "running", { round: 1, number: 2 })]), "1");
    expect(node).toMatchObject({ round: 1 });
    expect(node?.attempts).toHaveLength(2);
  });

  it("gives a group's members their own state, including skipped", () => {
    const flow = flowOf(FULL, [attempt("security", "succeeded"), attempt("ux", "skipped")]);
    expect(nodeWithKey(flow, "3.0")?.state).toBe("succeeded");
    expect(nodeWithKey(flow, "3.1")?.state).toBe("skipped");
  });

  it("prefers the model that really ran, then the one asked for, then the workflow's", () => {
    const steps = [agent("a", "from-workflow")];
    expect(allNodes(flowOf(steps))[0].model).toBe("from-workflow");
    expect(allNodes(flowOf(steps, [attempt("a", "running", { modelRequested: "asked" })]))[0].model).toBe("asked");
    expect(allNodes(flowOf(steps, [attempt("a", "running", { modelRequested: "asked", modelActual: "claude-real" })]))[0].model).toBe("claude-real");
  });

  it("gives steps with no model none, whatever their attempts carry", () => {
    expect(allNodes(flowOf([{ type: "check", id: "c", run: "x" }], [attempt("c", "running", { kind: "check", modelRequested: "" })]))[0].model).toBeNull();
  });

  it("finds the node of an attempt", () => {
    const shown = attempt("ux", "running");
    const flow = flowOf(FULL, [attempt("spec", "succeeded"), shown]);
    expect(nodeOfAttempt(flow, shown)?.id).toBe("ux");
    expect(nodeOfAttempt(flow, undefined)).toBeUndefined();
  });
});

describe("a step that depends on the change", () => {
  const steps: WorkflowStep[] = [agent("builder"), { type: "parallel", id: "reviews", steps: [{ ...spec("security"), when: "notSmall" }, spec("code")] }, { ...agent("approver"), when: "notSmall" } as WorkflowStep, { type: "approval", id: "human" }];

  it("says so on its node, and a step saved with nothing says nothing", () => {
    const nodes = allNodes(flowOf(steps));
    expect(nodes.map((node) => [node.id, node.condition])).toEqual([["builder", null], ["security", "notSmall"], ["code", null], ["approver", "notSmall"], ["human", null]]);
  });

  it("says when it only fixes what was found, or repeats a check, each with its own", () => {
    const nodes = allNodes(flowOf([{ ...agent("ship"), when: "somethingToFix" } as WorkflowStep, { type: "check", id: "check", run: "make test" }, { type: "check", id: "again", run: "make test", onlyIfChanged: true }]));
    expect(nodes.map((node) => node.condition)).toEqual(["somethingToFix", null, "onlyIfChanged"]);
  });

  it("counts as gone past once it's been skipped for being small, and work still goes back to where it says", () => {
    const flow = flowOf(steps, [attempt("builder", "succeeded"), attempt("approver", "skipped", { reason: { code: "treatedAsSmall" } })]);
    expect(allNodes(flow).find((node) => node.id === "approver")?.state).toBe("skipped");
    const line = layoutFlow(flowOf(steps, [attempt("builder", "succeeded"), attempt("human", "postponed")]));
    expect(line.lines.some((l) => l.travelled)).toBe(true);
  });
});

describe("where work goes back to", () => {
  const worker = (id: string) => ({ type: "agent", id, model: "sonnet", tools: "build", prompt: "p", effort: null, maxTurns: null, maxBudgetUsd: null }) as WorkflowStep;
  const reader = (id: string) => ({ ...worker(id), tools: "readOnly" }) as WorkflowStep;
  const human = { type: "approval", id: "human" } as WorkflowStep;

  it("is what the approver names, or else the last worker that builds, as in the engine", () => {
    expect(reworkIndex(FULL)).toBe(1);
    expect(reworkIndex([reader("spec"), worker("first"), worker("second"), human])).toBe(2);
    expect(reworkIndex([reader("spec"), human])).toBeNull();
    expect(reworkIndex([worker("first"), human, worker("after")])).toBe(0);
  });

  it("has to come before the person sending it back", () => {
    expect(canSendBack([worker("builder"), human], "human")).toBe(true);
    expect(canSendBack([human, worker("builder")], "human")).toBe(false);
    expect(canSendBack([reader("spec"), human], "human")).toBe(false);
    expect(canSendBack([worker("builder"), human], "missing")).toBe(false);
  });

  it("marks what ran before as out of date in a workflow with no approver too", () => {
    const steps = [reader("spec"), worker("builder"), { type: "check", id: "check", run: "x" } as WorkflowStep, human];
    const before = [attempt("spec", "succeeded"), attempt("builder", "succeeded"), attempt("check", "succeeded", { kind: "check" }), attempt("human", "returned", { kind: "approval" })];
    const flow = flowOf(steps, [...before, attempt("builder", "running", { round: 1, number: 2 })], 1);
    expect(nodeWithKey(flow, "0")).toMatchObject({ outdated: false });
    expect(nodeWithKey(flow, "2")).toMatchObject({ outdated: true });
    expect(nodeWithKey(flow, "3")).toMatchObject({ state: "returned", outdated: true });
  });

  it("doesn't count a failed check Keel sent back as the approver's doing either", () => {
    const failed = attempt("check", "returned", { kind: "check" });
    expect(flowOf(FULL, [failed, attempt("builder", "running", { round: 1, number: 2 })], 1).reworks[0].times).toBe(0);
    // The check that went back reads as out of date, not as a step that waits for anyone.
    expect(nodeWithKey(flowOf(FULL, [failed, attempt("builder", "running", { round: 1, number: 2 })], 1), "2")).toMatchObject({ state: "returned", outdated: true });
  });

  it("lets a person send the work back from a failed check only when a builder comes before it", () => {
    const check = { type: "check", id: "check", run: "x" } as WorkflowStep;
    expect(canSendBackFromCheck([reader("spec"), worker("builder"), check, human], "check")).toBe(true);
    expect(canSendBackFromCheck([worker("builder"), check, approver("approver", "builder"), human], "check")).toBe(true);
    expect(canSendBackFromCheck([check], "check")).toBe(false);
    expect(canSendBackFromCheck([check, worker("builder"), human], "check")).toBe(false);
    expect(canSendBackFromCheck([reader("spec"), check, human], "check")).toBe(false);
    expect(canSendBackFromCheck([worker("builder"), check, human], "builder")).toBe(false);
  });

  it("doesn't count what a person sent back as the approver's doing", () => {
    const attempts = [attempt("approver", "succeeded", { output: { decision: "approve" } }), attempt("human", "returned", { kind: "approval" })];
    expect(flowOf(FULL, attempts, 1).reworks[0].times).toBe(0);
    expect(flowOf(FULL, [...attempts, attempt("approver", "succeeded", { round: 1, output: { decision: "rework" } })], 2).reworks[0].times).toBe(1);
  });
});

describe("a step that ran before the run went back", () => {
  const roundOne = [attempt("spec", "succeeded"), attempt("builder", "succeeded"), attempt("check", "succeeded", { kind: "check" }), attempt("security", "succeeded"), attempt("ux", "skipped"), attempt("approver", "succeeded", { output: { decision: "rework" } })];
  const flow = flowOf(FULL, [...roundOne, attempt("builder", "running", { round: 1, number: 2 })], 1);

  it("isn't taken for done: it's marked as out of date, and keeps its state", () => {
    expect(nodeWithKey(flow, "2")).toMatchObject({ state: "succeeded", outdated: true, round: 0 });
    expect(nodeWithKey(flow, "3.0")).toMatchObject({ state: "succeeded", outdated: true });
    expect(nodeWithKey(flow, "3.1")).toMatchObject({ state: "skipped", outdated: true });
    expect(nodeWithKey(flow, "4")).toMatchObject({ state: "succeeded", outdated: true });
  });

  it("doesn't touch what comes before the step it went back to, which won't run again", () => {
    expect(nodeWithKey(flow, "0")).toMatchObject({ state: "succeeded", outdated: false });
  });

  it("nor the step it went back to, once that's running again", () => {
    expect(nodeWithKey(flow, "1")).toMatchObject({ state: "running", round: 1, outdated: false });
  });

  it("clears when the step runs in the new round", () => {
    const later = flowOf(FULL, [...roundOne, attempt("builder", "succeeded", { round: 1, number: 2 }), attempt("check", "running", { kind: "check", round: 1, number: 2 })], 1);
    expect(nodeWithKey(later, "2")).toMatchObject({ state: "running", outdated: false });
    expect(nodeWithKey(later, "4")).toMatchObject({ outdated: true });
  });

  it("knows the round moved on from the run, before any attempt of it exists", () => {
    expect(nodeWithKey(flowOf(FULL, roundOne, 1), "1")).toMatchObject({ outdated: true });
    expect(nodeWithKey(flowOf(FULL, roundOne), "1")).toMatchObject({ outdated: false });
  });

  it("never hides something under way or waiting for the person, whatever round it's from", () => {
    const states: AttemptState[] = ["queued", "preparing", "running", "stalled", "waitingApproval", "waitingAnswers", "blocked", "failed", "interrupted", "stopped", "rejected", "abandoned"];
    for (const state of states) {
      const node = nodeWithKey(flowOf(FULL, [attempt("human", state, { kind: "approval" }), attempt("builder", "running", { round: 1, number: 2 })], 1), "5");
      expect(node, state).toMatchObject({ state, outdated: false });
    }
  });

  it("marks nothing in a workflow that can't go back", () => {
    const steps = [agent("a"), agent("b")];
    const flow2 = flowOf(steps, [attempt("a", "succeeded"), attempt("b", "running", { round: 1 })]);
    expect(allNodes(flow2).some((n) => n.outdated)).toBe(false);
  });
});

describe("the way back", () => {
  it("goes from the approver to its target", () => {
    expect(flowOf(FULL).reworks).toEqual([{ from: 4, to: 1, fromId: "approver", toId: "builder", times: 0, maxRounds: 2 }]);
  });

  it("is absent without an approver", () => {
    expect(flowOf([agent("a"), agent("b")]).reworks).toEqual([]);
  });

  it("can target a group, as a hand-edited file may", () => {
    expect(flowOf([group("reviews", "a"), approver("z", "reviews")]).reworks[0]).toMatchObject({ from: 1, to: 0 });
  });

  it("is left out if the target is missing or isn't earlier, as it can be halfway through an edit", () => {
    expect(flowOf([agent("a"), approver("z", "gone")]).reworks).toEqual([]);
    expect(flowOf([approver("z", "a"), agent("a")]).reworks).toEqual([]);
    expect(flowOf([agent("a"), approver("z", "z")]).reworks).toEqual([]);
  });

  it("counts how many times it went back", () => {
    const times = (attempts: FlowAttempt[], round = 0) => flowOf(FULL, attempts, round).reworks[0].times;
    expect(times([attempt("approver", "succeeded", { output: { decision: "approve" } })])).toBe(0);
    expect(times([attempt("approver", "succeeded", { output: { decision: "rework" } }), attempt("builder", "running", { round: 1, number: 2 })], 1)).toBe(1);
    expect(times([attempt("builder", "running", { round: 2, number: 3 })], 2)).toBe(2);
  });

  it("doesn't count asking for rework at the limit, where the run stopped and a person decides", () => {
    const attempts = [attempt("approver", "succeeded", { output: { decision: "rework" } }), attempt("approver", "succeeded", { round: 1, number: 2, output: { decision: "rework" } }), attempt("approver", "blocked", { round: 2, number: 3, output: { decision: "rework" } })];
    expect(flowOf(FULL, attempts, 2).reworks[0].times).toBe(2);
  });

  it("counts a person's one more round", () => {
    expect(flowOf(FULL, [attempt("builder", "running", { round: 3, number: 4 })], 3).reworks[0].times).toBe(3);
  });

  it("gives each approver the times it went back, when a workflow has two", () => {
    const steps = [agent("a"), approver("first", "a"), agent("b"), approver("second", "b")];
    const attempts = [attempt("first", "succeeded", { output: { decision: "rework" } }), attempt("first", "succeeded", { round: 1, output: { decision: "approve" } }), attempt("second", "succeeded", { round: 1, output: { decision: "rework" } }), attempt("a", "running", { round: 2 })];
    expect(flowOf(steps, attempts).reworks.map((r) => [r.fromId, r.times])).toEqual([
      ["first", 1],
      ["second", 1],
    ]);
  });
});

describe("attempts outside the workflow", () => {
  it("stay reachable, in the order they first ran", () => {
    const stray = attempt("old-step", "failed");
    const other = attempt("gone", "succeeded", { kind: "check" });
    const flow = flowOf([agent("a")], [stray, attempt("a", "running"), other, attempt("old-step", "succeeded", { number: 2 })]);
    expect(flow.extras.map((n) => [n.key, n.id, n.kind, n.state, n.attempts.length])).toEqual([
      ["x0", "old-step", "agent", "succeeded", 2],
      ["x1", "gone", "check", "succeeded", 1],
    ]);
    expect(flow.extras[0].attempts[0]).toBe(stray);
  });

  it("are never marked out of date", () => {
    const flow = flowOf(FULL, [attempt("stray", "succeeded"), attempt("builder", "running", { round: 1 })]);
    expect(flow.extras[0].outdated).toBe(false);
  });

  it("include an attempt named like a group, which isn't a step", () => {
    const flow = flowOf(FULL, [attempt("reviews", "failed")]);
    expect(flow.extras.map((n) => n.id)).toEqual(["reviews"]);
  });

  it("keep their role and model", () => {
    const flow = flowOf([], [attempt("x", "running", { role: "reviewer", modelActual: "claude-x" }), attempt("c", "running", { kind: "check", modelRequested: "" })]);
    expect(flow.extras[0]).toMatchObject({ role: "reviewer", model: "claude-x" });
    expect(flow.extras[1]).toMatchObject({ role: null, model: null });
  });

  it("are all there is when the workflow can't be read", () => {
    const attempts = [attempt("a", "succeeded"), attempt("b", "running")];
    const flow = flowOf(parseSteps(null) ?? [], attempts);
    expect(flow.stages).toEqual([]);
    expect(flow.extras.map((n) => n.id)).toEqual(["a", "b"]);
  });

  it("leaves every attempt reachable once, whatever the mix", () => {
    const mix = [attempt("spec", "succeeded"), attempt("builder", "failed"), attempt("builder", "succeeded", { number: 2 }), attempt("builder", "running", { round: 1, number: 3 }), attempt("security", "succeeded"), attempt("ux", "skipped"), attempt("stray", "succeeded"), attempt("reviews", "failed")];
    expectAllReachable(FULL, mix);
    expectAllReachable([], mix);
    expectAllReachable([agent("builder"), agent("builder")], mix);
    expectAllReachable(parseSteps({ not: "a list" }) ?? [], mix);
  });

  it("goes to the first of two steps with one name, and only once", () => {
    const flow = flowOf([agent("a"), agent("a")], [attempt("a", "running")]);
    expect(nodeWithKey(flow, "0")?.attempts).toHaveLength(1);
    expect(nodeWithKey(flow, "1")?.attempts).toHaveLength(0);
  });
});

describe("a run's copy of its workflow", () => {
  it("reads the real thing", () => {
    expect(parseSteps(FULL)).toBe(FULL);
    expect(parseSteps([{ type: "agent", id: "builder" }])).toHaveLength(1);
  });

  it("reads an empty one", () => {
    expect(parseSteps([])).toEqual([]);
  });

  it.each([
    ["null, what a bad column comes back as", null],
    ["nothing", undefined],
    ["text", "not a workflow"],
    ["an object", { steps: [] }],
    ["a list of the wrong things", [1, 2]],
    ["a step with no name", [{ type: "agent" }]],
    ["a kind nobody knows", [{ type: "teleport", id: "a" }]],
    ["a group with no list", [{ type: "parallel", id: "g" }]],
    ["a group with a nameless member", [{ type: "parallel", id: "g", steps: [{}] }]],
  ])("refuses %s", (_, value) => {
    expect(parseSteps(value)).toBeNull();
  });
});

describe("a task an assistant started only to read", () => {
  const task = { startedBy: "Claude Code", workflowId: null, workflow: [agent("task")] };

  it("is one agent that only reads, with no saved workflow, that an assistant started", () => {
    expect(isReadOnlyTask(task)).toBe(true);
  });

  it.each([
    ["a person started it", { startedBy: null }],
    ["it came from a saved workflow", { workflowId: "builtin:full" }],
    ["it builds", { workflow: [{ ...agent("task"), tools: "build" }] }],
    ["it has more steps", { workflow: [agent("task"), agent("other")] }],
    ["its single step isn't an agent", { workflow: [{ type: "check", id: "check", run: "make test" }] }],
    ["its workflow can't be read", { workflow: null }],
  ])("isn't one when %s", (_, fields) => {
    expect(isReadOnlyTask({ ...task, ...fields })).toBe(false);
  });
});

describe("the choice between attempts", () => {
  it("names rounds when there are rounds", () => {
    const a = attempt("b", "succeeded");
    const b = attempt("b", "running", { round: 1, number: 2 });
    expect(attemptChoices([a, b])).toEqual([
      { attempt: a, round: 1, number: null },
      { attempt: b, round: 2, number: null },
    ]);
  });

  it("names attempts when it's only retries", () => {
    const a = attempt("b", "failed");
    const b = attempt("b", "succeeded", { number: 2 });
    expect(attemptChoices([a, b])).toEqual([
      { attempt: a, round: null, number: 1 },
      { attempt: b, round: null, number: 2 },
    ]);
  });

  it("names both when a round was retried, so no two read the same", () => {
    const attempts = [attempt("b", "succeeded"), attempt("b", "failed", { round: 1, number: 2 }), attempt("b", "running", { round: 1, number: 3 })];
    const labels = attemptChoices(attempts).map((c) => [c.round, c.number]);
    expect(labels).toEqual([
      [1, null],
      [2, 2],
      [2, 3],
    ]);
    expect(new Set(labels.map(String)).size).toBe(3);
  });

  it("still says it's a later round when only that round is there", () => {
    expect(attemptChoices([attempt("b", "running", { round: 1, number: 2 })])[0]).toMatchObject({ round: 2, number: null });
  });
});

describe("validation problems", () => {
  const flow = flowOf(FULL);

  it("land on the step they name", () => {
    expect(problemKeys(flow, "builder")).toEqual(["1"]);
  });

  it("reach inside groups", () => {
    expect(problemKeys(flow, "ux")).toEqual(["3.1"]);
  });

  it("land on a group's name when they're about the group", () => {
    expect(problemKeys(flow, "reviews")).toEqual(["3"]);
  });

  it("land on both steps when two share a name", () => {
    expect(problemKeys(flowOf([agent("a"), group("g", "a")]), "a")).toEqual(["0", "1.0"]);
  });

  it("land on a step whose name is empty, which is the name it complains about", () => {
    expect(problemKeys(flowOf([agent("ok"), agent("")]), "")).toEqual(["1"]);
  });

  it("land nowhere when the workflow as a whole is the problem", () => {
    expect(problemKeys(flowOf([]), "")).toEqual([]);
  });
});

describe("paths", () => {
  it("go to a key and back", () => {
    for (const path of [{ stage: 0, member: null }, { stage: 3, member: null }, { stage: 3, member: 1 }]) {
      expect(pathOfKey(pathKey(path))).toEqual(path);
    }
  });

  it("aren't anything else in the diagram", () => {
    expect(pathOfKey("x0")).toBeNull();
    expect(pathOfKey("gap1")).toBeNull();
  });
});

describe("the layout", () => {
  const overlaps = (a: { x: number; y: number; w: number; h: number }, b: { x: number; y: number; w: number; h: number }) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

  it.each([
    ["running", false],
    ["editing", true],
  ])("keeps every node apart and inside it, %s", (_, editing) => {
    const stray = editing ? undefined : [attempt("stray", "failed")];
    const layout = layoutFlow(flowOf(FULL, stray), editing);
    expect(layout.nodes).toHaveLength(editing ? 9 : 10);
    for (const a of layout.nodes) {
      expect(a.x).toBeGreaterThanOrEqual(0);
      expect(a.y).toBeGreaterThanOrEqual(0);
      expect(a.x + a.w).toBeLessThanOrEqual(layout.width);
      expect(a.y + a.h).toBeLessThanOrEqual(layout.height);
      for (const b of layout.nodes) if (a !== b) expect(overlaps(a, b), `${a.node.id} and ${b.node.id}`).toBe(false);
    }
  });

  it("stacks a group's members in one column, level with the rest", () => {
    const layout = layoutFlow(flowOf(FULL));
    const of = (id: string) => layout.nodes.find((n) => n.node.id === id) as (typeof layout.nodes)[number];
    expect(of("security").x).toBe(of("ux").x);
    expect(of("security").y).toBeLessThan(of("ux").y);
    const center = (n: typeof layout.nodes[number]) => n.y + n.h / 2;
    expect((center(of("security")) + center(of("ux"))) / 2).toBe(center(of("builder")));
    expect(center(of("builder"))).toBe(center(of("approver")));
  });

  it("gives the learner a smaller node than the rest", () => {
    const layout = layoutFlow(flowOf(FULL));
    const learner = layout.nodes.find((n) => n.node.id === "learn");
    const builder = layout.nodes.find((n) => n.node.id === "builder");
    expect(learner?.w).toBeLessThan(builder?.w ?? 0);
    expect(learner?.h).toBeLessThan(builder?.h ?? 0);
    expect(learner?.compact).toBe(true);
  });

  it("draws one line out of and into every member of a group, and a dashed one to the learner", () => {
    const layout = layoutFlow(flowOf(FULL));
    // spec→builder, builder→check and approver→human, human→pr, pr→learn: one out, one in.
    // check→group: one out, two in. group→approver: two out, one in.
    expect(layout.lines).toHaveLength(5 * 2 + 3 + 3);
    expect(layout.lines.filter((line) => line.head)).toHaveLength(5 + 2 + 1);
    expect(layout.lines.filter((line) => line.dashed)).toHaveLength(2);
  });

  it("marks only what the run has been through as travelled", () => {
    const steps = [agent("a"), agent("b"), agent("c")];
    const layout = layoutFlow(flowOf(steps, [attempt("a", "succeeded"), attempt("b", "running")]));
    expect(layout.lines.map((line) => line.travelled)).toEqual([true, true, false, false]);
  });

  it("doesn't count a step put off for a new setup as travelled, as it does an approval put off for more", () => {
    const steps: WorkflowStep[] = [{ type: "check", id: "check", run: "make test" }, { type: "approval", id: "human" }];
    const putOff = layoutFlow(flowOf(steps, [attempt("check", "failed", { kind: "check" }), attempt("check", "postponed", { kind: "check", number: 2 })]));
    expect(putOff.lines.length).toBeGreaterThan(0);
    expect(putOff.lines.every((line) => !line.travelled)).toBe(true);
    const asked = layoutFlow(flowOf([{ type: "approval", id: "human" }, { type: "deliver", id: "pr" }], [attempt("human", "postponed", { kind: "approval" })]));
    expect(asked.lines.every((line) => line.travelled)).toBe(true);
  });

  it("doesn't count an out-of-date step as travelled", () => {
    const steps = [agent("a"), approver("z", "a")];
    const layout = layoutFlow(flowOf(steps, [attempt("z", "succeeded"), attempt("a", "running", { round: 1 })]));
    expect(layout.lines.every((line) => !line.travelled)).toBe(true);
  });

  it("is one node, with no lines, for a single step", () => {
    const layout = layoutFlow(flowOf([agent("a")], [attempt("a", "running")]));
    expect(layout.nodes).toHaveLength(1);
    expect(layout.lines).toEqual([]);
    expect(layout.reworks).toEqual([]);
  });

  it("runs the way back underneath every node, from the approver to its target", () => {
    const layout = layoutFlow(flowOf(FULL));
    const [rework] = layout.reworks;
    const lowest = Math.max(...layout.nodes.map((n) => n.y + n.h));
    expect(rework.labelY).toBeGreaterThan(lowest);
    expect(rework.labelY).toBeLessThan(layout.height);
    const approverNode = layout.nodes.find((n) => n.node.id === "approver");
    const builderNode = layout.nodes.find((n) => n.node.id === "builder");
    expect(rework.d.startsWith(`M ${(approverNode?.x ?? 0) + (approverNode?.w ?? 0) / 2}`)).toBe(true);
    expect(rework.head).toContain(`${(builderNode?.x ?? 0) + (builderNode?.w ?? 0) / 2} `);
  });

  it("gives each way back its own lane", () => {
    const layout = layoutFlow(flowOf([agent("a"), approver("first", "a"), agent("b"), approver("second", "b")]));
    expect(layout.reworks[1].labelY).toBeGreaterThan(layout.reworks[0].labelY);
    expect(layout.height).toBeGreaterThan(layout.reworks[1].labelY);
  });

  it("puts what's not in the workflow after it, under a label, with no line to it", () => {
    const layout = layoutFlow(flowOf([agent("a")], [attempt("stray", "failed")]));
    const [a, stray] = layout.nodes;
    expect(stray.x).toBeGreaterThan(a.x + a.w);
    expect(layout.extrasLabel?.x).toBe(stray.x);
    expect(layout.lines).toEqual([]);
  });

  it("has no label when that's all there is", () => {
    expect(layoutFlow(flowOf([], [attempt("a", "failed")])).extrasLabel).toBeNull();
  });

  it("doesn't grow to fit a long name: a node is its size, whatever it holds", () => {
    const long = "a-very-long-step-name-that-goes-on-and-on-and-on-and-on";
    const short = layoutFlow(flowOf([agent("a")]));
    const wide = layoutFlow(flowOf([agent(long, long)]));
    expect(wide.width).toBe(short.width);
  });

  it("grows with the steps, so a long workflow scrolls instead of squeezing", () => {
    const many = Array.from({ length: 20 }, (_, i) => agent(`step-${i}`));
    expect(layoutFlow(flowOf(many)).width).toBeGreaterThan(900);
    const widths = new Set(layoutFlow(flowOf(many)).nodes.map((n) => n.w));
    expect(widths.size).toBe(1);
    expect(widths).toEqual(new Set(layoutFlow(flowOf(many.slice(0, 1))).nodes.map((n) => n.w)));
  });

  it("fits the full built-in workflow on a common window, without scrolling", () => {
    const setup: WorkflowStep = { type: "setup", id: "setup", run: "x" };
    const reviews = group("reviews", "security", "ux", "ui", "code");
    const full = [setup, agent("spec"), agent("builder"), { type: "check", id: "check", run: "x" } as WorkflowStep, reviews, approver("approver", "builder"), { type: "approval", id: "human" } as WorkflowStep, { type: "deliver", id: "pr" } as WorkflowStep, agent("learn", "sonnet", { kind: "learner" })];
    // A 1440 wide window, less the page's and the card's padding.
    expect(layoutFlow(flowOf(full)).width).toBeLessThanOrEqual(1440 - 2 * 20 - 2 * 12 - 2);
  });

  describe("in the editor", () => {
    const editing = layoutFlow(flowOf(FULL.slice(0, 5)), true);

    it("has a + at both ends and in every gap", () => {
      expect(editing.gaps.map((gap) => gap.index)).toEqual([0, 1, 2, 3, 4, 5]);
    });

    it("keeps each + out of the nodes", () => {
      for (const gap of editing.gaps) {
        for (const n of editing.nodes) expect(gap.x > n.x && gap.x < n.x + n.w && gap.y > n.y && gap.y < n.y + n.h).toBe(false);
      }
    });

    it("has no + after the learner, which has to be last", () => {
      const layout = layoutFlow(flowOf(FULL), true);
      expect(layout.gaps.map((gap) => gap.index)).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
    });

    it("has one + and nothing else with no steps", () => {
      const layout = layoutFlow(flowOf([]), true);
      expect(layout.gaps.map((gap) => gap.index)).toEqual([0]);
      expect(layout.nodes).toEqual([]);
      expect(layout.lines).toEqual([]);
      expect(layout.height).toBeGreaterThan(0);
    });

    it("has a + under each group", () => {
      expect(editing.memberAdds.map((add) => add.index)).toEqual([3]);
      const lowest = Math.max(...editing.nodes.map((n) => n.y + n.h));
      expect(editing.memberAdds[0].y).toBeGreaterThan(lowest);
      expect(editing.memberAdds[0].y).toBeLessThan(editing.height);
    });

    it("keeps room for an empty group, so it can be selected and filled", () => {
      const layout = layoutFlow(flowOf([agent("a"), { type: "parallel", id: "g", steps: [] }, agent("b")]), true);
      expect(layout.groups[0].empty).not.toBeNull();
      expect(layout.items.map((item) => item.key)).toContain("1");
      expect(layout.lines.filter((line) => line.head)).toHaveLength(2);
    });
  });
});

describe("the arrow keys", () => {
  const items = layoutFlow(flowOf(FULL.slice(0, 5))).items;
  const go = (from: string, key: string, list = items) => nextItem(list, from, key);

  it("move along the stages, in the order they run", () => {
    expect(go("0", "ArrowRight")).toBe("1");
    expect(go("1", "ArrowLeft")).toBe("0");
  });

  it("stop at the ends", () => {
    expect(go("0", "ArrowLeft")).toBeNull();
    expect(go("4", "ArrowRight")).toBeNull();
  });

  it("go up and down within a group, and nowhere from a single step", () => {
    expect(go("3.0", "ArrowDown")).toBe("3.1");
    expect(go("3.1", "ArrowUp")).toBe("3.0");
    expect(go("3.0", "ArrowUp")).toBeNull();
    expect(go("3.1", "ArrowDown")).toBeNull();
    expect(go("1", "ArrowDown")).toBeNull();
  });

  it("leave a group from whichever member they're on, and enter at the nearest", () => {
    expect(go("3.1", "ArrowRight")).toBe("4");
    expect(go("3.0", "ArrowLeft")).toBe("2");
    expect(go("2", "ArrowRight")).toMatch(/^3\.[01]$/);
  });

  it("go home and to the end", () => {
    expect(go("2", "Home")).toBe("0");
    expect(go("2", "End")).toBe("4");
  });

  it("do nothing from what isn't there, or with other keys", () => {
    expect(go("nope", "ArrowRight")).toBeNull();
    expect(go("1", "Enter")).toBeNull();
  });

  it("reach the + buttons and a group's name in the editor", () => {
    const editing = layoutFlow(flowOf(FULL.slice(0, 5)), true).items;
    expect(go("gap0", "ArrowRight", editing)).toBe("0");
    expect(go("0", "ArrowRight", editing)).toBe("gap1");
    expect(go("gap3", "ArrowRight", editing)).toMatch(/^3/);
    expect(editing.filter((item) => item.key === "3" || item.key.startsWith("3.") || item.key === "add3").map((item) => item.key)).toEqual(["3", "3.0", "3.1", "add3"]);
    expect(go("3", "ArrowDown", editing)).toBe("3.0");
    expect(go("3.1", "ArrowDown", editing)).toBe("add3");
  });

  it("reach what's outside the workflow too", () => {
    const list = layoutFlow(flowOf([agent("a")], [attempt("stray", "failed")])).items;
    expect(go("0", "ArrowRight", list)).toBe("x0");
  });
});

describe("scrolling a node into view", () => {
  it("leaves it alone if it's already showing", () => {
    expect(scrollToShow(100, 148, 0, 800)).toBe(0);
    expect(scrollToShow(900, 148, 500, 800)).toBe(500);
  });

  it("centers it if it's off to the right", () => {
    expect(scrollToShow(1000, 148, 0, 800)).toBe(1000 + 74 - 400);
  });

  it("centers it if it's off to the left", () => {
    expect(scrollToShow(200, 148, 600, 800)).toBe(0);
    expect(scrollToShow(1000, 148, 1100, 400)).toBe(1000 + 74 - 200);
  });

  it("does the same if it's only partly showing", () => {
    expect(scrollToShow(700, 148, 0, 800)).toBe(700 + 74 - 400);
  });

  it("does nothing before there's a width to go by", () => {
    expect(scrollToShow(1000, 148, 30, 0)).toBe(30);
  });
});
