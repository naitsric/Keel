import { open as openFolder } from "@tauri-apps/plugin-dialog";
import { FolderPlus, ShieldCheck } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type ToolProfile } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Field, inputClass, Modal, Segmented } from "../ui/primitives";

const MODELS = [
  { value: "fable", label: "Fable" },
  { value: "opus", label: "Opus" },
  { value: "sonnet", label: "Sonnet" },
  { value: "haiku", label: "Haiku" },
];
const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
const CUSTOM = "__custom__";
const DEFAULT = "__default__";

export function NewRunDialog() {
  const { t, i18n } = useTranslation();
  const open = useApp((s) => s.newRunOpen);
  const setOpen = useApp((s) => s.setNewRunOpen);
  const projects = useApp((s) => s.overview.projects);
  const refreshOverview = useApp((s) => s.refreshOverview);
  const navigate = useApp((s) => s.navigate);

  const [projectId, setProjectId] = useState("");
  const [prompt, setPrompt] = useState("");
  const [stepId, setStepId] = useState("");
  const [model, setModel] = useState("sonnet");
  const [customModel, setCustomModel] = useState("");
  const [effort, setEffort] = useState(DEFAULT);
  const [tools, setTools] = useState<ToolProfile>("build");
  const [maxTurns, setMaxTurns] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projects, projectId]);

  const addProject = async () => {
    setError(null);
    const path = await openFolder({ directory: true, multiple: false });
    if (typeof path !== "string") return;
    try {
      const project = await api.projectAdd(path);
      await refreshOverview();
      setProjectId(project.id);
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  const chosenModel = model === CUSTOM ? customModel.trim() : model;
  const turns = maxTurns.trim() === "" ? null : Number(maxTurns);
  const valid = projectId && prompt.trim() && chosenModel && (turns === null || (Number.isInteger(turns) && turns > 0));

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const runId = await api.runStart({
        projectId,
        stepId: stepId.trim() || t("newRun.stepNameDefault"),
        prompt,
        model: chosenModel,
        effort: effort === DEFAULT ? null : effort,
        tools,
        maxTurns: turns,
        language: i18n.language,
      });
      setPrompt("");
      setOpen(false);
      await refreshOverview();
      navigate({ name: "run", runId });
    } catch (e) {
      setError(errorText(t, e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal open={open} onOpenChange={setOpen} title={t("newRun.title")}>
      <div className="space-y-3.5">
        <Field label={t("newRun.project")}>
          <div className="flex gap-2">
            <select value={projectId} onChange={(e) => setProjectId(e.target.value)} className={inputClass} disabled={projects.length === 0}>
              {projects.length === 0 && <option value="">{t("newRun.noProjects")}</option>}
              {projects.map((project) => (
                <option key={project.id} value={project.id}>
                  {project.name}
                </option>
              ))}
            </select>
            <Button className="h-auto shrink-0" onClick={() => void addProject()}>
              <FolderPlus size={14} />
              {t("newRun.addProject")}
            </Button>
          </div>
        </Field>
        <Field label={t("newRun.prompt")}>
          <textarea rows={5} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder={t("newRun.promptPlaceholder")} className={`${inputClass} resize-y leading-relaxed`} />
        </Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label={t("newRun.model")}>
            <select value={model} onChange={(e) => setModel(e.target.value)} className={inputClass}>
              {MODELS.map((m) => (
                <option key={m.value} value={m.value}>
                  {m.label}
                </option>
              ))}
              <option value={CUSTOM}>{t("newRun.customModel")}</option>
            </select>
            {model === CUSTOM && <input value={customModel} onChange={(e) => setCustomModel(e.target.value)} placeholder={t("newRun.customModelPlaceholder")} className={`${inputClass} mt-1.5 font-mono`} />}
          </Field>
          <Field label={t("newRun.effort")}>
            <select value={effort} onChange={(e) => setEffort(e.target.value)} className={inputClass}>
              <option value={DEFAULT}>{t("newRun.effortDefault")}</option>
              {EFFORTS.map((level) => (
                <option key={level} value={level}>
                  {t(`newRun.effortLevels.${level}`)}
                </option>
              ))}
            </select>
          </Field>
          <Field label={t("newRun.stepName")}>
            <input value={stepId} onChange={(e) => setStepId(e.target.value.replace(/[^\w-]/g, ""))} placeholder={t("newRun.stepNameDefault")} className={inputClass} />
          </Field>
          <Field label={t("newRun.maxTurns")}>
            <input inputMode="numeric" value={maxTurns} onChange={(e) => setMaxTurns(e.target.value.replace(/\D/g, ""))} placeholder={t("newRun.maxTurnsPlaceholder")} className={inputClass} />
          </Field>
        </div>
        <div>
          <span className="mb-1 block text-[12px] font-medium text-muted">{t("newRun.tools")}</span>
          <Segmented<ToolProfile>
            label={t("newRun.tools")}
            value={tools}
            onChange={setTools}
            options={[
              { value: "build", label: t("newRun.toolsBuild") },
              { value: "readOnly", label: t("newRun.toolsReadOnly") },
            ]}
          />
        </div>
        <p className="flex gap-2 text-[11.5px] leading-snug text-subtle">
          <ShieldCheck size={14} className="mt-px shrink-0" />
          {t("newRun.isolation")}
        </p>
        {error && (
          <p role="alert" className="text-[12px] text-danger">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button onClick={() => setOpen(false)}>{t("newRun.cancel")}</Button>
          <Button variant="primary" disabled={!valid || busy} onClick={() => void start()}>
            {t("newRun.start")}
          </Button>
        </div>
      </div>
    </Modal>
  );
}
