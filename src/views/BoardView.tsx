import { type Announcements, type CollisionDetection, closestCenter, DndContext, type DragEndEvent, type DragOverEvent, DragOverlay, type DragStartEvent, KeyboardSensor, MeasuringStrategy, PointerSensor, pointerWithin, rectIntersection, useDroppable, useSensor, useSensors } from "@dnd-kit/core";
import { SortableContext, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { Check, Download, GitPullRequest, Lock, Pencil, Plus, Trash2, X } from "lucide-react";
import { type ComponentProps, type CSSProperties, type FormEvent, type ReactNode, type Ref, useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type Board, type BoardCard, type BoardColumn, type Issue } from "../lib/api";
import { type BoardNotice, boardNotice, canDelete, canEdit, cardLabel, clickAction, COLUMNS, dragOver, dropDestination, groupCards, importedIssues, importNotice, isColumn, isLane, type Lane, type Layout, layoutOf, moveNotice, newestOnly, otherLane, overColumn, pausedText, waitingForRoom, withLayout } from "../lib/board";
import { needsProjectSettings } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, cx, Field, inputClass, Modal, StateBadge } from "../ui/primitives";
import { BlockerNotice } from "./Home";
import { TopBar } from "./TopBar";

type Dialog = { kind: "task"; card: BoardCard | null } | { kind: "delete"; card: BoardCard } | { kind: "import" };

// Columns keep their place during a drag, but the cards inside them don't, so measure all the time.
const MEASURING = { droppable: { strategy: MeasuringStrategy.Always } };
// Enter opens the card, so only Space picks it up.
const KEYBOARD_CODES = { start: ["Space"], cancel: ["Escape"], end: ["Space", "Enter", "Tab"] };

const focusClass = "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent";

function NoticeBar({ notice, onDismiss }: { notice: BoardNotice; onDismiss?: () => void }) {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  return (
    <div role="alert" className="flex flex-wrap items-center gap-2 rounded-md border border-border-strong bg-inset px-3 py-2 text-[12.5px] text-warning">
      <span className="min-w-0 flex-1">{notice.text}</span>
      {notice.settings && <Button onClick={() => navigate({ name: "settings" })}>{t("newRun.openSettings")}</Button>}
      {onDismiss && (
        <Button variant="ghost" aria-label={t("board.dismiss")} title={t("board.dismiss")} className="px-1.5" onClick={onDismiss}>
          <X size={13} />
        </Button>
      )}
    </div>
  );
}

