import { useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type CommandError, type Project } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Modal } from "../ui/primitives";

/** Says plainly what removing does and doesn't do, before it's done. */
export function RemoveProjectDialog({ project, open, onOpenChange }: { project: Project; open: boolean; onOpenChange: (open: boolean) => void }) {
  const { t } = useTranslation();
  const refreshOverview = useApp((s) => s.refreshOverview);
  const navigate = useApp((s) => s.navigate);
  const [error, setError] = useState<string | null>(null);
  const [openRun, setOpenRun] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const change = (next: boolean) => {
    setError(null);
    setOpenRun(null);
    onOpenChange(next);
  };

  const remove = async () => {
    setError(null);
    setOpenRun(null);
    setBusy(true);
    try {
      await api.projectRemove(project.id);
      onOpenChange(false);
      await refreshOverview();
    } catch (e) {
      setError(errorText(t, e));
      const reason = (e as Partial<CommandError>).reason;
      // A card being started has no run yet.
      if (reason?.code === "projectHasOpenRuns" && reason.runId) setOpenRun(reason.runId as string);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal open={open} onOpenChange={change} title={t("remove.title", { name: project.name })}>
      <ul className="list-disc space-y-1.5 pl-4 text-[12.5px] leading-relaxed text-muted">
        <li>{t("remove.does")}</li>
        <li>{t("remove.doesNot")}</li>
        <li>{t("remove.comeBack")}</li>
      </ul>
      {error && (
        <p role="alert" className="mt-3 text-[12.5px] text-danger">
          {error}
        </p>
      )}
      <div className="mt-4 flex flex-wrap justify-end gap-2">
        {openRun && (
          <Button
            onClick={() => {
              change(false);
              navigate({ name: "run", runId: openRun });
            }}
          >
            {t("remove.openRun")}
          </Button>
        )}
        <Button autoFocus onClick={() => change(false)}>
          {t("remove.cancel")}
        </Button>
        <Button variant="danger" disabled={busy} onClick={() => void remove()}>
          {t("remove.confirm")}
        </Button>
      </div>
    </Modal>
  );
}
