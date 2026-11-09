import { AlertTriangle, Check, ExternalLink, FileDiff, GitPullRequest, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { keymapHeld, watchKeymapHold } from "../commands/keymapHold";
import { useRunCommands } from "../commands/pageCommands";
import { api, type BrainNote, type Attempt, type AttemptState, type AuditEvent, type CheckedCriterion, isActive, type PrState, type QuestionAnswer, type RawCheck, type Run, type SpecDecided, type SpecOutput, type SpecQuestion, type VerificationOutput } from "../lib/api";
import { accountsOf, activityFrom, type ActivityItem, approvalOutcome, askedText, commandsFrom, type CommandRun, explainingAttempts, filesChangedBy, focusAttempt, gist, latestOfEachStep, reviewsOfRound, roundsAnswered, sentBackByAssistant, specifyingAttempt, specOf, toolSummary, touchesWorkflows, stillGoing, verificationCounts, verificationInRound, verificationLine, verificationOf } from "../lib/audit";
import { attemptChoices, canSendBack, type FlowNode, flowOf, isReadOnlyTask, nodeOfAttempt, nodeWithKey, parseSteps } from "../lib/flow";
import { formatCost, formatDateTime, formatDuration, formatNumber } from "../lib/format";
import { errorText, needsProjectSettings, needsSignIn, reasonText } from "../lib/reason";
import { allRecommend, answerFrom, differsFromRecommended, isAnswered, needsText, type QuestionPick, recommendedAnswers, recommendedOption, startingPick } from "../lib/questions";
import { latestSizeDecision, type SizeDecision, skippedAsSmall, skippedSteps, whyNotSmallText } from "../lib/size";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass, Modal, Segmented, StateBadge } from "../ui/primitives";
import { ProposalRow } from "./BrainPage";
import { FlowDiagram } from "./FlowDiagram";

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

/** The rule of the product: never just "finished". What it did, changed, proved, and what's next. */
function Summary({ run, attempts, current, commands, rawCheck, closedBy, onShowChanges }: { run: Run | null; attempts: Attempt[]; current: Attempt; commands: CommandRun[]; rawCheck: RawCheck; closedBy: string | null; onShowChanges: () => void }) {
  const { t } = useTranslation();
  const files = filesChangedBy(attempts);
  const explaining = explainingAttempts(attempts);
  const navigate = useApp((s) => s.navigate);
  const mapper = attempts.find((a) => a.role === "mapper" && a.output);
  const passed = commands.filter((c) => c.status === "ok").length;
  const checks = attempts.filter((a) => a.kind === "check");
  const latestChecks = checks.filter((a, i) => !checks.slice(i + 1).some((b) => b.stepId === a.stepId));
  const checksPassed = latestChecks.filter((a) => a.state === "succeeded").length;
  const anyActive = attempts.some((a) => isActive(a.state));
  const verification = verificationInRound(attempts, run?.round ?? 0);
  const skipped = skippedSteps(attempts);
  // A step that's done says nothing about the run while others are still to come.
  const between = !run?.finishedAt && (current.state === "succeeded" || current.state === "skipped");
  // A run that only had to work out what's asked has that to say for itself, and no worker to say it.
  const onlyUnderstood = !mapper && explaining.length === 0 && current.state === "succeeded" ? specOf(current) : null;
  const decision = mapper && current.state === "succeeded" ? "run.decideMap" : run?.prUrl ? PR_DECISIONS[run.prState ?? "draft"] : current.reason?.code === "reworkLimit" ? "run.decideReworkLimit" : between ? "run.decideActive" : DECISIONS[current.state];
  return (
    <Card className="grid gap-5 p-4 md:grid-cols-2">
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
        {!mapper && !onlyUnderstood && explaining.length === 0 && <p className="selectable whitespace-pre-wrap">{(anyActive ? t("run.noResultYet") : current.state === "succeeded" ? t("run.noResult") : t("run.noResultEnded"))}</p>}
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
          {verification && <VerificationProof verification={verification} />}
          {skipped.length > 0 && <p className="text-muted">{t("fits.summary", { names: skipped.map((step) => step.stepId).join(", ") })}</p>}
          <p className="text-muted">{commands.length === 0 ? t("run.noCommands") : t("run.commandsPassed", { passed, total: commands.length })}</p>
          <p className={cx("mt-0.5", rawCheck === "changed" || rawCheck === "missing" ? "font-medium text-danger" : "text-muted")}>
            {t(auditKey(rawCheck))}
          </p>
        </SummaryBlock>
        <SummaryBlock title={t("run.whatToDecide")}>
          <p>{decision === "run.decideAbandoned" && closedBy ? t("run.decideAbandonedBy", { client: closedBy }) : t(decision)}</p>
        </SummaryBlock>
      </div>
    </Card>
  );
}

