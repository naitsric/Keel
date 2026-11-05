import { ArrowDown, ArrowUp, Bot, CheckCircle2, GitPullRequest, Layers, Plus, Trash2, UserCheck, Wrench } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type StepRole, type StepSpec, type ToolProfile, type Workflow, type WorkflowStep } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass, Segmented } from "../ui/primitives";
import { AddProject } from "./AddProject";
import { ModelField } from "./ModelField";

const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
const DEFAULT_EFFORT = "__default__";
const PROMPT_TOKENS = "{{request}}, {{diff}}, {{rework}}, {{steps.spec.output}}";

/** A step's name that isn't taken yet. */
export function freshId(base: string, taken: string[]): string {
  if (!taken.includes(base)) return base;
  let n = 2;
  while (taken.includes(`${base}-${n}`)) n += 1;
  return `${base}-${n}`;
}

export function allIds(steps: WorkflowStep[]): string[] {
  return steps.flatMap((s) => (s.type === "parallel" ? [s.id, ...s.steps.map((x) => x.id)] : [s.id]));
}

export function moved<T>(list: T[], index: number, by: number): T[] {
  const to = index + by;
  if (to < 0 || to >= list.length) return list;
  const next = [...list];
  [next[index], next[to]] = [next[to], next[index]];
  return next;
}

const newAgent = (id: string, role: StepRole, tools: ToolProfile): StepSpec => ({ id, model: "sonnet", effort: null, tools, prompt: "", maxTurns: null, maxBudgetUsd: null, paths: [], role });

