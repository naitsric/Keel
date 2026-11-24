import { AlertTriangle, Bot, CheckCircle2, Eye, FileText, GitPullRequest, Layers, Lightbulb, ListChecks, Map as MapIcon, Plus, Radar, ShieldCheck, UserCheck, Wrench } from "lucide-react";
import type { TFunction } from "i18next";
import { type FocusEvent, Fragment, type KeyboardEvent, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { AttemptState, StepRole } from "../lib/api";
import { type Flow, type FlowAttempt, type FlowNode, layoutFlow, nextItem, type PlacedNode, scrollToShow } from "../lib/flow";
import { putOffForSetup } from "../lib/audit";
import { skippedAsSmall, skippedAsUnchanged, skippedWithNothingToFix } from "../lib/size";
import { cx, StateBadge } from "../ui/primitives";

const KIND_LABELS: Record<FlowNode["kind"], string> = {
  agent: "workflows.stepAgent",
  setup: "workflows.stepSetup",
  check: "workflows.stepCheck",
  approval: "workflows.stepApproval",
  deliver: "workflows.stepDeliver",
  watch: "workflows.stepWatch",
};
const ROLE_LABELS: Record<StepRole["kind"], string> = {
  worker: "workflows.roleWorker",
  reviewer: "workflows.roleReviewer",
  approver: "workflows.roleApprover",
  mapper: "flow.roleMapper",
  learner: "workflows.roleLearner",
  specifier: "workflows.roleSpecifier",
  verifier: "workflows.roleVerifier",
};

const NODE_TONES: Partial<Record<AttemptState | "pending", string>> = {
  pending: "border-dashed opacity-60",
  preparing: "border-accent/60",
  running: "border-accent/60",
  // Whatever waits for the person stands out from the rest.
  waitingApproval: "border-warning/70 bg-warning/10",
  waitingAnswers: "border-warning/70 bg-warning/10",
  watching: "border-accent/60",
  stalled: "border-warning/70 bg-warning/10",
  blocked: "border-warning/70 bg-warning/10",
  interrupted: "border-warning/70 bg-warning/10",
  failed: "border-danger/60 bg-danger/10",
  skipped: "border-dashed opacity-70",
  abandoned: "opacity-70",
  rejected: "opacity-70",
  returned: "opacity-70",
  postponed: "opacity-70",
  stopped: "opacity-70",
};

const PLUS = "absolute flex size-5 -translate-x-1/2 -translate-y-1/2 items-center justify-center rounded-full border border-border-strong bg-panel text-muted transition-colors hover:bg-hover hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent";

function StepIcon({ node }: { node: FlowNode<FlowAttempt> }) {
  const icon = () => {
    if (node.kind === "setup") return <Wrench size={12} />;
    if (node.kind === "check") return <CheckCircle2 size={12} />;
    if (node.kind === "approval") return <UserCheck size={12} />;
    if (node.kind === "deliver") return <GitPullRequest size={12} />;
    if (node.kind === "watch") return <Radar size={12} />;
    if (node.role === "reviewer") return <Eye size={12} />;
    if (node.role === "approver") return <ShieldCheck size={12} />;
    if (node.role === "learner") return <Lightbulb size={12} />;
    if (node.role === "mapper") return <MapIcon size={12} />;
    if (node.role === "specifier") return <FileText size={12} />;
    if (node.role === "verifier") return <ListChecks size={12} />;
    return <Bot size={12} />;
  };
  return <span className="shrink-0 text-subtle">{icon()}</span>;
}

/** The round it last ran in, or how many times, when there's something to tell. Null otherwise. */
function roundText(t: TFunction, node: FlowNode<FlowAttempt>): string | null {
  if (node.outdated) return null;
  if (node.round > 0) return t("run.round", { number: node.round + 1 });
  return node.attempts.length > 1 ? t("flow.attempts", { count: node.attempts.length }) : null;
}

const skippedForBeingSmall = (node: FlowNode<FlowAttempt>) => node.attempt !== null && skippedAsSmall(node.attempt);

const CONDITION_LABELS: Record<NonNullable<FlowNode["condition"]>, string> = {
  notSmall: "flow.onlyIfNotSmall",
  somethingToFix: "flow.onlyIfSomethingToFix",
  onlyIfChanged: "flow.onlyIfChanged",
};

/** Why it was left out, in a few words, when that was its condition. Then the condition says nothing more. */
function conditionSkip(node: FlowNode<FlowAttempt>): string | null {
  if (node.attempt === null) return null;
  if (skippedAsSmall(node.attempt)) return "flow.skippedSmallWhy";
  if (skippedWithNothingToFix(node.attempt)) return "flow.skippedNothingToFix";
  return skippedAsUnchanged(node.attempt) ? "flow.skippedUnchanged" : null;
}

const skippedForItsCondition = (node: FlowNode<FlowAttempt>) => conditionSkip(node) !== null;

/** A node's state in a word or two. A step put off for a new setup isn't a person asking for more. */
const stateLabel = (t: TFunction, node: FlowNode<FlowAttempt>) => (node.attempt && putOffForSetup(node.attempt) ? t("states.afterSetup") : t(`states.${node.state}`));

function stateText(t: TFunction, node: FlowNode<FlowAttempt>): string {
  const left = conditionSkip(node);
  if (left) return t(left);
  return node.state === "pending" ? t("run.pending") : stateLabel(t, node);
}

/** Everything a node shows, in words, for screen readers and as its tooltip. */
function describeNode(t: TFunction, node: FlowNode<FlowAttempt>, live: boolean, problem?: string): string {
  return [
    node.id || t("flow.unnamed"),
    node.role ? t(ROLE_LABELS[node.role]) : t(KIND_LABELS[node.kind]),
    node.model,
    live && stateText(t, node),
    live && node.outdated && t("flow.lastRan", { number: node.round + 1 }),
    live && roundText(t, node),
    node.condition && !skippedForItsCondition(node) && t(CONDITION_LABELS[node.condition]),
    problem,
  ]
    .filter(Boolean)
    .join(", ");
}

function NodeButton<T extends FlowAttempt>({ placed, live, selected, tabbable, problem, onSelect }: { placed: PlacedNode<T>; live: boolean; selected: boolean; tabbable: boolean; problem?: string; onSelect: () => void }) {
  const { t } = useTranslation();
  const { node, compact } = placed;
  const description = describeNode(t, node, live, problem);
  const round = live ? roundText(t, node) : null;
  const chip = round && <span className="shrink-0 text-[10.5px] font-medium text-muted">{round}</span>;
  const sub = node.model ?? t(KIND_LABELS[node.kind]);
  return (
    <button
      type="button"
      data-flow-item={node.key}
      data-flow-id={node.id}
      tabIndex={tabbable ? 0 : -1}
      aria-label={description}
      aria-current={selected ? "true" : undefined}
      title={description}
      onClick={onSelect}
      style={{ left: placed.x, top: placed.y, width: placed.w, height: placed.h }}
      className={cx(
        "absolute flex flex-col justify-center gap-px rounded-md border border-border-strong bg-elevated px-2 text-left transition-colors hover:bg-hover",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent",
        live && !node.minor && !node.outdated && NODE_TONES[node.state],
        live && !node.minor && node.outdated && "border-dashed opacity-70",
        node.minor && "border-dashed border-border bg-transparent",
        // The diagram of a workflow that's being edited has nothing else to tell these apart by.
        !live && node.condition && "border-dashed",
        problem && "border-danger bg-danger/10",
        selected && "opacity-100 ring-2 ring-accent",
      )}
    >
      <span className="flex min-w-0 items-center gap-1.5">
        <StepIcon node={node} />
        <span className={cx("min-w-0 flex-1 truncate font-medium", node.minor ? "text-[11.5px] text-muted" : "text-[12.5px]")}>{node.id || t("flow.unnamed")}</span>
        {compact && chip}
        {problem && <AlertTriangle size={12} className="shrink-0 text-danger" />}
      </span>
      {!compact && (
        <span className="flex min-w-0 items-baseline justify-between gap-1.5">
          <span className={cx("min-w-0 truncate text-[11px] text-subtle", node.model && "font-mono")}>{sub}</span>
          {chip}
        </span>
      )}
      {live && (
        // StateBadge pulses whatever is under way, and it can't know the person asked for less motion here.
        <span className="flex min-w-0 items-baseline overflow-hidden motion-reduce:[&_*]:animate-none">
          {node.state === "pending" || node.outdated ? (
            <span className="min-w-0 truncate text-[11.5px] text-subtle">{node.outdated ? t("flow.lastRan", { number: node.round + 1 }) : t("run.pending")}</span>
          ) : (
            <StateBadge state={node.state} label={skippedForBeingSmall(node) ? t("flow.skippedSmall") : stateLabel(t, node)} />
          )}
        </span>
      )}
    </button>
  );
}

/**
 * The steps as a diagram, laid out from the workflow. Selecting a node only ever shows something:
 * a click or Enter here answers nothing. It also never takes focus on its own, because on a run page
 * the focus belongs to the approval's note, and it scrolls only itself, sideways, for the same reason.
 */
export function FlowDiagram<T extends FlowAttempt>({
  flow,
  label,
  selected,
  onSelect,
  editing = false,
  problems = {},
  adding = null,
  onAdd,
  onAddMember,
  focusRequest = null,
}: {
  flow: Flow<T>;
  label: string;
  selected: string | null;
  onSelect: (key: string) => void;
  /** With the editor's + buttons, and a group's name as something to select. */
  editing?: boolean;
  /** What's wrong with a node, by its key. */
  problems?: Record<string, string>;
  /** The gap whose + is open. */
  adding?: number | null;
  onAdd?: (gap: number) => void;
  onAddMember?: (stage: number) => void;
  /** After an action that removes what had focus. A new object asks again. */
  focusRequest?: { key: string } | null;
}) {
  const { t } = useTranslation();
  const layout = useMemo(() => layoutFlow(flow, editing), [flow, editing]);
  const scroller = useRef<HTMLDivElement>(null);
  const [lastFocused, setLastFocused] = useState<string | null>(null);

  const has = (key: string | null) => key !== null && layout.items.some((item) => item.key === key);
  const tabStop = has(lastFocused) ? lastFocused : has(selected) ? selected : (layout.items[0]?.key ?? null);

  const shown = layout.nodes.find((placed) => placed.node.key === selected) ?? layout.groups.find((group) => group.key === selected);
  const shownX = shown?.x;
  const shownW = shown?.w;
  useLayoutEffect(() => {
    const element = scroller.current;
    if (!element || shownX === undefined || shownW === undefined) return;
    element.scrollLeft = scrollToShow(shownX, shownW, element.scrollLeft, element.clientWidth);
  }, [selected, shownX, shownW]);

  const focusItem = (key: string) => scroller.current?.querySelector<HTMLElement>(`[data-flow-item="${key}"]`)?.focus();
  useEffect(() => {
    if (focusRequest) focusItem(focusRequest.key);
  }, [focusRequest]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.metaKey || e.ctrlKey || e.altKey || !["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(e.key)) return;
    const from = (e.target as HTMLElement).closest("[data-flow-item]")?.getAttribute("data-flow-item");
    if (!from) return;
    e.preventDefault();
    const next = nextItem(layout.items, from, e.key);
    if (next) focusItem(next);
  };
  const remember = (e: FocusEvent<HTMLDivElement>) => {
    const key = (e.target as HTMLElement).closest("[data-flow-item]")?.getAttribute("data-flow-item");
    if (key) setLastFocused(key);
  };

  const groupName = (id: string) => t("flow.groupLabel", { step: id || t("flow.unnamed") });
  const gapLabel = (gap: number) => {
    if (flow.stages.length === 0) return t("flow.addFirst");
    const stage = flow.stages[gap];
    if (!stage) return t("flow.addEnd");
    return t("flow.addBefore", { step: (stage.type === "parallel" ? stage.id : stage.node.id) || t("flow.unnamed") });
  };

  const renderNode = (key: string) => {
    const placed = layout.nodes.find((candidate) => candidate.node.key === key);
    return placed && <NodeButton key={key} placed={placed} live={flow.live} selected={selected === key} tabbable={tabStop === key} problem={problems[key]} onSelect={() => onSelect(key)} />;
  };
  const renderGap = (index: number) => {
    const gap = layout.gaps.find((candidate) => candidate.index === index);
    return (
      gap && (
        <button
          key={gap.key}
          type="button"
          data-flow-item={gap.key}
          tabIndex={tabStop === gap.key ? 0 : -1}
          aria-label={gapLabel(index)}
          aria-expanded={adding === index}
          title={gapLabel(index)}
          onClick={() => onAdd?.(index)}
          style={{ left: gap.x, top: gap.y }}
          className={cx(PLUS, adding === index && "border-accent bg-active text-fg")}
        >
          <Plus size={12} />
        </button>
      )
    );
  };
  const renderGroup = (id: string, key: string, stageIndex: number, nodes: { key: string }[]) => {
    const group = layout.groups.find((candidate) => candidate.key === key);
    const add = layout.memberAdds.find((candidate) => candidate.index === stageIndex);
    if (!group) return null;
    return (
      <div key={key} role="group" aria-label={groupName(id)}>
        {editing ? (
          <button
            type="button"
            data-flow-item={key}
            data-flow-id={id}
            tabIndex={tabStop === key ? 0 : -1}
            aria-label={[groupName(id), problems[key]].filter(Boolean).join(", ")}
            aria-current={selected === key ? "true" : undefined}
            title={problems[key] ?? groupName(id)}
            onClick={() => onSelect(key)}
            style={{ left: group.x, top: group.y, width: group.w, height: group.h - 2 }}
            className={cx(
              "absolute flex items-center gap-1 rounded px-1 text-left text-[11px] font-medium text-muted transition-colors hover:bg-hover hover:text-fg",
              "focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent",
              problems[key] && "text-danger",
              selected === key && "bg-active text-fg ring-2 ring-accent",
            )}
          >
            <Layers size={11} className="shrink-0" />
            <span className="min-w-0 truncate">{id || t("flow.unnamed")}</span>
            {problems[key] && <AlertTriangle size={11} className="shrink-0" />}
          </button>
        ) : (
          <span title={groupName(id)} style={{ left: group.x, top: group.y, width: group.w, height: group.h }} className="absolute flex items-center gap-1 px-1 text-[11px] font-medium text-subtle">
            <Layers size={11} className="shrink-0" />
            <span className="min-w-0 truncate">{groupName(id)}</span>
          </span>
        )}
        {group.empty && (
          <span style={{ left: group.empty.x, top: group.empty.y, width: group.empty.w, height: group.empty.h }} className="absolute flex items-center justify-center rounded-md border border-dashed border-border-strong text-[11.5px] text-subtle">
            {t("flow.emptyGroup")}
          </span>
        )}
        {nodes.map((node) => renderNode(node.key))}
        {add && (
          <button type="button" data-flow-item={add.key} tabIndex={tabStop === add.key ? 0 : -1} aria-label={t("flow.addMember", { step: id || t("flow.unnamed") })} title={t("flow.addMember", { step: id || t("flow.unnamed") })} onClick={() => onAddMember?.(stageIndex)} style={{ left: add.x, top: add.y }} className={PLUS}>
            <Plus size={12} />
          </button>
        )}
      </div>
    );
  };

  return (
    <>
      {flow.live && flow.stages.length === 0 && flow.extras.length > 0 && <p className="mb-2 px-1 text-[12px] text-subtle">{t("flow.unreadable")}</p>}
      <div ref={scroller} role="group" aria-label={label} className="overflow-x-auto" onKeyDown={onKeyDown} onFocus={remember}>
        <div className="relative" style={{ width: layout.width, height: layout.height }}>
          <svg aria-hidden="true" width={layout.width} height={layout.height} className="pointer-events-none absolute inset-0">
            {layout.lines.map((line, index) => (
              // Lines of a fixed drawing: their position is their identity.
              <g key={index}>
                <path d={line.d} fill="none" strokeWidth={1.5} strokeDasharray={line.dashed ? "3 3" : undefined} className={line.travelled ? "stroke-subtle" : "stroke-border-strong"} />
                {line.head && <path d={line.head} className={line.travelled ? "fill-subtle" : "fill-border-strong"} />}
              </g>
            ))}
            {layout.reworks.map(({ rework, d, head }) => (
              <g key={rework.from}>
                <path d={d} fill="none" strokeWidth={1.5} strokeDasharray={rework.times > 0 ? undefined : "3 3"} className={rework.times > 0 ? "stroke-warning/70" : "stroke-border-strong"} />
                <path d={head} className={rework.times > 0 ? "fill-warning/70" : "fill-border-strong"} />
              </g>
            ))}
          </svg>
          {flow.stages.map((stage, index) => (
            <Fragment key={stage.key}>
              {renderGap(index)}
              {stage.type === "parallel" ? renderGroup(stage.id, stage.key, index, stage.nodes) : renderNode(stage.key)}
            </Fragment>
          ))}
          {renderGap(flow.stages.length)}
          {editing && flow.stages.length === 0 && layout.gaps[0] && (
            <span style={{ left: layout.gaps[0].x + 18, top: layout.gaps[0].y }} className="absolute -translate-y-1/2 text-[12.5px] whitespace-nowrap text-muted">
              {t("flow.addFirst")}
            </span>
          )}
          {layout.extrasLabel && (
            <span style={{ left: layout.extrasLabel.x, top: layout.extrasLabel.y }} className="absolute flex h-5 items-center px-1 text-[11px] font-medium whitespace-nowrap text-subtle">
              {t("flow.notInWorkflow")}
            </span>
          )}
          {flow.extras.map((node) => renderNode(node.key))}
          {layout.reworks.map(({ rework, labelX, labelY }) => {
            const text = editing ? t("flow.sendsBackTo", { step: rework.toId, count: rework.maxRounds }) : rework.times > 0 ? t("flow.sentBack", { count: rework.times }) : null;
            return (
              text && (
                <span key={rework.from} style={{ left: labelX, top: labelY }} className={cx("absolute -translate-x-1/2 -translate-y-1/2 rounded bg-panel px-1.5 text-[11px] whitespace-nowrap", editing ? "text-subtle" : "font-medium text-warning")}>
                  {text}
                </span>
              )
            );
          })}
        </div>
      </div>
    </>
  );
}
