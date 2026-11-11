import { describe, expect, it } from "vitest";
import i18next from "../i18n";
import enText from "../i18n/en.json";
import esText from "../i18n/es.json";
import type { AttemptState, Board, BoardCard, BoardColumn, Reason } from "./api";
import { boardNotice, canDelete, canDrag, canEdit, cardLabel, clickAction, dragOver, dropDestination, groupCards, importedIssues, importNotice, jointLabel, type Layout, layoutOf, moveNotice, newestOnly, otherLane, overColumn, pausedText, waitingForRoom, withLayout } from "./board";
import { needsProjectSettings, needsSignIn, reasonText } from "./reason";

const t = (language: string) => i18next.getFixedT(language);
const en = t("en");

const card = (id: string, column: BoardColumn, extra: Partial<BoardCard> = {}): BoardCard => ({
  id,
  projectId: "p1",
  title: `Task ${id}`,
  body: "",
  issue: null,
  column,
  position: 0,
  runId: null,
  runState: null,
  reason: null,
  prUrl: null,
  prState: null,
  prStateAt: null,
  prCi: null,
  prCiFailed: [],
  rejection: null,
  startBlocked: null,
  createdAt: "2026-11-05T00:00:00Z",
  jointChangeId: null,
  alsoTouches: [],
  part: null,
  ...extra,
});

const layout: Layout = { backlog: ["a", "b", "c"], todo: ["x", "y"] };

describe("what a done card says about its pull request", () => {
  const done = (extra: Partial<BoardCard>) => cardLabel(en, card("d", "done", { runState: "succeeded", prUrl: "https://github.com/acme/app/pull/12", ...extra }));

  it("says nothing until Keel knows, or if there isn't one", () => {
    expect(done({})).toBeNull();
    expect(cardLabel(en, card("d", "done", { runState: "succeeded" }))).toBeNull();
  });

  it("only reads as a success once it's merged, and says when", () => {
    const merged = done({ prState: "merged", prStateAt: "2026-11-06T10:00:00Z" });
    expect(merged?.label).toBe("Merged");
    expect(merged?.state).toBe("succeeded");
    expect(merged?.detail).toContain("2026");
    const closed = done({ prState: "closed", prStateAt: "2026-11-06T10:00:00Z" });
    expect(closed?.label).toBe("Closed without merging");
    expect(closed?.state).toBe("rejected");
    expect(closed?.detail).toContain("2026");
  });

  it("doesn't date one that's still open, since that's only when someone last touched it", () => {
    const open = done({ prState: "open", prStateAt: "2026-11-06T10:00:00Z" });
    expect(open?.label).toBe("Pull request open");
    expect(open?.state).toBe("queued");
    expect(open?.detail).toBeNull();
    expect(done({ prState: "draft", prStateAt: "2026-11-06T10:00:00Z" })?.label).toBe("Draft pull request");
  });

  it("speaks Spanish too", () => {
    expect(cardLabel(t("es"), card("d", "done", { prState: "merged", prStateAt: null }))?.label).toBe("Mergeado");
    expect(cardLabel(t("es"), card("d", "done", { prState: "closed", prStateAt: null }))?.label).toBe("Cerrado sin mergear");
  });

  it("turns red when its CI fails, and says which checks", () => {
    for (const prState of ["open", "draft"] as const) {
      const red = done({ prState, prCi: "failed", prCiFailed: ["lint", "pyright"] });
      expect(red?.state).toBe("failed");
      expect(red?.label).toBe(en(`board.pr.${prState}`));
      expect(red?.detail).toBe("CI failed: lint, pyright");
    }
    expect(cardLabel(t("es"), card("d", "done", { prState: "open", prCi: "failed", prCiFailed: ["lint"] }))?.detail).toBe("CI en rojo: lint");
    // Anything else about the CI stays off the card, and so does a failure once it's merged or closed.
    for (const extra of [{ prState: "open", prCi: "pending" }, { prState: "open", prCi: "passed" }, { prState: "open", prCi: "noCi" }, { prState: "merged", prCi: "failed", prCiFailed: ["lint"] }] as Partial<BoardCard>[]) {
      const label = done(extra);
      expect(label?.state).not.toBe("failed");
      expect(label?.detail ?? "").not.toContain("CI");
    }
  });

  it("only shows on a card that's done: its column still comes from the run", () => {
    expect(cardLabel(en, card("w", "inProgress", { runState: "running", prState: "merged" }))?.label).toBe(en("states.running"));
  });
});