/** The title and what the card says about itself. It lives inside a button, so only spans. */
function CardFace({ card }: { card: BoardCard }) {
  const { t } = useTranslation();
  const label = cardLabel(t, card);
  return (
    <>
      <span className="line-clamp-3 text-[12.5px] leading-snug font-medium break-words">
        {card.issue && <span className="font-normal text-subtle">#{card.issue.number} </span>}
        {card.title}
      </span>
      {label && (
        <span className="mt-1 block">
          <StateBadge state={label.state} label={label.label} />
          {label.detail && (
            <span title={label.detail} className="mt-0.5 line-clamp-3 block text-[11.5px] leading-snug break-words text-muted">
              {label.detail}
            </span>
          )}
        </span>
      )}
    </>
  );
}

interface CardActions {
  onOpen: (card: BoardCard) => void;
  onMove: (card: BoardCard) => void;
  onEdit: (card: BoardCard) => void;
  onDelete: (card: BoardCard) => void;
  onOpenPr: (url: string) => void;
  onOpenSettings: () => void;
}

function IconAction({ label, children, ...props }: { label: string; children: ReactNode } & ComponentProps<"button">) {
  return (
    <Button variant="ghost" aria-label={label} title={label} className="h-6 px-0.5" {...props}>
      {children}
    </Button>
  );
}

/** A card, with what can be done to it below the part that opens it. */
function CardShell({ card, actions, nodeRef, style, faded, handleRef, handleProps, cursor }: { card: BoardCard; actions: CardActions; nodeRef?: Ref<HTMLLIElement>; style?: CSSProperties; faded?: boolean; handleRef?: Ref<HTMLButtonElement>; handleProps?: ComponentProps<"button">; cursor: string }) {
  const { t } = useTranslation();
  const target = isLane(card.column) ? (card.column === "backlog" ? "todo" : "backlog") : null;
  const hasFooter = target !== null || canDelete(card) || card.prUrl !== null;
  return (
    <li ref={nodeRef} style={style} className={cx("rounded-md border border-border bg-elevated hover:border-border-strong", faded && "opacity-40")}>
      <button ref={handleRef} type="button" data-card={card.id} className={cx("block w-full rounded-md px-2 py-1.5 text-left", cursor, focusClass)} {...handleProps} onClick={() => actions.onOpen(card)}>
        <CardFace card={card} />
      </button>
      {needsProjectSettings(card.startBlocked) && (
        <button type="button" className="mx-2 mb-1 block text-[11.5px] text-accent hover:underline" onClick={actions.onOpenSettings}>
          {t("newRun.openSettings")}
        </button>
      )}
      {hasFooter && (
        <div className="flex flex-wrap items-center gap-0.5 px-1 pb-1">
          {target && (
            <Button variant="ghost" className="h-6 px-1 text-[11px]" onClick={() => actions.onMove(card)}>
              {t(`board.moveTo.${target}`)}
            </Button>
          )}
          {card.prUrl && (
            <Button
              variant="ghost"
              className="h-6 px-1 text-[11px] text-accent"
              onClick={(e) => {
                e.stopPropagation();
                actions.onOpenPr(card.prUrl as string);
              }}
            >
              <GitPullRequest size={12} />
              {t("board.openPr")}
            </Button>
          )}
          <span className="flex-1" />
          {canEdit(card) && (
            <IconAction label={t("board.edit")} onClick={() => actions.onEdit(card)}>
              <Pencil size={12} />
            </IconAction>
          )}
          {canDelete(card) && (
            <IconAction label={t("board.delete")} onClick={() => actions.onDelete(card)}>
              <Trash2 size={12} />
            </IconAction>
          )}
        </div>
      )}
    </li>
  );
}

function DraggableCard({ card, actions, disabled }: { card: BoardCard; actions: CardActions; disabled: boolean }) {
  const { t } = useTranslation();
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } = useSortable({ id: card.id, disabled, attributes: { roleDescription: t("board.drag.role") } });
  const style = { transform: transform ? `translate3d(${transform.x}px, ${transform.y}px, 0)` : undefined, transition };
  return <CardShell card={card} actions={actions} nodeRef={setNodeRef} style={style} faded={isDragging} handleRef={setActivatorNodeRef} handleProps={{ ...attributes, ...listeners }} cursor="cursor-grab active:cursor-grabbing" />;
}

function Column({ column, cards, dragging, refused, moving, actions, children }: { column: BoardColumn; cards: BoardCard[]; dragging: boolean; refused: boolean; moving: boolean; actions: CardActions; children?: ReactNode }) {
  const { t } = useTranslation();
  const { setNodeRef } = useDroppable({ id: column });
  const lane = isLane(column);
  const list = (
    <ul className="flex min-h-0 flex-1 flex-col gap-1.5 overflow-y-auto px-1.5 pb-1.5">
      {cards.map((card) => (lane ? <DraggableCard key={card.id} card={card} actions={actions} disabled={moving} /> : <CardShell key={card.id} card={card} actions={actions} cursor="cursor-pointer" />))}
      {cards.length === 0 && <li className={cx("px-1.5 py-1 text-[11.5px] leading-snug text-subtle", dragging && lane && "rounded-md border border-dashed border-border-strong py-3")}>{t(`board.columnEmpty.${column}`)}</li>}
    </ul>
  );
  return (
    <section ref={setNodeRef} aria-label={t(`board.columns.${column}`)} className={cx("flex min-h-0 min-w-[164px] flex-1 basis-0 flex-col rounded-lg border border-border bg-panel transition-opacity", dragging && !lane && "cursor-not-allowed opacity-50", dragging && lane && "border-border-strong", refused && "border-danger opacity-100")}>
      <header className="flex items-center gap-1.5 px-2.5 pt-2 pb-1.5">
        <h2 className="truncate text-[12.5px] font-semibold">{t(`board.columns.${column}`)}</h2>
        <span className="text-[11.5px] text-subtle tabular-nums">{cards.length}</span>
        {!lane && (
          <span title={t("board.locked")} className="ml-auto text-subtle">
            <Lock size={11} aria-label={t("board.locked")} />
          </span>
        )}
      </header>
      {refused && (
        <p role="status" className="mx-1.5 mb-1.5 rounded-md bg-inset px-2 py-1.5 text-[11.5px] leading-snug text-danger">
          {t("board.refused")}
        </p>
      )}
      {children}
      {lane ? (
        <SortableContext id={column} items={cards.map((card) => card.id)} strategy={verticalListSortingStrategy}>
          {list}
        </SortableContext>
      ) : (
        list
      )}
    </section>
  );
}

