import type { Attempt, AttemptState, StepRole, WorkflowStep } from "./api";
import { putOffForSetup } from "./audit";

export type NodeState = AttemptState | "pending";

/** What a node reads from an attempt. */
export interface FlowAttempt {
  id: string;
  stepId: string;
  kind: Attempt["kind"];
  role: Attempt["role"];
  round: number;
  number: number;
  state: AttemptState;
  modelRequested: string;
  modelActual: string | null;
  output: { decision?: string } | null;
  reason?: { code: string } | null;
}

export interface FlowNode<T extends FlowAttempt = Attempt> {
  /** Its place in the flow. Names can't identify a step while they're being typed, or if two match. */
  key: string;
  id: string;
  kind: Attempt["kind"];
  role: StepRole["kind"] | null;
  model: string | null;
  /** Shown small: the learner runs after everything else, and its failing holds nothing back. */
  minor: boolean;
  /** Only runs if the change isn't small. */
  conditional: boolean;
  state: NodeState;
  /** Every attempt of this step, oldest first. */
  attempts: T[];
  /** The latest one, which is what the state comes from. */
  attempt: T | null;
  /** Of the latest attempt, from 0 as the engine counts. */
  round: number;
  /** The run went back past this step after it ran, so its state is about an earlier round. */
  outdated: boolean;
}

export type FlowStage<T extends FlowAttempt = Attempt> = { type: "step"; key: string; node: FlowNode<T> } | { type: "parallel"; key: string; id: string; nodes: FlowNode<T>[] };

export interface FlowRework {
  /** Indexes into the stages. */
  from: number;
  to: number;
  fromId: string;
  toId: string;
  times: number;
  maxRounds: number;
}

export interface Flow<T extends FlowAttempt = Attempt> {
  stages: FlowStage<T>[];
  reworks: FlowRework[];
  /** Attempts of steps the workflow doesn't have, so that they stay reachable. */
  extras: FlowNode<T>[];
  /** Whether attempts were given, so nodes have a state to show. */
  live: boolean;
}

const SETTLED: NodeState[] = ["succeeded", "skipped", "returned", "postponed"];

/** Whether the run is past this attempt. A step put off for a new setup still has to run. */
const settledAttempt = (attempt: FlowAttempt) => SETTLED.includes(attempt.state) && !putOffForSetup(attempt);

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;
const SINGLE_KINDS = ["agent", "setup", "check", "approval", "deliver"];

/** A run's copy of its workflow, or null if it isn't one. It comes from a column that can hold anything old. */
export function parseSteps(workflow: unknown): WorkflowStep[] | null {
  if (!Array.isArray(workflow)) return null;
  const readable = workflow.every((step) => {
    if (!isRecord(step) || typeof step.id !== "string") return false;
    if (step.type === "parallel") return Array.isArray(step.steps) && step.steps.every((member) => isRecord(member) && typeof member.id === "string");
    return SINGLE_KINDS.includes(step.type as string);
  });
  return readable ? (workflow as WorkflowStep[]) : null;
}

/**
 * Where rework starts, like `rework_index` in the engine: every round runs again from here. An
 * approver says where, and without one it's the last worker that can change files, as long as an
 * approval comes after it to send work back.
 */
export function reworkIndex(steps: WorkflowStep[]): number | null {
  for (const step of steps) {
    if (step.type !== "agent" || step.role?.kind !== "approver") continue;
    const { reworkTo } = step.role;
    const at = steps.findIndex((s) => s.id === reworkTo);
    if (at >= 0) return at;
  }
  const lastApproval = steps.map((s) => s.type).lastIndexOf("approval");
  const before = lastApproval < 0 ? steps : steps.slice(0, lastApproval);
  for (let at = before.length - 1; at >= 0; at--) {
    const step = before[at];
    if (step.type === "agent" && (step.role?.kind ?? "worker") === "worker" && step.tools === "build") return at;
  }
  return null;
}

/**
 * A task an assistant started to read or look into something: one agent that only reads, with no
 * saved workflow behind it. It can't change files or open a pull request.
 */
export function isReadOnlyTask(run: { startedBy: string | null; workflowId: string | null; workflow: unknown }): boolean {
  const steps = parseSteps(run.workflow);
  return !!run.startedBy && !run.workflowId && steps?.length === 1 && steps[0].type === "agent" && steps[0].tools === "readOnly";
}