describe("groupCards", () => {
  it("trusts the backend's column and keeps its order", () => {
    const groups = groupCards([card("b", "backlog"), card("r", "done"), card("a", "backlog"), card("w", "humanReview", { runState: "failed" })]);
    expect(groups.backlog.map((c) => c.id)).toEqual(["b", "a"]);
    expect(groups.done.map((c) => c.id)).toEqual(["r"]);
    expect(groups.humanReview.map((c) => c.id)).toEqual(["w"]);
    expect(groups.todo).toEqual([]);
    expect(groups.inProgress).toEqual([]);
  });

  it("doesn't read the run's state to pick a column", () => {
    expect(groupCards([card("r", "backlog", { runState: "rejected", runId: "r1" })]).backlog).toHaveLength(1);
    expect(groupCards([card("r", "done", { runState: "rejected", runId: "r1" })]).done).toHaveLength(1);
  });
});

describe("what can be done with a card", () => {
  const columns: BoardColumn[] = ["backlog", "todo", "inProgress", "humanReview", "done"];
  it("drags only out of Backlog and Todo", () => {
    expect(columns.filter((c) => canDrag(card("a", c)))).toEqual(["backlog", "todo"]);
  });

  it("drags a rejected card showing in Backlog", () => {
    expect(canDrag(card("a", "backlog", { runId: "r1", runState: "rejected" }))).toBe(true);
  });

  it("edits in Backlog and Todo, and deletes there and in Done", () => {
    expect(columns.filter((c) => canEdit(card("a", c)))).toEqual(["backlog", "todo"]);
    expect(columns.filter((c) => canDelete(card("a", c)))).toEqual(["backlog", "todo", "done"]);
  });

  it("opens the run if there is one, and the editor if there isn't", () => {
    expect(clickAction(card("a", "backlog"))).toBe("edit");
    expect(clickAction(card("a", "todo"))).toBe("edit");
    expect(clickAction(card("a", "backlog", { runId: "r1", runState: "rejected" }))).toBe("run");
    expect(clickAction(card("a", "done", { runId: "r1" }))).toBe("run");
    expect(clickAction(card("a", "inProgress"))).toBeNull();
  });
});

describe("dragging", () => {
  it("knows which column a card or a column id belongs to", () => {
    expect(overColumn(layout, "todo")).toBe("todo");
    expect(overColumn(layout, "done")).toBe("done");
    expect(overColumn(layout, "b")).toBe("backlog");
    expect(overColumn(layout, "y")).toBe("todo");
    expect(overColumn(layout, "elsewhere")).toBeNull();
  });

  it("takes the place of the card it hovers over in another lane", () => {
    expect(dragOver(layout, "a", "y", false)).toEqual({ backlog: ["b", "c"], todo: ["x", "a", "y"] });
    expect(dragOver(layout, "a", "y", true)).toEqual({ backlog: ["b", "c"], todo: ["x", "y", "a"] });
    expect(dragOver(layout, "x", "a", false)).toEqual({ backlog: ["x", "a", "b", "c"], todo: ["y"] });
  });

  it("goes to the bottom when it hovers over the lane itself", () => {
    expect(dragOver(layout, "a", "todo", false)).toEqual({ backlog: ["b", "c"], todo: ["x", "y", "a"] });
    expect(dragOver({ backlog: ["a"], todo: [] }, "a", "todo", false)).toEqual({ backlog: [], todo: ["a"] });
  });

  it("doesn't change anything within a lane or over a column Keel moves", () => {
    expect(dragOver(layout, "a", "c", false)).toBe(layout);
    expect(dragOver(layout, "a", "backlog", false)).toBe(layout);
    expect(dragOver(layout, "a", "inProgress", false)).toBe(layout);
    expect(dragOver(layout, "a", "humanReview", false)).toBe(layout);
    expect(dragOver(layout, "a", "done", false)).toBe(layout);
  });
});

