import type { AuditEvent } from "./api";

export interface CommandRun {
  id: string;
  command: string;
  description: string | null;
  status: "ok" | "failed" | "running";
  exitCode: number | null;
  output: string;
  truncated: boolean;
  at: string;
}

/** The commands a step ran, each with how it ended. A pending result means it's still going. */
export function commandsFrom(events: AuditEvent[]): CommandRun[] {
  const results = new Map(events.filter((e) => e.kind === "toolResult").map((e) => [e.data.id as string, e]));
  return events
    .filter((e) => e.kind === "toolCall" && e.data.name === "Bash")
    .map((call) => {
      const result = results.get(call.data.id);
      const failed = result?.data.isError === true;
      return {
        id: call.data.id,
        command: String(call.data.input?.command ?? ""),
        description: call.data.input?.description ?? null,
        status: !result ? "running" : failed ? "failed" : "ok",
        exitCode: result?.data.exitCode ?? null,
        output: String(result?.data.text ?? ""),
        truncated: result?.data.truncated === true,
        at: call.at,
      };
    });
}

export interface ActivityItem {
  key: string;
  at: string;
  kind: string;
  event: AuditEvent;
  /** For a tool call, what came back. */
  result?: AuditEvent;
}

/** Pairs each tool call with its result, so a call and its output read as one thing. */
export function activityFrom(events: AuditEvent[]): ActivityItem[] {
  const results = new Map(events.filter((e) => e.kind === "toolResult").map((e) => [e.data.id as string, e]));
  return events
    .filter((e) => e.kind !== "toolResult")
    .map((event) => ({
      key: String(event.seq),
      at: event.at,
      kind: event.kind,
      event,
      result: event.kind === "toolCall" ? results.get(event.data.id) : undefined,
    }));
}

/** The most useful line to show for a tool call. */
export function toolSummary(name: string, input: Record<string, any> | null | undefined): string {
  if (!input) return "";
  if (name === "Bash") return String(input.command ?? "");
  return String(input.file_path ?? input.path ?? input.pattern ?? JSON.stringify(input));
}

/** Everything the run's steps changed, without repeats. */
export function filesChangedBy(attempts: { filesChanged: string[] | null; runFiles?: string[] | null }[]): string[] {
  // A file added and later removed isn't part of what the run changed.
  const net = [...attempts].reverse().find((a) => a.runFiles)?.runFiles;
  return net ?? [...new Set(attempts.flatMap((a) => a.filesChanged ?? []))];
}

/** Files that can run with the repo's secrets as soon as the branch is pushed. */
export const touchesWorkflows = (files: string[]) => files.some((f) => f.startsWith(".github/workflows/"));

/**
 * The attempts whose final messages explain the run: every one that could change files, since after
 * rework there's one per round, and else the last worker.
 */
export function explainingAttempts<T extends { kind: string; tools: string; role: string; state: string; resultText: string | null }>(attempts: T[]): T[] {
  const agents = attempts.filter((a) => a.kind === "agent" && a.role === "worker" && a.resultText);
  const builders = agents.filter((a) => a.tools === "build");
  const finished = builders.filter((a) => a.state === "succeeded");
  if (finished.length > 0) return finished;
  if (builders.length > 0) return builders.slice(-1);
  return agents.slice(-1);
}

const flat = (text: string) =>
  text
    .replace(/^\s*(?:[-*•]|\d+\.)\s+/gm, "")
    .replace(/\*\*/g, "")
    .replace(/\s+/g, " ")
    .trim();