function ApprovalRow({ label, note, action, children }: { label: string; note?: string; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="grid grid-cols-[116px_1fr] gap-x-3 border-t border-border py-2 first:border-t-0 first:pt-0">
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
 * One line that opens to the list. The green check only goes with all of it met: anything else says
 * what it is, and not being able to verify something is a normal thing to find, not an alarm.
 */
function VerificationProof({ verification }: { verification: VerificationOutput }) {
  const { t } = useTranslation();
  const { met, unmet, total } = verificationCounts(verification);
  return (
    <details className="my-0.5">
      <summary className="cursor-pointer">
        {total > 0 && met === total && <Check size={13} className="mr-1 inline align-[-2px] text-success" />}
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

function ApprovalProof({ attempts, round, commands, rawCheck, reviewed }: { attempts: Attempt[]; round: number; commands: CommandRun[]; rawCheck: RawCheck; reviewed: boolean }) {
  const { t } = useTranslation();
  const checks = latestOfEachStep(attempts.filter((a) => a.kind === "check"));
  const passed = commands.filter((c) => c.status === "ok").length;
  const verification = verificationInRound(attempts, round);
  return (
    <>
      {checks.map((check) =>
        check.state === "succeeded" ? (
          <p key={check.id} className="selectable flex gap-1.5">
            <Check size={13} className="mt-1 shrink-0 text-success" />
            <span className="min-w-0 break-words">{t("run.approvalCheckPassed", { command: check.prompt })}</span>
          </p>
        ) : (
          <p key={check.id} className="selectable font-medium break-words text-danger">
            {t("run.approvalCheckFailed", { command: check.prompt })}
          </p>
        ),
      )}
      {checks.length === 0 && (
        <p>
          {t("run.approvalNoCheck")}
          {reviewed && ` ${t("run.approvalReviewsOnlyRead")}`}
        </p>
      )}
      {verification && <VerificationProof verification={verification} />}
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

function SpecList({ title, items }: { title: string; items: React.ReactNode[] }) {
  if (items.length === 0) return null;
  return (
    <div>
      <h4 className="text-[12px] font-semibold text-muted">{title}</h4>
      <ul className="selectable mt-1 list-disc space-y-0.5 pl-4 break-words">
        {items.map((item, index) => (
          // Lines of a fixed text: their position is their identity.
          <li key={index}>{item}</li>
        ))}
      </ul>
    </div>
  );
}

/** What a specifier understood, as text to read. */
function SpecView({ spec }: { spec: SpecOutput }) {
  const { t } = useTranslation();
  const kinds = { adds: "run.specAdds", changes: "run.specAlters", removes: "run.specRemoves" };
  return (
    <div className="space-y-3 text-[13px] leading-relaxed">
      <p className="selectable break-words">{spec.summary}</p>
      <SpecList
        title={t("run.specHolds")}
        items={spec.criteria.map((criterion) => (
          <>
            <span className="font-mono text-[12px] text-muted">{criterion.id}</span> {criterion.when} → {criterion.then}
          </>
        ))}
      />
      <SpecList title={t("run.specChanges")} items={spec.changes.map((change) => `${t(kinds[change.kind])}: ${change.text}`)} />
      <SpecList title={t("run.specLimits")} items={spec.limits} />
      <SpecList
        title={t("run.decidedTitle")}
        items={spec.decided.map((decided) => (
          <>
            {decided.text}
            {decided.taken === "person" && <span className="block text-[11.5px] text-subtle">{t("run.decidedYouChose")}</span>}
            {decided.taken === "roundsLimit" && <span className="block text-[11.5px] text-warning">{t("run.decidedKeelTook")}</span>}
          </>
        ))}
      />
      <SpecList title={t("run.specAsked")} items={spec.questions.map((question) => `${question.text} ${question.options.map((option) => option.label).join(" / ")}`)} />
      <SpecList title={t("run.specAnswered")} items={(spec.answered ?? []).map((answer) => `${answer.asked} ${answer.answer}`)} />
    </div>
  );
}

/** What it settled by itself. Each line can go back to whoever does the work, without starting over. */
function DecidedList({ attemptId, decided, canChange, armed, busy, onSend }: { attemptId: string; decided: SpecDecided[]; canChange: boolean; armed: boolean; busy: boolean; onSend: (decided: string, instruction: string, onDone: () => void) => void }) {
  const { t } = useTranslation();
  const [changing, setChanging] = useState<number | null>(null);
  const draftKey = `${attemptId}:change:${changing}`;
  const instruction = useApp((s) => s.approvalDrafts[draftKey] ?? "");
  const setDraft = useApp((s) => s.setApprovalDraft);
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
    </ul>
  );
}

/**
 * What's being decided, in one place, so nobody has to scroll to find out. Rejecting takes a reason,
 * so the focus starts on the note, where Enter only adds a line. Enter never answers, and approve
 * arms after a moment.
 */
export function ApprovalCard({ attempt, run, attempts, files, commands, rawCheck, sizeDecision = null, onShowChanges, onError }: { attempt: Attempt; run: Run; attempts: Attempt[]; files: string[]; commands: CommandRun[]; rawCheck: RawCheck; sizeDecision?: SizeDecision | null; onShowChanges: () => void; onError: (message: string) => void }) {
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
  // Right after working out what's asked, nobody has done anything yet. Saying it finished without
  // an explanation would read as something gone wrong.
  const nothingToExplain = asked !== null && !attempts.some((a) => a.kind === "agent" && a.role === "worker");
  const { reviewers, approver } = reviewsOfRound(attempts, run.round);
  const reviewed = reviewers.some((review) => review.state !== "skipped") || approver != null;
  const skippedForSize = skippedSteps(attempts);
  const hiddenFiles = files.length - SHOWN_FILES;
  const sentBack = sentBackByAssistant(attempts, run.round);

  return (
    // Short enough for its buttons to show without scrolling: the window less the top bar (38), the
    // status bar (24), and the run's title and the page's padding above and below (102).
    <Card role="alert" className="flex max-h-[calc(100vh-164px)] flex-col overflow-y-auto border-warning/40 p-4">
      <p className="text-[14px] font-semibold">{t(empty ? "run.approvalEmptyTitle" : outcome === "delivers" ? "run.approvalTitle" : "run.approvalContinueTitle")}</p>
      <p className="mt-1 mb-3 text-[12.5px] text-muted">{t(empty ? "run.approvalEmptyBody" : "run.approvalBody")}</p>
      {/* What's being approved never shrinks away: in a short window the card scrolls instead. */}
      <dl className="max-h-[50vh] min-h-24 shrink-0 overflow-y-auto">
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
        {spec && spec.decided.length > 0 && (
          <ApprovalRow label={t("run.decidedTitle")} note={t("run.generated")}>
            <DecidedList attemptId={attempt.id} decided={spec.decided} canChange={!empty && canSendBack(run.workflow, attempt.stepId)} armed={armed} busy={busy} onSend={sendBack} />
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
        <ApprovalRow label={t("run.whatProves")}>
          <ApprovalProof attempts={attempts} round={run.round} commands={commands} rawCheck={rawCheck} reviewed={reviewed} />
        </ApprovalRow>
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
                {t(outcome === "delivers" ? "run.approve" : "run.approveContinue")}
              </Button>
              <p className="mt-1 text-[11.5px] text-muted">{t(outcome === "delivers" ? "run.approvalApproveHint" : outcome === "deliversLater" ? "run.approvalLaterHint" : "run.approvalNoPushHint")}</p>
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

function Actions({ attempt, onError }: { attempt: Attempt; onError: (message: string) => void }) {
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
        {needsProjectSettings(attempt.reason) && <Button onClick={() => navigate({ name: "settings" })}>{t("newRun.openSettings")}</Button>}
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

function ActivityEntry({ item, role }: { item: ActivityItem; role: Attempt["role"] }) {
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
      return line(t("activity.specCorrected"), "text-warning");
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
      return line(t("activity.finished", { state: t(`states.${data.state}`) }), "font-medium");
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

function ActivityTab({ events, role }: { events: AuditEvent[]; role: Attempt["role"] }) {
  const { t } = useTranslation();
  const items = useMemo(() => activityFrom(events), [events]);
  if (items.length === 0) return <p className="text-[12.5px] text-subtle">{t("activity.empty")}</p>;
  return (
    <ol className="space-y-3">
      {items.map((item) => (
        <li key={item.key}>
          <ActivityEntry item={item} role={role} />
        </li>
      ))}
    </ol>
  );
}

function CommandsTab({ commands }: { commands: CommandRun[] }) {
  const { t } = useTranslation();
  if (commands.length === 0) return <p className="text-[12.5px] text-subtle">{t("commands.empty")}</p>;
  return (
    <ol className="space-y-3">
      {commands.map((command) => (
        <li key={command.id} className="rounded-md border border-border p-3">
          <div className="flex items-start gap-2">
            {command.status === "ok" && <Check size={14} className="mt-0.5 shrink-0 text-success" />}
            {command.status === "failed" && <X size={14} className="mt-0.5 shrink-0 text-danger" />}
            {command.status === "running" && <Loader2 size={14} className="mt-0.5 shrink-0 animate-spin text-accent" />}
            <code className="selectable min-w-0 flex-1 font-mono text-[12px] break-all">{command.command}</code>
            <span className={cx("shrink-0 text-[11.5px]", command.status === "failed" ? "text-danger" : "text-subtle")}>
              {command.status === "failed" && command.exitCode != null ? t("commands.failedCode", { code: command.exitCode }) : t(`commands.${command.status}`)}
            </span>
          </div>
          <Output text={command.output} truncated={command.truncated} />
        </li>
      ))}
    </ol>
  );
}

function OutputTab({ events, attempt }: { events: AuditEvent[]; attempt: Attempt }) {
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
            {(review.output?.findings ?? []).length > 0 && (
              <ul className="selectable mt-1 list-disc space-y-0.5 pl-4 text-muted">
                {review.output?.findings.map((finding, index) => (
                  <li key={index}>{finding}</li>
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
            <StateBadge state={choice.attempt.state} label="" />
            <span className="sr-only">{t(`states.${choice.attempt.state}`)}</span>
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
    const [nextRun, nextAttempts] = await Promise.all([api.runGet(runId), api.runAttempts(runId)]);
    setRun(nextRun);
    setAttempts(nextAttempts);
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

  // An assistant that closed the run says so on the step it closed.
  const closedBy: string | null = latest.state === "abandoned" ? ((eventsByAttempt[latest.id] ?? []).find((e) => e.kind === "abandoned")?.data.by ?? null) : null;

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
          <ApprovalCard key={latest.id} attempt={latest} run={run} attempts={attempts} files={files} commands={allCommands} rawCheck={rawCheck} sizeDecision={sizeDecision} onShowChanges={showChanges} onError={setError} />
        ) : latest.state === "waitingAnswers" ? (
          <QuestionsCard key={latest.id} attempt={latest} attempts={attempts} onError={setError} />
        ) : (
          <>
            <Actions attempt={latest} onError={setError} />
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
              {attempt && <StateBadge state={attempt.state} label={t(`states.${attempt.state}`)} />}
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
                {shownTab === "activity" && <ActivityTab events={events} role={attempt.role} />}
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
    </>
  );
}