/** Whether a person at this approval can send the work back: what does it again has to come before them. */
export function canSendBack(steps: WorkflowStep[], approvalId: string): boolean {
  const from = reworkIndex(steps);
  const at = steps.findIndex((s) => s.id === approvalId);
  return from !== null && at >= 0 && from < at;
}

function reworksOf(steps: WorkflowStep[], attempts: FlowAttempt[], round: number): FlowRework[] {
  const approvers = steps.flatMap((step, from) => (step.type === "agent" && step.role?.kind === "approver" ? [{ id: step.id, role: step.role, from }] : []));
  return approvers.flatMap(({ id, role, from }) => {
    const to = steps.findIndex((step) => step.id === role.reworkTo);
    // Halfway through an edit, or in a file nobody validated, it can point anywhere.
    if (to < 0 || to >= from) return [];
    // Saying "rework" isn't going back: at the limit the run stops and a person decides. With one
    // approver, every new round is its doing, a person's "one more round" counts too, and only what
    // a person sent back from their own approval doesn't.
    const times = approvers.length === 1 ? round - attempts.filter((a) => a.state === "returned").length : new Set(attempts.filter((a) => a.stepId === id && a.round < round && a.output?.decision === "rework").map((a) => a.round)).size;
    return [{ from, to, fromId: id, toId: role.reworkTo, times, maxRounds: role.maxRounds }];
  });
}

/**
 * The steps of a workflow as stages to draw, in order, and what each one's attempts say about it.
 * Give it `runRound` too when there's a run: it can move on before any attempt of the new round exists.
 */
export function flowOf<T extends FlowAttempt = Attempt>(steps: WorkflowStep[], attempts?: T[], runRound = 0): Flow<T> {
  const given = attempts ?? [];
  const unclaimed = new Map<string, T[]>();
  for (const attempt of given) unclaimed.set(attempt.stepId, [...(unclaimed.get(attempt.stepId) ?? []), attempt]);
  const round = Math.max(runRound, ...given.map((a) => a.round));
  const reworks = reworksOf(steps, given, round);
  const reruns = reworks[0]?.to ?? reworkIndex(steps) ?? Number.POSITIVE_INFINITY;

  const nodeOf = (key: string, id: string, kind: Attempt["kind"], role: StepRole["kind"] | null, model: string | null | undefined, stage: number, conditional = false): FlowNode<T> => {
    const found = unclaimed.get(id) ?? [];
    unclaimed.delete(id);
    const attempt = found.at(-1) ?? null;
    return {
      key,
      id,
      kind,
      role,
      model: (kind === "agent" && (attempt?.modelActual || attempt?.modelRequested || model)) || null,
      minor: role === "learner",
      conditional,
      state: attempt?.state ?? "pending",
      attempts: found,
      attempt,
      round: attempt?.round ?? 0,
      // Only for what finished fine: anything under way or waiting has to say so, whatever the round.
      outdated: attempt !== null && settledAttempt(attempt) && stage >= reruns && attempt.round < round,
    };
  };

  const stages = steps.map((step, index): FlowStage<T> => {
    const key = String(index);
    if (step.type === "parallel") return { type: "parallel", key, id: step.id, nodes: step.steps.map((spec, member) => nodeOf(`${index}.${member}`, spec.id, "agent", spec.role?.kind ?? "worker", spec.model, index, Boolean(spec.when))) };
    const node = step.type === "agent" ? nodeOf(key, step.id, "agent", step.role?.kind ?? "worker", step.model, index, Boolean(step.when)) : nodeOf(key, step.id, step.type, null, null, index);
    return { type: "step", key, node };
  });
  const extras = [...unclaimed.entries()].map(([id, found], index) => {
    const kind = found[0].kind;
    return nodeOf(`x${index}`, id, kind, kind === "agent" ? found[0].role : null, null, Number.NEGATIVE_INFINITY);
  });
  return { stages, reworks, extras, live: attempts !== undefined };
}

export function allNodes<T extends FlowAttempt>(flow: Flow<T>): FlowNode<T>[] {
  return [...flow.stages.flatMap((stage) => (stage.type === "parallel" ? stage.nodes : [stage.node])), ...flow.extras];
}

export const nodeWithKey = <T extends FlowAttempt>(flow: Flow<T>, key: string | null) => allNodes(flow).find((node) => node.key === key);

export const nodeOfAttempt = <T extends FlowAttempt>(flow: Flow<T>, attempt: T | undefined) => allNodes(flow).find((node) => attempt && node.attempts.includes(attempt));

