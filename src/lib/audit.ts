import type { TFunction } from "i18next";
import type { Attempt, AuditEvent, CheckedCriterion, GivenAnswer, ShownState, SpecOutput, VerificationOutput } from "./api";

export interface CommandEnd {
  /** `unclear` is a command that finished with code 0, but where that 0 may hide a failure. */
  status: "ok" | "failed" | "running" | "unclear";
  exitCode: number | null;
  /** It finished through a pipe, so its exit code is only the last command's. */
  piped: boolean;
  /** A word in its output that usually means something failed, when the exit code doesn't say so. */
  failureMark: string | null;
}

export interface CommandRun extends CommandEnd {
  id: string;
  command: string;
  description: string | null;
  output: string;
  truncated: boolean;
  at: string;
}

/**
 * Whether a shell command sends one command's output into another, which leaves it with the exit
 * code of the last one. `||` isn't a pipe, nor is a `|` in quotes, and `pipefail` keeps the failure.
 */
export function pipeHidesExitCode(command: string): boolean {
  if (/-[a-zA-Z]*o\s+pipefail\b/.test(command)) return false;
  let quote: string | null = null;
  for (let i = 0; i < command.length; i++) {
    const c = command[i];
    if (c === "\\" && quote !== "'") i++;
    else if (quote) quote = c === quote ? null : quote;
    else if (c === "'" || c === '"') quote = c;
    else if (c === "|" && command[i + 1] === "|") i++;
    else if (c === "|") return true;
  }
  return false;
}

