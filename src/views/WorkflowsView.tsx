import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Bot, CheckCircle2, GitPullRequest, Layers, Plus, Trash2, TriangleAlert, UserCheck, Wrench } from "lucide-react";
import { type KeyboardEvent, type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type CommandError, type StepCondition, type StepRole, type StepSpec, type ToolProfile, type Workflow, type WorkflowDraft, type WorkflowResults, type WorkflowStep } from "../lib/api";
import { draftFacts } from "../lib/drafts";
import { flowOf, pathKey, pathOfKey, problemKeys, type StepPath } from "../lib/flow";
import { formatRelative } from "../lib/format";
import { errorText, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass, Segmented } from "../ui/primitives";
import { AddProject } from "./AddProject";
import { FlowDiagram } from "./FlowDiagram";
import { ModelField } from "./ModelField";
import { WorkflowResultsCard } from "./WorkflowResultsCard";

const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
const DEFAULT_EFFORT = "__default__";
const PROMPT_TOKENS = "{{request}}, {{diff}}, {{rework}}, {{answers}}, {{check}}, {{steps.spec.output}}";

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

/** A change to the steps, and which one to show after it. */
export interface StepsEdit {
  steps: WorkflowStep[];
  selected: StepPath | null;
}

/** Whether a path still leads to a step: an edit or another workflow can leave it pointing at nothing. */
export function pathExists(steps: WorkflowStep[], path: StepPath | null): path is StepPath {
  if (!path) return false;
  const stage = steps[path.stage];
  if (!stage) return false;
  return path.member === null || (stage.type === "parallel" && stage.steps[path.member] !== undefined);
}

export const insertedStep = (steps: WorkflowStep[], at: number, step: WorkflowStep): StepsEdit => ({ steps: [...steps.slice(0, at), step, ...steps.slice(at)], selected: { stage: at, member: null } });

export function memberAdded(steps: WorkflowStep[], stage: number, spec: StepSpec): StepsEdit {
  const group = steps[stage];
  if (group?.type !== "parallel") return { steps, selected: { stage, member: null } };
  return { steps: steps.map((s, i) => (i === stage ? { ...group, steps: [...group.steps, spec] } : s)), selected: { stage, member: group.steps.length } };
}

/** The selection goes with the step that moved. */
export function movedStep(steps: WorkflowStep[], path: StepPath, by: number): StepsEdit {
  const group = steps[path.stage];
  if (path.member !== null && group?.type === "parallel") {
    const members = moved(group.steps, path.member, by);
    return { steps: steps.map((s, i) => (i === path.stage ? { ...group, steps: members } : s)), selected: { stage: path.stage, member: members === group.steps ? path.member : path.member + by } };
  }
  const next = moved(steps, path.stage, by);
  return { steps: next, selected: { stage: next === steps ? path.stage : path.stage + by, member: null } };
}

/** Then what took its place, or the one before it, or the group when its last member goes. */
export function removedStep(steps: WorkflowStep[], path: StepPath): StepsEdit {
  const group = steps[path.stage];
  if (path.member !== null && group?.type === "parallel") {
    const members = group.steps.filter((_, i) => i !== path.member);
    const member = Math.min(path.member, members.length - 1);
    return { steps: steps.map((s, i) => (i === path.stage ? { ...group, steps: members } : s)), selected: { stage: path.stage, member: member < 0 ? null : member } };
  }
  const next = steps.filter((_, i) => i !== path.stage);
  return { steps: next, selected: next.length === 0 ? null : { stage: Math.min(path.stage, next.length - 1), member: null } };
}

const newAgent = (id: string, role: StepRole, tools: ToolProfile): StepSpec => ({ id, model: "sonnet", effort: null, tools, prompt: "", maxTurns: null, maxBudgetUsd: null, paths: [], role });

const ALWAYS = "always";

/** A step with this condition. A step with none says nothing about it, like one saved before there were any. */
export function withWhen(spec: StepSpec, when: StepCondition | typeof ALWAYS): StepSpec {
  const { when: _, ...rest } = spec;
  return when === ALWAYS ? rest : { ...rest, when };
}