export interface AttemptChoice<T extends FlowAttempt> {
  attempt: T;
  /** What to call it, counting from 1: its round, its attempt, or both when one alone can't tell it apart. */
  round: number | null;
  number: number | null;
}

/** A step's attempts as choices, oldest first. */
export function attemptChoices<T extends FlowAttempt>(attempts: T[]): AttemptChoice<T>[] {
  const hasRounds = new Set(attempts.map((a) => a.round)).size > 1 || attempts.some((a) => a.round > 0);
  return attempts.map((attempt) => {
    const retried = attempts.filter((a) => a.round === attempt.round).length > 1;
    return { attempt, round: hasRounds ? attempt.round + 1 : null, number: retried || !hasRounds ? attempt.number : null };
  });
}

/** The nodes a validation problem is about. `duplicateId` is about both of them. */
export function problemKeys<T extends FlowAttempt>(flow: Flow<T>, step: string): string[] {
  return flow.stages.flatMap((stage) => {
    if (stage.type === "step") return stage.node.id === step ? [stage.key] : [];
    return [...(stage.id === step ? [stage.key] : []), ...stage.nodes.filter((node) => node.id === step).map((node) => node.key)];
  });
}

/** Where a step is in a workflow being edited: a stage, or one of the agents in a group. */
export interface StepPath {
  stage: number;
  member: number | null;
}

export const pathKey = (path: StepPath) => (path.member === null ? String(path.stage) : `${path.stage}.${path.member}`);

export function pathOfKey(key: string): StepPath | null {
  const match = /^(\d+)(?:\.(\d+))?$/.exec(key);
  return match ? { stage: Number(match[1]), member: match[2] === undefined ? null : Number(match[2]) } : null;
}

// Fixed sizes, so that the connectors are worked out rather than measured from the page.
const NODE_W = 132;
const MINOR_W = 112;
const LIVE_H = 62;
const COMPACT_H = 42;
const EDIT_H = 42;
const MEMBER_GAP = 6;
const LABEL_H = 20;
const ADD_MEMBER_H = 26;
const RUN_GAP = 24;
const EDIT_GAP = 44;
const END_GAP = 26;
const EXTRAS_GAP = 36;
const EXTRA_GAP = 10;
const LANE_GAP = 16;
const LANE_H = 22;
const PAD = 6;
const CORNER = 6;

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

export type PlacedNode<T extends FlowAttempt> = Box & { node: FlowNode<T>; compact: boolean };
export type PlacedGroup = Box & { key: string; id: string; empty: Box | null };
export interface PlacedButton {
  key: string;
  x: number;
  y: number;
  /** Which gap between stages, or which group, it adds to. */
  index: number;
}
export interface FlowLine {
  d: string;
  /** The arrowhead, on the lines that arrive somewhere. */
  head: string | null;
  /** The run has been along it. */
  travelled: boolean;
  dashed: boolean;
}
export interface PlacedRework {
  rework: FlowRework;
  d: string;
  head: string;
  labelX: number;
  labelY: number;
}
/** Something the arrow keys can land on. */
export interface FlowItem {
  key: string;
  column: number;
  y: number;
}
export interface FlowLayout<T extends FlowAttempt> {
  width: number;
  height: number;
  nodes: PlacedNode<T>[];
  groups: PlacedGroup[];
  gaps: PlacedButton[];
  memberAdds: PlacedButton[];
  lines: FlowLine[];
  reworks: PlacedRework[];
  extrasLabel: { x: number; y: number } | null;
  items: FlowItem[];
}

const settled = <T extends FlowAttempt>(node: FlowNode<T>) => node.attempt !== null && settledAttempt(node.attempt) && !node.outdated;

const leaving = (x: number, y: number, mid: number, cy: number) => {
  if (y === cy) return `M ${x} ${y} H ${mid}`;
  const down = cy > y ? 1 : -1;
  const r = Math.min(CORNER, Math.abs(cy - y));
  return `M ${x} ${y} H ${mid - r} Q ${mid} ${y} ${mid} ${y + down * r} V ${cy}`;
};
const arriving = (mid: number, cy: number, x: number, y: number) => {
  if (y === cy) return `M ${mid} ${cy} H ${x - 5}`;
  const down = y > cy ? 1 : -1;
  const r = Math.min(CORNER, Math.abs(y - cy));
  return `M ${mid} ${cy} V ${y - down * r} Q ${mid} ${y} ${mid + r} ${y} H ${x - 5}`;
};
const arrowRight = (x: number, y: number) => `M ${x - 6} ${y - 3.5} L ${x} ${y} L ${x - 6} ${y + 3.5} Z`;
const arrowUp = (x: number, y: number) => `M ${x - 3.5} ${y + 6} L ${x} ${y} L ${x + 3.5} ${y + 6} Z`;

