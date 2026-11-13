import type { TFunction } from "i18next";
import type { AttemptState, Board, BoardCard, BoardColumn, CommandError, PartOf, PartWarning, PrState, Reason } from "./api";
import { formatDateTime } from "./format";
import { errorText, needsProjectSettings, reasonText } from "./reason";

export const COLUMNS: BoardColumn[] = ["backlog", "todo", "inProgress", "humanReview", "done"];

/** The columns a person can drop cards in. Keel moves the others. */
export type Lane = "backlog" | "todo";

export type Groups = Record<BoardColumn, BoardCard[]>;

/** Card ids per lane, in the order they show. */
export type Layout = Record<Lane, string[]>;

export const isColumn = (id: string): id is BoardColumn => (COLUMNS as string[]).includes(id);
export const isLane = (id: string): id is Lane => id === "backlog" || id === "todo";

/** Sorts cards into columns. The backend decides each card's column and their order. */
export function groupCards(cards: BoardCard[]): Groups {
  const groups: Groups = { backlog: [], todo: [], inProgress: [], humanReview: [], done: [] };
  for (const card of cards) groups[card.column].push(card);
  return groups;
}

export const canDrag = (card: BoardCard) => isLane(card.column);
export const canEdit = (card: BoardCard) => isLane(card.column);
export const canDelete = (card: BoardCard) => isLane(card.column) || card.column === "done";

/** What a click on a card does: a card with a run opens it, any other opens for editing. */
export function clickAction(card: BoardCard): "run" | "edit" | null {
  if (card.runId) return "run";
  return canEdit(card) ? "edit" : null;
}

export const layoutOf = (groups: Groups): Layout => ({ backlog: groups.backlog.map((c) => c.id), todo: groups.todo.map((c) => c.id) });

/** What the columns show while a card is being dragged or its move is on its way. */
export function withLayout(groups: Groups, layout: Layout | null): Groups {
  if (!layout) return groups;
  const movable = new Map([...groups.backlog, ...groups.todo].map((card) => [card.id, card]));
  const placed = new Set([...layout.backlog, ...layout.todo]);
  // A refresh can arrive mid-drag: what left the lanes goes, what's new stays where it came.
  const lane = (name: Lane) => [...layout[name].flatMap((id) => movable.get(id) ?? []), ...groups[name].filter((card) => !placed.has(card.id))];
  return { ...groups, backlog: lane("backlog"), todo: lane("todo") };
}

const laneHolding = (layout: Layout, id: string): Lane | null => (layout.backlog.includes(id) ? "backlog" : layout.todo.includes(id) ? "todo" : null);

/** The column a drag is over. `overId` is a column, or a card in a lane. */
export function overColumn(layout: Layout, overId: string): BoardColumn | null {
  return isColumn(overId) ? overId : laneHolding(layout, overId);
}

/** The layout when a card hovers over another lane: it takes the place of the card it's over. */
export function dragOver(layout: Layout, activeId: string, overId: string, below: boolean): Layout {
  const from = laneHolding(layout, activeId);
  const to = overColumn(layout, overId);
  if (!from || !to || !isLane(to) || from === to) return layout;
  const index = layout[to].indexOf(overId);
  const at = index < 0 ? layout[to].length : index + (below ? 1 : 0);
  return { ...layout, [from]: layout[from].filter((id) => id !== activeId), [to]: [...layout[to].slice(0, at), activeId, ...layout[to].slice(at)] };
}

export interface Destination {
  lane: Lane;
  /** Where the card ends up within the lane, counting from the top. */
  index: number;
  layout: Layout;
}

