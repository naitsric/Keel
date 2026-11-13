import { Network } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { Attempt } from "../lib/api";
import { localhostOpenIn } from "../lib/audit";

/** At an approval, that the run's tests and builder could reach anything on localhost, which its project allowed. */
export function LocalhostNote({ attempts }: { attempts: Attempt[] }) {
  const { t } = useTranslation();
  if (!localhostOpenIn(attempts)) return null;
  return (
    <p className="mt-2 flex gap-1.5 rounded-md border border-border-strong bg-inset p-2 text-[12.5px]">
      <Network size={14} className="mt-0.5 shrink-0 text-muted" />
      {t("run.localhostOpen")}
    </p>
  );
}
