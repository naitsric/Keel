import { AlertTriangle, Check, CircleQuestionMark, ExternalLink, FileDiff, GitPullRequest, Layers, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { keymapHeld, watchKeymapHold } from "../commands/keymapHold";
import { useRunCommands } from "../commands/pageCommands";
import { api, type BrainNote, type Attempt, type AttemptState, type AuditEvent, type CheckedCriterion, isActive, type PrState, type QuestionAnswer, type Questioned, type RawCheck, type Reason, type Run, type RunJoint, type Screenshot, type Screenshots, type SpecDecided, type SpecQuestion, type VerificationOutput } from "../lib/api";
import { accountsOf, activityFrom, type ActivityItem, approvalOutcome, askedText, breakableIds, closedByAssistant, type CommandEnd, commandEnd, commandsFrom, type CommandRun, explainingAttempts, filesChangedBy, findingsOf, focusAttempt, gist, latestOfEachStep, reviewsOfRound, roundsAnswered, sentBackByAssistant, shownState, specifyingAttempt, specOf, toolSummary, touchesWorkflows, stillGoing, uncheckedInRound, uncheckedOf, verificationCounts, verificationInRound, verificationLine, verificationOf } from "../lib/audit";
import { partText, partWarningText } from "../lib/board";
import { attemptChoices, canSendBack, type FlowNode, flowOf, isReadOnlyTask, nodeOfAttempt, nodeWithKey, parseSteps } from "../lib/flow";
import { formatCost, formatDateTime, formatDuration, formatNumber } from "../lib/format";
import { errorText, needsProjectSettings, needsSignIn, reasonText, specProblemText } from "../lib/reason";
import { allRecommend, answerFrom, differsFromRecommended, isAnswered, needsText, type QuestionPick, recommendedAnswers, recommendedOption, startingPick } from "../lib/questions";
import { latestSizeDecision, type SizeDecision, skippedAsSmall, skippedSteps, whyNotSmallText } from "../lib/size";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass, Modal, Segmented, StateBadge } from "../ui/primitives";
import { ProposalRow } from "./BrainPage";
import { FlowDiagram } from "./FlowDiagram";
import { JointChangeDialog } from "./JointChangeDialog";
import { SpecView } from "./SpecView";

type Tab = "understood" | "found" | "activity" | "commands" | "changes" | "prompt" | "details" | "output";

const DECISIONS: Record<AttemptState, string> = {
  queued: "run.decideActive",
  preparing: "run.decideActive",
  running: "run.decideActive",
  stalled: "run.decideStalled",
  succeeded: "run.decideSucceeded",
  failed: "run.decideFailed",
  stopped: "run.decideStopped",
  interrupted: "run.decideInterrupted",
  blocked: "run.decideBlocked",
  waitingApproval: "run.decideApproval",
  waitingAnswers: "run.decideAnswers",
  rejected: "run.decideRejected",
  returned: "run.decideActive",
  postponed: "run.decideActive",
  skipped: "run.decideActive",
  abandoned: "run.decideAbandoned",
};

/** Once there's a pull request, what's left to decide depends on what became of it. */
const PR_DECISIONS: Record<PrState, string> = {
  draft: "run.decideDelivered",
  open: "run.decideOpen",
  merged: "run.decideMerged",
  closed: "run.decideClosed",
};

const PR_TONES: Record<PrState, string> = {
  draft: "text-subtle",
  open: "text-subtle",
  merged: "font-medium text-success",
  closed: "font-medium text-warning",
};

/** What became of the pull request, in the line under the run's title. */
function PrStatus({ state, at }: { state: PrState; at: string | null }) {
  const { t } = useTranslation();
  const dated = at !== null && (state === "merged" || state === "closed");
  return <span className={PR_TONES[state]}>{dated ? t(`run.pr.${state}At`, { when: formatDateTime(at) }) : t(`run.pr.${state}`)}</span>;
}

function SummaryBlock({ title, note, children }: { title: string; note?: string; children: React.ReactNode }) {
  return (
    <div>
      <h3 className="flex items-baseline gap-2 text-[12px] font-semibold text-muted">
        {title}
        {note && <span className="font-normal text-subtle">{note}</span>}
      </h3>
      <div className="mt-1.5 text-[13px] leading-relaxed">{children}</div>
    </div>
  );
}

/** What the agents that explain the run wrote, in full, one round after another. */
function Explanations({ attempts }: { attempts: Attempt[] }) {
  const { t } = useTranslation();
  return attempts.map((attempt) => (
    <div key={attempt.id} className="mb-3 last:mb-0">
      {attempt.round > 0 && <p className="mb-1 text-[12px] font-medium text-warning">{t("run.round", { number: attempt.round + 1 })}</p>}
      <p className="selectable whitespace-pre-wrap">{attempt.resultText}</p>
    </div>
  ));
}

const auditKey = (rawCheck: RawCheck) => `run.audit${rawCheck[0].toUpperCase()}${rawCheck.slice(1)}`;

/** How many tests a check that passed skipped, as its test tools said. One that failed proves nothing anyway, and one whose tools Keel didn't recognise has nothing to say. */
const skippedTests = (check: Attempt) => (check.state === "succeeded" ? (check.skippedTests ?? 0) : 0);

/** The rule of the product: never just "finished". What it did, changed, proved, and what's next. */
export function Summary({ run, attempts, current, commands, rawCheck, closedBy, onShowChanges }: { run: Run | null; attempts: Attempt[]; current: Attempt; commands: CommandRun[]; rawCheck: RawCheck; closedBy: string | null; onShowChanges: () => void }) {
  const { t } = useTranslation();
  const files = filesChangedBy(attempts);
  const explaining = explainingAttempts(attempts);
  const navigate = useApp((s) => s.navigate);
  const mapper = attempts.find((a) => a.role === "mapper" && a.output);
  const passed = commands.filter((c) => c.status === "ok").length;
  const checks = attempts.filter((a) => a.kind === "check");
  const latestChecks = checks.filter((a, i) => !checks.slice(i + 1).some((b) => b.stepId === a.stepId));
  const checksPassed = latestChecks.filter((a) => a.state === "succeeded").length;
  const checksSkipped = latestChecks.reduce((sum, check) => sum + skippedTests(check), 0);
  const anyActive = attempts.some((a) => isActive(a.state));
  // One answer for the title and the separation, so they never disagree. Without a run, the steps decide.
  const going = run ? stillGoing(run, attempts) : anyActive;
  // Of the steps working, the one that started last is the one to name.
  const working = attempts.filter((a) => isActive(a.state)).at(-1);
  const verification = verificationInRound(attempts, run?.round ?? 0);
  // Still not proven once the run is approved: a person approving it didn't make it so.
  const unchecked = uncheckedInRound(attempts, run?.round ?? 0);
  const skipped = skippedSteps(attempts);
  // A step that's done says nothing about the run while others are still to come.
  const between = !run?.finishedAt && (current.state === "succeeded" || current.state === "skipped");
  // A run that only had to work out what's asked has that to say for itself, and no worker to say it.
  const onlyUnderstood = !mapper && explaining.length === 0 && current.state === "succeeded" ? specOf(current) : null;
  const decision = mapper && current.state === "succeeded" ? "run.decideMap" : run?.prUrl ? PR_DECISIONS[run.prState ?? "draft"] : current.reason?.code === "reworkLimit" ? "run.decideReworkLimit" : between ? "run.decideActive" : DECISIONS[current.state];
  return (
    <Card className={cx("grid gap-5 p-4 md:grid-cols-2", going && "mt-6 border-l-2 border-l-accent")}>
      <h2 className="text-[13px] font-semibold md:col-span-2">{t(going ? "run.summaryTitle" : "run.summaryTitleDone")}</h2>
      <SummaryBlock title={t("run.whatItDid")} note={explaining.length > 0 ? t("run.generated") : undefined}>
        {mapper && (
          <>
            <p>{t("run.mapWritten", { count: mapper.output?.lines?.length ?? 0 })}</p>
            <Button variant="ghost" className="mt-1 -ml-2" onClick={() => navigate({ name: "brain" })}>
              {t("run.openBrain")}
            </Button>
          </>
        )}
        {onlyUnderstood && <p className="selectable whitespace-pre-wrap">{onlyUnderstood.summary}</p>}
        {!mapper && !onlyUnderstood && explaining.length === 0 && <p className="selectable whitespace-pre-wrap">{(anyActive ? t("run.noResultYet", { step: working?.stepId }) : current.state === "succeeded" ? t("run.noResult") : t("run.noResultEnded"))}</p>}
        <Explanations attempts={explaining} />
      </SummaryBlock>
      <div className="space-y-4">
        <SummaryBlock title={t("run.whatChanged")}>
          {files.length === 0 ? (
            <p className="text-muted">{t("run.noChanges")}</p>
          ) : (
            <>
              <ul className="selectable space-y-0.5 font-mono text-[12px]">
                {files.map((file) => (
                  <li key={file}>{file}</li>
                ))}
              </ul>
              <Button variant="ghost" className="mt-1 -ml-2" onClick={onShowChanges}>
                <FileDiff size={14} />
                {t("run.viewDiff")}
              </Button>
            </>
          )}
        </SummaryBlock>
        <SummaryBlock title={t("run.whatProves")}>
          {latestChecks.length > 0 && (
            <p className={cx(checksPassed === latestChecks.length ? "text-muted" : "font-medium text-danger")}>{t("run.checksPassed", { passed: checksPassed, total: latestChecks.length })}</p>
          )}
          {checksSkipped > 0 && <p className="font-medium text-warning">{t("run.testsSkipped", { count: checksSkipped })}</p>}
          <NotProvenByReviews unchecked={unchecked} />
          {verification && <VerificationProof verification={verification} allProven={unchecked.length === 0} />}
          {skipped.length > 0 && <p className="text-muted">{t("fits.summary", { names: skipped.map((step) => step.stepId).join(", ") })}</p>}
          <p className="text-muted">{commands.length === 0 ? t("run.noRunCommands") : t("run.runCommandsPassed", { passed, total: commands.length })}</p>
          <p className={cx("mt-0.5", rawCheck === "changed" || rawCheck === "missing" ? "font-medium text-danger" : "text-muted")}>
            {t(auditKey(rawCheck))}
          </p>
        </SummaryBlock>
        <SummaryBlock title={t("run.whatToDecide")}>
          <p>{closedBy ? t("run.decideAbandonedBy", { client: closedBy }) : t(decision)}</p>
        </SummaryBlock>
      </div>
    </Card>
  );
}

function ApprovalRow({ label, note, action, rowRef, children }: { label: string; note?: string; action?: React.ReactNode; rowRef?: React.Ref<HTMLDivElement>; children: React.ReactNode }) {
  return (
    <div ref={rowRef} className="grid grid-cols-[116px_1fr] gap-x-3 border-t border-border py-2 first:border-t-0 first:pt-0">
      <dt className="text-[12px] leading-5 font-semibold text-muted">
        {label}
        {note && <span className="block text-[11px] leading-4 font-normal text-subtle">{note}</span>}
      </dt>
      <dd className="flex min-w-0 items-start gap-3 text-[12.5px] leading-5">
        <div className="min-w-0 flex-1">{children}</div>
        {action && <div className="-my-1 shrink-0">{action}</div>}
      </dd>
    </div>
  );
}