/** Where everything goes, in a row of stages. The editor has room between them for its + buttons. */
export function layoutFlow<T extends FlowAttempt>(flow: Flow<T>, editing = false): FlowLayout<T> {
  const memberH = editing ? EDIT_H : COMPACT_H;
  const nodeH = (node: FlowNode<T>) => (editing ? EDIT_H : node.minor ? COMPACT_H : LIVE_H);
  const stackH = (count: number) => Math.max(count, 1) * memberH + Math.max(count - 1, 0) * MEMBER_GAP;

  let above = editing ? END_GAP / 2 : 0;
  let below = above;
  for (const stage of flow.stages) {
    if (stage.type === "step") {
      above = Math.max(above, nodeH(stage.node) / 2);
      below = Math.max(below, nodeH(stage.node) / 2);
    } else {
      above = Math.max(above, stackH(stage.nodes.length) / 2 + LABEL_H);
      below = Math.max(below, stackH(stage.nodes.length) / 2 + (editing ? ADD_MEMBER_H : 0));
    }
  }
  for (const node of flow.extras) {
    above = Math.max(above, nodeH(node) / 2 + (flow.stages.length > 0 ? LABEL_H : 0));
    below = Math.max(below, nodeH(node) / 2);
  }
  const cy = PAD + above;

  const layout: FlowLayout<T> = { width: 0, height: 0, nodes: [], groups: [], gaps: [], memberAdds: [], lines: [], reworks: [], extrasLabel: null, items: [] };
  let x = PAD;
  let column = 0;
  const addGap = (index: number, width: number) => {
    const key = `gap${index}`;
    layout.gaps.push({ key, x: x + width / 2, y: cy, index });
    layout.items.push({ key, column: column++, y: cy });
    x += width;
  };

  type Point = { x: number; y: number };
  let previous: { outs: (Point & { travelled: boolean })[]; right: number; travelled: boolean } | null = null;
  const stageBoxes: { x: number; w: number; bottom: number }[] = [];

  flow.stages.forEach((stage, index) => {
    if (editing) addGap(index, index === 0 ? END_GAP : EDIT_GAP);
    else if (index > 0) x += RUN_GAP;

    const stageColumn = column++;
    let ins: Point[];
    let outs: (Point & { travelled: boolean })[];
    let stageW: number;
    let bottom: number;
    let travelled: boolean;
    let dashed = false;

    if (stage.type === "step") {
      const { node } = stage;
      const h = nodeH(node);
      const w = node.minor ? MINOR_W : NODE_W;
      layout.nodes.push({ node, x, y: cy - h / 2, w, h, compact: !editing && node.minor });
      layout.items.push({ key: node.key, column: stageColumn, y: cy });
      ins = [{ x, y: cy }];
      outs = [{ x: x + w, y: cy, travelled: settled(node) }];
      stageW = w;
      bottom = cy + h / 2;
      travelled = settled(node);
      dashed = node.minor;
    } else {
      stageW = NODE_W;
      const stack = stackH(stage.nodes.length);
      const top = cy - stack / 2;
      const empty = stage.nodes.length === 0 ? { x, y: top, w: NODE_W, h: memberH } : null;
      layout.groups.push({ key: stage.key, id: stage.id, x, y: top - LABEL_H, w: NODE_W, h: LABEL_H, empty });
      if (editing) layout.items.push({ key: stage.key, column: stageColumn, y: top - LABEL_H / 2 });
      stage.nodes.forEach((node, member) => {
        const y = top + member * (memberH + MEMBER_GAP);
        layout.nodes.push({ node, x, y, w: NODE_W, h: memberH, compact: !editing });
        layout.items.push({ key: node.key, column: stageColumn, y: y + memberH / 2 });
      });
      const ys = stage.nodes.length > 0 ? stage.nodes.map((_, member) => top + member * (memberH + MEMBER_GAP) + memberH / 2) : [cy];
      ins = ys.map((y) => ({ x, y }));
      outs = ys.map((y, member) => ({ x: x + NODE_W, y, travelled: stage.nodes[member] ? settled(stage.nodes[member]) : false }));
      bottom = cy + stack / 2;
      travelled = stage.nodes.length > 0 && stage.nodes.every(settled);
      if (editing) {
        const key = `add${index}`;
        layout.memberAdds.push({ key, x: x + NODE_W / 2, y: bottom + ADD_MEMBER_H / 2, index });
        layout.items.push({ key, column: stageColumn, y: bottom + ADD_MEMBER_H / 2 });
        bottom += ADD_MEMBER_H;
      }
    }

    if (previous) {
      const before = previous;
      const mid = (before.right + x) / 2;
      for (const out of before.outs) layout.lines.push({ d: leaving(out.x, out.y, mid, cy), head: null, travelled: out.travelled, dashed });
      for (const point of ins) layout.lines.push({ d: arriving(mid, cy, point.x, point.y), head: arrowRight(point.x, point.y), travelled: before.travelled, dashed });
    }
    stageBoxes.push({ x, w: stageW, bottom });
    x += stageW;
    previous = { outs, right: x, travelled };
  });

  // Nothing can come after the learner: the engine requires it to be last.
  const last = flow.stages.at(-1);
  if (editing && !(last?.type === "step" && last.node.minor)) addGap(flow.stages.length, END_GAP);

  flow.extras.forEach((node, index) => {
    x += index === 0 ? (flow.stages.length > 0 ? EXTRAS_GAP : 0) : EXTRA_GAP;
    const h = nodeH(node);
    const w = node.minor ? MINOR_W : NODE_W;
    if (index === 0 && flow.stages.length > 0) layout.extrasLabel = { x, y: cy - h / 2 - LABEL_H };
    layout.nodes.push({ node, x, y: cy - h / 2, w, h, compact: !editing && node.minor });
    layout.items.push({ key: node.key, column: column++, y: cy });
    x += w;
  });

  let lane = cy + below + LANE_GAP;
  for (const rework of flow.reworks) {
    const from = stageBoxes[rework.from];
    const to = stageBoxes[rework.to];
    const fromX = from.x + from.w / 2;
    // Clear of a group's own + when it's the target, which only a hand-edited file can do.
    const toX = flow.stages[rework.to].type === "parallel" && editing ? to.x + 20 : to.x + to.w / 2;
    const toY = to.bottom + 2;
    layout.reworks.push({
      rework,
      d: `M ${fromX} ${from.bottom} V ${lane - CORNER} Q ${fromX} ${lane} ${fromX - CORNER} ${lane} H ${toX + CORNER} Q ${toX} ${lane} ${toX} ${lane - CORNER} V ${toY + 5}`,
      head: arrowUp(toX, toY),
      labelX: (fromX + toX) / 2,
      labelY: lane,
    });
    lane += LANE_H;
  }

  layout.width = Math.max(x, 0) + PAD;
  layout.height = layout.reworks.length > 0 ? lane - LANE_H + LANE_GAP / 2 + PAD : cy + below + PAD;
  return layout;
}

