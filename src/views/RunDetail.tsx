import { AlertTriangle, Check, FileDiff, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type Attempt, type AttemptState, type AuditEvent, isActive, type RawCheck } from "../lib/api";
import { activityFrom, type ActivityItem, commandsFrom, type CommandRun, toolSummary } from "../lib/audit";
import { formatCost, formatDateTime, formatDuration, formatNumber } from "../lib/format";
import { errorText, needsSignIn, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Segmented, StateBadge } from "../ui/primitives";
import { TopBar } from "./TopBar";

type Tab = "activity" | "commands" | "changes" | "prompt" | "details";

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
function Summary({ attempt, commands, rawCheck, onShowChanges }: { attempt: Attempt; commands: CommandRun[]; rawCheck: RawCheck; onShowChanges: () => void }) {
  const { t } = useTranslation();
  const files = attempt.filesChanged ?? [];
  const passed = commands.filter((c) => c.status === "ok").length;
  const active = isActive(attempt.state);
  return (
    <Card className="grid gap-5 p-4 md:grid-cols-2">
      <SummaryBlock title={t("run.whatItDid")} note={attempt.resultText ? t("run.generated") : undefined}>
        <p className="selectable whitespace-pre-wrap">{attempt.resultText || (active ? t("run.noResultYet") : attempt.state === "succeeded" ? t("run.noResult") : t("run.noResultEnded"))}</p>
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
          <p className="text-muted">{commands.length === 0 ? t("run.noCommands") : t("run.commandsPassed", { passed, total: commands.length })}</p>
          <p className={cx("mt-0.5", rawCheck === "changed" || rawCheck === "missing" ? "font-medium text-danger" : "text-muted")}>
            {t(`run.audit${rawCheck[0].toUpperCase()}${rawCheck.slice(1)}`)}
          </p>
        </SummaryBlock>
        <SummaryBlock title={t("run.whatToDecide")}>
          <p>{t(DECISIONS[attempt.state])}</p>
        </SummaryBlock>
      </div>
    </Card>
  );
}

function Actions({ attempt, onError }: { attempt: Attempt; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const act = (run: () => Promise<unknown>) => run().catch((e) => onError(errorText(t, e)));
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
  if (attempt.state === "succeeded" || attempt.state === "abandoned") return null;
  const canResume = attempt.state === "interrupted" || attempt.state === "blocked" || attempt.state === "stopped" || attempt.state === "failed";
  return (
    <Card className="p-4">
      {attempt.reason && <p className="selectable text-[12.5px] text-warning">{reasonText(t, attempt.reason)}</p>}
      <div className={cx("flex flex-wrap gap-2", attempt.reason && "mt-2.5")}>
        {needsSignIn(attempt.reason) && <Button onClick={() => void act(() => api.openClaudeLogin())}>{t("run.signIn")}</Button>}
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
      return line(t("activity.started"), "text-subtle");
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
        <Detail label={t("details.tools")}>{attempt.tools === "build" ? t("newRun.toolsBuild") : t("newRun.toolsReadOnly")}</Detail>
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
          <pre className="mt-1.5 max-h-72 overflow-auto rounded bg-inset p-2 font-mono text-[11.5px] whitespace-pre-wrap">{started.systemPrompt}</pre>
        </div>
      )}
    </div>
  );
}