/** A sentence or two from the start of what an agent wrote, for where its whole message doesn't fit. */
export function gist(text: string, limit = 240): { text: string; cut: boolean } {
  // A heading names a section. It doesn't say what happened.
  const body = text.replace(/^[ \t]*(?:#{1,6}\s.*|\*\*[^*\n]+\*\*:?)[ \t]*$/gm, "").trim() || text.trim();
  const [first = "", second = ""] = body.split(/\n\s*\n/);
  // "Done." or "Here's what I did:" say nothing without what follows them.
  const opensOnly = first.trimEnd().endsWith(":") || flat(first).length < 40;
  const lead = flat(opensOnly ? `${first}\n${second}` : first);
  const sentences = lead.match(/.+?(?:[.!?…]+(?=\s|$)|$)/g) ?? [];
  let short = sentences.slice(0, 2).join("").trim();
  if (short.length > limit) short = `${short.slice(0, limit).replace(/\s+\S*$/, "")}…`;
  return { text: short, cut: short !== flat(body) };
}

/** What a round's reviewers said, and what the approver made of it. */
export function reviewsOfRound<T extends { role: string; round: number; output: unknown }>(attempts: T[], round: number): { reviewers: T[]; approver: T | undefined } {
  const current = attempts.filter((a) => a.round === round);
  return {
    reviewers: current.filter((a) => a.role === "reviewer"),
    approver: [...current].reverse().find((a) => a.role === "approver" && a.output),
  };
}

export type ApprovalOutcome = "delivers" | "deliversLater" | "noDelivery" | "nothingToDeliver";

/**
 * What approving leads to. It isn't always a pull request: a workflow can ask before it builds, in
 * which case no files yet is normal, or never deliver at all. With no files and delivery next,
 * approving can only fail.
 */
export function approvalOutcome(steps: { type: string; id: string; role?: { kind: string } }[], approvalId: string, files: string[]): ApprovalOutcome {
  // The step that proposes notes runs after everything else, and pushes nothing.
  const rest = steps.slice(steps.findIndex((s) => s.id === approvalId) + 1).filter((s) => s.role?.kind !== "learner");
  if (!rest.some((s) => s.type === "deliver")) return "noDelivery";
  if (rest[0].type !== "deliver") return "deliversLater";
  return files.length === 0 ? "nothingToDeliver" : "delivers";
}

/** Which accounts did this, on the record. */
export function accountsOf(attempts: { kind: string; account: string | null }[]): { claude: string | null; github: string | null } {
  return {
    claude: attempts.find((a) => a.kind === "agent" && a.account)?.account ?? null,
    github: attempts.find((a) => a.kind === "deliver" && a.account)?.account ?? null,
  };
}

/** The latest try of each step, in the order they first ran. */
export function latestOfEachStep<T extends { stepId: string }>(attempts: T[]): T[] {
  const latest = new Map<string, T>();
  for (const attempt of attempts) latest.set(attempt.stepId, attempt);
  return [...latest.values()];
}

const NEEDS_A_PERSON = ["waitingApproval", "stalled", "blocked", "interrupted", "failed", "stopped", "rejected"];
const WORKING = ["running", "preparing", "queued"];

/**
 * Still moving on its own: nothing ended it and nothing holds it until a person answers.
 * Between two steps nothing is working yet, and a stalled step can wake up by itself: neither should rearrange a page.
 */
export function stillGoing(run: { finishedAt: string | null }, attempts: { stepId: string; state: string; role?: string }[]): boolean {
  if (run.finishedAt) return false;
  return !latestOfEachStep(attempts).some((a) => a.state !== "stalled" && a.role !== "learner" && NEEDS_A_PERSON.includes(a.state));
}

/** The step the run is about right now: whatever needs a person, then whatever is working. */
export function focusAttempt<T extends { stepId: string; state: string; role?: string }>(attempts: T[]): T | undefined {
  const current = latestOfEachStep(attempts);
  const asks = (a: T) => NEEDS_A_PERSON.includes(a.state) && a.role !== "learner";
  const works = (a: T) => WORKING.includes(a.state);
  for (const applies of [asks, works]) {
    const found = [...current].reverse().find(applies);
    if (found) return found;
  }
  // Proposals are a bonus: if they failed, the run is still about what came before them.
  return [...attempts].reverse().find((a) => a.role !== "learner") ?? attempts[attempts.length - 1];
}
