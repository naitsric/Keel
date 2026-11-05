import { Plus } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Reason, RunRow } from "../lib/api";
import { isActive } from "../lib/api";
import { elapsedMs, formatCost, formatDateTime, formatDuration, formatNumber, formatRelative } from "../lib/format";
import { needsSignIn, reasonText } from "../lib/reason";
import { accountLabel, sessionExpiry } from "../lib/session";
import { useApp } from "../lib/store";
import { api } from "../lib/api";
import { Button, Card, StateBadge } from "../ui/primitives";
import { Onboarding } from "./Onboarding";
import { ProjectsRow } from "./ProjectsRow";
import { TopBar } from "./TopBar";

/** Why runs can't start, and the way out. `text` replaces the reason's sentence, for a screen with more to say. */
export function BlockerNotice({ blocker, text }: { blocker: Reason; text?: string }) {
  const { t } = useTranslation();
  const refresh = useApp((s) => s.refreshSession);
  return (
    <div role="alert" className="flex flex-wrap items-center gap-2 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12.5px] text-warning">
      <span className="flex-1">{text ?? reasonText(t, blocker)}</span>
      {needsSignIn(blocker) && <Button onClick={() => void api.openClaudeLogin()}>{t("session.signIn")}</Button>}
      <Button variant="ghost" onClick={() => void refresh()}>
        {t("app.refresh")}
      </Button>
    </div>
  );
}

function SessionBar() {
  const { t } = useTranslation();
  const preflight = useApp((s) => s.preflight);
  if (!preflight) {
    return <p className="text-[12px] text-subtle">{t("session.checking")}</p>;
  }
  const { auth, version, ignoredEnv, blocker } = preflight;
  const account = accountLabel(auth);
  const expiry = auth?.loggedIn ? sessionExpiry(auth.expiresAt, Date.now()) : null;
  return (
    <div className="space-y-1.5">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
        {auth?.loggedIn && <span>{account ? t("session.account", { account }) : t("session.accountNoEmail")}</span>}
        {version && "found" in version && <span className="text-subtle">{t("session.version", { version: version.found })}</span>}
      </div>
      {expiry && <p className="text-[12px] text-warning">{t(expiry === "soon" ? "session.expiresSoon" : "session.expired", { date: formatDateTime(auth?.expiresAt as string) })}</p>}
      {ignoredEnv.length > 0 && <p className="text-[12px] text-warning">{t("session.ignoredEnv", { names: ignoredEnv.join(", ") })}</p>}
      {blocker && <BlockerNotice blocker={blocker} />}
    </div>
  );
}

function Metric({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <Card className="p-4">
      <p className="text-[12px] font-medium text-muted">{label}</p>
      <p className="mt-1.5 text-[26px] leading-none font-semibold tabular-nums">{value}</p>
      <p className="mt-2 text-[11.5px] leading-snug text-subtle">{detail}</p>
    </Card>
  );
}

function Row({ run, now }: { run: RunRow; now: number }) {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  const open = () => navigate({ name: "run", runId: run.id });
  const last = run.lastActivityAt ?? run.startedAt;
  return (
    <tr onClick={open} className="cursor-pointer border-t border-border hover:bg-hover">
      <td className="max-w-[340px] px-4 py-2.5">
        <button type="button" onClick={open} className="block max-w-full truncate text-left text-[12.5px] font-medium">
          {run.title}
        </button>
        <p className="truncate text-[11.5px] text-subtle">
          {run.projectName}
          {run.startedBy && <span className="ml-2">{t("run.startedBy", { client: run.startedBy })}</span>}
          {run.prUrl && (
            <button
              type="button"
              className="ml-2 text-accent hover:underline"
              onClick={(e) => {
                e.stopPropagation();
                void api.openGithubUrl(run.prUrl as string);
              }}
            >
              {t("run.openPr")}
            </button>
          )}
        </p>
        {run.reason && <p className="mt-0.5 line-clamp-2 text-[11.5px] text-warning">{reasonText(t, run.reason)}</p>}
      </td>
      <td className="px-3 py-2.5 text-[12px] whitespace-nowrap text-muted">
        {run.stepId}
        {run.attemptNumber > 1 && <span className="ml-1 text-subtle">({t("table.attempt", { number: run.attemptNumber })})</span>}
      </td>
      <td className="px-3 py-2.5">
        <StateBadge state={run.state} label={t(`states.${run.state}`)} />
      </td>
      <td className="px-3 py-2.5 text-[12px] whitespace-nowrap text-muted tabular-nums">
        {formatDuration(elapsedMs(run.startedAt, run.finishedAt, now))}
        {run.turns != null && ` / ${run.turns}`}
      </td>
      <td className="px-3 py-2.5 font-mono text-[11.5px] whitespace-nowrap text-muted">{run.model}</td>
      <td className="px-4 py-2.5 text-[12px] whitespace-nowrap text-subtle">{formatRelative(last, now)}</td>
    </tr>
  );
}