export function RunDetail({ runId }: { runId: string }) {
  const { t } = useTranslation();
  const revision = useApp((s) => s.revision);
  const runs = useApp((s) => s.overview.runs);
  const title = runs.find((r) => r.id === runId)?.title ?? "";
  const [attempts, setAttempts] = useState<Attempt[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [events, setEvents] = useState<AuditEvent[]>([]);
  const [rawCheck, setRawCheck] = useState<RawCheck>("pending");
  const [tab, setTab] = useState<Tab>("activity");
  const [error, setError] = useState<string | null>(null);
  const eventsOf = useRef<string | null>(null);
  const lastSeq = useRef(0);

  const attempt = attempts.find((a) => a.id === selected) ?? attempts[attempts.length - 1];
  const attemptId = attempt?.id;

  const load = useCallback(async () => {
    setAttempts(await api.runAttempts(runId));
  }, [runId]);

  useEffect(() => {
    void load();
  }, [load, revision]);

  // Audit events only ever grow, so only what's new is fetched.
  useEffect(() => {
    if (!attemptId) return;
    if (eventsOf.current !== attemptId) {
      eventsOf.current = attemptId;
      lastSeq.current = 0;
      setEvents([]);
    }
    void api.attemptEvents(attemptId, lastSeq.current).then((fetched) => {
      const fresh = fetched.filter((e) => e.seq > lastSeq.current);
      if (eventsOf.current !== attemptId || fresh.length === 0) return;
      lastSeq.current = fresh[fresh.length - 1].seq;
      setEvents((current) => [...current, ...fresh]);
    });
    void api.attemptRawCheck(attemptId).then(setRawCheck);
  }, [attemptId, revision]);

  const commands = useMemo(() => commandsFrom(events), [events]);

  if (!attempt) {
    return <TopBar back />;
  }

  const tabs: { value: Tab; label: string }[] = [
    { value: "activity", label: t("tabs.activity") },
    { value: "commands", label: t("tabs.commands") },
    { value: "changes", label: t("tabs.changes") },
    { value: "prompt", label: t("tabs.prompt") },
    { value: "details", label: t("tabs.details") },
  ];

  return (
    <>
      <TopBar back />
      <main className="flex-1 space-y-4 overflow-y-auto p-5">
        <div className="flex items-start gap-3">
          <h1 className="selectable min-w-0 flex-1 text-[17px] leading-snug font-semibold">{title}</h1>
          <StateBadge state={attempt.state} label={t(`states.${attempt.state}`)} />
        </div>
        {error && (
          <p role="alert" className="text-[12.5px] text-danger">
            {error}
          </p>
        )}
        <Actions attempt={attempt} onError={setError} />
        <Summary attempt={attempt} commands={commands} rawCheck={rawCheck} onShowChanges={() => setTab("changes")} />
        <div className="grid gap-4 lg:grid-cols-[210px_1fr]">
          <Card className="self-start p-2">
            <h2 className="px-2 pt-1 pb-1.5 text-[12px] font-semibold text-muted">{t("run.stepsTitle")}</h2>
            <ul className="space-y-0.5">
              {attempts.map((a) => (
                <li key={a.id}>
                  <button
                    type="button"
                    onClick={() => setSelected(a.id)}
                    className={cx("w-full rounded-md px-2 py-1.5 text-left hover:bg-hover", a.id === attempt.id && "bg-active")}
                  >
                    <span className="block truncate text-[12.5px] font-medium">{a.stepId}</span>
                    <span className="mt-0.5 flex items-center justify-between gap-2">
                      <span className="text-[11px] text-subtle">{t("run.attemptTitle", { number: a.number })}</span>
                      <StateBadge state={a.state} label={t(`states.${a.state}`)} />
                    </span>
                    <span className="mt-0.5 block truncate font-mono text-[11px] text-subtle">{a.modelActual ?? a.modelRequested}</span>
                  </button>
                </li>
              ))}
            </ul>
          </Card>
          <Card className="min-w-0 p-4">
            <Segmented<Tab> label={t("tabs.details")} value={tab} onChange={setTab} options={tabs} />
            <div className="mt-4">
              {tab === "activity" && <ActivityTab events={events} />}
              {tab === "commands" && <CommandsTab commands={commands} />}
              {tab === "changes" && <ChangesTab attempt={attempt} revision={revision} />}
              {tab === "prompt" && <pre className="overflow-auto rounded bg-inset p-3 font-mono text-[12px] leading-relaxed whitespace-pre-wrap">{attempt.prompt}</pre>}
              {tab === "details" && <DetailsTab attempt={attempt} events={events} />}
            </div>
          </Card>
        </div>
      </main>
    </>
  );
}
