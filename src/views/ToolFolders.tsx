import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type ToolFolder } from "../lib/api";

/** How long typing has to stop before Keel looks again. */
const SETTLE_MS = 400;

/**
 * Where in the home folder the programs these commands use are, looked up again a moment after the
 * commands change. Null until the first answer. An answer that comes late never wins, and one that
 * fails shows nothing: it's only a proposal.
 */
export function useToolFolders(projectId: string, setup: string, check: string, enabled = true): ToolFolder[] | null {
  const [found, setFound] = useState<ToolFolder[] | null>(null);
  const latest = useRef(0);
  useEffect(() => {
    if (!enabled) return;
    const request = ++latest.current;
    const timer = setTimeout(() => {
      api
        .projectToolFolders(projectId, setup, check)
        .then((next) => {
          if (request === latest.current) setFound(Array.isArray(next) ? next : []);
        })
        .catch(() => {
          if (request === latest.current) setFound([]);
        });
    }, SETTLE_MS);
    return () => clearTimeout(timer);
  }, [projectId, setup, check, enabled]);
  return found;
}

/** For the setup dialog: each folder ticked, so saving lets the commands read it, unless the person unticks it. */
export function FolderChoices({ folders, skipped, onToggle }: { folders: ToolFolder[]; skipped: string[]; onToggle: (folder: string, chosen: boolean) => void }) {
  const { t } = useTranslation();
  if (folders.length === 0) return null;
  return (
    <div className="text-[12px]">
      <p className="text-muted">{t("setup.folders")}</p>
      <ul className="mt-1 space-y-0.5">
        {folders.map((found) => (
          <li key={found.folder}>
            <label className="flex items-start gap-2">
              <input type="checkbox" className="mt-0.5" checked={!skipped.includes(found.folder)} onChange={(e) => onToggle(found.folder, e.target.checked)} />
              <span className="min-w-0 break-all">
                <span className="selectable font-mono text-[11.5px]">{found.folder}</span>
                <span className="ml-1.5 text-subtle">{t("setup.folderFor", { programs: found.programs.join(", ") })}</span>
              </span>
            </label>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** For Settings: each folder found that isn't listed yet, to add with a click. */
export function FolderOffers({ folders, onAdd }: { folders: ToolFolder[]; onAdd: (folder: string) => void }) {
  const { t } = useTranslation();
  if (folders.length === 0) return null;
  return (
    <div className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[12px] text-muted">
      <span>{t("settings.foundFolders")}</span>
      {folders.map((found) => {
        const programs = found.programs.join(", ");
        return (
          <button
            key={found.folder}
            type="button"
            title={t("setup.folderFor", { programs })}
            aria-label={t("settings.addFolder", { folder: found.folder, programs })}
            className="rounded border border-border px-1.5 py-0.5 font-mono text-[11.5px] text-accent hover:bg-inset"
            onClick={() => onAdd(found.folder)}
          >
            {found.folder}
          </button>
        );
      })}
    </div>
  );
}