export function Home() {
  const { t } = useTranslation();
  const overview = useApp((s) => s.overview);
  const setNewRunOpen = useApp((s) => s.setNewRunOpen);
  const { totals, runs } = overview;
  const loaded = useApp((s) => s.overviewLoaded);
  const isNew = loaded && overview.projects.length === 0;
  const anyActive = runs.some((r) => isActive(r.state));

  // Keeps elapsed times moving while something runs.
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!anyActive) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [anyActive]);

  return (
    <>
      <TopBar>
        {!isNew && (
          <Button variant="primary" onClick={() => setNewRunOpen(true)}>
            <Plus size={14} />
            {t("app.newRun")}
          </Button>
        )}
      </TopBar>
      <main className="flex-1 space-y-4 overflow-y-auto p-5">
        {isNew && <Onboarding />}
        {!isNew && <SessionBar />}
        {loaded && !isNew && <ProjectsRow />}
        {/* With every project removed, their runs are still history to look at. */}
        {(!isNew || runs.length > 0) && (
          <>
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
          <Metric label={t("cards.running")} value={formatNumber(totals.running)} detail={t("cards.runningDetail")} />
          <Metric label={t("cards.retrying")} value={formatNumber(totals.retrying)} detail={t("cards.retryingDetail")} />
          <Metric label={t("cards.blocked")} value={formatNumber(totals.blocked)} detail={t("cards.blockedDetail")} />
          <Metric label={t("cards.tokens")} value={formatNumber(totals.tokens)} detail={totals.costUsd > 0 ? t("cards.cost", { cost: formatCost(totals.costUsd) }) : t("cards.tokensDetail")} />
          <Metric label={t("cards.time")} value={formatDuration(totals.durationMs)} detail={t("cards.timeDetail")} />
        </div>
        <Card>
          <div className="px-4 pt-3.5 pb-3">
            <h2 className="text-[14px] font-semibold">{t("table.title")}</h2>
            <p className="mt-0.5 text-[12px] text-muted">{t("table.subtitle")}</p>
          </div>
          {runs.length === 0 ? (
            <div className="border-t border-border px-4 py-10 text-center">
              <p className="text-[13px] font-medium">{t("table.empty")}</p>
              <p className="mt-1 text-[12px] text-subtle">{t("table.emptyHint")}</p>
              <Button variant="primary" className="mt-3" onClick={() => setNewRunOpen(true)}>
                <Plus size={14} />
                {t("app.newRun")}
              </Button>
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full border-collapse text-left">
                <thead>
                  <tr className="border-t border-border text-[11.5px] text-subtle">
                    <th className="px-4 py-2 font-medium">{t("table.request")}</th>
                    <th className="px-3 py-2 font-medium">{t("table.step")}</th>
                    <th className="px-3 py-2 font-medium">{t("table.state")}</th>
                    <th className="px-3 py-2 font-medium">{t("table.time")}</th>
                    <th className="px-3 py-2 font-medium">{t("table.model")}</th>
                    <th className="px-4 py-2 font-medium">{t("table.lastActivity")}</th>
                  </tr>
                </thead>
                <tbody>
                  {runs.map((run) => (
                    <Row key={run.id} run={run} now={now} />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Card>
          </>
        )}
      </main>
    </>
  );
}
