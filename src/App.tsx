import { useTranslation } from "react-i18next";
import { Shortcuts } from "./commands/Shortcuts";
import { api } from "./lib/api";
import { useApp } from "./lib/store";
import { Button, Modal } from "./ui/primitives";
import { BoardView } from "./views/BoardView";
import { BrainPage } from "./views/BrainPage";
import { Home } from "./views/Home";
import { NewRunDialog } from "./views/NewRunDialog";
import { ProjectSetupDialog } from "./views/ProjectSetupDialog";
import { RunDetail } from "./views/RunDetail";
import { SettingsView } from "./views/SettingsView";
import { WorkflowsView } from "./views/WorkflowsView";

function QuitDialog() {
  const { t } = useTranslation();
  const open = useApp((s) => s.quitOpen);
  const setOpen = useApp((s) => s.setQuitOpen);
  return (
    <Modal open={open} onOpenChange={setOpen} title={t("quit.title")} description={t("quit.body")}>
      <div className="flex justify-end gap-2">
        <Button autoFocus onClick={() => setOpen(false)}>
          {t("quit.stay")}
        </Button>
        <Button onClick={() => void api.appQuit(false)}>{t("quit.leaveRunning")}</Button>
        <Button variant="danger" onClick={() => void api.appQuit(true)}>
          {t("quit.stopAndQuit")}
        </Button>
      </div>
    </Modal>
  );
}

export default function App() {
  const view = useApp((s) => s.view);
  return (
    <div className="flex h-full flex-col">
      {/* First, so its commands register before a page's and the palette keeps them on top. */}
      <Shortcuts />
      {view.name === "home" && <Home />}
      {view.name === "run" && <RunDetail key={view.runId} runId={view.runId} backTo={view.backTo} />}
      {view.name === "settings" && <SettingsView />}
      {view.name === "workflows" && <WorkflowsView />}
      {view.name === "brain" && <BrainPage />}
      {view.name === "board" && <BoardView initialProjectId={view.projectId} />}
      <NewRunDialog />
      <ProjectSetupDialog />
      <QuitDialog />
    </div>
  );
}