/** Where a dropped card goes. Null if it changes nothing, or if it was dropped where it can't go. */
export function dropDestination(saved: Layout, layout: Layout, activeId: string, overId: string | null): Destination | null {
  if (overId === null) return null;
  const column = overColumn(layout, overId);
  const lane = laneHolding(layout, activeId);
  if (!column || !isLane(column) || !lane) return null;
  // Across lanes the card already moved while hovering. Within one, it takes the place of the card it's over.
  const rest = layout[lane].filter((id) => id !== activeId);
  const over = layout[lane].indexOf(overId);
  const index = isColumn(overId) ? layout[lane].indexOf(activeId) : over;
  if (index < 0) return null;
  if (saved[lane].indexOf(activeId) === index) return null;
  return { lane, index, layout: { ...layout, [lane]: [...rest.slice(0, index), activeId, ...rest.slice(index)] } };
}

/** The other lane, for moving without dragging: the card goes to the bottom of it. */
export function otherLane(groups: Groups, card: BoardCard): { lane: Lane; index: number } | null {
  if (!isLane(card.column)) return null;
  const lane: Lane = card.column === "backlog" ? "todo" : "backlog";
  return { lane, index: groups[lane].length };
}

/** Whether cards are waiting in this project's Todo because the app-wide limit is reached. When it's paused, that's not why. */
export const waitingForRoom = (board: Board, groups: Groups) => !board.paused && board.working >= board.maxAgents && groups.todo.length > 0;

/** Says Todo is paused and why. A usage limit isn't something to fix, so that one says what lifts it. */
export function pausedText(t: TFunction, reason: Reason): string {
  if (reason.code !== "rateLimited") return `${t("board.paused")} ${reasonText(t, reason)}`;
  const resetsAt = reason.resetsAt as number | null | undefined;
  return resetsAt ? t("board.pausedLimitUntil", { date: formatDateTime(new Date(resetsAt * 1000).toISOString()) }) : t("board.pausedLimit");
}

/**
 * Gives `apply` only the newest call's answer. A call that was overtaken settles when the newest
 * one does, so whoever awaits it knows what's on screen is already current.
 */
export function newestOnly<T>(apply: (value: T) => void) {
  let latest = 0;
  let newest: Promise<void> = Promise.resolve();
  return (fetch: () => Promise<T>): Promise<void> => {
    const request = ++latest;
    newest = fetch().then(
      (value) => {
        if (request !== latest) return newest;
        apply(value);
      },
      (error) => {
        if (request !== latest) return newest;
        throw error;
      },
    );
    return newest;
  };
}

export const importedIssues = (cards: BoardCard[]) => new Set(cards.flatMap((card) => (card.issue ? [card.issue.number] : [])));

export interface CardLabel {
  /** Picks the badge's color. */
  state: AttemptState;
  label: string;
  detail: string | null;
}

/** Its badge's color. Only a merged one reads as a success: a closed one went nowhere, and an open one isn't over. */
const PR_BADGES: Record<PrState, AttemptState> = { open: "queued", draft: "queued", merged: "succeeded", closed: "rejected" };

/** What a card says about itself, beyond its title. */
export function cardLabel(t: TFunction, card: BoardCard): CardLabel | null {
  const state = card.runState;
  const reason = card.reason ? reasonText(t, card.reason) : null;
  switch (card.column) {
    case "backlog":
      if (card.startBlocked) return { state: "blocked", label: t("board.labels.couldNotStart"), detail: reasonText(t, card.startBlocked) };
      if (state === "rejected") return { state, label: t("board.labels.rejected"), detail: card.rejection?.trim() || null };
      if (state === "abandoned") return { state, label: t("board.labels.abandoned"), detail: null };
      return null;
    case "inProgress":
      return { state: state ?? "queued", label: t(`states.${state ?? "queued"}`), detail: reason };
    case "humanReview":
      if (!state) return null;
      return { state, label: t(`board.labels.${state}`, { defaultValue: t(`states.${state}`) }), detail: reason ?? t(`board.needed.${state}`, { defaultValue: "" }) };
    case "done":
      if (!card.prState) return null;
      // Only while it can still be merged: a red CI is what's in the way.
      if (card.prCi === "failed" && (card.prState === "open" || card.prState === "draft")) return { state: "failed", label: t(`board.pr.${card.prState}`), detail: card.prCiFailed.length > 0 ? t("board.ciFailed", { checks: card.prCiFailed.join(", ") }) : t("board.ciFailedUnnamed") };
      return { state: PR_BADGES[card.prState], label: t(`board.pr.${card.prState}`), detail: card.prStateAt && (card.prState === "merged" || card.prState === "closed") ? formatDateTime(card.prStateAt) : null };
    default:
      return null;
  }
}

