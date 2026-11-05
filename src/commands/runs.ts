import type { AttemptState, RunRow } from "../lib/api";
import { useApp } from "../lib/store";

/**
 * Runs that stay where they are until a person does something: the board's Human review, and what
 * MCP calls needing a person. Home's Blocked card leaves failed and stopped out, so it can show
 * fewer. Rejected and abandoned count on the board only if the run is still open, which is rare,
 * and a row doesn't say.
 */
const WAITING_FOR_YOU: AttemptState[] = ["waitingApproval", "blocked", "stalled", "failed", "stopped", "interrupted"];

export const isWaitingForYou = (state: AttemptState) => WAITING_FOR_YOU.includes(state);

/**
 * The next waiting run after the one showing, going round; `step` -1 goes the other way. It only
 * sees the runs the overview loads, the newest 200: one waiting further back can't be reached with
 * ⌘G, though Home's Blocked card still counts it.
 */
export function nextWaitingRun(runs: RunRow[], currentId: string | null, step: 1 | -1): RunRow | null {
  const waiting = runs.filter((run) => isWaitingForYou(run.state));
  const ordered = step > 0 ? waiting : [...waiting].reverse();
  const at = runs.findIndex((run) => run.id === currentId);
  const ahead = at < 0 ? undefined : ordered.find((run) => step * (runs.indexOf(run) - at) > 0);
  return ahead ?? ordered[0] ?? null;
}

/**
 * Opens a run. Back then goes to where the person was, not through every run they passed on the
 * way. The run page is keyed by run, so a waiting one mounts its approval card fresh: its wait
 * starts over and focus lands on the note.
 */
export function openRun(runId: string) {
  const { view, navigate } = useApp.getState();
  if (view.name === "run" && view.runId === runId) return;
  navigate({ name: "run", runId, backTo: view.name === "run" ? view.backTo : view });
}
