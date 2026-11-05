import { AlertTriangle, Check, ExternalLink, FileDiff, GitPullRequest, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type BrainNote, type Attempt, type AttemptState, type AuditEvent, isActive, type RawCheck, type Run } from "../lib/api";
import { activityFrom, type ActivityItem, commandsFrom, type CommandRun, explainingAttempts, filesChangedBy, flatSteps, focusAttempt, toolSummary, touchesWorkflows } from "../lib/audit";
import { formatCost, formatDateTime, formatDuration, formatNumber } from "../lib/format";
import { errorText, needsProjectSettings, needsSignIn, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass, Modal, Segmented, StateBadge } from "../ui/primitives";
import { ProposalRow } from "./BrainPage";
import { TopBar } from "./TopBar";

type Tab = "activity" | "commands" | "changes" | "prompt" | "details" | "output";

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
  rejected: "run.decideRejected",
  skipped: "run.decideActive",
  abandoned: "run.decideAbandoned",
};

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

/** The rule of the product: never just "finished". What it did, changed, proved, and what's next. */
function Summary({ run, attempts, current, commands, rawCheck, onShowChanges }: { run: Run | null; attempts: Attempt[]; current: Attempt; commands: CommandRun[]; rawCheck: RawCheck; onShowChanges: () => void }) {
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
  // A step that's done says nothing about the run while others are still to come.
  const between = !run?.finishedAt && (current.state === "succeeded" || current.state === "skipped");
  const decision = mapper && current.state === "succeeded" ? "run.decideMap" : run?.prUrl ? "run.decideDelivered" : current.reason?.code === "reworkLimit" ? "run.decideReworkLimit" : between ? "run.decideActive" : DECISIONS[current.state];
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
        {!mapper && explaining.length === 0 && <p className="selectable whitespace-pre-wrap">{(anyActive ? t("run.noResultYet") : current.state === "succeeded" ? t("run.noResult") : t("run.noResultEnded"))}</p>}
        {explaining.map((attempt) => (
          <div key={attempt.id} className="mb-3 last:mb-0">
            {attempt.round > 0 && <p className="mb-1 text-[12px] font-medium text-warning">{t("run.round", { number: attempt.round + 1 })}</p>}
            <p className="selectable whitespace-pre-wrap">{attempt.resultText}</p>
          </div>
        ))}
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
          <p className="text-muted">{commands.length === 0 ? t("run.noCommands") : t("run.commandsPassed", { passed, total: commands.length })}</p>
          <p className={cx("mt-0.5", rawCheck === "changed" || rawCheck === "missing" ? "font-medium text-danger" : "text-muted")}>
            {t(`run.audit${rawCheck[0].toUpperCase()}${rawCheck.slice(1)}`)}
          </p>
        </SummaryBlock>
        <SummaryBlock title={t("run.whatToDecide")}>
          <p>{t(decision)}</p>
        </SummaryBlock>
      </div>
    </Card>
  );
}

/** Rejecting is the easy path: it has the focus, Enter never approves, and approve arms after a moment. */
function ApprovalCard({ attempt, files, onError }: { attempt: Attempt; files: string[]; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const [note, setNote] = useState("");
  const [armed, setArmed] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    const timer = setTimeout(() => setArmed(true), 800);
    return () => clearTimeout(timer);
  }, []);
  const answer = (approve: boolean) => {
    setBusy(true);
    api.approvalAnswer(attempt.id, approve, note).catch((e) => {
      setBusy(false);
      onError(errorText(t, e));
    });
  };
  return (
    <Card role="alert" className="border-warning/40 p-4">
      <p className="text-[14px] font-semibold">{t("run.approvalTitle")}</p>
      <p className="mt-1 text-[12.5px] text-muted">{t("run.approvalBody")}</p>
      {touchesWorkflows(files) && (
        <p className="mt-2 flex gap-1.5 rounded-md border border-warning/40 bg-inset p-2 text-[12.5px] font-medium text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          {t("run.workflowWarning")}
        </p>
      )}
      <div className="mt-3">
        <Field label={t("run.approvalNote")}>
          <textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} placeholder={t("run.approvalNotePlaceholder")} className={`${inputClass} resize-y`} />
        </Field>
      </div>
      <div className="mt-3 flex gap-2">
        <Button autoFocus disabled={busy} onClick={() => answer(false)}>
          {t("run.reject")}
        </Button>
        <Button
          variant="primary"
          disabled={!armed || busy}
          onKeyDown={(e) => {
            if (e.key === "Enter") e.preventDefault();
          }}
          onClick={() => answer(true)}
        >
          <GitPullRequest size={14} />
          {t("run.approve")}
        </Button>
      </div>
    </Card>
  );
}

