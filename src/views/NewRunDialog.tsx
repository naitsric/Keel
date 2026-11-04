import { open as openFolder } from "@tauri-apps/plugin-dialog";
import { FolderPlus, ShieldCheck } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type Issue, type ToolProfile } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, cx, Field, inputClass, Modal, Segmented } from "../ui/primitives";

const MODELS = [
  { value: "fable", label: "Fable" },
  { value: "opus", label: "Opus" },
  { value: "sonnet", label: "Sonnet" },
  { value: "haiku", label: "Haiku" },
];
const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
const CUSTOM = "__custom__";
const DEFAULT = "__default__";

type Mode = "full" | "single";
type Source = "text" | "issue";

/** A model picker, with a free field for any other model name. */
function ModelField({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  const { t } = useTranslation();
  const known = MODELS.some((m) => m.value === value);
  const [custom, setCustom] = useState(!known);
  return (
    <Field label={label}>
      <select
        value={custom ? CUSTOM : value}
        onChange={(e) => {
          const isCustom = e.target.value === CUSTOM;
          setCustom(isCustom);
          onChange(isCustom ? "" : e.target.value);
        }}
        className={inputClass}
      >
        {MODELS.map((m) => (
          <option key={m.value} value={m.value}>
            {m.label}
          </option>
        ))}
        <option value={CUSTOM}>{t("newRun.customModel")}</option>
      </select>
      {custom && <input value={value} onChange={(e) => onChange(e.target.value.trim())} placeholder={t("newRun.customModelPlaceholder")} className={`${inputClass} mt-1.5 font-mono`} />}
    </Field>
  );
}

export function NewRunDialog() {
  const { t, i18n } = useTranslation();
  const open = useApp((s) => s.newRunOpen);
  const setOpen = useApp((s) => s.setNewRunOpen);
  const projects = useApp((s) => s.overview.projects);
  const refreshOverview = useApp((s) => s.refreshOverview);
  const navigate = useApp((s) => s.navigate);

  const [projectId, setProjectId] = useState("");
  const [mode, setMode] = useState<Mode>("full");
  const [source, setSource] = useState<Source>("text");
  const [prompt, setPrompt] = useState("");
  const [issues, setIssues] = useState<Issue[] | null>(null);
  const [issue, setIssue] = useState<Issue | null>(null);
  const [specModel, setSpecModel] = useState("opus");
  const [builderModel, setBuilderModel] = useState("sonnet");
  const [stepId, setStepId] = useState("");
  const [model, setModel] = useState("sonnet");
  const [effort, setEffort] = useState(DEFAULT);
  const [tools, setTools] = useState<ToolProfile>("build");
  const [maxTurns, setMaxTurns] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const project = projects.find((p) => p.id === projectId);

  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projects, projectId]);

  // Issues load only when asked for, and importing one takes a click. Keel never polls them.
  useEffect(() => {
    setIssue(null);
    setIssues(null);
    if (source !== "issue" || !projectId) return;
    let current = true;
    api
      .issuesList(projectId)
      .then((list) => current && setIssues(list))
      .catch((e) => {
        if (!current) return;
        setIssues([]);
        setError(errorText(t, e));
      });
    return () => {
      current = false;
    };
  }, [source, projectId, t]);

  const addProject = async () => {
    setError(null);
    const path = await openFolder({ directory: true, multiple: false });
    if (typeof path !== "string") return;
    try {
      const added = await api.projectAdd(path);
      await refreshOverview();
      setProjectId(added.id);
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  const turns = maxTurns.trim() === "" ? null : Number(maxTurns);
  const hasRequest = source === "issue" ? issue !== null : prompt.trim() !== "";
  const needsGithub = mode === "full" && project != null && (!project.githubAccount || !project.githubRepo);
  const validModels = mode === "full" ? specModel && builderModel : model && (turns === null || (Number.isInteger(turns) && turns > 0));
  const valid = projectId && hasRequest && validModels && !needsGithub;

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const runId = await api.runStart({
        projectId,
        prompt,
        issue,
        language: i18n.language,
        workflow:
          mode === "full"
            ? { mode: "full", specModel, builderModel }
            : { mode: "single", stepId: stepId.trim() || t("newRun.stepNameDefault"), model, effort: effort === DEFAULT ? null : effort, tools, maxTurns: turns },
      });
      setPrompt("");
      setIssue(null);
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
    <Modal open={open} onOpenChange={setOpen} title={t("newRun.title")} className="w-[560px]">
      <div className="space-y-3.5">
        <Field label={t("newRun.project")}>
          <div className="flex gap-2">
            <select value={projectId} onChange={(e) => setProjectId(e.target.value)} className={inputClass} disabled={projects.length === 0}>
              {projects.length === 0 && <option value="">{t("newRun.noProjects")}</option>}
              {projects.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
            <Button className="h-auto shrink-0" onClick={() => void addProject()}>
              <FolderPlus size={14} />
              {t("newRun.addProject")}
            </Button>
          </div>
        </Field>

        <div>
          <span className="mb-1 block text-[12px] font-medium text-muted">{t("newRun.workflow")}</span>
          <Segmented<Mode>
            label={t("newRun.workflow")}
            value={mode}
            onChange={setMode}
            options={[
              { value: "full", label: t("newRun.workflowFull") },
              { value: "single", label: t("newRun.workflowSingle") },
            ]}
          />
          {mode === "full" && <p className="mt-1.5 text-[11.5px] text-subtle">{t("newRun.workflowFullHint")}</p>}
        </div>

        <div>
          <span className="mb-1 block text-[12px] font-medium text-muted">{t("newRun.source")}</span>
          <Segmented<Source>
            label={t("newRun.source")}
            value={source}
            onChange={setSource}
            options={[
              { value: "text", label: t("newRun.sourceText") },
              { value: "issue", label: t("newRun.sourceIssue") },
            ]}
          />
        </div>

        {source === "issue" && (
          <div>
            {issues === null && <p className="text-[12px] text-subtle">{t("newRun.issuesLoading")}</p>}
            {issues?.length === 0 && !error && <p className="text-[12px] text-subtle">{t("newRun.issuesEmpty")}</p>}
            {issues && issues.length > 0 && (
              <ul aria-label={t("newRun.issueChoose")} className="max-h-40 overflow-y-auto rounded-md border border-border-strong bg-inset">
                {issues.map((item) => (
                  <li key={item.number}>
                    <button
                      type="button"
                      onClick={() => setIssue(item)}
                      className={cx("w-full border-t border-border px-2.5 py-1.5 text-left first:border-t-0 hover:bg-hover", issue?.number === item.number && "bg-active")}
                    >
                      <span className="block truncate text-[12.5px]">
                        <span className="text-subtle">#{item.number}</span> {item.title}
                      </span>
                      {item.author && <span className="text-[11px] text-subtle">{t("newRun.issueBy", { author: item.author })}</span>}
                    </button>
                  </li>
                ))}
              </ul>
            )}
            <p className="mt-1.5 text-[11.5px] leading-snug text-subtle">{t("newRun.issueNote")}</p>
          </div>
        )}

        <Field label={source === "issue" ? t("newRun.issueExtra") : t("newRun.prompt")}>
          <textarea rows={source === "issue" ? 2 : 4} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder={source === "issue" ? undefined : t("newRun.promptPlaceholder")} className={`${inputClass} resize-y leading-relaxed`} />
        </Field>

        {mode === "full" ? (
          <>
            <div className="grid grid-cols-2 gap-3">
              <ModelField label={t("newRun.specModel")} value={specModel} onChange={setSpecModel} />
              <ModelField label={t("newRun.builderModel")} value={builderModel} onChange={setBuilderModel} />
            </div>
            <p className="font-mono text-[11.5px] text-subtle">{project?.checkCommand ? t("newRun.checkCommand", { command: project.checkCommand }) : <span className="font-sans">{t("newRun.checkNone")}</span>}</p>
            {needsGithub && (
              <div role="alert" className="flex items-center gap-2 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12.5px] text-warning">
                <span className="flex-1">{project?.githubRepo ? t("newRun.needsGithub") : t("reasons.noGithubRemote")}</span>
                <Button
                  onClick={() => {
                    setOpen(false);
                    navigate({ name: "settings" });
                  }}
                >
                  {t("newRun.openSettings")}
                </Button>
              </div>
            )}
          </>
        ) : (
          <>
            <div className="grid grid-cols-2 gap-3">
              <ModelField label={t("newRun.model")} value={model} onChange={setModel} />
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
          </>
        )}

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
