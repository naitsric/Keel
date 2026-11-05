import { ArrowLeft, Plus, Settings } from "lucide-react";
import { useTranslation } from "react-i18next";
import { KEYS, withKey } from "../commands/keys";
import { runUnlessHeld } from "../commands/registry";
import { useApp, type View } from "../lib/store";
import { Button, cx } from "../ui/primitives";

const PAGES = [
  { page: "home", label: "app.runs", command: "navigate.runs", key: KEYS.runs },
  { page: "board", label: "app.board", command: "navigate.board", key: KEYS.board },
  { page: "workflows", label: "app.workflows", command: "navigate.workflows", key: KEYS.workflows },
  { page: "brain", label: "app.brain", command: "navigate.brain", key: KEYS.brain },
] as const;

/** A run sits below the page it was opened from, so that page stays the current one. */
const currentPage = (view: View): View["name"] => (view.name === "run" ? currentPage(view.backTo ?? { name: "home" }) : view.name);

/**
 * The title bar: draggable, and clear of the window's traffic lights. The pages are siblings, and
 * only a run has a Back. Each click runs the command of its key, so both go to the same place.
 */
export function TopBar() {
  const { t } = useTranslation();
  const view = useApp((s) => s.view);
  const proposals = useApp((s) => s.overview.pendingProposals);
  const setNewRunOpen = useApp((s) => s.setNewRunOpen);
  const noProjects = useApp((s) => s.overviewLoaded && s.overview.projects.length === 0);
  const noRuns = useApp((s) => s.overview.runs.length === 0);
  // The welcome screen says what to do first, and there's nothing yet on the other pages. Anywhere
  // else, like Settings, these are the way back.
  const welcome = view.name === "home" && noProjects && noRuns;
  const current = currentPage(view);
  return (
    <header data-tauri-drag-region className="flex h-[38px] shrink-0 items-center gap-2 border-b border-border pr-3 pl-[var(--traffic-lights-inset)]">
      {/* The same width with or without Back, so the pages don't move under the pointer. */}
      <div data-tauri-drag-region className="flex w-[76px] shrink-0 items-center">
        {view.name === "run" ? (
          <Button variant="ghost" className="-ml-1 px-1.5" title={withKey(t("app.back"), KEYS.back)} onClick={() => runUnlessHeld("navigate.back")}>
            <ArrowLeft size={14} />
            {t("app.back")}
          </Button>
        ) : (
          <span data-tauri-drag-region className="text-[13px] font-semibold">
            {t("app.name")}
          </span>
        )}
      </div>
      {!welcome && (
        <nav data-tauri-drag-region aria-label={t("app.pages")} className="flex shrink-0 items-center gap-0.5">
          {PAGES.map(({ page, label, command, key }) => (
            <Button key={page} variant="ghost" aria-current={current === page ? "page" : undefined} className={cx("px-2", current === page && "bg-active text-fg")} title={withKey(t(label), key)} onClick={() => runUnlessHeld(command)}>
              {t(label)}
              {page === "brain" && proposals > 0 && (
                <span aria-label={t("app.brainProposals", { count: proposals })} className="rounded-full bg-accent px-1.5 text-[10.5px] font-semibold text-accent-fg">
                  {proposals}
                </span>
              )}
            </Button>
          ))}
        </nav>
      )}
      <div data-tauri-drag-region className="min-w-0 flex-1" />
      {view.name === "home" && !noProjects && (
        <Button variant="primary" title={withKey(t("app.newRun"), KEYS.newRun)} onClick={() => setNewRunOpen(true)}>
          <Plus size={14} />
          {t("app.newRun")}
        </Button>
      )}
      <Button variant="ghost" aria-label={t("app.settings")} aria-current={current === "settings" ? "page" : undefined} className={cx(current === "settings" && "bg-active text-fg")} title={withKey(t("app.settings"), KEYS.settings)} onClick={() => runUnlessHeld("navigate.settings")}>
        <Settings size={14} />
      </Button>
    </header>
  );
}
