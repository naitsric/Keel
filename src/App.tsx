import { useTranslation } from "react-i18next";
import { Shortcuts } from "./commands/Shortcuts";
import { api } from "./lib/api";
import { useApp } from "./lib/store";
import { Button, Modal } from "./ui/primitives";
import { BoardView } from "./views/BoardView";
import { BrainPage } from "./views/BrainPage";
import { Home } from "./views/Home";
import { NewRunDialog } from "./views/NewRunDialog";
import { ProjectProposals } from "./views/ProjectProposals";
import { ProjectSetupDialog } from "./views/ProjectSetupDialog";
import { ProjectsView } from "./views/ProjectsView";
import { RunDetail } from "./views/RunDetail";
import { SettingsView } from "./views/SettingsView";
import { StatusBar } from "./views/StatusBar";
import { TopBar } from "./views/TopBar";
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
      <TopBar />
      <ProjectProposals />
      {/* One page at a time, which scrolls by itself, so what's above and below it stays put. */}
      <div className="flex min-h-0 flex-1 flex-col">
        {view.name === "home" && <Home />}
        {view.name === "run" && <RunDetail key={view.runId} runId={view.runId} />}
        {view.name === "settings" && <SettingsView />}
        {view.name === "workflows" && <WorkflowsView />}
        {view.name === "brain" && <BrainPage />}
        {view.name === "projects" && <ProjectsView projectId={view.projectId} />}
        {view.name === "board" && <BoardView initialProjectId={view.projectId} />}
      </div>
      <StatusBar />
      <NewRunDialog />
      <ProjectSetupDialog />
      <QuitDialog />
    </div>
  );
}