describe("dropDestination", () => {
  const drop = (hovered: Layout, active: string, over: string | null) => dropDestination(layout, hovered, active, over);

  it("reorders within a lane", () => {
    expect(drop(layout, "a", "c")).toEqual({ lane: "backlog", index: 2, layout: { backlog: ["b", "c", "a"], todo: ["x", "y"] } });
    expect(drop(layout, "c", "a")).toEqual({ lane: "backlog", index: 0, layout: { backlog: ["c", "a", "b"], todo: ["x", "y"] } });
    expect(drop(layout, "a", "b")?.index).toBe(1);
    expect(drop(layout, "y", "x")).toEqual({ lane: "todo", index: 0, layout: { backlog: ["a", "b", "c"], todo: ["y", "x"] } });
  });

  it("uses the place the card took while hovering over another lane", () => {
    const hovered = dragOver(layout, "a", "y", false);
    expect(drop(hovered, "a", "y")?.lane).toBe("todo");
    expect(drop(hovered, "a", "a")).toEqual({ lane: "todo", index: 1, layout: hovered });
    expect(drop(hovered, "a", "x")).toEqual({ lane: "todo", index: 0, layout: { backlog: ["b", "c"], todo: ["a", "x", "y"] } });
  });

  it("sends the bottom of the lane when dropped on the lane itself", () => {
    const hovered = dragOver(layout, "a", "todo", false);
    expect(drop(hovered, "a", "todo")).toEqual({ lane: "todo", index: 2, layout: hovered });
    const empty = { backlog: ["a"], todo: [] };
    const moved = dragOver(empty, "a", "todo", false);
    expect(dropDestination(empty, moved, "a", "todo")).toEqual({ lane: "todo", index: 0, layout: moved });
  });

  it("is null when nothing changes", () => {
    expect(drop(layout, "b", "b")).toBeNull();
    expect(drop(layout, "b", "backlog")).toBeNull();
  });

  it("is null when dropped nowhere, or on a column Keel moves", () => {
    expect(drop(layout, "a", null)).toBeNull();
    for (const column of ["inProgress", "humanReview", "done"]) expect(drop(layout, "a", column)).toBeNull();
  });

  it("is null when the card left the lanes during the drag", () => {
    expect(drop(layout, "gone", "a")).toBeNull();
  });
});

describe("withLayout", () => {
  const groups = groupCards([card("a", "backlog"), card("b", "backlog"), card("x", "todo"), card("r", "inProgress", { runState: "running" })]);

  it("shows the server's columns when no drag is going on", () => {
    expect(withLayout(groups, null)).toBe(groups);
    expect(layoutOf(groups)).toEqual({ backlog: ["a", "b"], todo: ["x"] });
  });

  it("puts the cards where the drag has them, without changing the card itself", () => {
    const shown = withLayout(groups, { backlog: ["b"], todo: ["a", "x"] });
    expect(shown.backlog.map((c) => c.id)).toEqual(["b"]);
    expect(shown.todo.map((c) => c.id)).toEqual(["a", "x"]);
    expect(shown.todo[0].column).toBe("backlog");
    expect(shown.inProgress.map((c) => c.id)).toEqual(["r"]);
  });

  it("copes with a refresh during a drag", () => {
    // The scheduler started "x", so it can't stay in Todo. "n" just got created.
    const refreshed = groupCards([card("a", "backlog"), card("n", "backlog"), card("x", "inProgress", { runState: "running" })]);
    const shown = withLayout(refreshed, { backlog: ["a"], todo: ["x"] });
    expect(shown.backlog.map((c) => c.id)).toEqual(["a", "n"]);
    expect(shown.todo).toEqual([]);
    expect(shown.inProgress.map((c) => c.id)).toEqual(["x"]);
  });
});

describe("otherLane", () => {
  const groups = groupCards([card("a", "backlog"), card("b", "backlog"), card("x", "todo")]);
  it("goes to the bottom of the other lane", () => {
    expect(otherLane(groups, groups.backlog[0])).toEqual({ lane: "todo", index: 1 });
    expect(otherLane(groups, groups.todo[0])).toEqual({ lane: "backlog", index: 2 });
  });

  it("isn't there for columns Keel moves", () => {
    expect(otherLane(groups, card("r", "inProgress"))).toBeNull();
    expect(otherLane(groups, card("r", "humanReview"))).toBeNull();
    expect(otherLane(groups, card("r", "done"))).toBeNull();
  });
});

describe("waitingForRoom", () => {
  const board = (working: number, maxAgents: number, paused: Reason | null = null): Board => ({ cards: [], working, maxAgents, defaultWorkflow: null, paused });
  const withTodo = groupCards([card("x", "todo")]);
  it("is true only at the limit with cards in Todo", () => {
    expect(waitingForRoom(board(2, 2), withTodo)).toBe(true);
    expect(waitingForRoom(board(3, 2), withTodo)).toBe(true);
    expect(waitingForRoom(board(1, 2), withTodo)).toBe(false);
    expect(waitingForRoom(board(2, 2), groupCards([]))).toBe(false);
  });

  it("isn't the explanation when the board is paused", () => {
    expect(waitingForRoom(board(2, 2, { code: "rateLimited", resetsAt: null }), withTodo)).toBe(false);
    expect(waitingForRoom(board(0, 2, { code: "claudeLoggedOut" }), withTodo)).toBe(false);
  });
});

