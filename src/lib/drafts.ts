import type { WorkflowStep } from "./api";

/** What saving a proposed workflow would let happen, whatever the prompts say. */
export interface DraftFacts {
  commands: { id: string; run: string }[];
  /** Agents that can change files and run commands. Groups can't have any. */
  builders: string[];
  delivers: boolean;
  /** Steps that act between the person's approval and the pull request, so the person never sees what they do. */
  afterApproval: string[];
  /** Agents that don't run for a change the engine finds small, in a group or not. */
  conditional: string[];
}

export function draftFacts(steps: WorkflowStep[]): DraftFacts {
  const commands = steps.flatMap((step) => (step.type === "check" || step.type === "setup" ? [{ id: step.id, run: step.run }] : []));
  const builders = steps.flatMap((step) => (step.type === "agent" && step.tools === "build" ? [step.id] : []));
  const deliver = steps.findIndex((step) => step.type === "deliver");
  const approval = deliver < 0 ? -1 : steps.slice(0, deliver).map((step) => step.type).lastIndexOf("approval");
  const between = approval < 0 ? [] : steps.slice(approval + 1, deliver);
  const afterApproval = between.flatMap((step) => ((step.type === "agent" && step.tools === "build") || step.type === "check" || step.type === "setup" ? [step.id] : []));
  const conditional = steps.flatMap((step) => (step.type === "parallel" ? step.steps : step.type === "agent" ? [step] : [])).flatMap((spec) => (spec.when ? [spec.id] : []));
  return { commands, builders, delivers: deliver >= 0, afterApproval, conditional };
}