/** The fields of an agent, standing alone or inside a group. */
function AgentFields({ spec, earlier, inGroup, onChange }: { spec: StepSpec; earlier: string[]; inGroup: boolean; onChange: (spec: StepSpec) => void }) {
  const { t } = useTranslation();
  const role = spec.role ?? { kind: "worker" };
  const setRole = (kind: StepRole["kind"]) => {
    if (kind === "approver") onChange({ ...spec, tools: "readOnly", role: { kind, reworkTo: earlier[earlier.length - 1] ?? "", maxRounds: 2 } });
    else if (kind === "reviewer" || kind === "learner" || kind === "mapper" || kind === "specifier" || kind === "verifier") onChange({ ...spec, tools: "readOnly", role: { kind } });
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
              { value: "specifier", label: t("workflows.roleSpecifier") },
              { value: "verifier", label: t("workflows.roleVerifier") },
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
      <div>
        <span className="mb-1 block text-[12px] font-medium text-muted">{t("workflows.when")}</span>
        <Segmented<StepCondition | typeof ALWAYS>
          label={t("workflows.when")}
          value={spec.when ?? ALWAYS}
          onChange={(when) => onChange(withWhen(spec, when))}
          options={[
            { value: ALWAYS, label: t("workflows.whenAlways") },
            { value: "notSmall", label: t("workflows.whenNotSmall") },
          ]}
        />
        <p className="mt-1 text-[11.5px] leading-snug text-subtle">{t("workflows.whenHint")}</p>
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

/**
 * A step's form and what can be done with it. The move buttons stay where they are at the ends, only
 * inactive, because taking away the one that has focus would throw the person out of the page.
 */
function StepCard({ icon, title, vertical = false, onEarlier, onLater, onRemove, children }: { icon: ReactNode; title: string; vertical?: boolean; onEarlier?: () => void; onLater?: () => void; onRemove: () => void; children?: ReactNode }) {
  const { t } = useTranslation();
  return (
    <Card className="p-3.5">
      <div className="flex items-center gap-2">
        <span className="text-muted">{icon}</span>
        <h3 className="flex-1 text-[12.5px] font-semibold">{title}</h3>
        <Button variant="ghost" className={cx("px-1.5", !onEarlier && "opacity-40")} aria-label={t("workflows.moveEarlier")} aria-disabled={!onEarlier} title={t("workflows.moveEarlier")} onClick={onEarlier}>
          {vertical ? <ArrowUp size={13} /> : <ArrowLeft size={13} />}
        </Button>
        <Button variant="ghost" className={cx("px-1.5", !onLater && "opacity-40")} aria-label={t("workflows.moveLater")} aria-disabled={!onLater} title={t("workflows.moveLater")} onClick={onLater}>
          {vertical ? <ArrowDown size={13} /> : <ArrowRight size={13} />}
        </Button>
        <Button variant="ghost" className="px-1.5" aria-label={t("workflows.remove")} title={t("workflows.remove")} onClick={onRemove}>
          <Trash2 size={13} />
        </Button>
      </div>
      {children && <div className="mt-2.5">{children}</div>}
    </Card>
  );
}

/** The form of one step of a group, which is what opens when it's selected in the diagram. */
function MemberEditor({ spec, group, formKey, earlier, onChange, onEarlier, onLater, onRemove }: { spec: StepSpec; group: string; formKey: string; earlier: string[]; onChange: (spec: StepSpec) => void; onEarlier?: () => void; onLater?: () => void; onRemove: () => void }) {
  const { t } = useTranslation();
  return (
    <StepCard icon={<Bot size={14} />} title={t("flow.memberOf", { group: group || t("flow.unnamed") })} vertical onEarlier={onEarlier} onLater={onLater} onRemove={onRemove}>
      <AgentFields key={formKey} spec={spec} earlier={earlier} inGroup onChange={onChange} />
    </StepCard>
  );
}

function StepEditor({ step, formKey, earlier, onChange, onEarlier, onLater, onRemove, onSelectMember, onAddMember, onRemoveMember }: { step: WorkflowStep; formKey: string; earlier: string[]; onChange: (step: WorkflowStep) => void; onEarlier?: () => void; onLater?: () => void; onRemove: () => void; onSelectMember: (member: number) => void; onAddMember: () => void; onRemoveMember: (member: number) => void }) {
  const { t } = useTranslation();
  const common = { onEarlier, onLater, onRemove };
  switch (step.type) {
    case "agent":
      return (
        <StepCard icon={<Bot size={14} />} title={t("workflows.stepAgent")} {...common}>
          <AgentFields key={formKey} spec={step} earlier={earlier} inGroup={false} onChange={(spec) => onChange({ type: "agent", ...spec })} />
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
            {step.steps.length > 0 && (
              <div>
                <span className="mb-1 block text-[12px] font-medium text-muted">{t("workflows.members")}</span>
                <ul className="space-y-1">
                  {step.steps.map((spec, index) => (
                    <li key={index} className="flex items-center gap-1 rounded-md border border-border bg-inset pr-1">
                      <button type="button" onClick={() => onSelectMember(index)} className="flex min-w-0 flex-1 items-baseline gap-2 rounded-md px-2 py-1.5 text-left hover:bg-hover">
                        <span className="min-w-0 truncate font-mono text-[12px] font-medium">{spec.id || t("flow.unnamed")}</span>
                        <span className="min-w-0 truncate font-mono text-[11px] text-subtle">{spec.model}</span>
                      </button>
                      <Button variant="ghost" className="px-1.5" aria-label={t("workflows.remove")} title={t("workflows.remove")} onClick={() => onRemoveMember(index)}>
                        <Trash2 size={13} />
                      </Button>
                    </li>
                  ))}
                </ul>
              </div>
            )}
            <Button onClick={onAddMember}>
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

/** What a + opens: the same kinds of step as always, right below the diagram. */
function AddBar({ onPick, onCancel }: { onPick: (kind: "agent" | "parallel" | "check" | "approval" | "deliver") => void; onCancel: () => void }) {
  const { t } = useTranslation();
  const escape = (e: KeyboardEvent) => {
    if (e.key === "Escape") onCancel();
  };
  return (
    <div className="mt-2 flex flex-wrap items-center gap-2 border-t border-border px-1 pt-2.5" onKeyDown={escape}>
      <span className="text-[12px] font-medium text-muted">{t("workflows.add")}</span>
      <Button autoFocus onClick={() => onPick("agent")}>
        {t("workflows.addAgent")}
      </Button>
      <Button onClick={() => onPick("parallel")}>{t("workflows.addParallel")}</Button>
      <Button onClick={() => onPick("check")}>{t("workflows.addCheck")}</Button>
      <Button onClick={() => onPick("approval")}>{t("workflows.addApproval")}</Button>
      <Button onClick={() => onPick("deliver")}>{t("workflows.addDeliver")}</Button>
      <Button variant="ghost" onClick={onCancel}>
        {t("newRun.cancel")}
      </Button>
    </div>
  );
}

/** What saving this proposed workflow would let happen, from what's on screen, so it follows every edit. */
function DraftReview({ draft, steps, project }: { draft: WorkflowDraft; steps: WorkflowStep[]; project: string }) {
  const { t } = useTranslation();
  const facts = draftFacts(steps);
  const label = "text-[12px] font-medium text-muted";
  return (
    <Card aria-label={t("workflows.draftTitle", { client: draft.proposedBy, project })} className="space-y-2.5 border-accent p-3.5">
      <div>
        <h2 className="selectable text-[12.5px] font-semibold break-words">{t("workflows.draftTitle", { client: draft.proposedBy, project })}</h2>
        <p className="mt-0.5 text-[12px] text-muted">{t("workflows.draftNote")}</p>
      </div>
      <dl className="space-y-2 text-[12.5px]">
        <div>
          <dt className={label}>{t("workflows.draftCommands")}</dt>
          {facts.commands.length === 0 && <dd>{t("workflows.draftNoCommands")}</dd>}
          {facts.commands.map((command) => (
            <dd key={command.id} className="selectable font-mono text-[11.5px] break-words">
              {command.run}
            </dd>
          ))}
        </div>
        <div>
          <dt className={label}>{t("workflows.draftBuilders")}</dt>
          <dd className={cx(facts.builders.length > 0 && "font-mono text-[11.5px] break-words")}>{facts.builders.length > 0 ? facts.builders.join(", ") : t("workflows.draftNoBuilders")}</dd>
        </div>
        <div>
          <dt className={label}>{t("workflows.draftDelivery")}</dt>
          <dd>{facts.delivers ? t("workflows.draftDelivers") : t("workflows.draftNoDelivery")}</dd>
        </div>
        {facts.conditional.length > 0 && (
          <div>
            <dt className={label}>{t("workflows.draftConditional")}</dt>
            <dd className="font-mono text-[11.5px] break-words">{facts.conditional.join(", ")}</dd>
          </div>
        )}
      </dl>
      {facts.afterApproval.length > 0 && (
        <p role="alert" className="flex gap-1.5 text-[12.5px] font-medium text-warning">
          <TriangleAlert size={14} className="mt-0.5 shrink-0" />
          <span className="min-w-0 break-words">{t("workflows.draftAfterApproval", { count: facts.afterApproval.length, steps: facts.afterApproval.join(", ") })}</span>
        </p>
      )}
      <p className="text-[12px] text-muted">{t("workflows.draftPrompts")}</p>
    </Card>
  );
}

export function WorkflowsView() {
  const { t, i18n } = useTranslation();
  const projects = useApp((s) => s.overview.projects);
  const revision = useApp((s) => s.revision);
  const [projectId, setProjectId] = useState("");
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [editing, setEditing] = useState<Workflow | null>(null);
  const [drafts, setDrafts] = useState<WorkflowDraft[]>([]);
  const [results, setResults] = useState<WorkflowResults[]>([]);
  // The proposed workflow in the editor. It isn't a workflow until it's saved, so it has no id there.
  const [reviewing, setReviewing] = useState<WorkflowDraft | null>(null);
  const openingDraft = useRef(false);
  const saving = useRef(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  // By position, not name: names change with every key while they're being typed.
  const [selected, setSelected] = useState<StepPath | null>(null);
  const [adding, setAdding] = useState<number | null>(null);
  const [problem, setProblem] = useState<{ problem: string; step: string } | null>(null);
  const [focusRequest, setFocusRequest] = useState<{ key: string } | null>(null);
  // Goes up whenever another step can land on the same position, such as after a delete. The form
  // starts over then, or the model picker would keep what it knew about the step that was there.
  const [formGeneration, setFormGeneration] = useState(0);

  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projects, projectId]);

  const load = useCallback(
    async (select?: string) => {
      if (!projectId) return;
      const list = await api.workflowsList(projectId, i18n.language);
      setWorkflows(list);
      // Opening a draft of another project changes the project too, and the draft is what stays open.
      if (openingDraft.current) {
        openingDraft.current = false;
        return;
      }
      setReviewing(null);
      setEditing((current) => list.find((w) => w.id === (select ?? current?.id)) ?? list[0] ?? null);
    },
    [projectId, i18n.language],
  );

  useEffect(() => {
    load().catch((e) => setError(errorText(t, e)));
  }, [load, t]);

  const loadDrafts = useCallback(async () => setDrafts(await api.workflowDraftsList()), []);
  // Only the list: one arriving never opens by itself or touches what's in the editor.
  useEffect(() => {
    loadDrafts().catch((e) => setError(errorText(t, e)));
  }, [loadDrafts, revision, t]);

  // What a pull request became arrives as a change like any other.
  useEffect(() => {
    if (!projectId) return;
    let current = true;
    api
      .workflowResults(projectId)
      .then((found) => current && setResults(found))
      .catch((e) => current && setError(errorText(t, e)));
    return () => {
      current = false;
    };
  }, [projectId, revision, t]);

  const steps = editing?.steps ?? [];
  const flow = useMemo(() => flowOf(steps), [steps]);
  const shown: StepPath | null = pathExists(steps, selected) ? selected : steps.length > 0 ? { stage: 0, member: null } : null;

  const save = async () => {
    // Saving a draft takes it away, so a double click would find it gone.
    if (!editing || saving.current) return;
    saving.current = true;
    setError(null);
    setProblem(null);
    try {
      const result = reviewing ? await api.workflowDraftSave(reviewing.projectId, reviewing.id, editing) : await api.workflowSave(projectId, editing);
      await load(result.id);
      if (reviewing) await loadDrafts();
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setError(errorText(t, e));
      const reason = (e as Partial<CommandError> | null)?.reason;
      if (reason?.code !== "invalidWorkflow") return;
      const step = String(reason.step ?? "");
      setProblem({ problem: String(reason.problem), step });
      // Straight to the form that can fix it, unless that's where the person already is.
      const keys = problemKeys(flow, step);
      if (keys.length > 0 && !(shown && keys.includes(pathKey(shown)))) setSelected(pathOfKey(keys[0]));
    } finally {
      saving.current = false;
    }
  };

  /** On another workflow, the last one's selection and problem mean nothing. */
  const startOver = () => {
    setSelected(null);
    setAdding(null);
    setProblem(null);
    setError(null);
    setFocusRequest(null);
    setFormGeneration((generation) => generation + 1);
  };
  const open = (workflow: Workflow) => {
    setEditing(workflow);
    setReviewing(null);
    startOver();
  };
  const openDraft = (proposal: WorkflowDraft) => {
    startOver();
    setEditing({ id: "", name: proposal.name, steps: proposal.steps, builtin: false });
    setReviewing(proposal);
    if (proposal.projectId !== projectId) {
      openingDraft.current = true;
      setProjectId(proposal.projectId);
    }
  };
  const discard = async () => {
    if (!reviewing) return;
    setError(null);
    try {
      await api.workflowDraftDiscard(reviewing.projectId, reviewing.id);
      startOver();
      await load();
      await loadDrafts();
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  const remove = async () => {
    if (!editing || editing.builtin) return;
    await api.workflowDelete(projectId, editing.id);
    startOver();
    await load(workflows[0]?.id);
  };

  /** What's being typed into the form of the step on show. */
  const changeSteps = (next: WorkflowStep[]) => editing && setEditing({ ...editing, steps: next });
  const apply = (edit: StepsEdit) => {
    changeSteps(edit.steps);
    setSelected(edit.selected);
    setAdding(null);
    setFormGeneration((generation) => generation + 1);
  };
  /** After an action that takes away the button it was done with. */
  const focusAfter = (edit: StepsEdit) => {
    apply(edit);
    setFocusRequest({ key: edit.selected ? pathKey(edit.selected) : "gap0" });
  };
  const taken = allIds(steps);

  const newStep = (kind: "agent" | "parallel" | "check" | "approval" | "deliver"): WorkflowStep => {
    switch (kind) {
      case "agent":
        return { type: "agent", ...newAgent(freshId("agent", taken), { kind: "worker" }, "build") };
      case "parallel":
        return { type: "parallel", id: freshId("reviews", taken), steps: [newAgent(freshId("reviewer", taken), { kind: "reviewer" }, "readOnly")] };
      case "check":
        return { type: "check", id: freshId("check", taken), run: "" };
      case "approval":
        return { type: "approval", id: freshId("approval", taken) };
      case "deliver":
        return { type: "deliver", id: freshId("pull-request", taken) };
    }
  };
  const addMember = (stage: number) => focusAfter(memberAdded(steps, stage, newAgent(freshId("reviewer", taken), { kind: "reviewer" }, "readOnly")));

  const problems: Record<string, string> = {};
  if (problem) {
    for (const key of problemKeys(flow, problem.step)) problems[key] = reasonText(t, { code: "invalidWorkflow", ...problem });
  }

  const stage = shown ? steps[shown.stage] : undefined;
  const member = shown && stage?.type === "parallel" && shown.member !== null ? stage.steps[shown.member] : undefined;
  const earlier = shown ? steps.slice(0, shown.stage).filter((s) => s.type !== "parallel").map((s) => s.id) : [];
  const moveIf = (path: StepPath, by: number, count: number) => {
    const index = path.member ?? path.stage;
    return index + by >= 0 && index + by < count ? () => apply(movedStep(steps, path, by)) : undefined;
  };

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
                <select
                  value={projectId}
                  onChange={(e) => {
                    setProjectId(e.target.value);
                    startOver();
                  }}
                  className={inputClass}
                >
                  {projects.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name}
                    </option>
                  ))}
                </select>
              </Field>
              {drafts.length > 0 && (
                <div>
                  <h2 className="mb-1 text-[12px] font-medium text-muted">{t("workflows.draftsTitle")}</h2>
                  <Card className="border-accent p-1.5">
                    <ul className="space-y-0.5">
                      {drafts.map((proposal) => (
                        <li key={proposal.id}>
                          <button type="button" onClick={() => openDraft(proposal)} className={cx("w-full rounded-md px-2 py-1.5 text-left text-[12.5px] break-words hover:bg-hover", reviewing?.id === proposal.id && "bg-active font-medium")}>
                            {proposal.name}
                            <span className="block text-[11px] font-normal text-subtle">{t("workflows.draftFrom", { client: proposal.proposedBy, when: formatRelative(proposal.createdAt), project: projects.find((p) => p.id === proposal.projectId)?.name ?? "" })}</span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  </Card>
                </div>
              )}
              <Card className="p-1.5">
                <ul className="space-y-0.5">
                  {workflows.map((w) => (
                    <li key={w.id}>
                      <button type="button" onClick={() => open(w)} className={cx("w-full rounded-md px-2 py-1.5 text-left text-[12.5px] hover:bg-hover", editing?.id === w.id && "bg-active font-medium")}>
                        {w.name}
                        {w.builtin && <span className="block text-[11px] font-normal text-subtle">{t("workflows.builtin")}</span>}
                      </button>
                    </li>
                  ))}
                </ul>
              </Card>
              <Button onClick={() => open({ id: "", name: t("workflows.newName"), steps: [], builtin: false })}>
                <Plus size={13} />
                {t("workflows.new")}
              </Button>
            </div>
            {editing && (
              <div className="min-w-0 space-y-3">
                <div className="flex flex-wrap items-end gap-2">
                  <div className="min-w-56 flex-1">
                    <Field label={t("workflows.name")}>
                      <input value={editing.name} onChange={(e) => setEditing({ ...editing, name: e.target.value })} className={inputClass} />
                    </Field>
                  </div>
                  <Button variant="primary" onClick={() => void save()}>
                    {editing.builtin ? t("workflows.saveCopy") : t("workflows.save")}
                  </Button>
                  {!editing.builtin && editing.id && (
                    <Button variant="danger" onClick={() => void remove()}>
                      {t("workflows.delete")}
                    </Button>
                  )}
                  {reviewing && (
                    <Button variant="danger" onClick={() => void discard()}>
                      {t("workflows.draftDiscard")}
                    </Button>
                  )}
                  {saved && <span className="pb-1.5 text-[12px] text-success">{t("workflows.saved")}</span>}
                </div>
                {editing.builtin && <p className="text-[12px] text-subtle">{t("workflows.builtinNote")}</p>}
                {reviewing && <DraftReview draft={reviewing} steps={steps} project={projects.find((p) => p.id === reviewing.projectId)?.name ?? ""} />}
                {error && (
                  <p role="alert" className="text-[12.5px] text-danger">
                    {error}
                  </p>
                )}
                <Card className="p-3">
                  <h2 className="px-1 pb-2 text-[12px] font-semibold text-muted">{t("run.stepsTitle")}</h2>
                  <FlowDiagram
                    flow={flow}
                    label={t("run.stepsTitle")}
                    editing
                    selected={shown ? pathKey(shown) : null}
                    onSelect={(key) => {
                      setSelected(pathOfKey(key));
                      setAdding(null);
                    }}
                    problems={problems}
                    adding={adding}
                    onAdd={(gap) => setAdding(adding === gap ? null : gap)}
                    onAddMember={addMember}
                    focusRequest={focusRequest}
                  />
                  {adding !== null && (
                    <AddBar
                      key={adding}
                      onPick={(kind) => focusAfter(insertedStep(steps, adding, newStep(kind)))}
                      onCancel={() => {
                        setFocusRequest({ key: `gap${adding}` });
                        setAdding(null);
                      }}
                    />
                  )}
                </Card>
                {shown && stage?.type === "parallel" && member ? (
                  <MemberEditor
                    spec={member}
                    group={stage.id}
                    formKey={`${pathKey(shown)}/${formGeneration}`}
                    earlier={earlier}
                    onChange={(next) => changeSteps(steps.map((s, i) => (i === shown.stage ? { ...stage, steps: stage.steps.map((m, j) => (j === shown.member ? next : m)) } : s)))}
                    onEarlier={moveIf(shown, -1, stage.steps.length)}
                    onLater={moveIf(shown, 1, stage.steps.length)}
                    onRemove={() => focusAfter(removedStep(steps, shown))}
                  />
                ) : (
                  shown &&
                  stage && (
                    <StepEditor
                      step={stage}
                      formKey={`${pathKey(shown)}/${formGeneration}`}
                      earlier={earlier}
                      onChange={(next) => changeSteps(steps.map((s, i) => (i === shown.stage ? next : s)))}
                      onEarlier={moveIf(shown, -1, steps.length)}
                      onLater={moveIf(shown, 1, steps.length)}
                      onRemove={() => focusAfter(removedStep(steps, shown))}
                      onSelectMember={(index) => setSelected({ stage: shown.stage, member: index })}
                      onAddMember={() => addMember(shown.stage)}
                      onRemoveMember={(index) => focusAfter({ ...removedStep(steps, { stage: shown.stage, member: index }), selected: shown })}
                    />
                  )
                )}
              </div>
            )}
          </div>
        )}
        {projects.length > 0 && <WorkflowResultsCard results={results} workflows={workflows} />}
      </main>
    </>
  );
}
