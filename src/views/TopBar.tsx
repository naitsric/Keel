import { ArrowLeft, Brain, Columns3, Folder, ListChecks, Plus, Search, Settings, Workflow } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { KEYS, withKey } from "../commands/keys";
import { focusContext, pickCommand, runUnlessHeld, useCommands } from "../commands/registry";
import { useApp, type View } from "../lib/store";
import { Button, cx, Kbd } from "../ui/primitives";

const PAGES = [
  { page: "home", label: "app.runs", command: "navigate.runs", key: KEYS.runs, icon: ListChecks },
  { page: "board", label: "app.board", command: "navigate.board", key: KEYS.board, icon: Columns3 },
  { page: "workflows", label: "app.workflows", command: "navigate.workflows", key: KEYS.workflows, icon: Workflow },
  { page: "brain", label: "app.brain", command: "navigate.brain", key: KEYS.brain, icon: Brain },
  { page: "projects", label: "app.projects", command: "navigate.projects", key: KEYS.projects, icon: Folder },
] as const;

/**
 * Under this window width the bar has no room for every page's name: in Spanish, with both badges
 * showing, the names, the search and the buttons need a little over 1000 px, and the window goes
 * down to 900. The same in every language, so the bar doesn't change shape with the language.
 */
export const NAMES_FROM_WIDTH = 1100;

/** Whether the window is too narrow for every page's name. */
function useNarrow() {
  const [narrow, setNarrow] = useState(() => window.innerWidth < NAMES_FROM_WIDTH);
  useEffect(() => {
    const look = () => setNarrow(window.innerWidth < NAMES_FROM_WIDTH);
    look();
    window.addEventListener("resize", look);
    return () => window.removeEventListener("resize", look);
  }, []);
  return narrow;
}

/** A run sits below the page it was opened from, so that page stays the current one. */
const currentPage = (view: View): View["name"] => (view.name === "run" ? currentPage(view.backTo ?? { name: "home" }) : view.name);

/**
 * The title bar: draggable, and clear of the window's traffic lights. The pages are siblings, and
 * only a run has a Back. Each click runs the command of its key, so both go to the same place.
 *
 * Each page has its icon and its name. In a narrow window the others keep only the icon and the
 * current one keeps its name, which is what says where you are. The name stays in the button for
 * a screen reader either way. Nothing here may be cut off: the search is what gives way.
 */
export function TopBar() {
  const { t } = useTranslation();
  const view = useApp((s) => s.view);
  const proposals = useApp((s) => s.overview.pendingProposals);
  const drafts = useApp((s) => s.overview.workflowDrafts);
  const commands = useCommands((s) => s.commands);
  const noProjects = useApp((s) => s.overviewLoaded && s.overview.projects.length === 0);
  const noRuns = useApp((s) => s.overview.runs.length === 0);
  // The welcome screen says what to do first, and there's nothing yet on the other pages. Anywhere
  // else, like Settings, these are the way back.
  const welcome = view.name === "home" && noProjects && noRuns;
  const current = currentPage(view);
  const narrow = useNarrow();
  // Whatever ⌘N does here, so the button and the key can't disagree: New task on the board, and New run everywhere else.
  const primary = pickCommand(
    Object.values(commands).filter((command) => command.keybinding === KEYS.newRun),
    focusContext(),
  );
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
          {PAGES.map(({ page, label, command, key, icon: Icon }) => (
            <Button key={page} variant="ghost" aria-current={current === page ? "page" : undefined} className={cx("px-2", current === page && "bg-active text-fg")} title={withKey(t(label), key)} onClick={() => runUnlessHeld(command)}>
              <Icon size={14} aria-hidden className="shrink-0" />
              <span className={cx(narrow && current !== page && "sr-only")}>{t(label)}</span>
              {page === "brain" && proposals > 0 && (
                <span aria-label={t("app.brainProposals", { count: proposals })} className="rounded-full bg-accent px-1.5 text-[10.5px] font-semibold text-accent-fg">
                  {proposals}
                </span>
              )}
              {page === "workflows" && drafts > 0 && (
                <span aria-label={t("app.workflowDrafts", { count: drafts })} className="rounded-full bg-accent px-1.5 text-[10.5px] font-semibold text-accent-fg">
                  {drafts}
                </span>
              )}
            </Button>
          ))}
        </nav>
      )}
      <div data-tauri-drag-region className="min-w-0 flex-1" />
      {!welcome && (
        <button
          type="button"
          title={withKey(t("palette.label"), KEYS.searchEverywhere)}
          // A click leaves focus where it was, so Esc comes back to it, as with ⇧⇧.
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => runUnlessHeld("search.everywhere")}
          // The one thing in the bar that gives way when there's no room, down to its icon.
          className="flex h-6 w-[clamp(120px,20vw,320px)] min-w-7 shrink items-center gap-2 overflow-hidden rounded-md border border-border bg-inset px-2 text-[12px] text-subtle transition-colors hover:border-border-strong hover:text-muted"
        >
          <Search size={13} className="shrink-0" />
          <span className="min-w-0 truncate">{t("palette.label")}</span>
          <span className="ml-auto shrink-0">
            <Kbd binding={KEYS.searchEverywhere} />
          </span>
        </button>
      )}
      <div data-tauri-drag-region className="min-w-0 flex-1" />
      {primary && !noProjects && (
        <Button variant="primary" className="shrink-0" title={withKey(t(primary.title), KEYS.newRun)} onClick={() => runUnlessHeld(primary.id)}>
          <Plus size={14} />
          {t(primary.title)}
        </Button>
      )}
      <Button variant="ghost" aria-label={t("app.settings")} aria-current={current === "settings" ? "page" : undefined} className={cx("shrink-0", current === "settings" && "bg-active text-fg")} title={withKey(t("app.settings"), KEYS.settings)} onClick={() => runUnlessHeld("navigate.settings")}>
        <Settings size={14} />
      </Button>
    </header>
  );
}
