import type { Workflow, WorkflowResults } from "./api";

/** Fewer than this, and an average says more about those few runs than about the workflow. */
export const ENOUGH_FOR_AN_AVERAGE = 3;

/** Null until there are enough merged pull requests to go on. */
export const costPerMerged = (results: WorkflowResults) => (results.merged >= ENOUGH_FOR_AN_AVERAGE ? results.costPerMergedUsd : null);

/** Null until there are enough finished runs to go on. */
export const averageRounds = (results: WorkflowResults) => (results.finished >= ENOUGH_FOR_AN_AVERAGE ? results.averageRounds : null);

/** The share of answers that took the recommended option, from 0 to 1. Null until there are enough to go on. */
export const matchedShare = (results: WorkflowResults) => (results.answers >= ENOUGH_FOR_AN_AVERAGE ? results.answersRecommended / results.answers : null);

/**
 * The workflows to compare, in the order of the list beside them and under the name they have now.
 * One that's been deleted comes last, under the name its latest run knew.
 */
export function comparedResults(results: WorkflowResults[], workflows: Pick<Workflow, "id" | "name">[]): WorkflowResults[] {
  const position = (id: string) => {
    const index = workflows.findIndex((w) => w.id === id);
    return index < 0 ? workflows.length : index;
  };
  return [...results]
    .sort((a, b) => position(a.workflowId) - position(b.workflowId))
    .map((r) => ({ ...r, workflowName: workflows.find((w) => w.id === r.workflowId)?.name ?? r.workflowName }));
}