describe("pausedText", () => {
  const es = t("es");
  const resetsAt = 1800000000;

  it("says what lifts a usage limit, with the time when it's known", () => {
    const text = pausedText(en, { code: "rateLimited", resetsAt });
    expect(text).toContain("Todo is paused");
    expect(text).toContain("usage limit");
    expect(text).toContain("resets (");
    expect(text).toContain("blocked card");
    expect(text).not.toContain("until this is fixed");
    const noTime = pausedText(en, { code: "rateLimited", resetsAt: null });
    expect(noTime).toContain("when the limit resets.");
    expect(noTime).toContain("blocked card");
    expect(noTime).not.toContain("(");
  });

  it("says it in Spanish too", () => {
    const text = pausedText(es, { code: "rateLimited", resetsAt: null });
    expect(text).toContain("Por hacer está en pausa");
    expect(text).toContain("se renueve");
    expect(text).toContain("tarjeta bloqueada");
  });

  it("gives the reason's own sentence for anything else", () => {
    const reason = { code: "claudeLoggedOut" };
    expect(pausedText(en, reason)).toBe(`${en("board.paused")} ${reasonText(en, reason)}`);
    expect(pausedText(es, reason)).toContain("No has iniciado sesión");
  });

  it("offers signing in only when that's the way out", () => {
    expect(needsSignIn({ code: "claudeLoggedOut" })).toBe(true);
    expect(needsSignIn({ code: "rateLimited", resetsAt })).toBe(false);
  });
});

describe("newestOnly", () => {
  const deferred = <T>() => {
    let resolve!: (value: T) => void;
    let reject!: (error: unknown) => void;
    const promise = new Promise<T>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    return { promise, resolve, reject };
  };
  const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

  it("applies a single answer", async () => {
    const applied: string[] = [];
    await newestOnly<string>((value) => applied.push(value))(() => Promise.resolve("a"));
    expect(applied).toEqual(["a"]);
  });

  it("drops an old answer that arrives after the newest", async () => {
    const applied: string[] = [];
    const load = newestOnly<string>((value) => applied.push(value));
    const [old, fresh] = [deferred<string>(), deferred<string>()];
    const first = load(() => old.promise);
    const second = load(() => fresh.promise);
    fresh.resolve("fresh");
    await second;
    old.resolve("old");
    await first;
    expect(applied).toEqual(["fresh"]);
  });

  it("keeps an overtaken caller waiting until the newest answer is in", async () => {
    const applied: string[] = [];
    const load = newestOnly<string>((value) => applied.push(value));
    const [old, fresh] = [deferred<string>(), deferred<string>()];
    let firstDone = false;
    const first = load(() => old.promise).then(() => {
      firstDone = true;
    });
    const second = load(() => fresh.promise);
    old.resolve("old");
    await settle();
    expect(applied).toEqual([]);
    expect(firstDone).toBe(false);
    fresh.resolve("fresh");
    await Promise.all([first, second]);
    expect(applied).toEqual(["fresh"]);
    expect(firstDone).toBe(true);
  });

  it("ignores an overtaken failure, and reports the newest one to everyone waiting", async () => {
    const applied: string[] = [];
    const load = newestOnly<string>((value) => applied.push(value));
    const [old, fresh] = [deferred<string>(), deferred<string>()];
    const first = load(() => old.promise);
    const second = load(() => fresh.promise);
    old.reject(new Error("old"));
    await settle();
    fresh.resolve("fresh");
    await expect(first).resolves.toBeUndefined();
    await expect(second).resolves.toBeUndefined();
    expect(applied).toEqual(["fresh"]);

    const [third, fourth] = [deferred<string>(), deferred<string>()];
    const waiting = load(() => third.promise);
    const latest = load(() => fourth.promise);
    third.resolve("third");
    await settle();
    fourth.reject(new Error("newest"));
    await expect(waiting).rejects.toThrow("newest");
    await expect(latest).rejects.toThrow("newest");
    expect(applied).toEqual(["fresh"]);
  });
});