function TaskDialog({ projectId, card, onClose, onSaved }: { projectId: string; card: BoardCard | null; onClose: () => void; onSaved: () => void }) {
  const { t } = useTranslation();
  const [title, setTitle] = useState(card?.title ?? "");
  const [body, setBody] = useState(card?.body ?? "");
  const [notice, setNotice] = useState<BoardNotice | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setNotice(null);
    try {
      if (card) await api.cardUpdate(card.id, title.trim(), body);
      else await api.cardCreate(projectId, title.trim(), body);
      onSaved();
    } catch (error) {
      setNotice(boardNotice(t, error));
      setBusy(false);
    }
  };

  return (
    <Modal open onOpenChange={(open) => !open && onClose()} title={t(card ? "board.task.editTitle" : "board.task.newTitle")}>
      <form className="space-y-3.5" onSubmit={(e) => void submit(e)}>
        <Field label={t("board.task.title")}>
          <input autoFocus required value={title} onChange={(e) => setTitle(e.target.value)} className={inputClass} />
        </Field>
        {/* An issue already says what's wrong, so here the person only adds to it, as in New Run. */}
        <Field label={card?.issue ? t("newRun.issueExtra") : t("board.task.description")}>
          <textarea rows={5} value={body} onChange={(e) => setBody(e.target.value)} placeholder={card?.issue ? undefined : t("newRun.promptPlaceholder")} className={`${inputClass} resize-y leading-relaxed`} />
        </Field>
        {notice && <NoticeBar notice={notice} />}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>{t("newRun.cancel")}</Button>
          <Button type="submit" variant="primary" disabled={busy || title.trim() === ""}>
            {t(card ? "board.task.save" : "board.task.create")}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function DeleteDialog({ card, onClose, onDeleted }: { card: BoardCard; onClose: () => void; onDeleted: () => void }) {
  const { t } = useTranslation();
  const [notice, setNotice] = useState<BoardNotice | null>(null);
  const [busy, setBusy] = useState(false);

  const remove = async () => {
    setBusy(true);
    setNotice(null);
    try {
      await api.cardDelete(card.id);
      onDeleted();
    } catch (error) {
      setNotice(boardNotice(t, error));
      setBusy(false);
    }
  };

  return (
    <Modal open onOpenChange={(open) => !open && onClose()} title={t("board.deleteTitle")} description={t("board.deleteBody", { title: card.title })}>
      {notice && (
        <div className="mb-3">
          <NoticeBar notice={notice} />
        </div>
      )}
      <div className="flex justify-end gap-2">
        <Button autoFocus onClick={onClose}>
          {t("newRun.cancel")}
        </Button>
        <Button variant="danger" disabled={busy} onClick={() => void remove()}>
          {t("board.delete")}
        </Button>
      </div>
    </Modal>
  );
}

function ImportDialog({ projectId, cards, onClose, onImported }: { projectId: string; cards: BoardCard[]; onClose: () => void; onImported: () => Promise<void> }) {
  const { t } = useTranslation();
  const [issues, setIssues] = useState<Issue[] | null>(null);
  const [notice, setNotice] = useState<BoardNotice | null>(null);
  const [importing, setImporting] = useState<number | null>(null);
  const imported = useMemo(() => importedIssues(cards), [cards]);

  // Issues load only when asked for. Keel never polls them.
  useEffect(() => {
    let current = true;
    api
      .issuesList(projectId)
      .then((list) => current && setIssues(list))
      .catch((error) => {
        if (!current) return;
        setIssues([]);
        setNotice(importNotice(t, error));
      });
    return () => {
      current = false;
    };
  }, [projectId, t]);

  const importOne = async (issue: Issue) => {
    setImporting(issue.number);
    setNotice(null);
    try {
      await api.cardImportIssue(projectId, issue);
      await onImported();
    } catch (error) {
      setNotice(boardNotice(t, error));
    } finally {
      setImporting(null);
    }
  };

  return (
    <Modal open onOpenChange={(open) => !open && onClose()} title={t("board.import.title")} description={t("board.import.hint")} className="w-[560px]">
      <div className="space-y-3">
        {issues === null && <p className="text-[12px] text-subtle">{t("newRun.issuesLoading")}</p>}
        {issues?.length === 0 && !notice && <p className="text-[12.5px] text-muted">{t("newRun.issuesEmpty")}</p>}
        {notice && <NoticeBar notice={notice} />}
        {issues && issues.length > 0 && (
          <ul aria-label={t("board.import.title")} className="max-h-72 overflow-y-auto rounded-md border border-border-strong bg-inset">
            {issues.map((issue) => {
              const done = imported.has(issue.number);
              return (
                <li key={issue.number} className="flex items-center gap-3 border-t border-border px-2.5 py-1.5 first:border-t-0">
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-[12.5px]">
                      <span className="text-subtle">#{issue.number}</span> {issue.title}
                    </p>
                    {issue.author && <p className="text-[11px] text-subtle">{t("newRun.issueBy", { author: issue.author })}</p>}
                  </div>
                  {done ? (
                    <span className="inline-flex items-center gap-1 text-[12px] font-medium whitespace-nowrap text-success">
                      <Check size={13} />
                      {t("board.import.imported")}
                    </span>
                  ) : (
                    <Button disabled={importing !== null} onClick={() => void importOne(issue)}>
                      {t("board.import.import")}
                    </Button>
                  )}
                </li>
              );
            })}
          </ul>
        )}
        {issues && issues.length > 0 && <p className="text-[11.5px] leading-snug text-subtle">{t("newRun.issueNote")}</p>}
        <div className="flex justify-end">
          <Button onClick={onClose}>{t("board.import.close")}</Button>
        </div>
      </div>
    </Modal>
  );
}

export function BoardView({ initialProjectId }: { initialProjectId?: string }) {
  const { t, i18n } = useTranslation();
  const projects = useApp((s) => s.overview.projects);
  const loaded = useApp((s) => s.overviewLoaded);
  const revision = useApp((s) => s.revision);
  const session = useApp((s) => s.preflight);
  const navigate = useApp((s) => s.navigate);

  const [projectId, setProjectId] = useState(initialProjectId ?? "");
  const [board, setBoard] = useState<Board | null>(null);
  const [layout, setLayout] = useState<Layout | null>(null);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [overId, setOverId] = useState<string | null>(null);
  const [moving, setMoving] = useState(false);
  const [notice, setNotice] = useState<BoardNotice | null>(null);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const applyNewest = useMemo(() => newestOnly<Board>(setBoard), []);
  const paused = board?.paused ?? null;

  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projects, projectId]);

  // Only the newest answer counts, and awaiting this means it's on screen. Otherwise a card that
  // just moved could jump back to where it was.
  const load = useCallback(async () => {
    if (projectId) await applyNewest(() => api.boardGet(projectId));
  }, [projectId, applyNewest]);

  // "Check again" refreshes the session, which is what can lift a pause, so look again after it too.
  useEffect(() => {
    load().catch((error) => setNotice(boardNotice(t, error)));
  }, [load, revision, session, t]);

  const saved = useMemo(() => groupCards(board?.cards ?? []), [board]);
  const groups = useMemo(() => withLayout(saved, layout), [saved, layout]);
  const shown = useMemo(() => layoutOf(groups), [groups]);
  const cardsById = useMemo(() => new Map((board?.cards ?? []).map((card) => [card.id, card])), [board]);
  const activeCard = activeId ? (cardsById.get(activeId) ?? null) : null;
  const hoveredColumn = activeId && overId ? overColumn(shown, overId) : null;

  const sensors = useSensors(useSensor(PointerSensor, { activationConstraint: { distance: 5 } }), useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates, keyboardCodes: KEYBOARD_CODES }));

  // The column under the pointer decides. Inside a lane, the card closest to the one being dragged.
  const collisionDetection = useCallback<CollisionDetection>(
    (args) => {
      const pointer = pointerWithin(args);
      const hits = pointer.length > 0 ? pointer : rectIntersection(args);
      const column = hits.find((hit) => isColumn(String(hit.id)))?.id;
      if (column === undefined) return hits;
      const cards = isLane(String(column)) ? shown[column as Lane] : [];
      if (cards.length === 0) return [{ id: column }];
      return closestCenter({ ...args, droppableContainers: args.droppableContainers.filter((container) => cards.includes(String(container.id))) });
    },
    [shown],
  );

  const moveCard = async (card: BoardCard, lane: Lane, index: number) => {
    setNotice(null);
    setMoving(true);
    try {
      await api.cardMove(card.id, lane, index, i18n.language);
    } catch (error) {
      // Back to what the backend says, so a card never looks moved when it wasn't.
      setLayout(null);
      setMoving(false);
      setNotice(moveNotice(t, card, error));
      return false;
    }
    await load().catch((error) => setNotice(boardNotice(t, error)));
    setLayout(null);
    setMoving(false);
    return true;
  };

  const onDragStart = ({ active }: DragStartEvent) => {
    setNotice(null);
    setActiveId(String(active.id));
    setLayout(layoutOf(saved));
  };

  const onDragOver = ({ active, over }: DragOverEvent) => {
    const id = over ? String(over.id) : null;
    setOverId(id);
    if (!over || !id) return;
    const column = overColumn(shown, id);
    if (column && !isLane(column)) {
      setLayout(layoutOf(saved));
      return;
    }
    const translated = active.rect.current.translated;
    setLayout(dragOver(shown, String(active.id), id, translated != null && translated.top > over.rect.top + over.rect.height));
  };

  const endDrag = () => {
    setActiveId(null);
    setOverId(null);
  };

  const onDragEnd = ({ active, over }: DragEndEvent) => {
    endDrag();
    const id = String(active.id);
    const card = cardsById.get(id);
    const destination = card ? dropDestination(layoutOf(saved), shown, id, over ? String(over.id) : null) : null;
    if (!card || !destination) {
      setLayout(null);
      return;
    }
    setLayout(destination.layout);
    void moveCard(card, destination.lane, destination.index);
  };

  const onDragCancel = () => {
    endDrag();
    setLayout(null);
  };

  const changed = () => {
    setDialog(null);
    load().catch((error) => setNotice(boardNotice(t, error)));
  };

  const actions: CardActions = {
    onOpen: (card) => {
      const action = clickAction(card);
      if (action === "run" && card.runId) navigate({ name: "run", runId: card.runId, backTo: { name: "board", projectId } });
      if (action === "edit") setDialog({ kind: "task", card });
    },
    onMove: (card) => {
      const target = otherLane(saved, card);
      if (!target) return;
      void moveCard(card, target.lane, target.index).then((moved) => {
        // The button that was pressed is gone with the move, so keep the keyboard on the card.
        if (moved) setTimeout(() => document.querySelector<HTMLElement>(`[data-card="${CSS.escape(card.id)}"]`)?.focus(), 0);
      });
    },
    onEdit: (card) => setDialog({ kind: "task", card }),
    onDelete: (card) => setDialog({ kind: "delete", card }),
    onOpenPr: (url) => {
      api.openGithubUrl(url).catch((error) => setNotice(boardNotice(t, error)));
    },
    onOpenSettings: () => navigate({ name: "settings" }),
  };

  const titleOf = (id: string | number) => cardsById.get(String(id))?.title ?? "";
  const announcements: Announcements = {
    onDragStart: ({ active }) => t("board.drag.picked", { title: titleOf(active.id) }),
    onDragOver: ({ over }) => {
      const column = over ? overColumn(shown, String(over.id)) : null;
      return column ? t("board.drag.over", { column: t(`board.columns.${column}`) }) : undefined;
    },
    onDragEnd: ({ active }) => t("board.drag.dropped", { title: titleOf(active.id) }),
    onDragCancel: ({ active }) => t("board.drag.cancelled", { title: titleOf(active.id) }),
  };

  const selectProject = (id: string) => {
    setProjectId(id);
    setBoard(null);
    setLayout(null);
    setNotice(null);
  };

  return (
    <>
      <TopBar back />
      <main className="flex min-h-0 flex-1 flex-col gap-3 p-4">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
          <h1 className="text-[18px] font-semibold">{t("board.title")}</h1>
          {projects.length > 0 && (
            <select aria-label={t("board.project")} value={projectId} onChange={(e) => selectProject(e.target.value)} className={cx(inputClass, "w-48")}>
              {projects.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          )}
          {board && (
            <p className="text-[12px] text-muted">
              {t("board.working", { working: board.working, maxAgents: board.maxAgents })}
              {waitingForRoom(board, saved) && ` ${t("board.waiting")}`}
            </p>
          )}
        </div>
        {loaded && projects.length === 0 && <p className="text-[12.5px] text-subtle">{t("board.noProjects")}</p>}
        {paused && <BlockerNotice blocker={paused} text={pausedText(t, paused)} />}
        {notice && <NoticeBar notice={notice} onDismiss={() => setNotice(null)} />}
        {board && board.cards.length === 0 && <p className="text-[12.5px] text-muted">{t("board.empty")}</p>}
        {projects.length > 0 && board && (
          <DndContext sensors={sensors} collisionDetection={collisionDetection} measuring={MEASURING} accessibility={{ announcements, screenReaderInstructions: { draggable: t("board.drag.help") } }} onDragStart={onDragStart} onDragOver={onDragOver} onDragEnd={onDragEnd} onDragCancel={onDragCancel}>
            <div className="flex min-h-0 flex-1 gap-2 overflow-x-auto">
              {COLUMNS.map((column) => (
                <Column key={column} column={column} cards={groups[column]} dragging={activeId !== null} refused={hoveredColumn === column && !isLane(column)} moving={moving} actions={actions}>
                  {column === "backlog" && (
                    <div className="mx-1.5 mb-1.5 flex flex-wrap gap-1">
                      <Button className="flex-1 justify-center px-1.5" onClick={() => setDialog({ kind: "task", card: null })}>
                        <Plus size={13} />
                        {t("board.newTask")}
                      </Button>
                      <Button className="flex-1 justify-center px-1.5" onClick={() => setDialog({ kind: "import" })}>
                        <Download size={13} />
                        {t("board.importIssue")}
                      </Button>
                    </div>
                  )}
                </Column>
              ))}
            </div>
            <DragOverlay>
              {activeCard && (
                <div className={cx("cursor-grabbing rounded-md border border-border-strong bg-elevated px-2 py-1.5 shadow-lg", hoveredColumn && !isLane(hoveredColumn) && "cursor-not-allowed border-danger")}>
                  <span className="block">
                    <CardFace card={activeCard} />
                  </span>
                </div>
              )}
            </DragOverlay>
          </DndContext>
        )}
      </main>
      {dialog?.kind === "task" && <TaskDialog projectId={projectId} card={dialog.card} onClose={() => setDialog(null)} onSaved={changed} />}
      {dialog?.kind === "delete" && <DeleteDialog card={dialog.card} onClose={() => setDialog(null)} onDeleted={changed} />}
      {dialog?.kind === "import" && <ImportDialog projectId={projectId} cards={board?.cards ?? []} onClose={() => setDialog(null)} onImported={load} />}
    </>
  );
}