/** Where the arrow keys go from here. Null when there's nowhere further that way. */
export function nextItem(items: FlowItem[], from: string, key: string): string | null {
  const here = items.find((item) => item.key === from);
  if (!here || items.length === 0) return null;
  if (key === "Home") return items[0].key;
  if (key === "End") return items[items.length - 1].key;
  if (key === "ArrowLeft" || key === "ArrowRight") {
    const column = here.column + (key === "ArrowLeft" ? -1 : 1);
    const inColumn = items.filter((item) => item.column === column);
    // Into a group, whichever member is level with where the person is.
    return inColumn.reduce<FlowItem | null>((best, item) => (!best || Math.abs(item.y - here.y) < Math.abs(best.y - here.y) ? item : best), null)?.key ?? null;
  }
  if (key === "ArrowUp" || key === "ArrowDown") {
    const inColumn = items.filter((item) => item.column === here.column);
    return inColumn[inColumn.indexOf(here) + (key === "ArrowUp" ? -1 : 1)]?.key ?? null;
  }
  return null;
}

/** The scroll position that shows a box: where it is if it's already in view, centered if not. */
export function scrollToShow(x: number, w: number, scrollLeft: number, viewWidth: number): number {
  if (viewWidth <= 0) return scrollLeft;
  if (x >= scrollLeft + PAD && x + w <= scrollLeft + viewWidth - PAD) return scrollLeft;
  return Math.max(0, x + w / 2 - viewWidth / 2);
}