describe("importedIssues", () => {
  it("are the issue numbers already on the board", () => {
    const issue = (number: number) => ({ number, title: "t", author: "a", url: "u" });
    expect([...importedIssues([card("a", "backlog", { issue: issue(7) }), card("b", "done", { issue: issue(9) }), card("c", "todo")])].sort()).toEqual([7, 9]);
  });
});

describe("cardLabel", () => {
  const review = (runState: AttemptState, reason: Reason | null = null) => cardLabel(en, card("a", "humanReview", { runId: "r1", runState, reason }));

  it("says what Human Review needs, for each state", () => {
    expect(review("waitingApproval")).toEqual({ state: "waitingApproval", label: "Needs your approval", detail: "Open it to approve or reject." });
    expect(review("waitingAnswers")).toEqual({ state: "waitingAnswers", label: "Needs your decision", detail: "Open it to answer." });
    expect(review("failed")?.label).toBe("Failed");
    expect(review("blocked")?.label).toBe("Blocked");
    expect(review("stalled")?.label).toBe("Looks stuck");
    expect(review("interrupted")?.label).toBe("Interrupted");
    expect(review("stopped")?.label).toBe("Stopped");
    for (const state of ["failed", "blocked", "stalled", "interrupted", "stopped"] as const) {
      expect(review(state)?.state).toBe(state);
      expect(review(state)?.detail).not.toBe("");
      expect(review(state)?.detail).toContain("Open it");
    }
  });

  it("uses the reason's sentence when there is one", () => {
    const reason = { code: "reworkLimit", rounds: 2 };
    expect(review("blocked", reason)?.detail).toBe(reasonText(en, reason));
    expect(review("interrupted", { code: "agentLost" })?.detail).toContain("lost track");
    expect(review("stalled", { code: "stalled", command: "make test", minutes: 3, cpuPercent: 0 })?.detail).toContain("make test");
  });

  it("speaks Spanish too", () => {
    expect(cardLabel(t("es"), card("a", "humanReview", { runState: "waitingApproval" }))?.label).toBe("Falta tu aprobación");
    expect(cardLabel(t("es"), card("a", "humanReview", { runState: "failed" }))?.detail).toContain("Ábrela");
  });

  it("shows why a card came back to Backlog", () => {
    expect(cardLabel(en, card("a", "backlog", { runState: "rejected", rejection: "Use the helper." }))).toEqual({ state: "rejected", label: "Rejected", detail: "Use the helper." });
    expect(cardLabel(en, card("a", "backlog", { runState: "rejected", rejection: "  " }))?.detail).toBeNull();
    expect(cardLabel(en, card("a", "backlog", { runState: "abandoned" }))).toEqual({ state: "abandoned", label: "Abandoned", detail: null });
  });

  it("says why a card couldn't start", () => {
    const label = cardLabel(en, card("a", "backlog", { startBlocked: { code: "defaultWorkflowMissing" } }));
    expect(label?.label).toBe("Couldn't start");
    expect(label?.detail).toContain("no longer exists");
  });

  it("shows the state of what's in progress, including a run with no attempt yet", () => {
    expect(cardLabel(en, card("a", "inProgress", { runState: "running" }))).toEqual({ state: "running", label: "Running", detail: null });
    expect(cardLabel(en, card("a", "inProgress"))?.state).toBe("queued");
    expect(cardLabel(en, card("a", "inProgress", { runState: "queued", reason: { code: "waitingForCapacity" } }))?.detail).toContain("free agent slot");
  });

  it("adds nothing to plain cards", () => {
    expect(cardLabel(en, card("a", "backlog"))).toBeNull();
    expect(cardLabel(en, card("a", "todo"))).toBeNull();
    expect(cardLabel(en, card("a", "done", { runState: "succeeded" }))).toBeNull();
  });
});

describe("needsProjectSettings", () => {
  it("is true when the fix is in Settings", () => {
    for (const code of ["githubAccountMissing", "noGithubRemote", "defaultWorkflowMissing"]) expect(needsProjectSettings({ code })).toBe(true);
    expect(needsProjectSettings({ code: "deliveryFailed", kind: "notSignedIn" })).toBe(true);
    expect(needsProjectSettings({ code: "deliveryFailed", kind: "noPermission" })).toBe(true);
  });

  it("is false otherwise", () => {
    expect(needsProjectSettings({ code: "deliveryFailed", kind: "repoRules" })).toBe(false);
    expect(needsProjectSettings({ code: "claudeLoggedOut" })).toBe(false);
    expect(needsProjectSettings(null)).toBe(false);
    expect(needsProjectSettings(undefined)).toBe(false);
  });
});

