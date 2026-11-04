import { ArrowLeft, Settings } from "lucide-react";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { useApp } from "../lib/store";
import { Button } from "../ui/primitives";

/** The title bar: draggable, and clear of the window's traffic lights. */
export function TopBar({ back, children }: { back?: boolean; children?: ReactNode }) {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  return (
    <header data-tauri-drag-region className="flex h-[38px] shrink-0 items-center gap-2 border-b border-border pr-3 pl-[var(--traffic-lights-inset)]">
      {back ? (
        <Button variant="ghost" onClick={() => navigate({ name: "home" })}>
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
      <Button variant="ghost" aria-label={t("app.settings")} title={t("app.settings")} onClick={() => navigate({ name: "settings" })}>
        <Settings size={14} />
      </Button>
    </header>
  );
}
