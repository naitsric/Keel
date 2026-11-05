import { ArrowLeft, Brain, ListChecks, Settings, SquareKanban } from "lucide-react";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { KEYS, withKey } from "../commands/keys";
import { useApp, type View } from "../lib/store";
import { Button } from "../ui/primitives";

/** The title bar: draggable, and clear of the window's traffic lights. Back goes home unless it's given a view. */
export function TopBar({ back, children }: { back?: boolean | View; children?: ReactNode }) {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  const proposals = useApp((s) => s.overview.pendingProposals);
  return (
    <header data-tauri-drag-region className="flex h-[38px] shrink-0 items-center gap-2 border-b border-border pr-3 pl-[var(--traffic-lights-inset)]">
      {back ? (
        <Button variant="ghost" title={withKey(t("app.back"), KEYS.back)} onClick={() => navigate(back === true ? { name: "home" } : back)}>
          <ArrowLeft size={14} />
          {t("app.back")}
        </Button>
      ) : (
        <span data-tauri-drag-region className="text-[13px] font-semibold">
          {t("app.name")}
        </span>
      )}
      <div data-tauri-drag-region className="flex-1" />
      {children}
      <Button variant="ghost" title={withKey(t("app.board"), KEYS.board)} onClick={() => navigate({ name: "board" })}>
        <SquareKanban size={14} />
        {t("app.board")}
      </Button>
      <Button variant="ghost" title={withKey(t("app.brain"), KEYS.brain)} onClick={() => navigate({ name: "brain" })}>
        <Brain size={14} />
        {t("app.brain")}
        {proposals > 0 && (
          <span aria-label={t("app.brainProposals", { count: proposals })} className="rounded-full bg-accent px-1.5 text-[10.5px] font-semibold text-accent-fg">
            {proposals}
          </span>
        )}
      </Button>
      <Button variant="ghost" title={withKey(t("app.workflows"), KEYS.workflows)} onClick={() => navigate({ name: "workflows" })}>
        <ListChecks size={14} />
        {t("app.workflows")}
      </Button>
      <Button variant="ghost" aria-label={t("app.settings")} title={withKey(t("app.settings"), KEYS.settings)} onClick={() => navigate({ name: "settings" })}>
        <Settings size={14} />
      </Button>
    </header>
  );
}