const VERIFY_TONES: Record<CheckedCriterion["result"], string> = {
  met: "text-success",
  unmet: "font-medium text-warning",
  unverified: "text-muted",
};

/** Each thing that had to be true, in its own words, with what became of it and why. */
function VerificationItems({ verification }: { verification: VerificationOutput }) {
  const { t } = useTranslation();
  return (
    <ul className="space-y-1.5">
      {verification.criteria.map((item) => {
        const why = item.corrected ? `run.verifyWhy.${item.corrected}` : item.result === "unverified" && !item.evidence ? "run.verifyWhy.noProof" : null;
        return (
          <li key={item.id} className="min-w-0">
            <p className="selectable break-words">
              <span className={cx("mr-1.5", VERIFY_TONES[item.result])}>{t(`run.verifyResult.${item.result}`)}</span>
              {item.when} → {item.then}
            </p>
            {why && <p className="selectable text-[11.5px] leading-4 break-words text-muted">{t(why)}</p>}
            {item.evidence && <p className="selectable font-mono text-[11.5px] leading-4 break-words text-muted">{item.evidence}</p>}
          </li>
        );
      })}
    </ul>
  );
}

/**
 * One line that opens to the list. The green check only goes with all of it met and nothing else left
 * unproven: anything else says what it is, and not being able to verify something is a normal thing to
 * find, not an alarm.
 */
function VerificationProof({ verification, allProven }: { verification: VerificationOutput; allProven: boolean }) {
  const { t } = useTranslation();
  const { met, unmet, total } = verificationCounts(verification);
  return (
    <details className="my-0.5">
      <summary className="cursor-pointer">
        {allProven && total > 0 && met === total && <Check size={13} className="mr-1 inline align-[-2px] text-success" />}
        <span className={cx(unmet > 0 && "font-medium text-warning")}>{verificationLine(t, verification)}</span>
      </summary>
      <div className="mt-1 mb-1.5 pl-3">
        <VerificationItems verification={verification} />
      </div>
    </details>
  );
}

/** What a verifier found, as text to read. */
function VerificationView({ verification }: { verification: VerificationOutput }) {
  const { t } = useTranslation();
  return (
    <div className="space-y-3 text-[13px] leading-relaxed">
      <p className="selectable font-medium break-words">{verificationLine(t, verification)}</p>
      {verification.summary && <p className="selectable break-words">{verification.summary}</p>}
      <VerificationItems verification={verification} />
    </div>
  );
}

