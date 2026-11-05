import { AlertTriangle } from "lucide-react";
import { Fragment, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { KEYS, withKey } from "../commands/keys";
import { runUnlessHeld } from "../commands/registry";
import { isWaitingForYou, isWorking } from "../commands/runs";
import { accountLabel } from "../lib/session";
import { useApp } from "../lib/store";
import { cx, Kbd } from "../ui/primitives";

/** Which Claude Code, and whose. What needs doing about it is on Home, in SessionBar. */
function useSessionItems(): ReactNode[] {
  const { t } = useTranslation();
  const preflight = useApp((s) => s.preflight);
  if (!preflight) return [t("session.checking")];
  const { auth, version, claudeFound } = preflight;
  if (!claudeFound) return [<span className="text-warning">{t("statusBar.noClaude")}</span>];
  const account = accountLabel(auth);
  return [
    version && "found" in version && <span className="whitespace-nowrap">{t("statusBar.version", { version: version.found })}</span>,
    auth?.loggedIn && (
      <span title={account ?? undefined} className="block min-w-0 truncate">
        {account ?? t("statusBar.signedIn")}
      </span>
    ),
    auth && !auth.loggedIn && <span className="whitespace-nowrap text-warning">{t("statusBar.signedOut")}</span>,
  ];
}

function useMcpItem(): ReactNode {
  const { t } = useTranslation();
  const mcp = useApp((s) => s.mcp);
  if (!mcp) return null;
  const { error } = mcp;
  if (error) {
    return (
      <span title={error.code === "portInUse" ? t("mcp.portInUse", { port: error.port }) : t("mcp.listenFailed", { port: error.port, message: error.message })} className="whitespace-nowrap text-warning">
        {t("statusBar.mcpFailed")}
      </span>
    );
  }
  return <span className="whitespace-nowrap">{t(mcp.running ? "statusBar.mcpOn" : "statusBar.mcpOff")}</span>;
}

/**
 * The bottom of the window, on every page. "Waiting" means what ⌘G means, read from the same list,
 * and the click runs the same command, so the count and the key can't disagree. It only goes to a
 * run, and only the run's own screen can answer it.
 */
export function StatusBar() {
  const { t } = useTranslation();
  const runs = useApp((s) => s.overview.runs);
  const nothingYet = useApp((s) => s.overviewLoaded && s.overview.projects.length === 0 && s.overview.runs.length === 0);
  const running = runs.filter((run) => isWorking(run.state)).length;
  const waiting = runs.filter((run) => isWaitingForYou(run.state)).length;
  const session = useSessionItems();
  const mcp = useMcpItem();
  const right = [...session, mcp].filter(Boolean);
  return (
    <footer className="flex h-6 shrink-0 items-center gap-3 border-t border-border px-3 text-[11px] text-subtle">
      {!nothingYet && (
        <span className="flex shrink-0 items-center gap-1.5 whitespace-nowrap">
          <span className={cx("inline-block size-1.5 rounded-full", running > 0 ? "animate-pulse bg-accent" : "bg-subtle")} />
          {running > 0 ? t("statusBar.running", { count: running }) : t("statusBar.idle")}
        </span>
      )}
      {waiting > 0 && (
        <button
          type="button"
          title={withKey(t("shortcuts.nextWaiting"), KEYS.nextWaiting)}
          className="flex h-5 shrink-0 items-center gap-1.5 rounded px-1 whitespace-nowrap text-warning hover:bg-hover"
          // Held down, Enter stops at the first run instead of going through them all.
          onKeyDown={(e) => {
            if (e.key === "Enter" && e.repeat) e.preventDefault();
          }}
          onClick={() => runUnlessHeld("run.nextWaiting")}
        >
          <AlertTriangle size={11} className="shrink-0" />
          {t("statusBar.waiting", { count: waiting })}
          <Kbd binding={KEYS.nextWaiting} />
        </button>
      )}
      <div className="ml-auto flex min-w-0 items-center gap-1.5">
        {right.map((item, index) => (
          <Fragment key={index}>
            {index > 0 && <span aria-hidden>·</span>}
            {item}
          </Fragment>
        ))}
      </div>
    </footer>
  );
}