/** The fields of an agent, standing alone or inside a group. */
function AgentFields({ spec, earlier, inGroup, onChange }: { spec: StepSpec; earlier: string[]; inGroup: boolean; onChange: (spec: StepSpec) => void }) {
  const { t } = useTranslation();
  const role = spec.role ?? { kind: "worker" };
  const setRole = (kind: StepRole["kind"]) => {
    if (kind === "approver") onChange({ ...spec, tools: "readOnly", role: { kind, reworkTo: earlier[earlier.length - 1] ?? "", maxRounds: 2 } });
    else if (kind === "reviewer" || kind === "learner" || kind === "mapper") onChange({ ...spec, tools: "readOnly", role: { kind } });
    else onChange({ ...spec, role: { kind } });
  };
  const judges = role.kind !== "worker";
  return (
    <div className="space-y-2.5">
      <div className="grid gap-2.5 md:grid-cols-3">
        <Field label={t("workflows.stepId")}>
          <input value={spec.id} onChange={(e) => onChange({ ...spec, id: e.target.value.replace(/[^\w-]/g, "") })} className={`${inputClass} font-mono`} />
        </Field>
        <ModelField label={t("newRun.model")} value={spec.model} onChange={(model) => onChange({ ...spec, model })} />
        <Field label={t("newRun.effort")}>
          <select value={spec.effort ?? DEFAULT_EFFORT} onChange={(e) => onChange({ ...spec, effort: e.target.value === DEFAULT_EFFORT ? null : e.target.value })} className={inputClass}>
            <option value={DEFAULT_EFFORT}>{t("newRun.effortDefault")}</option>
            {EFFORTS.map((level) => (
              <option key={level} value={level}>
                {t(`newRun.effortLevels.${level}`)}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <div className="flex flex-wrap items-end gap-x-4 gap-y-2.5">
        <div>
          <span className="mb-1 block text-[12px] font-medium text-muted">{t("workflows.role")}</span>
          <Segmented<StepRole["kind"]>
            label={t("workflows.role")}
            value={role.kind}
            onChange={setRole}
            options={inGroup ? [{ value: "reviewer", label: t("workflows.roleReviewer") }, { value: "worker", label: t("workflows.roleWorker") }] : [
              { value: "worker", label: t("workflows.roleWorker") },
              { value: "reviewer", label: t("workflows.roleReviewer") },
              { value: "approver", label: t("workflows.roleApprover") },
              { value: "learner", label: t("workflows.roleLearner") },
            ]}
          />
        </div>
        {!judges && !inGroup && (
          <div>
            <span className="mb-1 block text-[12px] font-medium text-muted">{t("newRun.tools")}</span>
            <Segmented<ToolProfile>
              label={t("newRun.tools")}
              value={spec.tools}
              onChange={(tools) => onChange({ ...spec, tools })}
              options={[
                { value: "build", label: t("newRun.toolsBuild") },
                { value: "readOnly", label: t("newRun.toolsReadOnly") },
              ]}
            />
          </div>
        )}
        {role.kind === "approver" && (
          <>
            <div className="w-44">
              <Field label={t("workflows.reworkTo")}>
                <select value={role.reworkTo} onChange={(e) => onChange({ ...spec, role: { ...role, reworkTo: e.target.value } })} className={inputClass}>
                  {earlier.map((id) => (
                    <option key={id} value={id}>
                      {id}
                    </option>
                  ))}
                </select>
              </Field>
            </div>
            <div className="w-32">
              <Field label={t("workflows.maxRounds")}>
                <input inputMode="numeric" value={role.maxRounds} onChange={(e) => onChange({ ...spec, role: { ...role, maxRounds: Number(e.target.value.replace(/\D/g, "")) || 0 } })} className={inputClass} />
              </Field>
            </div>
          </>
        )}
      </div>
      <Field label={t("workflows.paths")} hint={t("workflows.pathsHint")}>
        <input
          value={(spec.paths ?? []).join(", ")}
          onChange={(e) =>
            onChange({
              ...spec,
              paths: e.target.value
                .split(",")
                .map((p) => p.trim())
                .filter(Boolean),
            })
          }
          className={`${inputClass} font-mono`}
        />
      </Field>
      <Field label={t("workflows.prompt")} hint={t("workflows.promptHint", { tokens: PROMPT_TOKENS, interpolation: { escapeValue: false } })}>
        <textarea rows={4} value={spec.prompt} onChange={(e) => onChange({ ...spec, prompt: e.target.value })} className={`${inputClass} resize-y font-mono text-[11.5px] leading-relaxed`} />
      </Field>
    </div>
  );
}

function StepCard({ icon, title, onUp, onDown, onRemove, children }: { icon: ReactNode; title: string; onUp?: () => void; onDown?: () => void; onRemove: () => void; children?: ReactNode }) {
  const { t } = useTranslation();
  return (
    <Card className="p-3.5">
      <div className="flex items-center gap-2">
        <span className="text-muted">{icon}</span>
        <h3 className="flex-1 text-[12.5px] font-semibold">{title}</h3>
        {onUp && (
          <Button variant="ghost" className="px-1.5" aria-label={t("workflows.moveUp")} title={t("workflows.moveUp")} onClick={onUp}>
            <ArrowUp size={13} />
          </Button>
        )}
        {onDown && (
          <Button variant="ghost" className="px-1.5" aria-label={t("workflows.moveDown")} title={t("workflows.moveDown")} onClick={onDown}>
            <ArrowDown size={13} />
          </Button>
        )}
        <Button variant="ghost" className="px-1.5" aria-label={t("workflows.remove")} title={t("workflows.remove")} onClick={onRemove}>
          <Trash2 size={13} />
        </Button>
      </div>
      {children && <div className="mt-2.5">{children}</div>}
    </Card>
  );
}

function StepEditor({ step, earlier, onChange, onUp, onDown, onRemove }: { step: WorkflowStep; earlier: string[]; onChange: (step: WorkflowStep) => void; onUp?: () => void; onDown?: () => void; onRemove: () => void }) {
  const { t } = useTranslation();
  const common = { onUp, onDown, onRemove };
  switch (step.type) {
    case "agent":
      return (
        <StepCard icon={<Bot size={14} />} title={t("workflows.stepAgent")} {...common}>
          <AgentFields spec={step} earlier={earlier} inGroup={false} onChange={(spec) => onChange({ type: "agent", ...spec })} />
        </StepCard>
      );
    case "setup":
    case "check":
      return (
        <StepCard icon={step.type === "check" ? <CheckCircle2 size={14} /> : <Wrench size={14} />} title={step.type === "check" ? t("workflows.stepCheck") : t("workflows.stepSetup")} {...common}>
          <div className="grid gap-2.5 md:grid-cols-[1fr_2fr]">
            <Field label={t("workflows.stepId")}>
              <input value={step.id} onChange={(e) => onChange({ ...step, id: e.target.value.replace(/[^\w-]/g, "") })} className={`${inputClass} font-mono`} />
            </Field>
            <Field label={t("workflows.command")}>
              <input value={step.run} onChange={(e) => onChange({ ...step, run: e.target.value })} className={`${inputClass} font-mono`} />
            </Field>
          </div>
        </StepCard>
      );
    case "parallel":
      return (
        <StepCard icon={<Layers size={14} />} title={t("workflows.stepParallel")} {...common}>
          <div className="space-y-2.5">
            <div className="max-w-xs">
              <Field label={t("workflows.stepId")}>
                <input value={step.id} onChange={(e) => onChange({ ...step, id: e.target.value.replace(/[^\w-]/g, "") })} className={`${inputClass} font-mono`} />
              </Field>
            </div>
            {step.steps.map((spec, index) => (
              <div key={index} className="rounded-md border border-border bg-inset p-3">
                <div className="mb-2 flex justify-end">
                  <Button variant="ghost" className="px-1.5" aria-label={t("workflows.remove")} title={t("workflows.remove")} onClick={() => onChange({ ...step, steps: step.steps.filter((_, i) => i !== index) })}>
                    <Trash2 size={13} />
                  </Button>
                </div>
                <AgentFields spec={spec} earlier={earlier} inGroup onChange={(next) => onChange({ ...step, steps: step.steps.map((s, i) => (i === index ? next : s)) })} />
              </div>
            ))}
            <Button onClick={() => onChange({ ...step, steps: [...step.steps, newAgent(freshId("reviewer", step.steps.map((s) => s.id)), { kind: "reviewer" }, "readOnly")] })}>
              <Plus size={13} />
              {t("workflows.addReviewer")}
            </Button>
          </div>
        </StepCard>
      );
    case "approval":
      return (
        <StepCard icon={<UserCheck size={14} />} title={t("workflows.stepApproval")} {...common}>
          <p className="text-[12px] text-muted">{t("workflows.approvalHint")}</p>
        </StepCard>
      );
    case "deliver":
      return (
        <StepCard icon={<GitPullRequest size={14} />} title={t("workflows.stepDeliver")} {...common}>
          <p className="text-[12px] text-muted">{t("workflows.deliverHint")}</p>
        </StepCard>
      );
  }
}

export function WorkflowsView() {
  const { t, i18n } = useTranslation();
  const projects = useApp((s) => s.overview.projects);
  const [projectId, setProjectId] = useState("");
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [draft, setDraft] = useState<Workflow | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projects, projectId]);

  const load = useCallback(
    async (select?: string) => {
      if (!projectId) return;
      const list = await api.workflowsList(projectId, i18n.language);
      setWorkflows(list);
      setDraft((current) => list.find((w) => w.id === (select ?? current?.id)) ?? list[0] ?? null);
    },
    [projectId, i18n.language],
  );

  useEffect(() => {
    load().catch((e) => setError(errorText(t, e)));
  }, [load, t]);

  const save = async () => {
    if (!draft) return;
    setError(null);
    try {
      const result = await api.workflowSave(projectId, draft);
      await load(result.id);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  const remove = async () => {
    if (!draft || draft.builtin) return;
    await api.workflowDelete(projectId, draft.id);
    await load(workflows[0]?.id);
  };

  const steps = draft?.steps ?? [];
  const setSteps = (next: WorkflowStep[]) => draft && setDraft({ ...draft, steps: next });
  const add = (step: WorkflowStep) => setSteps([...steps, step]);
  const taken = allIds(steps);

  return (
    <>
      <main className="flex-1 overflow-y-auto p-5">
        {/* The top bar already says which page this is. */}
        <h1 className="sr-only">{t("workflows.title")}</h1>
        <p className="text-[12.5px] text-muted">{t("workflows.subtitle")}</p>
        {projects.length === 0 ? (
          <div className="mt-4">
            <p className="text-[12.5px] text-subtle">{t("workflows.noProjects")}</p>
            <AddProject className="mt-2" variant="primary" />
          </div>
        ) : (
          <div className="mt-4 grid gap-4 lg:grid-cols-[240px_1fr]">
            <div className="space-y-3 self-start">
              <Field label={t("workflows.project")}>
                <select value={projectId} onChange={(e) => setProjectId(e.target.value)} className={inputClass}>
                  {projects.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name}
                    </option>
                  ))}
                </select>
              </Field>
              <Card className="p-1.5">
                <ul className="space-y-0.5">
                  {workflows.map((w) => (
                    <li key={w.id}>
                      <button type="button" onClick={() => setDraft(w)} className={cx("w-full rounded-md px-2 py-1.5 text-left text-[12.5px] hover:bg-hover", draft?.id === w.id && "bg-active font-medium")}>
                        {w.name}
                        {w.builtin && <span className="block text-[11px] font-normal text-subtle">{t("workflows.builtin")}</span>}
                      </button>
                    </li>
                  ))}
                </ul>
              </Card>
            </div>
            {draft && (
              <div className="min-w-0 space-y-3">
                <div className="flex flex-wrap items-end gap-2">
                  <div className="min-w-56 flex-1">
                    <Field label={t("workflows.name")}>
                      <input value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} className={inputClass} />
                    </Field>
                  </div>
                  <Button variant="primary" onClick={() => void save()}>
                    {draft.builtin ? t("workflows.saveCopy") : t("workflows.save")}
                  </Button>
                  {!draft.builtin && (
                    <Button variant="danger" onClick={() => void remove()}>
                      {t("workflows.delete")}
                    </Button>
                  )}
                  {saved && <span className="pb-1.5 text-[12px] text-success">{t("workflows.saved")}</span>}
                </div>
                {draft.builtin && <p className="text-[12px] text-subtle">{t("workflows.builtinNote")}</p>}
                {error && (
                  <p role="alert" className="text-[12.5px] text-danger">
                    {error}
                  </p>
                )}
                {steps.map((step, index) => (
                  <StepEditor
                    // Steps have no identity of their own while their names are being typed.
                    key={index}
                    step={step}
                    earlier={steps.slice(0, index).filter((s) => s.type !== "parallel").map((s) => s.id)}
                    onChange={(next) => setSteps(steps.map((s, i) => (i === index ? next : s)))}
                    onUp={index > 0 ? () => setSteps(moved(steps, index, -1)) : undefined}
                    onDown={index < steps.length - 1 ? () => setSteps(moved(steps, index, 1)) : undefined}
                    onRemove={() => setSteps(steps.filter((_, i) => i !== index))}
                  />
                ))}
                <div className="flex flex-wrap items-center gap-2 pt-1">
                  <span className="text-[12px] font-medium text-muted">{t("workflows.add")}</span>
                  <Button onClick={() => add({ type: "agent", ...newAgent(freshId("agent", taken), { kind: "worker" }, "build") })}>{t("workflows.addAgent")}</Button>
                  <Button onClick={() => add({ type: "parallel", id: freshId("reviews", taken), steps: [newAgent(freshId("reviewer", taken), { kind: "reviewer" }, "readOnly")] })}>{t("workflows.addParallel")}</Button>
                  <Button onClick={() => add({ type: "check", id: freshId("check", taken), run: "" })}>{t("workflows.addCheck")}</Button>
                  <Button onClick={() => add({ type: "approval", id: freshId("approval", taken) })}>{t("workflows.addApproval")}</Button>
                  <Button onClick={() => add({ type: "deliver", id: freshId("pull-request", taken) })}>{t("workflows.addDeliver")}</Button>
                </div>
              </div>
            )}
          </div>
        )}
      </main>
    </>
  );
}