/** The first word in a command's output that usually means something failed, such as a test or a build. */
export const failureMarkIn = (output: string): string | null => output.match(/\b(?:FAILED\b|panicked\b|error(?=[:[]))/)?.[0] ?? null;

/**
 * How a command ended. Its exit code alone can say "it went well" over tests that failed, so the
 * command and what it printed have a say too.
 */
export function commandEnd(command: string, result: AuditEvent | undefined): CommandEnd {
  if (!result) return { status: "running", exitCode: null, piped: false, failureMark: null };
  const exitCode = result.data.exitCode ?? null;
  const piped = pipeHidesExitCode(command);
  if (result.data.isError === true) return { status: "failed", exitCode, piped, failureMark: null };
  const failureMark = failureMarkIn(String(result.data.text ?? ""));
  return { status: piped || failureMark ? "unclear" : "ok", exitCode, piped, failureMark };
}

/** The commands a step ran, each with how it ended. A pending result means it's still going. */
export function commandsFrom(events: AuditEvent[]): CommandRun[] {
  const results = new Map(events.filter((e) => e.kind === "toolResult").map((e) => [e.data.id as string, e]));
  return events
    .filter((e) => e.kind === "toolCall" && e.data.name === "Bash")
    .map((call) => {
      const result = results.get(call.data.id);
      const command = String(call.data.input?.command ?? "");
      return {
        id: call.data.id,
        command,
        description: call.data.input?.description ?? null,
        ...commandEnd(command, result),
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
 * Whether any of the run's steps had localhost open, from what each was given as it started: a check,
 * or a builder's commands. Not the project's setting now, which may have changed since.
 */
export function localhostOpenIn(attempts: { settings: unknown }[]): boolean {
  return attempts.some((attempt) => {
    const settings = attempt.settings as { localhost?: unknown; sandbox?: { network?: { allowLocalBinding?: unknown } } } | null | undefined;
    return settings?.localhost === true || settings?.sandbox?.network?.allowLocalBinding === true;
  });
}

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

/**
 * The attempt that says what was asked: the latest that worked of a step with the specifier's role,
 * or else of a step called `spec`, which is how workflows saved before that role say it.
 */
export function specifyingAttempt<T extends { kind: string; role: string; stepId: string; state: string }>(attempts: T[]): T | undefined {
  const latest = (applies: (attempt: T) => boolean) => [...attempts].reverse().find((a) => a.kind === "agent" && a.state === "succeeded" && applies(a));
  return latest((a) => a.role === "specifier") ?? latest((a) => a.stepId === "spec");
}

/** A specifier's structured answer, or null for anything else, including one Keel can't read. */
export function specOf(attempt: { role?: string; output?: unknown }): SpecOutput | null {
  const output = attempt.output as Partial<SpecOutput> | null | undefined;
  if (attempt.role !== "specifier" || typeof output?.summary !== "string") return null;
  return { criteria: [], changes: [], limits: [], decided: [], questions: [], ...output, summary: output.summary };
}

/** What that attempt says, in words a person can read: the sentence that sums it up, or else what it wrote. */
export const askedText = (attempt: { resultText: string | null; role?: string; output?: unknown }): string | null => specOf(attempt)?.summary.trim() || attempt.resultText?.trim() || null;

/** What a person has answered in this run, oldest first. */
// As in the engine, only one that succeeded: any other holds what the agent wrote, which isn't an answer.
export const answersIn = (attempts: { role?: string; state?: string; output?: unknown }[]): GivenAnswer[] =>
  attempts.filter((a) => a.state === undefined || a.state === "succeeded").flatMap((a) => specOf(a)?.answered ?? []);

/** How many rounds of questions a person has already answered at this step, before the one it's on. */
export const roundsAnswered = (attempts: { stepId: string; round: number; role?: string; output?: unknown }[], attempt: { stepId: string; round: number }) => attempts.filter((a) => a.stepId === attempt.stepId && a.round === attempt.round && (specOf(a)?.answered?.length ?? 0) > 0).length;

/**
 * A verifier's answer, or null for anything else. Only one that succeeded has been corrected by the
 * engine: an attempt that was stopped can still hold what the agent first said.
 */
export function verificationOf(attempt: { role?: string; state?: string; output?: unknown }): VerificationOutput | null {
  const output = attempt.output as Partial<VerificationOutput> | null | undefined;
  if (attempt.role !== "verifier" || attempt.state !== "succeeded" || !Array.isArray(output?.criteria)) return null;
  return { summary: "", ...output, criteria: output.criteria };
}

/**
 * The step that changed the code after this attempt looked at it, in the same round, like
 * `changed_since` in the engine: what it said was about the code before that.
 */
export function changedSince(attempts: Pick<Attempt, "id" | "stepId" | "kind" | "tools" | "round" | "shaBefore" | "shaAfter">[], looked: { id: string; round: number; shaBefore: string | null; shaAfter: string | null }): string | null {
  const seen = looked.shaAfter ?? looked.shaBefore;
  const at = attempts.findIndex((a) => a.id === looked.id);
  if (at < 0 || !seen) return null;
  // However it ended: one that failed after fixing something, and only had to finish when tried again, changed it all the same.
  const changed = attempts.slice(at + 1).find((a) => a.round === looked.round && a.kind === "agent" && a.tools === "build" && a.shaAfter && a.shaAfter !== seen);
  return changed?.stepId ?? null;
}

/**
 * The checks that say how the code ends. One that only repeats another and was left out says
 * nothing of its own, and one that ran after the same command in another step stands for it.
 */
export function standingChecks<T extends { kind: string; stepId: string; state: string; prompt: string }>(attempts: T[]): T[] {
  const ran = latestOfEachStep(attempts.filter((a) => a.kind === "check")).filter((a) => a.state !== "skipped");
  const last = (check: T) => attempts.lastIndexOf(check);
  return ran.filter((check) => !ran.some((other) => other.stepId !== check.stepId && other.prompt.trim() === check.prompt.trim() && last(other) > last(check)));
}

/** What a verifier said in this round. What it said in an earlier one is about code that has changed since. */
export function verificationInRound(attempts: { round: number; role?: string; state?: string; output?: unknown }[], round: number): VerificationOutput | null {
  const found = [...attempts].reverse().find((a) => a.round === round && verificationOf(a));
  return found ? verificationOf(found) : null;
}

export function verificationCounts(verification: VerificationOutput) {
  const count = (result: string) => verification.criteria.filter((c) => c.result === result).length;
  return { met: count("met"), unmet: count("unmet"), unverified: count("unverified"), total: verification.criteria.length };
}

/** Why nothing proves one thing that had to be true, in the engine's words: the MCP's `why`. */
export type WhyNotProven = "notRun" | "noTest" | "noCheckPassed" | "noEvidence" | "leftOut" | "seeEvidence";

/**
 * Why nothing proves it: what Keel corrected, or else what the verifier said. Null when it was met or
 * found not met, which isn't unproven.
 */
export function whyNotProven(item: CheckedCriterion): WhyNotProven | null {
  if (item.result !== "unverified") return null;
  if (item.corrected) return item.corrected;
  if (item.why === "notRun" || item.why === "noTest") return item.why;
  // Without why apart: pointing at nothing, it found nothing that proves it. What it pointed at may say
  // why, as the prompt from before asked, so Keel claims no reason of its own.
  return item.evidence ? "seeEvidence" : "noTest";
}

/** What the reviews couldn't check by reading the code comes first, as in what proves it. */
const NOT_PROVEN_ORDER = ["byReviews", "notRun", "noTest", "noCheckPassed", "noEvidence", "leftOut", "seeEvidence"] as const;
export type NotProvenGroup = { why: (typeof NOT_PROVEN_ORDER)[number]; count: number };

/** How many things weren't proven in a round, by why. Nothing when everything was. */
export function notProvenInRound(attempts: { stepId: string; role: string; state?: string; round: number; output: unknown }[], round: number): NotProvenGroup[] {
  const whys: NotProvenGroup["why"][] = [...uncheckedInRound(attempts, round).map(() => "byReviews" as const), ...(verificationInRound(attempts, round)?.criteria.map(whyNotProven) ?? []).filter((why) => why !== null)];
  return NOT_PROVEN_ORDER.map((why) => ({ why, count: whys.filter((each) => each === why).length })).filter((group) => group.count > 0);
}

/** "3 of 4 met, 1 not verified", or that none of them could be, which is a normal thing to find. */
export function verificationLine(t: TFunction, verification: VerificationOutput): string {
  const { met, unmet, unverified, total } = verificationCounts(verification);
  if (met === 0 && unmet === 0) return t("run.verifyNone", { count: total });
  return [t("run.verifyMet", { count: met, total }), unmet > 0 && t("run.verifyUnmet", { count: unmet }), unverified > 0 && t("run.verifyUnverified", { count: unverified })].filter(Boolean).join(", ");
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

/**
 * What an assistant asked for when it sent the work back into this round. Nothing when a person did,
 * or when something else started this round since: they know why.
 */
export function sentBackByAssistant(attempts: { state: string; round: number; output: unknown }[], round: number): { by: string; instructions: string } | null {
  // Each return moves the run to the next round, so only one can have started this one.
  const returned = attempts.find((a) => a.state === "returned" && a.round === round - 1);
  const output = returned?.output as { by?: unknown; instructions?: unknown } | null | undefined;
  if (typeof output?.by !== "string" || !output.by) return null;
  return { by: output.by, instructions: typeof output.instructions === "string" ? output.instructions : "" };
}

/**
 * Whether this round began with a check failing and Keel sending the work back to the builder by
 * itself. When a person or an assistant sent it back from one, their instructions are with it.
 */
export function checkSentBackInto(attempts: { kind: string; state: string; round: number; output: unknown }[], round: number): boolean {
  return attempts.some((a) => a.kind === "check" && a.state === "returned" && a.round === round - 1 && typeof (a.output as { instructions?: unknown } | null | undefined)?.instructions !== "string");
}

/**
 * The assistant that closed a finished run, if one did. It may have closed another step than the one
 * the page is about, such as the first of two reviews that failed. Of the steps as they stand, the
 * last one closed is what ended the run: one tried again since no longer says anything.
 */
export function closedByAssistant(run: { finishedAt: string | null }, attempts: { id: string; stepId: string; state: string }[], events: Record<string, AuditEvent[]>): string | null {
  if (!run.finishedAt) return null;
  let last: AuditEvent | undefined;
  for (const attempt of latestOfEachStep(attempts)) {
    if (attempt.state !== "abandoned") continue;
    for (const event of events[attempt.id] ?? []) {
      if (event.kind === "abandoned" && (!last || event.seq > last.seq)) last = event;
    }
  }
  const by: unknown = last?.data.by;
  return typeof by === "string" && by ? by : null;
}

/** What a round's reviewers said, and what the approver made of it. */
export function reviewsOfRound<T extends { role: string; round: number; output: unknown }>(attempts: T[], round: number): { reviewers: T[]; approver: T | undefined } {
  const current = attempts.filter((a) => a.round === round);
  return {
    reviewers: current.filter((a) => a.role === "reviewer"),
    approver: [...current].reverse().find((a) => a.role === "approver" && a.output),
  };
}

/**
 * What a round's reviews said they couldn't check by reading the code, such as how it looks on screen,
 * with the review that said it. No test proves it either, whatever their verdict.
 */
export function uncheckedInRound(attempts: { stepId: string; role: string; round: number; output: unknown }[], round: number): { step: string; text: string }[] {
  return latestOfEachStep(reviewsOfRound(attempts, round).reviewers).flatMap((review) => uncheckedOf(review.output).map((text) => ({ step: review.stepId, text })));
}

/** What one review said it couldn't check by reading the code. Nothing for an answer from before Keel asked, or one it can't read. */
export function uncheckedOf(output: unknown): string[] {
  const unchecked = (output as { unchecked?: unknown } | null)?.unchecked;
  if (!Array.isArray(unchecked)) return [];
  return unchecked.filter((text): text is string => typeof text === "string" && text.trim() !== "").map((text) => text.trim());
}

/** A finding as the app shows it: where, what, and the criterion or limit it breaks. One from an older answer is only text. */
export interface FindingShown {
  where: string | null;
  text: string;
  breaks: string | null;
}

/** The id of a specification's limit at `index`, from `L1`, in the order the person sees them. Limits have no id of their own, and a review names the one a change breaks by this one, as it does a criterion. */
export function limitId(index: number): string {
  return `L${index + 1}`;
}

/** What a finding can say the change breaks: each criterion of the specification, and each of its limits from `L1`. */
export function breakableIds(spec: SpecOutput | null): string[] {
  if (!spec) return [];
  return [...spec.criteria.map((criterion) => criterion.id), ...spec.limits.map((_, index) => limitId(index))];
}

/** What one review found, old text included. Anything else is left out. As for Keel, only what's in `breakable` breaks anything. */
export function findingsOf(output: unknown, breakable: string[]): FindingShown[] {
  const findings = (output as { findings?: unknown } | null)?.findings;
  if (!Array.isArray(findings)) return [];
  const filled = (value: unknown) => (typeof value === "string" && value.trim() !== "" ? value.trim() : null);
  return findings.flatMap((finding): FindingShown[] => {
    if (typeof finding === "string") return [{ where: null, text: finding, breaks: null }];
    if (!finding || typeof finding !== "object") return [];
    const { file, line, text, breaks } = finding as Record<string, unknown>;
    const at = filled(file);
    const id = filled(breaks);
    // A line is a whole number from 0, or none, as Keel reads it.
    const lineKept = typeof line === "number" && Number.isInteger(line) && line >= 0;
    return [{ where: at && lineKept ? `${at}:${line}` : at, text: typeof text === "string" ? text : "", breaks: id && breakable.includes(id) ? id : null }];
  });
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

const NEEDS_A_PERSON = ["waitingApproval", "waitingAnswers", "stalled", "blocked", "interrupted", "failed", "stopped", "rejected"];
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

/**
 * What an attempt's state reads as. An answer that didn't fit within the limits is recorded as
 * failed, but Keel went straight on with the same session to have it fixed: for the person that's
 * not a failure. Until that next attempt exists, nothing is fixing it, and it did fail.
 */
export function shownState(attempt: Pick<Attempt, "id" | "state" | "reason"> & { kind?: Attempt["kind"] }, attempts: Pick<Attempt, "resumedFrom">[]): ShownState {
  const fixed = attempt.state === "failed" && attempt.reason?.code === "specNotUsable" && attempts.some((a) => a.resumedFrom === attempt.id);
  if (fixed) return "corrected";
  return putOffForSetup(attempt) ? "afterSetup" : attempt.state;
}

/**
 * Whether this stands for a step a retry with the project's new commands put off for a new setup to
 * run first, as `sizing::put_off_for_setup` says. It never ran, and the step comes round again once
 * the setup is through. The only other thing put off is an approval, by a person who asked for more.
 */
export const putOffForSetup = (attempt: { state: string; kind?: string }) => attempt.state === "postponed" && attempt.kind !== undefined && attempt.kind !== "approval";

/**
 * Whether an agent that can change files has had the run's copy, as `runner::before_any_agent_could_write`
 * says: from then on, a setup gets no more than the agents do. One that only reads has no Bash, Edit or
 * Write, and a skipped one never had it. Tools Keel doesn't know may write.
 */
export const agentCouldHaveWritten = (attempts: Pick<Attempt, "kind" | "state" | "tools">[]) => attempts.some((attempt) => attempt.kind === "agent" && attempt.state !== "skipped" && attempt.tools !== "readOnly");