function Actions({ attempt, files, onError }: { attempt: Attempt; files: string[]; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  const act = (run: () => Promise<unknown>) => run().catch((e) => onError(errorText(t, e)));
  if (attempt.state === "waitingApproval") {
    return <ApprovalCard attempt={attempt} files={files} onError={onError} />;
  }
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
  if (attempt.state === "succeeded" || attempt.state === "abandoned" || attempt.state === "rejected" || attempt.state === "skipped") return null;
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

function ActivityEntry({ item }: { item: ActivityItem }) {
  const { t } = useTranslation();
  const { event, result } = item;
  const data = event.data;
  const line = (text: string, tone = "text-muted") => <p className={cx("text-[12.5px]", tone)}>{text}</p>;
  switch (item.kind) {
    case "text":
      return <p className="selectable text-[13px] leading-relaxed whitespace-pre-wrap">{data.text}</p>;
    case "toolCall": {
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
      return line(t("activity.skipped"), "text-subtle");
    case "rework":
      return line(t("activity.rework", { round: data.round }), "text-warning");
    case "reworkLimit":
      return line(t("activity.reworkLimit"), "text-warning");
    case "reworkLimitAnswer":
      return line(data.anotherRound ? t("activity.reworkLimitAnswerRound") : t("activity.reworkLimitAnswerApproval"));
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
      return line(t("activity.abandoned"));
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

function ActivityTab({ events }: { events: AuditEvent[] }) {
  const { t } = useTranslation();
  const items = useMemo(() => activityFrom(events), [events]);
  if (items.length === 0) return <p className="text-[12.5px] text-subtle">{t("activity.empty")}</p>;
  return (
    <ol className="space-y-3">
      {items.map((item) => (
        <li key={item.key}>
          <ActivityEntry item={item} />
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

/** What every reviewer said in the current round, and what the approver made of it. */
function ReviewsCard({ attempts, round }: { attempts: Attempt[]; round: number }) {
  const { t } = useTranslation();
  const current = attempts.filter((a) => a.round === round);
  const reviewers = current.filter((a) => a.role === "reviewer");
  const approver = [...current].reverse().find((a) => a.role === "approver" && a.output);
  if (reviewers.length === 0 && !approver) return null;
  return (
    <Card className="p-4">
      <h2 className="flex items-baseline gap-2 text-[13px] font-semibold">
        {t("run.reviewsTitle")}
        {round > 0 && <span className="text-[12px] font-normal text-warning">{t("run.round", { number: round + 1 })}</span>}
      </h2>
      <ul className="mt-2.5 space-y-2">
        {reviewers.map((review) => (
          <li key={review.id} className="text-[12.5px]">
            <div className="flex flex-wrap items-baseline gap-2">
              <span className="font-mono text-[12px] font-medium">{review.stepId}</span>
              {review.output ? (
                <span className={review.output.verdict === "approved" ? "text-success" : "text-warning"}>{review.output.verdict === "approved" ? t("run.verdictApproved") : t("run.verdictChanges")}</span>
              ) : (
                <StateBadge state={review.state} label={t(`states.${review.state}`)} />
              )}
            </div>
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
          <div className="flex flex-wrap items-baseline gap-2">
            <span className="font-medium">{t("run.approverTitle")}</span>
            <span className={approver.output.decision === "approve" ? "text-success" : "text-warning"}>
              {approver.output.overriddenByPerson ? t("run.decisionOverridden") : approver.output.decision === "approve" ? t("run.decisionApprove") : t("run.decisionRework")}
            </span>
            <span className="text-subtle">
              {t("run.risk")}: {t(`run.risk${approver.output.risk[0].toUpperCase()}${approver.output.risk.slice(1)}`)}
            </span>
          </div>
          {approver.output.reasons && <p className="selectable mt-0.5 text-muted">{approver.output.reasons}</p>}
        </div>
      )}
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
  const [selected, setSelected] = useState<string | null>(null);
  const [eventsByAttempt, setEventsByAttempt] = useState<Record<string, AuditEvent[]>>({});
  const [rawChecks, setRawChecks] = useState<Record<string, RawCheck>>({});
  const [tab, setTab] = useState<Tab>("activity");
  const [error, setError] = useState<string | null>(null);
  const lastSeq = useRef<Record<string, number>>({});

  const latest = focusAttempt(attempts);
  const attempt = attempts.find((a) => a.id === selected) ?? latest;
  const [runDiff, setRunDiff] = useState<string | null>(null);

  const load = useCallback(async () => {
    const [nextRun, nextAttempts] = await Promise.all([api.runGet(runId), api.runAttempts(runId)]);
    setRun(nextRun);
    setAttempts(nextAttempts);
  }, [runId]);

  useEffect(() => {
    void load();
  }, [load, revision]);

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

  if (!attempt || !latest) {
    return <TopBar back />;
  }

  // A step can have run several times. The list shows each attempt, then what's still ahead.
  const pending = flatSteps(run?.workflow ?? []).filter((step) => !attempts.some((a) => a.stepId === step.id));
  const kindTabs: Tab[] = attempt.state === "skipped" ? ["activity", "prompt"] : attempt.kind === "agent" ? ["activity", "commands", "changes", "prompt", "details"] : attempt.kind === "check" || attempt.kind === "setup" ? ["output", "activity", "details"] : ["activity", "details"];
  const shownTab: Tab = kindTabs.includes(tab) ? tab : kindTabs[0];

  return (
    <>
      <TopBar back />
      <main className="flex-1 space-y-4 overflow-y-auto p-5">
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1">
            <h1 className="selectable text-[17px] leading-snug font-semibold">{run?.title}</h1>
            <p className="mt-0.5 flex flex-wrap gap-x-3 text-[12px] text-subtle">
              {run?.sourceIssue && <span>{t("run.fromIssue", { number: run.sourceIssue.number })}</span>}
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
        <Actions attempt={latest} files={files} onError={setError} />
        <Summary run={run} attempts={attempts} current={latest} commands={allCommands} rawCheck={rawCheck} onShowChanges={() => void api.runDiff(runId).then(setRunDiff)} />
        <ReviewsCard attempts={attempts} round={run?.round ?? 0} />
        <ProposalsCard runId={runId} revision={revision} />
        <Modal open={runDiff !== null} onOpenChange={(open) => !open && setRunDiff(null)} title={t("run.changesTitle")} className="w-[900px]">
          {runDiff ? <DiffView diff={runDiff} /> : <p className="text-[12.5px] text-subtle">{t("changes.empty")}</p>}
        </Modal>
        <div className="grid gap-4 lg:grid-cols-[210px_1fr]">
          <Card className="self-start p-2">
            <h2 className="px-2 pt-1 pb-1.5 text-[12px] font-semibold text-muted">{t("run.stepsTitle")}</h2>
            <ul className="space-y-0.5">
              {attempts.map((a) => (
                <li key={a.id}>
                  <button type="button" onClick={() => setSelected(a.id)} className={cx("w-full rounded-md px-2 py-1.5 text-left hover:bg-hover", a.id === attempt.id && "bg-active")}>
                    <span className="block truncate text-[12.5px] font-medium">{a.stepId}</span>
                    <span className="mt-0.5 flex items-center justify-between gap-2">
                      <span className="text-[11px] text-subtle">{a.round > 0 ? t("run.round", { number: a.round + 1 }) : a.number > 1 && t("run.attemptTitle", { number: a.number })}</span>
                      <StateBadge state={a.state} label={t(`states.${a.state}`)} />
                    </span>
                    {a.kind === "agent" && <span className="mt-0.5 block truncate font-mono text-[11px] text-subtle">{a.modelActual ?? a.modelRequested}</span>}
                  </button>
                </li>
              ))}
              {pending.map((step) => (
                <li key={step.id} className="px-2 py-1.5 opacity-60">
                  <span className="block truncate text-[12.5px] font-medium">{step.id}</span>
                  <span className="text-[11px] text-subtle">{t("run.pending")}</span>
                </li>
              ))}
            </ul>
          </Card>
          <Card className="min-w-0 p-4">
            <Segmented<Tab> label={t("tabs.details")} value={shownTab} onChange={setTab} options={kindTabs.map((value) => ({ value, label: t(`tabs.${value}`) }))} />
            <div className="mt-4">
              {shownTab === "activity" && <ActivityTab events={events} />}
              {shownTab === "commands" && <CommandsTab commands={commands} />}
              {shownTab === "changes" && <ChangesTab attempt={attempt} revision={revision} />}
              {shownTab === "output" && <OutputTab events={events} attempt={attempt} />}
              {shownTab === "prompt" && <pre className="overflow-auto rounded bg-inset p-3 font-mono text-[12px] leading-relaxed whitespace-pre-wrap">{attempt.prompt}</pre>}
              {shownTab === "details" && <DetailsTab attempt={attempt} events={events} />}
            </div>
          </Card>
        </div>
      </main>
    </>
  );
}
