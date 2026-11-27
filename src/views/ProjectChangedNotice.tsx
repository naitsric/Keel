import { AlertTriangle } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { Configured } from "../lib/projects";

/** That a project changed under an open form, with what's saved now: saving the form writes over it. */
export function ProjectChangedNotice({ title, saved, fields }: { title: string; saved: Configured; fields: (keyof Configured)[] }) {
  const { t } = useTranslation();
  return (
    <div role="status" className="w-full min-w-0 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12px]">
      <p className="flex gap-1.5 text-warning">
        <AlertTriangle size={13} className="mt-0.5 shrink-0" />
        {title}
      </p>
      <p className="mt-1 text-muted">{t("projects.changedKept")}</p>
      {/* Whole commands, since they run on this Mac, in a height that leaves the form in sight. */}
      <ul tabIndex={0} className="mt-1 max-h-24 overflow-y-auto">
        {fields.map((field) => (
          <li key={field} className="break-all">
            <span className="text-muted">{t(`mcp.field.${field}`)}: </span>
            <span className="selectable whitespace-pre-wrap font-mono text-[11.5px]">{saved[field] ?? t("mcp.noValue")}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