export const partText = (t: TFunction, part: PartOf) => t("board.joint.part", { position: part.position, total: part.total });

/** What a part waits for, while it does. */
export function waitsForText(t: TFunction, part: PartOf): string | null {
  const before = part.waitsFor;
  if (!before) return null;
  return before.position === null ? t("board.joint.waitsForCard", { project: before.project }) : t("board.joint.waitsFor", { position: before.position, project: before.project });
}

export const partWarningText = (t: TFunction, warning: PartWarning) => t(`board.joint.warnings.${warning.kind}`, { position: warning.position, project: warning.project });

/** What a part says when it was specified with points of the contract as they no longer are. Null when it wasn't. */
export const contractChangedText = (t: TFunction, part: PartOf): string | null => (part.contractChanged.length > 0 ? t("board.joint.contractChanged", { points: part.contractChanged.join(", ") }) : null);

/** Everything a part warns about: what became of the parts before it, then its own contract. */
export const partWarningTexts = (t: TFunction, part: PartOf): string[] => {
  const contract = contractChangedText(t, part);
  return [...part.warnings.map((warning) => partWarningText(t, warning)), ...(contract ? [contract] : [])];
};

export interface JointLabel {
  /** A part, or where a change across projects is agreed. */
  text: string;
  /** What a part waits for, or what else the request touches. */
  detail: string | null;
  warnings: string[];
}

/**
 * What a card says about the change across projects it's in. `cardLabel` says nothing in Todo,
 * where a part waits, so this comes on top of it. `nameOf` gives a project's name by its id.
 */
export function jointLabel(t: TFunction, card: BoardCard, nameOf: (projectId: string) => string): JointLabel | null {
  if (card.part) return { text: partText(t, card.part), detail: waitsForText(t, card.part), warnings: partWarningTexts(t, card.part) };
  if (!card.jointChangeId) return null;
  const others = card.alsoTouches.map(nameOf);
  return { text: t("board.joint.agreeing"), detail: others.length > 0 ? t("board.joint.alsoTouches", { projects: others.join(", ") }) : null, warnings: [] };
}

export interface BoardNotice {
  text: string;
  /** Whether the way out is in Settings. */
  settings: boolean;
}

const reasonOf = (error: unknown) => (typeof error === "object" ? ((error as Partial<CommandError> | null)?.reason ?? null) : null);

/** The sentence for a failed call on a card. */
export function boardNotice(t: TFunction, error: unknown): BoardNotice {
  const refused = typeof error === "object" && (error as Partial<CommandError> | null)?.code === "invalidState";
  return { text: refused ? t("board.cardBusy") : errorText(t, error), settings: needsProjectSettings(reasonOf(error)) };
}

export function moveNotice(t: TFunction, card: BoardCard, error: unknown): BoardNotice {
  const notice = boardNotice(t, error);
  return { ...notice, text: t("board.moveFailed", { title: card.title, reason: notice.text }) };
}

/** Why the open issues can't be listed. The usual reasons talk about pull requests, so these talk about issues. */
export function importNotice(t: TFunction, error: unknown): BoardNotice {
  const code = reasonOf(error)?.code;
  if (code === "noGithubRemote") return { text: t("board.import.noRemote"), settings: true };
  if (code === "githubAccountMissing") return { text: t("board.import.noAccount"), settings: true };
  return boardNotice(t, error);
}
