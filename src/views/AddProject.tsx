import { open as openFolder } from "@tauri-apps/plugin-dialog";
import { FolderPlus } from "lucide-react";
import { type ComponentProps, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type Project } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button } from "../ui/primitives";

/** The one way a project gets added: pick a folder, add it, and its setup dialog opens. */
export function useAddProject({ fromNewRun = false, onAdded }: { fromNewRun?: boolean; onAdded?: (project: Project) => void } = {}) {
  const { t, i18n } = useTranslation();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const add = async () => {
    setError(null);
    setBusy(true);
    try {
      const path = await openFolder({ directory: true, multiple: false });
      if (typeof path !== "string") return;
      const added = await api.projectAdd(path, i18n.language);
      await useApp.getState().refreshOverview();
      onAdded?.(added.project);
      useApp.getState().openProjectSetup({ projectId: added.project.id, added, returnToNewRun: fromNewRun });
    } catch (e) {
      setError(errorText(t, e));
    } finally {
      setBusy(false);
    }
  };

  return { add, error, busy };
}

export function AddProjectButton({ add, busy, disabled, ...props }: { add: () => Promise<void>; busy: boolean } & ComponentProps<typeof Button>) {
  const { t } = useTranslation();
  return (
    <Button disabled={disabled || busy} onClick={() => void add()} {...props}>
      <FolderPlus size={14} />
      {t("projects.add")}
    </Button>
  );
}

/** The button with its own error, for a screen with nothing else to say. */
export function AddProject({ variant, className }: { variant?: ComponentProps<typeof Button>["variant"]; className?: string }) {
  const { add, error, busy } = useAddProject();
  return (
    <div className={className}>
      <AddProjectButton add={add} busy={busy} variant={variant} />
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
    </div>
  );
}
