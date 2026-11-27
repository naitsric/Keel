import { AlertTriangle } from "lucide-react";
import { useId } from "react";
import { useTranslation } from "react-i18next";
import { type Configured, configuredValue } from "../lib/projects";

/**
 * That a project changed under an open form, with what's saved now. `kept` are the fields where
 * the form still holds what the person edited, which saving writes over what's saved: the rest
 * already show the new value.
 */
export function ProjectChangedNotice({ title, saved, fields, kept }: { title: string; saved: Configured; fields: (keyof Configured)[]; kept: (keyof Configured)[] }) {
  const { t } = useTranslation();
  const titleId = useId();
  const keeps = fields.some((field) => kept.includes(field));
  return (
    <div role="status" className="w-full min-w-0 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12px]">
      <p id={titleId} className="flex gap-1.5 text-warning">
        <AlertTriangle size={13} className="mt-0.5 shrink-0" />
        {title}
      </p>
      <p className="mt-1 text-muted">{t(keeps ? "projects.changedKept" : "projects.changedTaken")}</p>
      {/* Whole commands, since they run on this Mac, in a height that leaves the form in sight. */}
      <ul tabIndex={0} aria-labelledby={titleId} className="mt-1 max-h-24 overflow-y-auto">
        {fields.map((field) => (
          <li key={field} className="break-all">
            <span className="text-muted">{t(`mcp.field.${field}`)}: </span>
            <span className="selectable whitespace-pre-wrap font-mono text-[11.5px]">{configuredValue(saved, field) ?? t("mcp.noValue")}</span>
            {kept.includes(field) && <span className="text-warning"> ({t("projects.yoursKept")})</span>}
          </li>
        ))}
      </ul>
    </div>
  );
}
