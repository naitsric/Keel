import { ShieldCheck, UserRound } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { openRun } from "../commands/runs";
import { api, type Issue, type ToolProfile, type Workflow } from "../lib/api";
import { errorText } from "../lib/reason";
import { accountLabel } from "../lib/session";
import { useApp } from "../lib/store";
import { Button, cx, Field, inputClass, Modal, Segmented } from "../ui/primitives";
import { AddProjectButton, useAddProject } from "./AddProject";
import { ModelField } from "./ModelField";

const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
const DEFAULT = "__default__";

type Source = "text" | "issue";

const SINGLE = "__single__";

/** The steps of a workflow in one line, with the model of each agent. */
function StepChain({ workflow }: { workflow: Workflow }) {
  const label = (id: string, model?: string) => (model ? `${id} (${model})` : id);
  const parts = workflow.steps.map((step) => {
    if (step.type === "agent") return label(step.id, step.model);
    if (step.type === "parallel") return step.steps.map((s) => label(s.id, s.model)).join(" + ");
    return step.id;
  });
  return <p className="mt-1.5 font-mono text-[11px] leading-relaxed break-words text-subtle">{parts.join(" → ")}</p>;
}

export function NewRunDialog() {
  const { t, i18n } = useTranslation();
  const open = useApp((s) => s.newRunOpen);
  const setOpen = useApp((s) => s.setNewRunOpen);
  const projects = useApp((s) => s.overview.projects);
  const auth = useApp((s) => s.preflight?.auth);
  const refreshOverview = useApp((s) => s.refreshOverview);
  const navigate = useApp((s) => s.navigate);
  const openSetup = useApp((s) => s.openProjectSetup);

  const [projectId, setProjectId] = useState("");
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [workflowId, setWorkflowId] = useState("builtin:full");
  const [source, setSource] = useState<Source>("text");
  const [prompt, setPrompt] = useState("");
  const [issues, setIssues] = useState<Issue[] | null>(null);
  const [issue, setIssue] = useState<Issue | null>(null);
  const [stepId, setStepId] = useState("");
  const [model, setModel] = useState("sonnet");
  const [effort, setEffort] = useState(DEFAULT);
  const [tools, setTools] = useState<ToolProfile>("build");
  const [maxTurns, setMaxTurns] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const adding = useAddProject({ fromNewRun: true, onAdded: (added) => setProjectId(added.id) });

  const project = projects.find((p) => p.id === projectId);
  const finishSetup = () => project && openSetup({ projectId: project.id, added: null, returnToNewRun: true });

  // This stays mounted, so what's selected can have been removed since.
  useEffect(() => {
    if (!projects.some((p) => p.id === projectId)) setProjectId(projects[0]?.id ?? "");
  }, [projects, projectId]);

  useEffect(() => {
    if (!open || !projectId) return;
    api
      .workflowsList(projectId, i18n.language)
      .then((list) => {
        setWorkflows(list);
        // One from another project, or one that's been deleted, can't stay selected.
        setWorkflowId((current) => (current === SINGLE || list.some((w) => w.id === current) ? current : (list[0]?.id ?? SINGLE)));
      })
      .catch((e) => setError(errorText(t, e)));
  }, [open, projectId, i18n.language, t]);

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

  const turns = maxTurns.trim() === "" ? null : Number(maxTurns);
  const hasRequest = source === "issue" ? issue !== null : prompt.trim() !== "";
  const single = workflowId === SINGLE;
  const workflow = workflows.find((w) => w.id === workflowId);
  const delivers = workflow?.steps.some((step) => step.type === "deliver") ?? false;
  const needsGithub = !single && delivers && project != null && (!project.githubAccount || !project.githubRepo);
  const validChoice = single ? model && (turns === null || (Number.isInteger(turns) && turns > 0)) : workflow != null;
  const valid = projectId && hasRequest && validChoice && !needsGithub;
  const claudeAccount = accountLabel(auth);
  // A project missing its account has the warning below instead.
  const accounts = [
    auth?.loggedIn && (claudeAccount ? t("session.account", { account: claudeAccount }) : t("session.accountNoEmail")),
    !single && delivers && !needsGithub && project && t("newRun.accountGithub", { account: project.githubAccount, repo: project.githubRepo }),
  ]
    .filter(Boolean)
    .join(" · ");

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const runId = await api.runStart({
        projectId,
        prompt,
        issue,
        language: i18n.language,
        workflow: !single
          ? { mode: "saved", workflowId }
          : { mode: "single", stepId: stepId.trim() || t("newRun.stepNameDefault"), model, effort: effort === DEFAULT ? null : effort, tools, maxTurns: turns },
      });
      setPrompt("");
      setIssue(null);
      setOpen(false);
      await refreshOverview();
      openRun(runId);
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
            <AddProjectButton add={adding.add} busy={adding.busy} className="h-auto shrink-0" />
          </div>
        </Field>

        <div>
          <Field label={t("newRun.workflow")}>
            <div className="flex gap-2">
              <select value={workflowId} onChange={(e) => setWorkflowId(e.target.value)} className={inputClass}>
                {workflows.map((w) => (
                  <option key={w.id} value={w.id}>
                    {w.name}
                  </option>
                ))}
                <option value={SINGLE}>{t("newRun.workflowSingleOption")}</option>
              </select>
              <Button
                className="h-auto shrink-0"
                onClick={() => {
                  setOpen(false);
                  navigate({ name: "workflows" });
                }}
              >
                {t("newRun.editWorkflows")}
              </Button>
            </div>
          </Field>
          {workflow && <StepChain workflow={workflow} />}
        </div>

        <div>
          <span className="mb-1 block text-[12.5px] font-medium text-fg">{t("newRun.source")}</span>
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
            <p className="mt-1.5 text-[12px] leading-snug text-muted">{t("newRun.issueNote")}</p>
          </div>
        )}

        <Field label={source === "issue" ? t("newRun.issueExtra") : t("newRun.prompt")}>
          <textarea rows={source === "issue" ? 2 : 4} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder={source === "issue" ? undefined : t("newRun.promptPlaceholder")} className={`${inputClass} resize-y leading-relaxed`} />
        </Field>

        {!single ? (
          <>
            {needsGithub && (
              <div role="alert" className="flex items-center gap-2 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12.5px] text-warning">
                <span className="flex-1">{project?.githubRepo ? t("newRun.needsGithub") : t("reasons.noGithubRemote")}</span>
                <Button onClick={finishSetup}>{t("projects.finishSetup")}</Button>
              </div>
            )}
            {project && !project.checkCommand && (
              <div className="flex items-center gap-2 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12.5px] text-warning">
                <span className="flex-1">{t("newRun.checkNone")}</span>
                <Button onClick={finishSetup}>{t("projects.finishSetup")}</Button>
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
              <span className="mb-1 block text-[12.5px] font-medium text-fg">{t("newRun.tools")}</span>
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

        {accounts && (
          <p className="flex gap-2 text-[12px] leading-snug text-muted">
            <UserRound size={14} className="mt-px shrink-0" />
            <span className="selectable min-w-0 break-words">{accounts}</span>
          </p>
        )}
        <p className="flex gap-2 text-[12px] leading-snug text-muted">
          <ShieldCheck size={14} className="mt-px shrink-0" />
          {t("newRun.isolation")}
        </p>
        {(error || adding.error) && (
          <p role="alert" className="text-[12px] text-danger">
            {error ?? adding.error}
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