describe("notices", () => {
  const blocked = (code: string) => ({ code: "blocked", reason: { code }, detail: null });

  it("leads to Settings when a move fails for a missing account", () => {
    const notice = moveNotice(en, card("a", "backlog"), blocked("githubAccountMissing"));
    expect(notice.settings).toBe(true);
    expect(notice.text).toContain("Task a");
    expect(notice.text).toContain("Choose which GitHub account");
  });

  it("doesn't lead to Settings for what can't be fixed there", () => {
    expect(boardNotice(en, blocked("claudeLoggedOut")).settings).toBe(false);
    expect(boardNotice(en, "boom")).toEqual({ text: en("errors.internal", { detail: "boom" }), settings: false });
  });

  it("says plainly when Keel got to the card first", () => {
    expect(boardNotice(en, { code: "invalidState", reason: null, detail: null })).toEqual({ text: en("board.cardBusy"), settings: false });
  });

  it("talks about issues when they can't be listed", () => {
    expect(importNotice(en, blocked("noGithubRemote"))).toEqual({ text: en("board.import.noRemote"), settings: true });
    expect(importNotice(en, blocked("githubAccountMissing"))).toEqual({ text: en("board.import.noAccount"), settings: true });
    expect(importNotice(en, blocked("ghMissing")).text).toContain("GitHub CLI");
  });
});

describe("what a card says about a change across several projects", () => {
  const names = (id: string) => ({ p2: "valora", p3: "dupplaBack" })[id] ?? id;
  const part = (extra: Partial<NonNullable<BoardCard["part"]>> = {}) => ({ changeId: "plan", position: 2, total: 2, waitsFor: null, warnings: [], ...extra });

  it("says nothing on an ordinary card", () => {
    expect(jointLabel(en, card("a", "todo"), names)).toBeNull();
  });

  it("names the other projects where it's agreed", () => {
    const agreeing = card("plan", "backlog", { jointChangeId: "plan", alsoTouches: ["p2", "p3"] });
    expect(jointLabel(en, agreeing, names)).toEqual({ text: "Change across several projects", detail: "Also touches valora, dupplaBack", warnings: [] });
    expect(jointLabel(t("es"), agreeing, names)?.text).toBe("Cambio en varios proyectos");
  });

  it("says which part it is in Todo, and what it waits for", () => {
    const waiting = card("b", "todo", { jointChangeId: "plan", part: part({ waitsFor: { cardId: "a", position: 1, project: "valora" } }) });
    expect(jointLabel(en, waiting, names)).toEqual({ text: "Part 2 of 2", detail: "Waits for part 1, in valora, to have a pull request", warnings: [] });
    expect(jointLabel(t("es"), waiting, names)?.detail).toBe("Espera a que la parte 1, en valora, tenga pull request");
    expect(jointLabel(en, card("b", "todo", { jointChangeId: "plan", part: part() }), names)?.detail).toBeNull();
  });

  it("warns about what became of the parts before it", () => {
    const warned = card("b", "humanReview", {
      jointChangeId: "plan",
      part: part({
        warnings: [
          { kind: "changed", position: 1, project: "valora" },
          { kind: "closed", position: 1, project: "valora" },
        ],
      }),
    });
    expect(jointLabel(en, warned, names)?.warnings).toEqual(["Part 1, in valora, has commits this part didn't read.", "Part 1, in valora: its pull request was closed."]);
  });
});

describe("the words for a change across several projects", () => {
  it("never say plan, which already means the subscription, nor what Keel calls things inside", () => {
    const flatten = (value: unknown): string[] => (typeof value === "string" ? [value] : Object.values(value as object).flatMap(flatten));
    const ofRun = (run: object) => Object.entries(run).flatMap(([key, text]) => (typeof text === "string" && (key.startsWith("agreement") || key === "specParts" || key === "specTogether") ? [text] : []));
    for (const text of [enText, esText].flatMap((words) => [...flatten(words.joint), ...flatten(words.board.joint), words.board.task.alsoTouches, words.board.task.alsoTouchesHint, ...ofRun(words.run)])) {
      for (const word of ["plan", "planes", "plans", "joint", "spec", "specifier", "mirror", "basis", "worktree", "agreement"]) {
        expect(text.toLowerCase().split(/[^\p{L}]+/u), text).not.toContain(word);
      }
    }
  });
});