/** What the reviews said they couldn't check by reading the code, with the review that said it. Nothing if there's none. */
function NotProvenByReviews({ unchecked }: { unchecked: { step: string; text: string }[] }) {
  const { t } = useTranslation();
  if (unchecked.length === 0) return null;
  return (
    <div className="mb-1.5">
      <p className="font-medium">{t("run.notProvenByReviews")}</p>
      <ul className="selectable list-disc space-y-0.5 pl-4 break-words">
        {unchecked.map((item, index) => (
          // Lines of a fixed text: their position is their identity.
          <li key={index}>
            {item.text} <span className="font-mono text-[11.5px] text-muted">{item.step}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function ApprovalProof({ attempts, round, commands, rawCheck, reviewed }: { attempts: Attempt[]; round: number; commands: CommandRun[]; rawCheck: RawCheck; reviewed: boolean }) {
  const { t } = useTranslation();
  const checks = latestOfEachStep(attempts.filter((a) => a.kind === "check"));
  const passed = commands.filter((c) => c.status === "ok").length;
  const verification = verificationInRound(attempts, round);
  // What nobody could check comes first, so a count of what was met can't stand for all of it.
  const unchecked = uncheckedInRound(attempts, round);
  return (
    <>
      <NotProvenByReviews unchecked={unchecked} />
      {checks.map((check) => {
        const skipped = skippedTests(check);
        return check.state === "succeeded" ? (
          <p key={check.id} className="selectable flex gap-1.5">
            {skipped > 0 ? <AlertTriangle size={13} className="mt-1 shrink-0 text-warning" /> : <Check size={13} className="mt-1 shrink-0 text-success" />}
            <span className="min-w-0 break-words">{skipped > 0 ? t("run.approvalCheckSkipped", { command: check.prompt, count: skipped }) : t("run.approvalCheckPassed", { command: check.prompt })}</span>
          </p>
        ) : (
          <p key={check.id} className="selectable font-medium break-words text-danger">
            {t("run.approvalCheckFailed", { command: check.prompt })}
          </p>
        );
      })}
      {checks.length === 0 && (
        <p>
          {t("run.approvalNoCheck")}
          {reviewed && ` ${t("run.approvalReviewsOnlyRead")}`}
        </p>
      )}
      {verification && <VerificationProof verification={verification} allProven={unchecked.length === 0} />}
      <p className="text-muted">
        {commands.length === 0 ? t("run.noCommands") : t("run.commandsPassed", { passed, total: commands.length })} <span className={cx((rawCheck === "changed" || rawCheck === "missing") && "font-medium text-danger")}>{t(auditKey(rawCheck))}</span>
      </p>
    </>
  );
}

/** The conclusions only. What each reviewer said in full is one click away. */
function ReviewsGlance({ attempts, round }: { attempts: Attempt[]; round: number }) {
  const { t } = useTranslation();
  const { reviewers, approver } = reviewsOfRound(attempts, round);
  const latest = latestOfEachStep(reviewers);
  const ran = latest.filter((review) => review.state !== "skipped");
  // Those it skipped for being small are on a line of their own, with what can be done about it.
  const skipped = latest.filter((review) => review.state === "skipped" && !skippedAsSmall(review));
  return (
    <div className="space-y-0.5">
      {ran.length > 0 && (
        <ul className="flex flex-wrap gap-x-3">
          {ran.map((review) => (
            <li key={review.id} className="flex items-baseline gap-1.5">
              <span className="font-mono text-[12px] font-medium">{review.stepId}</span>
              <ReviewVerdict review={review} />
            </li>
          ))}
        </ul>
      )}
      {ran
        .filter((review) => review.output?.verdict === "changes" && review.output.summary)
        .map((review) => (
          <p key={review.id} className="selectable text-muted">
            <span className="font-mono text-[12px]">{review.stepId}</span>: {gist(review.output?.summary ?? "", 140).text}
          </p>
        ))}
      {skipped.length > 0 && <p className="text-subtle">{t("run.approvalSkipped", { names: skipped.map((review) => review.stepId).join(", ") })}</p>}
      {approver?.output && (
        <div className={cx(ran.length + skipped.length > 0 && "pt-1")}>
          <ApproverVerdict output={approver.output} riskClass={approver.output.risk === "high" ? "font-medium text-warning" : "text-fg"} />
          {approver.output.reasons && <p className="selectable text-muted">{gist(approver.output.reasons, 160).text}</p>}
        </div>
      )}
    </div>
  );
}

const SHOWN_FILES = 5;

const pictureOf = (shot: Screenshot) => `data:${shot.mime};base64,${shot.data}`;

/**
 * What this round's steps saved of how the change looks, small, each with the step it came from, and
 * what was left out. Keel doesn't take them.
 */
function ScreenshotsRow({ shots, onOpen }: { shots: Screenshots; onOpen: (shot: Screenshot) => void }) {
  const { t } = useTranslation();
  const names = shots.leftOut.join(", ");
  return (
    <ApprovalRow label={t("run.screenshots")}>
      {shots.shown.length > 0 && (
        <ul className="flex flex-wrap gap-2">
          {shots.shown.map((shot, index) => (
            // Two steps can save pictures with the same name.
            <li key={`${index}-${shot.step}-${shot.name}`} className="w-32 min-w-0">
              <button type="button" aria-label={t("run.screenshotOpen", { name: shot.name })} onClick={() => onOpen(shot)} className="block w-full overflow-hidden rounded border border-border bg-inset hover:border-border-strong focus-visible:border-accent focus-visible:outline-none">
                <img src={pictureOf(shot)} alt={shot.name} className="h-20 w-full object-cover object-top" />
              </button>
              <p className="selectable mt-0.5 truncate font-mono text-[11px] text-muted" title={shot.name}>
                {shot.name}
              </p>
              <p className="selectable truncate text-[11px] text-subtle" title={shot.step}>
                {t("run.screenshotStep", { step: shot.step })}
              </p>
              {shot.beforeLastChange && <p className="text-[11px] leading-4 text-warning">{t("run.screenshotBeforeChange")}</p>}
            </li>
          ))}
        </ul>
      )}
      {shots.leftOut.length > 0 && (
        <p className={cx("selectable text-[11.5px] break-words text-muted", shots.shown.length > 0 && "mt-1")}>
          {shots.moreLeftOut > 0 ? t("run.screenshotsLeftOutMore", { names, count: shots.moreLeftOut }) : t("run.screenshotsLeftOut", { names })}
        </p>
      )}
    </ApprovalRow>
  );
}

/** What the engine skipped because the change was small, and why, with a way to ask for it anyway. */
function SkippedForSize({ skipped, onRunNow, disabled }: { skipped: Attempt[]; onRunNow?: () => void; disabled: boolean }) {
  const { t } = useTranslation();
  const reason = skipped[0].reason as { lines?: number; files?: number } | null;
  return (
    <ApprovalRow
      label={t("fits.smallLabel")}
      action={
        onRunNow && (
          <Button disabled={disabled} onKeyDown={ignoreEnter} onClick={onRunNow}>
            {t("fits.runNow")}
          </Button>
        )
      }
    >
      <p>{t("fits.small", { lines: t("fits.lines", { count: reason?.lines ?? 0 }), files: t("fits.files", { count: reason?.files ?? 0 }) })}</p>
      <p className="selectable break-words text-muted">{t("fits.didNotRun", { names: skipped.map((step) => step.stepId).join(", ") })}</p>
      {onRunNow && <p className="mt-0.5 text-[11.5px] text-subtle">{t("fits.runNowHint")}</p>}
    </ApprovalRow>
  );
}

/** Why it got the full review, in plain words: every reason, and not only the first. */
function MovedUp({ decision, round }: { decision: SizeDecision; round: number }) {
  const { t } = useTranslation();
  // It stays there, so in a later round these can be about code that has changed since.
  return (
    <ApprovalRow label={t("fits.upLabel")} note={decision.round < round ? t("run.round", { number: decision.round + 1 }) : undefined}>
      <p>{t("fits.up")}</p>
      <ul className="selectable list-disc space-y-0.5 pl-4 break-words text-muted">
        {decision.because.map((why) => (
          <li key={why.code}>{whyNotSmallText(t, why)}</li>
        ))}
      </ul>
    </ApprovalRow>
  );
}

/** Enter is what opens a run from the palette, so a second one must not answer it. A click or Space does. */
const ignoreEnter = (e: React.KeyboardEvent) => {
  if (e.key === "Enter") e.preventDefault();
};

/**
 * Whether what answers for the person can be pressed yet. A click on this window while it's in the
 * background acts as a click, not only as a way to bring it forward. So it's off while the window
 * is away, and arms again a moment after it's back.
 */
function useArmed() {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout>;
    const disarm = () => {
      clearTimeout(timer);
      setArmed(false);
    };
    // The palette and every dialog hold the keymap. Their overlays cover the card, and a second
    // click on one would land here once it's gone, so it waits for them. New run is one click away.
    const arm = () => {
      disarm();
      if (!keymapHeld()) timer = setTimeout(() => setArmed(true), 800);
    };
    arm();
    window.addEventListener("focus", arm);
    window.addEventListener("blur", disarm);
    const unwatchHold = watchKeymapHold(arm);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("focus", arm);
      window.removeEventListener("blur", disarm);
      unwatchHold();
    };
  }, []);
  return armed;
}

/** The same line, however a quote of it was spaced, cased or put between quotation marks. */
function sameLine(quoted: string, line: string): boolean {
  const plain = (text: string) =>
    text
      .replace(/\s+/g, " ")
      .replace(/^[\s"'`“”«»]+|[\s"'`“”«».]+$/g, "")
      .toLowerCase();
  return plain(quoted) === plain(line);
}

/**
 * Which lines a questioned decision is about: the ones its quote is, or else the one its number
 * names. A quote that is a line word for word wins over a number, which an agent can get wrong.
 */
function questionedLines(q: Questioned, decided: SpecDecided[]): number[] {
  const quoted = decided.flatMap((item, index) => (sameLine(q.decided, item.text) ? [index] : []));
  if (quoted.length > 0) return quoted;
  const { number } = q;
  return typeof number === "number" && Number.isInteger(number) && number >= 1 && number <= decided.length ? [number - 1] : [];
}

/**
 * What it settled by itself. Each line can go back to whoever does the work, without starting over.
 * What a review questions of it is the person's to settle, so it shows on its line.
 */
function DecidedList({ attemptId, decided, questioned, canChange, armed, busy, onSend }: { attemptId: string; decided: SpecDecided[]; questioned: Questioned[]; canChange: boolean; armed: boolean; busy: boolean; onSend: (decided: string, instruction: string, onDone: () => void) => void }) {
  const { t } = useTranslation();
  const [changing, setChanging] = useState<number | null>(null);
  const draftKey = `${attemptId}:change:${changing}`;
  const instruction = useApp((s) => s.approvalDrafts[draftKey] ?? "");
  const setDraft = useApp((s) => s.setApprovalDraft);
  const lines = questioned.map((q) => questionedLines(q, decided));
  // One that matches no line still reaches the person, quoted as it came.
  const elsewhere = questioned.filter((_, at) => lines[at].length === 0);
  return (
    <ul className="space-y-1.5">
      {decided.map((item, index) => (
        // Lines of a fixed text: their position is their identity.
        <li key={index}>
          <div className="flex items-start gap-2">
            <p className="selectable min-w-0 flex-1 break-words">{item.text}</p>
            {canChange && changing !== index && (
              <Button variant="ghost" className="-my-1 shrink-0" aria-label={t("run.decidedChangeThis", { text: item.text })} onClick={() => setChanging(index)}>
                {t("run.decidedChange")}
              </Button>
            )}
          </div>
          {item.taken === "person" && <p className="text-[11.5px] text-subtle">{t("run.decidedYouChose")}</p>}
          {item.taken === "roundsLimit" && <p className="text-[11.5px] text-warning">{t("run.decidedKeelTook")}</p>}
          {questioned
            .filter((_, at) => lines[at].includes(index))
            .map((q, at) => (
              <p key={at} className="selectable text-[11.5px] break-words text-warning">
                {t("run.decidedQuestioned", { why: q.why })}
              </p>
            ))}
          {changing === index && (
            <div className="mt-1.5 rounded-md border border-border bg-inset p-2">
              <Field label={t("run.sendBackLabel")} hint={t("run.sendBackHint")}>
                <textarea autoFocus rows={2} value={instruction} onChange={(e) => setDraft(draftKey, e.target.value)} className={`${inputClass} resize-y`} />
              </Field>
              <div className="mt-2 flex flex-wrap gap-2">
                <Button variant="primary" disabled={!armed || busy || !instruction.trim()} onKeyDown={ignoreEnter} onClick={() => onSend(item.text, instruction, () => setDraft(draftKey, ""))}>
                  {t("run.sendBackAction")}
                </Button>
                <Button variant="ghost" onKeyDown={ignoreEnter} onClick={() => setChanging(null)}>
                  {t("run.sendBackKeep")}
                </Button>
              </div>
            </div>
          )}
        </li>
      ))}
      {elsewhere.map((q, at) => (
        <li key={`elsewhere-${at}`} className="selectable text-[11.5px] break-words text-warning">
          {t("run.decidedQuestionedElsewhere", { decided: q.decided, why: q.why })}
        </li>
      ))}
    </ul>
  );
}

/**
 * What's being decided, in one place, so nobody has to scroll to find out. Rejecting takes a reason,
 * so the focus starts on the note, where Enter only adds a line. Enter never answers, and approve
 * arms after a moment.
 */
export function ApprovalCard({ attempt, run, attempts, files, commands, rawCheck, sizeDecision = null, joint = null, onShowChanges, onError }: { attempt: Attempt; run: Run; attempts: Attempt[]; files: string[]; commands: CommandRun[]; rawCheck: RawCheck; sizeDecision?: SizeDecision | null; joint?: RunJoint | null; onShowChanges: () => void; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const project = useApp((s) => s.overview.projects.find((p) => p.id === run.projectId));
  const draft = useApp((s) => s.approvalDrafts[attempt.id] ?? "");
  const setDraft = useApp((s) => s.setApprovalDraft);
  // What was sent stays on the card until the card goes, instead of blinking empty.
  const [sent, setSent] = useState<string | null>(null);
  const note = sent ?? draft;
  const armed = useArmed();
  const [busy, setBusy] = useState(false);
  const [reading, setReading] = useState<"asked" | "explanation" | "reviews" | null>(null);
  // Masking only hides a secret here. Approving would still push it with the branch.
  const [addsSecret, setAddsSecret] = useState(false);
  const changesSomething = files.length > 0;
  useEffect(() => {
    if (!changesSomething) return;
    let current = true;
    api
      .runAddsSecret(run.id)
      .then((found) => current && setAddsSecret(found))
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [run.id, changesSomething]);
  const [shots, setShots] = useState<Screenshots | null>(null);
  const [looking, setLooking] = useState<Screenshot | null>(null);
  const provesRow = useRef<HTMLDivElement>(null);
  // Each approval looks again: a later round may have new ones, or have changed the code since.
  useEffect(() => {
    let current = true;
    api
      .runScreenshots(run.id)
      .then((found) => current && setShots(found))
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [run.id, attempt.id]);
  const anyShots = shots != null && (shots.shown.length > 0 || shots.leftOut.length > 0);
  const answer = (approve: boolean) => {
    setBusy(true);
    setSent(note);
    api.approvalAnswer(attempt.id, approve, note).then(
      () => setDraft(attempt.id, ""),
      (e) => {
        setBusy(false);
        setSent(null);
        onError(errorText(t, e));
      },
    );
  };

  const sendBack = (decided: string, instruction: string, onDone: () => void) => {
    setBusy(true);
    api.approvalSendBack(attempt.id, t("run.sendBackAbout", { decided, instruction: instruction.trim() })).then(onDone, (e) => {
      setBusy(false);
      onError(errorText(t, e));
    });
  };

  const runSkipped = () => {
    setBusy(true);
    // The card goes when it's done, and the approval comes round again once what was skipped has run.
    api.approvalRunSkipped(attempt.id).catch((e) => {
      setBusy(false);
      onError(errorText(t, e));
    });
  };

  const outcome = approvalOutcome(run.workflow, attempt.stepId, files);
  const empty = outcome === "nothingToDeliver";
  const delivers = outcome === "delivers" || outcome === "deliversLater";
  const explaining = explainingAttempts(attempts);
  // After rework, how it started and how it ended tell the story.
  const told = explaining.length > 2 ? [explaining[0], explaining[explaining.length - 1]] : explaining;
  const gists = told.map((a) => ({ attempt: a, ...gist(a.resultText ?? "") }));
  const hasMore = explaining.length > told.length || gists.some((g) => g.cut);
  const specifying = specifyingAttempt(attempts);
  // At an approval right after that step, "What it did" is already showing this same text.
  const asked = specifying && !explaining.some((a) => a.id === specifying.id) ? askedText(specifying) : null;
  const askedGist = asked ? gist(asked, 160) : null;
  const spec = specifying ? specOf(specifying) : null;
  // Where a change across projects is agreed, what's approved is mostly its parts, and they mustn't hide behind "Read all".
  const parts = spec?.parts ?? [];
  const agreeing = parts.length > 0;
  const together = agreeing ? (spec?.together ?? []) : [];
  // Right after working out what's asked, nobody has done anything yet. Saying it finished without
  // an explanation would read as something gone wrong.
  const nothingToExplain = asked !== null && !attempts.some((a) => a.kind === "agent" && a.role === "worker");
  const { reviewers, approver } = reviewsOfRound(attempts, run.round);
  const reviewed = reviewers.some((review) => review.state !== "skipped") || approver != null;
  // What a review questions of what was decided alone, which the approver left to the person.
  const raised = approver?.output?.questioned;
  const questioned = Array.isArray(raised) ? raised.filter((q) => typeof q?.decided === "string" && typeof q.why === "string") : [];
  const skippedForSize = skippedSteps(attempts);
  const hiddenFiles = files.length - SHOWN_FILES;
  const sentBack = sentBackByAssistant(attempts, run.round);
  // What nobody could prove, the same as in "What proves it". In a short window that row can be far
  // down the list, so how much of it there is shows by the note, which never scrolls away.
  const verification = verificationInRound(attempts, run.round);
  const notProven = uncheckedInRound(attempts, run.round).length + (verification ? verificationCounts(verification).unverified : 0);
  const showWhatProves = () => provesRow.current?.scrollIntoView({ block: "nearest" });

  return (
    // Short enough for its buttons to show without scrolling: the window less the top bar (38), the
    // status bar (24), and the run's title and the page's padding above and below (102).
    <Card role="alert" className="flex max-h-[calc(100vh-164px)] flex-col overflow-y-auto border-warning/40 p-4">
      <p className="text-[14px] font-semibold">{t(agreeing ? "run.agreementTitle" : empty ? "run.approvalEmptyTitle" : outcome === "delivers" ? "run.approvalTitle" : "run.approvalContinueTitle")}</p>
      <p className="mt-1 mb-3 text-[12.5px] text-muted">{t(agreeing ? "run.agreementBody" : empty ? "run.approvalEmptyBody" : "run.approvalBody")}</p>
      {/* What's being approved never shrinks away: in a short window the card scrolls instead. */}
      <dl className="max-h-[50vh] min-h-24 shrink-0 overflow-y-auto">
        {agreeing && (
          <ApprovalRow label={t("run.agreementParts")} note={t("run.generated")}>
            <ol className="selectable list-decimal space-y-0.5 pl-4 break-words">
              {parts.map((part, index) => (
                // Lines of a fixed text: their position is their identity.
                <li key={index}>
                  <span className="font-medium">{part.project}</span>: {part.text}
                </li>
              ))}
            </ol>
          </ApprovalRow>
        )}
        {together.length > 0 && (
          <ApprovalRow label={t("run.specTogether")} note={t("run.generated")}>
            <ul className="selectable list-disc space-y-0.5 pl-4 break-words">
              {together.map((text, index) => (
                <li key={index}>{text}</li>
              ))}
            </ul>
          </ApprovalRow>
        )}
        {askedGist && (
          <ApprovalRow
            label={t("run.whatWasAsked")}
            note={t("run.generated")}
            action={
              (askedGist.cut || spec) && (
                <Button variant="ghost" aria-label={t("run.approvalReadAsked")} onClick={() => setReading("asked")}>
                  {t("run.approvalReadAll")}
                </Button>
              )
            }
          >
            <p className="selectable break-words">{askedGist.text}</p>
          </ApprovalRow>
        )}
        {spec && (spec.decided.length > 0 || questioned.length > 0) && (
          <ApprovalRow label={t("run.decidedTitle")} note={t("run.generated")}>
            <DecidedList attemptId={attempt.id} decided={spec.decided} questioned={questioned} canChange={!empty && canSendBack(run.workflow, attempt.stepId)} armed={armed} busy={busy} onSend={sendBack} />
          </ApprovalRow>
        )}
        {sentBack && (
          <ApprovalRow label={t("run.whyThisRound")}>
            <p className="font-medium">{t("run.sentBackBy", { client: sentBack.by })}</p>
            <p className="selectable break-words whitespace-pre-wrap text-muted">{sentBack.instructions}</p>
          </ApprovalRow>
        )}
        {!nothingToExplain && (
          <ApprovalRow
            label={t("run.whatItDid")}
            note={explaining.length > 0 ? t("run.generated") : undefined}
            action={
              !empty &&
              hasMore && (
                <Button variant="ghost" onClick={() => setReading("explanation")}>
                  {t("run.approvalReadAll")}
                </Button>
              )
            }
          >
            {explaining.length === 0 && <p>{t("run.noResult")}</p>}
            {empty ? (
              <Explanations attempts={explaining} />
            ) : (
              gists.map((g) => (
                <p key={g.attempt.id} className="selectable mb-1 break-words last:mb-0">
                  {g.attempt.round > 0 && <span className="mr-1.5 font-medium text-warning">{t("run.round", { number: g.attempt.round + 1 })}</span>}
                  {g.text}
                </p>
              ))
            )}
          </ApprovalRow>
        )}
        {!empty && (
          <ApprovalRow
            label={t("run.whatChanged")}
            action={
              files.length > 0 && (
                <Button variant="ghost" onClick={onShowChanges}>
                  <FileDiff size={14} />
                  {t("run.viewDiff")}
                </Button>
              )
            }
          >
            {files.length === 0 ? (
              <p className="text-muted">{t("run.noChanges")}</p>
            ) : (
              <ul className="selectable flex flex-wrap gap-x-3 font-mono text-[12px]">
                {files.slice(0, SHOWN_FILES).map((file) => (
                  <li key={file} className="min-w-0 break-all">
                    {file}
                  </li>
                ))}
                {hiddenFiles > 0 && <li className="font-sans text-[12.5px] text-muted">{t("run.approvalMoreFiles", { count: hiddenFiles })}</li>}
              </ul>
            )}
          </ApprovalRow>
        )}
        <ApprovalRow label={t("run.whatProves")} rowRef={provesRow}>
          <ApprovalProof attempts={attempts} round={run.round} commands={commands} rawCheck={rawCheck} reviewed={reviewed} />
        </ApprovalRow>
        {/* After what proves it: in a short window they'd take all its room, and push what wasn't proven out of view. */}
        {anyShots && <ScreenshotsRow shots={shots} onOpen={setLooking} />}
        {skippedForSize.length > 0 && <SkippedForSize skipped={skippedForSize} onRunNow={empty ? undefined : runSkipped} disabled={!armed || busy} />}
        {sizeDecision && !sizeDecision.small && <MovedUp decision={sizeDecision} round={run.round} />}
        {(reviewers.some((review) => !skippedAsSmall(review)) || approver) && (
          <ApprovalRow
            label={t("run.reviewsTitle")}
            note={run.round > 0 ? t("run.round", { number: run.round + 1 }) : undefined}
            action={
              <Button variant="ghost" onClick={() => setReading("reviews")}>
                {t("run.approvalSeeReviews")}
              </Button>
            }
          >
            <ReviewsGlance attempts={attempts} round={run.round} />
          </ApprovalRow>
        )}
      </dl>
      {joint?.part?.warnings.map((warning) => (
        <p key={`${warning.kind}-${warning.position}`} className="mt-2 flex gap-1.5 rounded-md border border-warning/40 bg-inset p-2 text-[12.5px] font-medium text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          {partWarningText(t, warning)}
        </p>
      ))}
      {touchesWorkflows(files) && (
        <p className="mt-2 flex gap-1.5 rounded-md border border-warning/40 bg-inset p-2 text-[12.5px] font-medium text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          {t("run.workflowWarning")}
        </p>
      )}
      {addsSecret && (
        <p className="mt-2 flex gap-1.5 rounded-md border border-warning/40 bg-inset p-2 text-[12.5px] font-medium text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          {t(delivers ? "run.secretWarning" : "run.secretWarningLocal")}
        </p>
      )}
      {/* Right above the note and the buttons: the focus starts there, so this is in view with them. */}
      {notProven > 0 && (
        <div className="mt-2 flex items-center gap-2 rounded-md border border-border-strong bg-inset py-1 pr-1 pl-2 text-[12.5px]">
          <CircleQuestionMark size={14} className="shrink-0 text-muted" />
          <p className="min-w-0 flex-1 font-medium">{t("run.notProvenCount", { count: notProven })}</p>
          <Button variant="ghost" onClick={showWhatProves}>
            {t("run.notProvenSee", { count: notProven })}
          </Button>
        </div>
      )}
      {empty ? (
        <div className="mt-3 border-t border-border pt-3">
          <Button autoFocus variant="primary" disabled={busy} onKeyDown={ignoreEnter} onClick={() => answer(false)}>
            {t("run.approvalClose")}
          </Button>
          <p className="mt-1 text-[11.5px] text-muted">{t("run.approvalCloseHint")}</p>
        </div>
      ) : (
        <>
          <div className="mt-2 border-t border-border pt-3">
            <Field label={t("run.approvalNote")} hint={delivers ? `${t("run.approvalNoteHint")} ${t("run.approvalNotePrHint")}` : t("run.approvalNoteHint")}>
              <textarea autoFocus rows={2} value={note} onChange={(e) => setDraft(attempt.id, e.target.value)} className={`${inputClass} resize-y`} />
            </Field>
          </div>
          <div className="mt-3 grid grid-cols-2 gap-4">
            <div>
              <Button disabled={busy || !note.trim()} onKeyDown={ignoreEnter} onClick={() => answer(false)}>
                {t("run.reject")}
              </Button>
              <p className="mt-1 text-[11.5px] text-muted">{t("run.approvalRejectHint")}</p>
              {!note.trim() && <p className="mt-0.5 text-[11.5px] text-muted">{t("run.approvalRejectNeedsReason")}</p>}
            </div>
            <div>
              <Button
                variant="primary"
                disabled={!armed || busy}
                onKeyDown={ignoreEnter}
                onClick={() => answer(true)}
              >
                {outcome === "delivers" && <GitPullRequest size={14} />}
                {t(agreeing ? "run.agreementApprove" : outcome === "delivers" ? "run.approve" : "run.approveContinue")}
              </Button>
              <p className="mt-1 text-[11.5px] text-muted">{t(agreeing ? "run.agreementApproveHint" : outcome === "delivers" ? "run.approvalApproveHint" : outcome === "deliversLater" ? "run.approvalLaterHint" : "run.approvalNoPushHint")}</p>
              {delivers && (
                <p className="selectable mt-0.5 flex flex-wrap gap-x-2 text-[11.5px] text-muted">
                  {t("run.approvalWhere")}
                  {project?.githubRepo && <span className="font-mono text-fg">{project.githubRepo}</span>}
                  {run.branch && <span className="font-mono text-fg">{run.branch}</span>}
                  {project?.githubAccount && <span className="text-fg">{t("run.accountGithub", { account: project.githubAccount })}</span>}
                </p>
              )}
              {delivers && project && !project.githubRepo && <p className="mt-0.5 text-[11.5px] text-warning">{t("reasons.noGithubRemote")}</p>}
              {delivers && project?.githubRepo && !project.githubAccount && <p className="mt-0.5 text-[11.5px] text-warning">{t("reasons.githubAccountMissing")}</p>}
            </div>
          </div>
        </>
      )}
      <Modal open={reading !== null} onOpenChange={(open) => !open && setReading(null)} title={t(reading === "reviews" ? "run.reviewsTitle" : reading === "asked" ? "run.whatWasAsked" : "run.approvalExplanationTitle")} className="w-[720px]">
        {reading === "reviews" ? (
          <ReviewsList attempts={attempts} round={run.round} />
        ) : reading === "asked" ? (
          spec ? <SpecView spec={spec} /> : <p className="selectable text-[13px] leading-relaxed whitespace-pre-wrap">{asked}</p>
        ) : (
          <div className="text-[13px] leading-relaxed">
            <Explanations attempts={explaining} />
          </div>
        )}
      </Modal>
      <Modal open={looking !== null} onOpenChange={(open) => !open && setLooking(null)} title={looking?.name ?? ""} className="w-[960px]">
        {looking && (
          <>
            <p className="selectable mb-2 text-[12.5px] text-muted">{t("run.screenshotStep", { step: looking.step })}</p>
            {looking.beforeLastChange && <p className="mb-2 text-[12.5px] text-warning">{t("run.screenshotBeforeChange")}</p>}
            <img src={pictureOf(looking)} alt={looking.name} className="max-w-full rounded border border-border" />
          </>
        )}
      </Modal>
    </Card>
  );
}

function QuestionBlock({ question, pick, name, autoFocus, disabled, onPick }: { question: SpecQuestion; pick: QuestionPick; name: string; autoFocus: boolean; disabled: boolean; onPick: (pick: QuestionPick) => void }) {
  const { t } = useTranslation();
  const missing = needsText(pick);
  return (
    <fieldset className="min-w-0 rounded-md border border-border p-3">
      {/* Floated, or a question on two lines would sit across the border. */}
      <legend className="selectable float-left mb-1.5 w-full p-0 text-[13px] leading-snug font-medium break-words">{question.text}</legend>
      {recommendedOption(question) === undefined && <p className="clear-both mb-1.5 text-[11.5px] text-warning">{t("run.questionNoneRecommended")}</p>}
      <div className="clear-both space-y-1">
        {question.options.map((option, index) => (
          <label key={option.label} className="flex items-start gap-2 text-[12.5px] leading-5">
            {/* With nothing chosen yet, the focus goes to the first option without choosing it. */}
            <input type="radio" name={name} className="mt-1 shrink-0" autoFocus={autoFocus && (pick.option === undefined ? index === 0 : pick.option === option.label)} checked={pick.option === option.label} disabled={disabled} onChange={() => onPick({ ...pick, option: option.label })} />
            <span className="min-w-0 break-words">
              {option.label}
              {option.recommended && <span className="ml-1.5 rounded border border-accent/40 px-1 text-[11px] whitespace-nowrap text-accent">{t("run.questionRecommended")}</span>}
            </span>
          </label>
        ))}
        <label className="flex items-start gap-2 text-[12.5px] leading-5">
          <input type="radio" name={name} className="mt-1 shrink-0" checked={pick.option === null} disabled={disabled} onChange={() => onPick({ ...pick, option: null })} />
          {t("run.questionOther")}
        </label>
      </div>
      {pick.option === null && (
        <div className="mt-1.5 pl-5">
          <textarea autoFocus rows={2} aria-label={t("run.questionOtherLabel", { question: question.text })} value={pick.own} disabled={disabled} onChange={(e) => onPick({ ...pick, own: e.target.value })} className={`${inputClass} resize-y`} />
          {missing && <p className="mt-0.5 text-[11.5px] text-warning">{t("run.questionOtherNeeded")}</p>}
        </div>
      )}
      <label className="mt-2 flex items-start gap-2 border-t border-border pt-2 text-[12px] text-muted">
        <input type="checkbox" className="mt-0.5 shrink-0" checked={pick.remember} disabled={disabled} onChange={(e) => onPick({ ...pick, remember: e.target.checked })} />
        {t("run.questionRemember")}
      </label>
    </fieldset>
  );
}

/**
 * What only the person can decide, in one place, so nobody has to scroll to find out. The focus
 * starts on the first question's recommended option, where Enter does nothing and the arrows move
 * between options. Enter never answers, and what answers arms after a moment, as on an approval.
 * A question with no recommended option starts with nothing chosen, and then the recommended
 * can't answer for the person: they have to choose.
 */
export function QuestionsCard({ attempt, attempts, onError }: { attempt: Attempt; attempts: Attempt[]; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const spec = specOf(attempt);
  const questions = spec?.questions ?? [];
  const drafts = useApp((s) => s.questionDrafts[attempt.id]);
  const setDraft = useApp((s) => s.setQuestionDraft);
  const clearDrafts = useApp((s) => s.clearQuestionDrafts);
  const armed = useArmed();
  const [busy, setBusy] = useState(false);
  const picks = questions.map((question) => drafts?.[question.id] ?? startingPick(question));
  const anyDifferent = questions.some((question, index) => differsFromRecommended(question, picks[index]));
  const complete = picks.every(isAnswered);
  const recommends = allRecommend(questions);
  const rounds = roundsAnswered(attempts, attempt);

  const answer = (answers: QuestionAnswer[]) => {
    setBusy(true);
    api.questionsAnswer(attempt.id, answers).then(
      () => clearDrafts(attempt.id),
      (e) => {
        setBusy(false);
        onError(errorText(t, e));
      },
    );
  };
  const cancel = () => {
    setBusy(true);
    api.attemptAbandon(attempt.id).then(
      () => clearDrafts(attempt.id),
      (e) => {
        setBusy(false);
        onError(errorText(t, e));
      },
    );
  };

  return (
    // Short enough for its buttons to show without scrolling: the window less the same as on the
    // approval card. The questions are what gives way, and they scroll on their own.
    <Card role="alert" className="flex max-h-[calc(100vh-164px)] flex-col overflow-y-auto border-warning/40 p-4">
      <p className="shrink-0 text-[14px] font-semibold">{t("run.questionsTitle")}</p>
      <p className="mt-1 shrink-0 text-[12.5px] text-muted">{t("run.questionsBody")}</p>
      {spec?.summary && (
        <p className="selectable mt-2 shrink-0 text-[12.5px] leading-5 break-words">
          <span className="font-semibold text-muted">{t("run.questionsWillChange")}:</span> {spec.summary}
        </p>
      )}
      <div className="mt-3 min-h-28 space-y-2.5 overflow-y-auto">
        {questions.map((question, index) => (
          <QuestionBlock key={question.id} question={question} pick={picks[index]} name={`${attempt.id}-${question.id}`} autoFocus={index === 0} disabled={busy} onPick={(pick) => setDraft(attempt.id, question.id, pick)} />
        ))}
      </div>
      <p className="mt-2 shrink-0 text-[11.5px] text-subtle">{t("run.questionRememberHint")}</p>
      {rounds > 0 && <p className="mt-1 shrink-0 text-[11.5px] text-warning">{t(rounds > 1 ? "run.questionsExtraRound" : "run.questionsLastRound")}</p>}
      <div className="mt-3 grid shrink-0 grid-cols-3 gap-4 border-t border-border pt-3">
        <div>
          <Button variant={anyDifferent || !recommends ? "secondary" : "primary"} disabled={!armed || busy || !recommends} onKeyDown={ignoreEnter} onClick={() => answer(recommendedAnswers(questions, picks))}>
            {t("run.questionsUseRecommended")}
          </Button>
          <p className="mt-1 text-[11.5px] text-muted">{t(recommends ? "run.questionsUseRecommendedHint" : "run.questionsUseRecommendedNone")}</p>
        </div>
        <div>
          <Button variant={anyDifferent || !recommends ? "primary" : "secondary"} disabled={!armed || busy || !anyDifferent || !complete} onKeyDown={ignoreEnter} onClick={() => answer(questions.map((question, index) => answerFrom(question, picks[index])))}>
            {t("run.questionsAnswer")}
          </Button>
          <p className="mt-1 text-[11.5px] text-muted">{t("run.questionsAnswerHint")}</p>
          {!complete ? <p className="mt-0.5 text-[11.5px] text-muted">{t("run.questionsAnswerEach")}</p> : !anyDifferent && <p className="mt-0.5 text-[11.5px] text-muted">{t("run.questionsAnswerSame")}</p>}
        </div>
        <div>
          <Button variant="ghost" disabled={!armed || busy} onKeyDown={ignoreEnter} onClick={cancel}>
            {t("run.questionsCancel")}
          </Button>
          <p className="mt-1 text-[11.5px] text-muted">{t("run.questionsCancelHint")}</p>
        </div>
      </div>
    </Card>
  );
}

function Actions({ attempt, projectId, onError }: { attempt: Attempt; projectId: string; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  const act = (run: () => Promise<unknown>) => run().catch((e) => onError(errorText(t, e)));
  if (attempt.reason?.code === "reworkLimit") {
    return (
      <Card role="alert" className="border-warning/40 p-4">
        <p className="selectable text-[12.5px] text-warning">{reasonText(t, attempt.reason)}</p>
        <div className="mt-2.5 flex flex-wrap gap-2">
          <Button onClick={() => void act(() => api.reworkLimitAnswer(attempt.id, true))}>{t("run.anotherRound")}</Button>
          <Button variant="primary" onClick={() => void act(() => api.reworkLimitAnswer(attempt.id, false))}>
            {t("run.toMyApproval")}
          </Button>
          <Button variant="ghost" onClick={() => void act(() => api.attemptAbandon(attempt.id))}>
            {t("run.abandon")}
          </Button>
        </div>
      </Card>
    );
  }
  if (attempt.state === "stalled") {
    return (
      <Card role="alert" className="border-warning/40 p-4">
        <p className="flex items-center gap-2 text-[13px] font-semibold text-warning">
          <AlertTriangle size={15} />
          {t("run.stalledTitle")}
        </p>
        {attempt.reason && <p className="selectable mt-1.5 text-[12.5px]">{reasonText(t, attempt.reason)}</p>}
        <p className="mt-2 text-[12px] text-muted">{t("run.stalledNote")}</p>
        <div className="mt-2 flex flex-wrap gap-2">
          <Button onClick={() => void act(() => api.attemptWait(attempt.id))}>{t("run.waitLonger")}</Button>
          {attempt.reason?.command != null && <Button onClick={() => void act(() => api.attemptKillCommand(attempt.id))}>{t("run.killCommand")}</Button>}
          <Button variant="danger" onClick={() => void act(() => api.attemptStop(attempt.id))}>
            {t("run.stopStep")}
          </Button>
        </div>
      </Card>
    );
  }
  if (isActive(attempt.state)) {
    return (
      <div className="flex items-center gap-3">
        {attempt.reason && <p className="flex-1 text-[12.5px] text-muted">{reasonText(t, attempt.reason)}</p>}
        <Button variant="danger" onClick={() => void act(() => api.attemptStop(attempt.id))}>
          {t("run.stop")}
        </Button>
      </div>
    );
  }
  if (attempt.state === "succeeded" || attempt.state === "abandoned" || attempt.state === "rejected" || attempt.state === "skipped" || attempt.state === "returned" || attempt.state === "postponed") return null;
  const canResume = attempt.kind === "agent" && ["interrupted", "blocked", "stopped", "failed"].includes(attempt.state);
  return (
    <Card className="p-4">
      {attempt.reason && <p className="selectable text-[12.5px] text-warning">{reasonText(t, attempt.reason)}</p>}
      <div className={cx("flex flex-wrap gap-2", attempt.reason && "mt-2.5")}>
        {needsSignIn(attempt.reason) && <Button onClick={() => void act(() => api.openClaudeLogin())}>{t("run.signIn")}</Button>}
        {needsProjectSettings(attempt.reason) && <Button onClick={() => navigate({ name: "projects", projectId })}>{t("newRun.openProject")}</Button>}
        {canResume && <Button variant="primary" onClick={() => void act(() => api.attemptRetry(attempt.id, true))}>{t("run.resume")}</Button>}
        <Button onClick={() => void act(() => api.attemptRetry(attempt.id, false))}>{t("run.retry")}</Button>
        <Button variant="ghost" onClick={() => void act(() => api.attemptAbandon(attempt.id))}>
          {t("run.abandon")}
        </Button>
      </div>
    </Card>
  );
}

function Output({ text, truncated }: { text: string; truncated?: boolean }) {
  const { t } = useTranslation();
  if (!text) return null;
  return (
    <>
      <pre className="mt-1.5 max-h-64 overflow-auto rounded bg-inset p-2 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap text-muted">{text}</pre>
      {truncated && <p className="mt-1 text-[11px] text-subtle">{t("activity.truncated")}</p>}
    </>
  );
}

/** What a command's exit code can't say on its own: that its output mentions a failure, or that a pipe can hide one. */
function CommandNotes({ end }: { end: CommandEnd }) {
  const { t } = useTranslation();
  return (
    <>
      {end.failureMark && (
        <p className="mt-1 flex items-center gap-1 text-[11.5px] text-warning">
          <AlertTriangle size={12} className="shrink-0" />
          {t("commands.outputSays", { mark: end.failureMark })}
        </p>
      )}
      {end.piped && <p className="mt-1 text-[11.5px] text-subtle">{t("commands.pipedNote")}</p>}
    </>
  );
}

function ActivityEntry({ item, role, corrected }: { item: ActivityItem; role: Attempt["role"]; corrected: boolean }) {
  const { t } = useTranslation();
  const { event, result } = item;
  const data = event.data;
  const line = (text: string, tone = "text-muted") => <p className={cx("text-[12.5px]", tone)}>{text}</p>;
  switch (item.kind) {
    case "text":
      return <p className="selectable text-[13px] leading-relaxed whitespace-pre-wrap">{data.text}</p>;
    case "toolCall": {
      // Its answer has a tab of its own, as text to read.
      if (role === "specifier" && data.name === "StructuredOutput") return line(t("activity.specAnswer"), "text-subtle");
      if (role === "verifier" && data.name === "StructuredOutput") return line(t("activity.verifyAnswer"), "text-subtle");
      const failed = result?.data.isError === true;
      // Only a command's output is read for failures: a file that was read can say anything.
      const end = data.name === "Bash" ? commandEnd(String(data.input?.command ?? ""), result) : null;
      return (
        <div>
          <p className="flex items-baseline gap-2 text-[12.5px]">
            <span className="font-semibold">{data.name}</span>
            <code className="selectable min-w-0 font-mono text-[11.5px] break-all text-muted">{toolSummary(data.name, data.input)}</code>
            {!result && <Loader2 size={12} className="shrink-0 animate-spin self-center text-accent" />}
            {failed && (
              <span className="shrink-0 text-[11.5px] text-danger">
                {result?.data.exitCode != null ? t("activity.exitCode", { code: result.data.exitCode }) : t("activity.toolError")}
              </span>
            )}
          </p>
          {end && <CommandNotes end={end} />}
          {result && <Output text={result.data.text} truncated={result.data.truncated} />}
        </div>
      );
    }
    case "started":
      return line(data.command ? t("activity.startedCheck", { command: data.command }) : t("activity.started"), "text-subtle");
    case "waiting":
      return line(t("activity.waiting"), "text-warning");
    case "skipped":
      return line(data.reason?.code === "treatedAsSmall" ? t("activity.skippedSmall") : t("activity.skipped"), "text-subtle");
    case "postponed":
      return line(t("activity.postponed", { names: (data.steps ?? []).join(", ") }), "font-medium");
    case "sizeDecided":
      return data.small ? (
        line(t("activity.sizeSmall", { lines: t("fits.lines", { count: data.lines ?? 0 }), files: t("fits.files", { count: data.files ?? 0 }) }))
      ) : (
        <div>
          {line(t("fits.up"))}
          <ul className="selectable mt-0.5 list-disc space-y-0.5 pl-4 text-[12.5px] break-words text-muted">
            {(data.because ?? []).map((why: { code: string }) => (
              <li key={why.code}>{whyNotSmallText(t, why)}</li>
            ))}
          </ul>
        </div>
      );
    case "rework":
      return line(t("activity.rework", { round: data.round }), "text-warning");
    case "reworkLimit":
      return line(t("activity.reworkLimit"), "text-warning");
    case "reworkLimitAnswer":
      return line(data.anotherRound ? t("activity.reworkLimitAnswerRound") : t("activity.reworkLimitAnswerApproval"));
    case "questionsAsked":
      return line(t("activity.questionsAsked", { count: (data.questions ?? []).length }), "text-warning");
    case "answers":
      return (
        <div>
          {line(t("activity.answers"), "font-medium")}
          <ul className="selectable mt-0.5 space-y-0.5 text-[12.5px] text-muted">
            {(data.answers ?? []).map((answer: { question: string; asked: string; answer: string }) => (
              <li key={answer.question}>{t("activity.answersLine", { asked: answer.asked, answer: answer.answer })}</li>
            ))}
          </ul>
        </div>
      );
    case "questionsTaken":
      return line(t("activity.questionsTaken", { rounds: data.rounds }), "text-warning");
    case "specCorrected":
      return (
        <div>
          {line(t("activity.specCorrected"), "text-warning")}
          <ul className="selectable mt-0.5 list-disc space-y-0.5 pl-4 text-[12.5px] break-words text-muted">
            {(data.problems ?? []).map((problem: Reason, index: number) => (
              // The engine's list, in its order: two problems can say the same.
              <li key={index}>{specProblemText(t, problem)}</li>
            ))}
          </ul>
        </div>
      );
    case "reviewIncomplete":
      return line(t("activity.reviewIncomplete", { files: (data.unread ?? []).join(", ") }), "text-warning");
    case "reviewBreaks":
      return line(t("activity.reviewBreaks", { ids: (data.breaks ?? []).join(", ") }), "text-warning");
    case "verificationCorrected":
      return (
        <div>
          {(data.corrected ?? []).length > 0 && line(t("activity.verificationCorrected", { count: data.corrected.length }), "text-warning")}
          <ul className="selectable mt-0.5 space-y-0.5 text-[12.5px] text-muted">
            {(data.corrected ?? []).map((item: { id: string; when: string; then: string; corrected: string }) => (
              <li key={item.id}>
                {item.when} → {item.then}: {t(`run.verifyWhy.${item.corrected}`)}
              </li>
            ))}
          </ul>
          {(data.dropped ?? []).length > 0 && line(t("activity.verificationDropped", { count: data.dropped.length }))}
        </div>
      );
    case "returned":
      return (
        <div>
          {line(data.by ? t("activity.returnedBy", { client: data.by, round: data.round + 1 }) : t("activity.returned", { round: data.round + 1 }), "font-medium")}
          <p className="selectable mt-0.5 text-[12.5px] text-muted">{data.instructions}</p>
        </div>
      );
    case "services":
      return line(t("activity.services", { names: (data.services ?? []).map((service: { name: string }) => service.name).join(", ") }), "text-subtle");
    case "servicesLost":
      return line(t("activity.servicesLost", { error: data.error }), "text-danger");
    case "decision":
      return (
        <div>
          {line(data.answer === "approve" ? t("activity.decisionApprove") : t("activity.decisionReject"), "font-medium")}
          {data.text && <p className="selectable mt-0.5 text-[12.5px] text-muted">{t("activity.decisionNote", { text: data.text })}</p>}
        </div>
      );
    case "checkResult":
      return (
        <div>
          {line(t(data.kind === "setup" ? "activity.setupResult" : "activity.checkResult", { code: data.exitCode ?? "?" }), data.exitCode === 0 ? "text-success" : "text-danger")}
          {data.leftWorkspaceDirty && line(t("activity.dirty"), "text-warning")}
        </div>
      );
    case "delivered":
      return line(t("activity.delivered", { account: data.account, url: data.url }), "font-medium");
    case "init":
      return line(t("activity.init", { model: data.model }), "text-subtle");
    case "apiRetry":
      return line(t("activity.apiRetry", { attempt: data.attempt ?? "?" }), "text-warning");
    case "permissionDenied":
      return line(t("activity.permissionDenied", { tool: data.tool ?? "?" }), "text-warning");
    case "rateLimit":
      return data.status === "allowed" ? null : line(t("activity.rateLimit", { status: data.status }), "text-warning");
    case "result":
      return line(t("activity.result"), "text-subtle");
    case "finished":
      return line(t("activity.finished", { state: t(`states.${corrected && data.state === "failed" ? "corrected" : data.state}`) }), "font-medium");
    case "stalled":
      return line(t("activity.stalled"), "text-warning");
    case "stallCleared":
      return line(t("activity.stallCleared"));
    case "stopRequested":
      return line(t("activity.stopRequested"));
    case "waitLonger":
      return line(t("activity.waitLonger"));
    case "commandKilled":
      return line(t("activity.commandKilled", { command: data.command }));
    case "abandoned":
      return line(data.by ? t("activity.abandonedBy", { client: data.by }) : t("activity.abandoned"));
    // Only an assistant's retry leaves one, so the person sees it wasn't them.
    case "retryRequested":
      return data.by ? line(t("activity.retryRequested", { client: data.by })) : null;
    case "recovered":
      return line(t("activity.recovered"), "text-subtle");
    case "snapshotFailed":
      return line(t("activity.snapshotFailed", { error: data.error }), "text-danger");
    case "brainMap":
      return line(t("activity.brainMap", { kept: data.kept, dropped: data.dropped }), data.dropped > 0 ? "text-warning" : undefined);
    case "brainProposals":
      return line(t("activity.brainProposals", { count: data.count }));
    case "brainNote":
      return line(t(`activity.brainNote_${data.state}`, { text: data.text }));
    case "brainFailed":
      return line(t("activity.brainFailed", { error: data.error }), "text-danger");
    default:
      return null;
  }
}

/** `corrected` is an attempt that ended failed, and Keel went straight on to have it fixed: see `shownState`. */
export function ActivityTab({ events, role, corrected = false }: { events: AuditEvent[]; role: Attempt["role"]; corrected?: boolean }) {
  const { t } = useTranslation();
  const items = useMemo(() => activityFrom(events), [events]);
  if (items.length === 0) return <p className="text-[12.5px] text-subtle">{t("activity.empty")}</p>;
  return (
    <ol className="space-y-3">
      {items.map((item) => (
        <li key={item.key}>
          <ActivityEntry item={item} role={role} corrected={corrected} />
        </li>
      ))}
    </ol>
  );
}

export function CommandsTab({ commands }: { commands: CommandRun[] }) {
  const { t } = useTranslation();
  if (commands.length === 0) return <p className="text-[12.5px] text-subtle">{t("commands.empty")}</p>;
  return (
    <ol className="space-y-3">
      {commands.map((command) => (
        <li key={command.id} className="rounded-md border border-border p-3">
          <div className="flex items-start gap-2">
            {command.status === "ok" && <Check size={14} className="mt-0.5 shrink-0 text-success" />}
            {command.status === "failed" && <X size={14} className="mt-0.5 shrink-0 text-danger" />}
            {command.status === "unclear" && (command.failureMark ? <AlertTriangle size={14} className="mt-0.5 shrink-0 text-warning" /> : <CircleQuestionMark size={14} className="mt-0.5 shrink-0 text-subtle" />)}
            {command.status === "running" && <Loader2 size={14} className="mt-0.5 shrink-0 animate-spin text-accent" />}
            <code className="selectable min-w-0 flex-1 font-mono text-[12px] break-all">{command.command}</code>
            <span className={cx("shrink-0 text-[11.5px]", command.status === "failed" ? "text-danger" : command.failureMark ? "text-warning" : "text-subtle")}>
              {command.status === "failed" && command.exitCode != null ? t("commands.failedCode", { code: command.exitCode }) : t(`commands.${command.status}`)}
            </span>
          </div>
          <CommandNotes end={command} />
          <Output text={command.output} truncated={command.truncated} />
        </li>
      ))}
    </ol>
  );
}

export function OutputTab({ events, attempt }: { events: AuditEvent[]; attempt: Attempt }) {
  const { t } = useTranslation();
  const result = events.find((e) => e.kind === "checkResult")?.data;
  return (
    <div className="space-y-2">
      <code className="selectable block font-mono text-[12px] break-all">{attempt.prompt}</code>
      {!result ? (
        <p className="text-[12.5px] text-subtle">{t("activity.empty")}</p>
      ) : (
        <>
          <p className={cx("text-[12.5px]", result.exitCode === 0 ? "text-success" : "text-danger")}>{t(result.kind === "setup" ? "activity.setupResult" : "activity.checkResult", { code: result.exitCode ?? "?" })}</p>
          {skippedTests(attempt) > 0 && <p className="text-[12.5px] font-medium text-warning">{t("run.testsSkipped", { count: skippedTests(attempt) })}</p>}
          <Output text={[result.stdout, result.stderr].filter(Boolean).join("\n")} truncated={result.truncated} />
        </>
      )}
    </div>
  );
}

function DiffView({ diff }: { diff: string }) {
  return (
    <pre className="overflow-auto rounded bg-inset p-2 font-mono text-[11.5px] leading-relaxed">
      {diff.split("\n").map((line, index) => (
        <div
          // Lines of a fixed text: their position is their identity.
          key={index}
          className={cx(
            line.startsWith("+") && !line.startsWith("+++") && "bg-diff-add",
            line.startsWith("-") && !line.startsWith("---") && "bg-diff-del",
            line.startsWith("@@") && "text-accent",
            (line.startsWith("diff ") || line.startsWith("index ") || line.startsWith("+++") || line.startsWith("---")) && "text-subtle",
          )}
        >
          {line || " "}
        </div>
      ))}
    </pre>
  );
}

/** What this run proposes for the Brain. Nothing goes in until a person says so. */
function ProposalsCard({ runId, revision }: { runId: string; revision: number }) {
  const { t } = useTranslation();
  const refreshOverview = useApp((s) => s.refreshOverview);
  const navigate = useApp((s) => s.navigate);
  const [proposals, setProposals] = useState<BrainNote[]>([]);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => {
    void api.runProposals(runId).then(setProposals);
  }, [runId]);
  useEffect(load, [load, revision]);
  if (proposals.length === 0) return null;
  return (
    <Card className="p-4">
      <h2 className="text-[13px] font-semibold">{t("run.proposalsTitle")}</h2>
      <p className="mt-0.5 mb-2.5 text-[12px] text-muted">{t("run.proposalsHint")}</p>
      {error && (
        <p role="alert" className="mb-2 flex flex-wrap items-center gap-2 text-[12.5px] text-warning">
          {error}
          <Button variant="ghost" onClick={() => navigate({ name: "brain" })}>
            {t("run.openBrain")}
          </Button>
        </p>
      )}
      <ul className="space-y-2">
        {proposals.map((note) => (
          <ProposalRow
            key={note.seq}
            note={note}
            onDone={() => {
              setError(null);
              load();
              void refreshOverview();
            }}
            onError={setError}
          />
        ))}
      </ul>
    </Card>
  );
}

function ReviewVerdict({ review }: { review: Attempt }) {
  const { t } = useTranslation();
  if (!review.output) return <StateBadge state={review.state} label={t(`states.${review.state}`)} />;
  if ((review.output.unread ?? []).length > 0) return <span className="text-warning">{t("run.verdictUnread")}</span>;
  return <span className={review.output.verdict === "approved" ? "text-success" : "text-warning"}>{review.output.verdict === "approved" ? t("run.verdictApproved") : t("run.verdictChanges")}</span>;
}

function ApproverVerdict({ output, riskClass = "text-subtle" }: { output: NonNullable<Attempt["output"]>; riskClass?: string }) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-wrap items-baseline gap-2">
      <span className="font-medium">{t("run.approverTitle")}</span>
      <span className={output.decision === "approve" ? "text-success" : "text-warning"}>{output.overriddenByPerson ? t("run.decisionOverridden") : output.decision === "approve" ? t("run.decisionApprove") : t("run.decisionRework")}</span>
      <span className={riskClass}>
        {t("run.risk")}: {t(`run.risk${output.risk[0].toUpperCase()}${output.risk.slice(1)}`)}
      </span>
    </div>
  );
}

function ReviewsList({ attempts, round }: { attempts: Attempt[]; round: number }) {
  const { t } = useTranslation();
  const { reviewers, approver } = reviewsOfRound(attempts, round);
  const asked = specifyingAttempt(attempts);
  const breakable = breakableIds(asked ? specOf(asked) : null);
  // A person can ask for a review that was skipped, and then that's what became of it.
  return (
    <>
      <ul className="space-y-2">
        {latestOfEachStep(reviewers).map((review) => (
          <li key={review.id} className="text-[12.5px]">
            <div className="flex flex-wrap items-baseline gap-2">
              <span className="font-mono text-[12px] font-medium">{review.stepId}</span>
              <ReviewVerdict review={review} />
            </div>
            {review.state === "skipped" && review.reason && <p className="selectable mt-0.5 text-muted">{reasonText(t, review.reason)}</p>}
            {review.output?.summary && <p className="selectable mt-0.5 text-muted">{review.output.summary}</p>}
            {(review.output?.unread ?? []).length > 0 && <p className="selectable mt-0.5 text-warning">{t("run.unreadFiles", { files: review.output?.unread?.join(", ") })}</p>}
            {uncheckedOf(review.output).length > 0 && <p className="selectable mt-0.5 text-muted">{t("run.reviewUnchecked", { items: uncheckedOf(review.output).join("; ") })}</p>}
            {findingsOf(review.output, breakable).length > 0 && (
              <ul className="selectable mt-1 list-disc space-y-0.5 pl-4 text-muted">
                {findingsOf(review.output, breakable).map((finding, index) => (
                  // A review's list, in its order: two findings can say the same.
                  <li key={index}>
                    {finding.breaks && <span className="font-medium text-warning">{t("run.findingBreaks", { id: finding.breaks })} </span>}
                    {finding.where && <span className="font-mono text-[12px]">{finding.where} </span>}
                    {finding.text}
                  </li>
                ))}
              </ul>
            )}
          </li>
        ))}
      </ul>
      {approver?.output && (
        <div className="mt-3 border-t border-border pt-2.5 text-[12.5px]">
          <ApproverVerdict output={approver.output} />
          {approver.output.reasons && <p className="selectable mt-0.5 text-muted">{approver.output.reasons}</p>}
        </div>
      )}
    </>
  );
}

/** What every reviewer said in the current round, and what the approver made of it. */
function ReviewsCard({ attempts, round }: { attempts: Attempt[]; round: number }) {
  const { t } = useTranslation();
  const { reviewers, approver } = reviewsOfRound(attempts, round);
  if (reviewers.length === 0 && !approver) return null;
  return (
    <Card className="p-4">
      <h2 className="mb-2.5 flex items-baseline gap-2 text-[13px] font-semibold">
        {t("run.reviewsTitle")}
        {round > 0 && <span className="text-[12px] font-normal text-warning">{t("run.round", { number: round + 1 })}</span>}
      </h2>
      <ReviewsList attempts={attempts} round={round} />
    </Card>
  );
}

function ChangesTab({ attempt, revision }: { attempt: Attempt; revision: number }) {
  const { t } = useTranslation();
  const [diff, setDiff] = useState("");
  useEffect(() => {
    void api.attemptDiff(attempt.id).then(setDiff);
  }, [attempt.id, revision]);
  return (
    <div className="space-y-2">
      {attempt.shaBefore && (
        <p className="selectable font-mono text-[11.5px] text-subtle">
          {t("changes.before")}: {attempt.shaBefore.slice(0, 12)}
          {attempt.shaAfter && ` · ${t("changes.after")}: ${attempt.shaAfter.slice(0, 12)}`}
        </p>
      )}
      {!diff ? (
        <p className="text-[12.5px] text-subtle">{t("changes.empty")}</p>
      ) : (
        <DiffView diff={diff} />
      )}
    </div>
  );
}

function Detail({ label, children }: { label: string; children: React.ReactNode }) {
  const { t } = useTranslation();
  return (
    <div className="grid grid-cols-[170px_1fr] gap-3 border-t border-border py-1.5 first:border-t-0">
      <dt className="text-[12px] text-muted">{label}</dt>
      <dd className="selectable min-w-0 text-[12px] break-all">{children ?? t("details.none")}</dd>
    </div>
  );
}

function DetailsTab({ attempt, events }: { attempt: Attempt; events: AuditEvent[] }) {
  const { t } = useTranslation();
  const started = events.find((e) => e.kind === "started")?.data;
  const brainSource = (started?.systemPromptSources ?? []).find((source: { source: string }) => source.source === "brain");
  const num = (value: number | null) => (value == null ? null : formatNumber(value));
  const source = attempt.apiKeySource === "none" ? t("details.authSubscription") : attempt.apiKeySource;
  return (
    <div className="space-y-4">
      <dl>
        <Detail label={t("details.modelRequested")}>{attempt.modelRequested}</Detail>
        <Detail label={t("details.modelActual")}>{attempt.modelActual}</Detail>
        <Detail label={t("details.effort")}>{attempt.effort}</Detail>
        <Detail label={t("details.account")}>{attempt.account}</Detail>
        <Detail label={t("details.authSource")}>{source}</Detail>
        <Detail label={t("details.cliVersion")}>{attempt.cliVersion}</Detail>
        {attempt.kind === "agent" && <Detail label={t("details.tools")}>{attempt.tools === "build" ? t("newRun.toolsBuild") : t("newRun.toolsReadOnly")}</Detail>}
        <Detail label={t("details.started")}>{formatDateTime(attempt.startedAt)}</Detail>
        <Detail label={t("details.finished")}>{attempt.finishedAt && formatDateTime(attempt.finishedAt)}</Detail>
        <Detail label={t("details.duration")}>{attempt.durationMs != null && formatDuration(attempt.durationMs)}</Detail>
        <Detail label={t("details.turns")}>{num(attempt.turns)}</Detail>
        <Detail label={t("details.tokensIn")}>{num(attempt.inputTokens)}</Detail>
        <Detail label={t("details.tokensOut")}>{num(attempt.outputTokens)}</Detail>
        <Detail label={t("details.cost")}>{attempt.costUsd != null && formatCost(attempt.costUsd)}</Detail>
        <Detail label={t("details.session")}>{attempt.sessionId}</Detail>
        <Detail label={t("details.workspace")}>{attempt.workspace}</Detail>
        <Detail label={t("details.rawFile")}>
          <span>{attempt.rawPath}</span>
          <Button variant="ghost" className="ml-1" onClick={() => void api.revealRaw(attempt.id)}>
            {t("details.revealRaw")}
          </Button>
          <span className="mt-0.5 block break-words text-[11.5px] leading-snug text-subtle">{t("details.rawMasked")}</span>
        </Detail>
        <Detail label={t("details.rawHash")}>{attempt.rawSha256}</Detail>
      </dl>
      {started?.args && (
        <div>
          <h3 className="text-[12px] font-semibold text-muted">{t("details.command")}</h3>
          <pre className="mt-1.5 overflow-auto rounded bg-inset p-2 font-mono text-[11.5px] whitespace-pre-wrap">{`claude ${started.args.join(" ")}`}</pre>
        </div>
      )}
      <div>
        <h3 className="text-[12px] font-semibold text-muted">{t("details.settings")}</h3>
        <pre className="mt-1.5 overflow-auto rounded bg-inset p-2 font-mono text-[11.5px]">{JSON.stringify(attempt.settings, null, 2)}</pre>
      </div>
      {started?.systemPrompt && (
        <div>
          <h3 className="text-[12px] font-semibold text-muted">{t("details.systemPrompt")}</h3>
          {brainSource && <p className="mt-1 text-[12px] text-muted">{t("details.brainReceived", { notes: brainSource.notes.length, decisions: brainSource.decisions.length, leftOut: brainSource.leftOut })}</p>}
          <pre className="mt-1.5 max-h-72 overflow-auto rounded bg-inset p-2 font-mono text-[11.5px] whitespace-pre-wrap">{started.systemPrompt}</pre>
        </div>
      )}
    </div>
  );
}

/** Each round and retry of a step keeps its own record, so all of them stay one click away. */
function AttemptChoice({ node, attempt, onChoose }: { node: FlowNode; attempt: Attempt; onChoose: (attemptId: string) => void }) {
  const { t } = useTranslation();
  const choices = attemptChoices(node.attempts);
  if (choices.length < 2) return null;
  return (
    <Segmented<string>
      label={t("flow.whichAttempt")}
      value={attempt.id}
      onChange={onChoose}
      options={choices.map((choice) => ({
        value: choice.attempt.id,
        label: (
          <span className="flex items-center gap-1.5">
            {[choice.round !== null && t("run.round", { number: choice.round }), choice.number !== null && t("run.attemptTitle", { number: choice.number })].filter(Boolean).join(" · ")}
            <StateBadge state={shownState(choice.attempt, node.attempts)} label="" />
            <span className="sr-only">{t(`states.${shownState(choice.attempt, node.attempts)}`)}</span>
          </span>
        ),
      }))}
    />
  );
}

/** If any part of the record doesn't check out, that's what the person needs to know. */
function worstRawCheck(checks: RawCheck[]): RawCheck {
  for (const check of ["changed", "missing", "pending"] as const) {
    if (checks.includes(check)) return check;
  }
  return checks.length > 0 ? "intact" : "pending";
}

export function RunDetail({ runId }: { runId: string }) {
  const { t } = useTranslation();
  const revision = useApp((s) => s.revision);
  const [run, setRun] = useState<Run | null>(null);
  const [attempts, setAttempts] = useState<Attempt[]>([]);
  const [joint, setJoint] = useState<RunJoint | null>(null);
  const [jointOpen, setJointOpen] = useState(false);
  const view = useApp((s) => s.view);
  // A node, and which of its attempts if the person chose one. Until then the page follows the run.
  const [selectedNode, setSelectedNode] = useState<string | null>(null);
  const [selectedAttempt, setSelectedAttempt] = useState<string | null>(null);
  const [eventsByAttempt, setEventsByAttempt] = useState<Record<string, AuditEvent[]>>({});
  const [rawChecks, setRawChecks] = useState<Record<string, RawCheck>>({});
  const [tab, setTab] = useState<Tab>("activity");
  const [error, setError] = useState<string | null>(null);
  const lastSeq = useRef<Record<string, number>>({});

  const latest = focusAttempt(attempts);
  const accounts = accountsOf(attempts);
  const flow = useMemo(() => flowOf(parseSteps(run?.workflow) ?? [], attempts, run?.round), [run, attempts]);
  const shownNode = nodeWithKey(flow, selectedNode) ?? nodeOfAttempt(flow, latest);
  const attempt = selectedNode === null ? latest : (shownNode?.attempts.find((a) => a.id === selectedAttempt) ?? shownNode?.attempt);
  const [runDiff, setRunDiff] = useState<string | null>(null);

  const load = useCallback(async () => {
    // Whether it's in a change across projects is extra, and not finding out mustn't hide the run.
    const [nextRun, nextAttempts, nextJoint] = await Promise.all([api.runGet(runId), api.runAttempts(runId), api.runJoint(runId).catch(() => null)]);
    setRun(nextRun);
    setAttempts(nextAttempts);
    setJoint(nextJoint ?? null);
  }, [runId]);

  useEffect(() => {
    void load();
  }, [load, revision]);

  // A person opening a run may be wondering. Whether there's anything to ask, and the answer, are
  // the engine's: a change arrives if there's news, and a failure stays quiet.
  useEffect(() => {
    api.runLookAtPr(runId).catch(() => {});
  }, [runId]);

  // Audit events only ever grow, so only what's new is fetched, for every attempt of the run.
  useEffect(() => {
    for (const a of attempts) {
      const after = lastSeq.current[a.id] ?? 0;
      void api.attemptEvents(a.id, after).then((fetched) => {
        const fresh = fetched.filter((e) => e.seq > (lastSeq.current[a.id] ?? 0));
        if (fresh.length === 0) return;
        lastSeq.current[a.id] = fresh[fresh.length - 1].seq;
        setEventsByAttempt((current) => ({ ...current, [a.id]: [...(current[a.id] ?? []), ...fresh] }));
      });
      if (a.kind === "agent" || a.kind === "check" || a.kind === "setup") {
        void api.attemptRawCheck(a.id).then((check) => setRawChecks((current) => ({ ...current, [a.id]: check })));
      }
    }
  }, [attempts, revision]);

  const events = attempt ? (eventsByAttempt[attempt.id] ?? []) : [];
  const commands = commandsFrom(events);
  // The summary speaks for the whole run, not for the step selected below.
  const allCommands = useMemo(() => attempts.flatMap((a) => commandsFrom(eventsByAttempt[a.id] ?? [])), [attempts, eventsByAttempt]);
  const rawCheck = worstRawCheck(Object.values(rawChecks));
  const files = useMemo(() => filesChangedBy(attempts), [attempts]);
  const sizeDecision = useMemo(() => latestSizeDecision(Object.values(eventsByAttempt).flat()), [eventsByAttempt]);
  const showChanges = () => void api.runDiff(runId).then(setRunDiff);

  useRunCommands({
    showChanges: files.length > 0 ? showChanges : null,
    stopStep: latest && isActive(latest.state) ? () => void api.attemptStop(latest.id).catch((e) => setError(errorText(t, e))) : null,
  });

  if (!latest || !run) return null;

  // An assistant that closed the run says so, whichever step it closed.
  const closedBy = closedByAssistant(run, attempts, eventsByAttempt);

  const understood = attempt ? specOf(attempt) : null;
  const found = attempt ? verificationOf(attempt) : null;
  const kindTabs: Tab[] = !attempt ? [] : attempt.state === "skipped" ? ["activity", "prompt"] : attempt.kind === "agent" ? [...(understood ? (["understood"] as const) : []), ...(found ? (["found"] as const) : []), "activity", "commands", "changes", "prompt", "details"] : attempt.kind === "check" || attempt.kind === "setup" ? ["output", "activity", "details"] : ["activity", "details"];
  const shownTab: Tab = kindTabs.includes(tab) ? tab : kindTabs[0];
  // Of the attempt on show, which may be an earlier one than the node's own.
  const shownModel = attempt?.kind === "agent" ? (attempt.modelActual ?? attempt.modelRequested) : shownNode?.model;
  // While it runs there's little to say yet, so the flow comes first. The summary moves, not the flow, so the flow keeps its focus and scroll.
  const going = stillGoing(run, attempts);
  const outcome = (
    <>
      <Summary run={run} attempts={attempts} current={latest} commands={allCommands} rawCheck={rawCheck} closedBy={closedBy} onShowChanges={showChanges} />
      <ReviewsCard attempts={attempts} round={run.round} />
    </>
  );

  return (
    <>
      <main className="flex-1 space-y-4 overflow-y-auto p-5">
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1">
            <h1 className="selectable text-[17px] leading-snug font-semibold">{run?.title}</h1>
            <p className="mt-0.5 flex flex-wrap gap-x-3 text-[12px] text-subtle">
              {run?.prUrl && run.prState && <PrStatus state={run.prState} at={run.prStateAt} />}
              {isReadOnlyTask(run) && <span className="rounded border border-border px-1 text-[11px] text-muted">{t("run.readOnly")}</span>}
              {joint && (
                <button type="button" className="inline-flex items-center gap-1 text-accent hover:underline" onClick={() => setJointOpen(true)}>
                  <Layers size={11} />
                  {t("board.joint.agreeing")}
                  {joint.part && <span>· {partText(t, joint.part)}</span>}
                </button>
              )}
              {run?.startedBy && <span>{t("run.startedBy", { client: run.startedBy })}</span>}
              {run?.sourceIssue && <span>{t("run.fromIssue", { number: run.sourceIssue.number })}</span>}
              {accounts.claude && <span>{t("run.accountClaude", { account: accounts.claude })}</span>}
              {accounts.github && <span>{t("run.accountGithub", { account: accounts.github })}</span>}
              {run?.baseRef && <span>{t("run.base", { base: run.baseRef })}</span>}
            </p>
          </div>
          {run?.prUrl && (
            <Button onClick={() => void api.openGithubUrl(run.prUrl as string)}>
              <GitPullRequest size={14} />
              {t("run.pullRequest", { number: run.prNumber ?? "" })}
              <ExternalLink size={12} />
            </Button>
          )}
          <StateBadge state={latest.state} label={t(`states.${latest.state}`)} />
        </div>
        {error && (
          <p role="alert" className="text-[12.5px] text-danger">
            {error}
          </p>
        )}
        {latest.state === "waitingApproval" ? (
          <ApprovalCard key={latest.id} attempt={latest} run={run} attempts={attempts} files={files} commands={allCommands} rawCheck={rawCheck} sizeDecision={sizeDecision} joint={joint} onShowChanges={showChanges} onError={setError} />
        ) : latest.state === "waitingAnswers" ? (
          <QuestionsCard key={latest.id} attempt={latest} attempts={attempts} onError={setError} />
        ) : (
          <>
            <Actions attempt={latest} projectId={run.projectId} onError={setError} />
            {!going && outcome}
          </>
        )}
        <ProposalsCard runId={runId} revision={revision} />
        <Modal open={runDiff !== null} onOpenChange={(open) => !open && setRunDiff(null)} title={t("run.changesTitle")} className="w-[900px]">
          {runDiff ? <DiffView diff={runDiff} /> : <p className="text-[12.5px] text-subtle">{t("changes.empty")}</p>}
        </Modal>
        <Card className="p-3">
          <h2 className="px-1 pb-2 text-[12px] font-semibold text-muted">{t("run.stepsTitle")}</h2>
          <FlowDiagram
            flow={flow}
            label={t("run.stepsTitle")}
            selected={shownNode?.key ?? null}
            onSelect={(key) => {
              setSelectedNode(key);
              setSelectedAttempt(null);
            }}
          />
        </Card>
        <Card className="min-w-0 p-4">
          {shownNode && (
            <div className="mb-3 flex flex-wrap items-center gap-x-3 gap-y-2">
              <h2 className="min-w-0 truncate text-[13px] font-semibold">{shownNode.id}</h2>
              {shownModel && <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{shownModel}</span>}
              {attempt && <StateBadge state={shownState(attempt, attempts)} label={t(`states.${shownState(attempt, attempts)}`)} />}
              {attempt && (
                <div className="ml-auto max-w-full overflow-x-auto">
                  <AttemptChoice
                    node={shownNode}
                    attempt={attempt}
                    onChoose={(attemptId) => {
                      setSelectedNode(shownNode.key);
                      setSelectedAttempt(attemptId);
                    }}
                  />
                </div>
              )}
            </div>
          )}
          {attempt ? (
            <>
              <Segmented<Tab> arrows label={t("tabs.details")} value={shownTab} onChange={setTab} options={kindTabs.map((value) => ({ value, label: t(`tabs.${value}`) }))} />
              <div className="mt-4">
                {shownTab === "understood" && understood && <SpecView spec={understood} />}
                {shownTab === "found" && found && <VerificationView verification={found} />}
                {shownTab === "activity" && <ActivityTab events={events} role={attempt.role} corrected={shownState(attempt, attempts) === "corrected"} />}
                {shownTab === "commands" && <CommandsTab commands={commands} />}
                {shownTab === "changes" && <ChangesTab attempt={attempt} revision={revision} />}
                {shownTab === "output" && <OutputTab events={events} attempt={attempt} />}
                {shownTab === "prompt" && <pre className="overflow-auto rounded bg-inset p-3 font-mono text-[12px] leading-relaxed whitespace-pre-wrap">{attempt.prompt}</pre>}
                {shownTab === "details" && <DetailsTab attempt={attempt} events={events} />}
              </div>
            </>
          ) : (
            <p className="text-[12.5px] text-subtle">{t("flow.notStarted")}</p>
          )}
        </Card>
        {going && outcome}
      </main>
      {jointOpen && joint && <JointChangeDialog changeId={joint.changeId} backTo={view} onClose={() => setJointOpen(false)} />}
    </>
  );
}
