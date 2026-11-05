import { Check } from "lucide-react";
import { useTranslation } from "react-i18next";
import { needsSetup } from "../lib/projects";
import { useApp } from "../lib/store";
import { Button, Card } from "../ui/primitives";
import { AddProjectButton, useAddProject } from "./AddProject";

/** Home's list of projects: which are ready, which aren't, and the way to add another. */
export function ProjectsRow() {
  const { t } = useTranslation();
  const projects = useApp((s) => s.overview.projects);
  const openSetup = useApp((s) => s.openProjectSetup);
  const { add, error, busy } = useAddProject();
  return (
    <Card className="px-4 py-2.5">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5">
        <h2 className="text-[13px] font-semibold">{t("projects.title")}</h2>
        <ul className="flex min-w-0 flex-1 flex-wrap items-center gap-x-4 gap-y-1">
          {projects.map((project) =>
            needsSetup(project) ? (
              <li key={project.id} className="flex items-center gap-1.5 text-[12.5px]">
                {project.name}
                <Button variant="ghost" className="h-6 px-1.5 text-warning" onClick={() => openSetup({ projectId: project.id, added: null, returnToNewRun: false })}>
                  {t("projects.finishSetup")}
                </Button>
              </li>
            ) : (
              <li key={project.id} className="flex h-6 items-center gap-1.5 text-[12.5px]">
                <Check size={13} className="shrink-0 text-success" aria-label={t("projects.ready")} />
                {project.name}
              </li>
            ),
          )}
        </ul>
        <AddProjectButton add={add} busy={busy} />
      </div>
      {error && (
        <p role="alert" className="mt-1.5 text-[12px] text-danger">
          {error}
        </p>
      )}
    </Card>
  );
}
