import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { BoardCard } from "../lib/api";
import { useApp } from "../lib/store";
import { BoardView, TaskDialog } from "./BoardView";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const card = (extra: Partial<BoardCard> = {}): BoardCard => ({ id: "c1", projectId: "p1", title: "Log in through Valora", body: "", issue: null, column: "backlog", position: 0, runId: null, runState: null, reason: null, prUrl: null, prState: null, prStateAt: null, prCi: null, prCiFailed: [], prCommitsAdded: null, rejection: null, startBlocked: null, createdAt: "2026-11-10T00:00:00Z", jointChangeId: null, alsoTouches: [], part: null, ...extra });

describe("a card that touches other projects", () => {
  let root: Root;
  let calls: { command: string; args: unknown }[];
  let saved: number;

  const show = (shown: BoardCard | null) => act(() => root.render(<TaskDialog projectId="p1" card={shown} onClose={() => {}} onSaved={() => saved++} />));
  const page = () => document.body.textContent ?? "";
  const box = (name: string) => [...document.querySelectorAll("label")].find((label) => label.textContent === name)?.querySelector("input") as HTMLInputElement | undefined;
  const submit = () => act(async () => document.querySelector("form")?.requestSubmit());

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    calls = [];
    saved = 0;
    mockIPC((command, args) => {
      calls.push({ command, args });
      return command === "card_create" ? "new" : null;
    });
    useApp.setState({
      overview: {
        ...useApp.getState().overview,
        projects: [
          { id: "p1", name: "app" },
          { id: "p2", name: "valora" },
          { id: "p3", name: "dupplaBack" },
        ] as never,
      },
    });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  it("offers every other project, and a new card that touches one says so once it exists", async () => {
    show(null);
    expect(page()).toContain("Also touches");
    expect(box("app")).toBeUndefined();
    const title = document.querySelector("input:not([type])") as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(title, "Log in through Valora");
      title.dispatchEvent(new Event("input", { bubbles: true }));
    });
    act(() => box("valora")?.click());
    await submit();
    expect(calls.map((call) => call.command)).toEqual(["card_create", "card_set_projects"]);
    expect(calls[1].args).toEqual({ cardId: "new", projects: ["p2"] });
    expect(saved).toBe(1);
  });

  it("only says what changed, and taking every project away makes it an ordinary card", async () => {
    show(card({ jointChangeId: "c1", alsoTouches: ["p2"] }));
    expect(box("valora")?.checked).toBe(true);
    await submit();
    expect(calls.map((call) => call.command)).toEqual(["card_update"]);

    // Saving closes the dialog, so the next change is made in a new one.
    act(() => root.unmount());
    root = createRoot(document.body.appendChild(document.createElement("div")));
    calls = [];
    show(card({ jointChangeId: "c1", alsoTouches: ["p2"] }));
    act(() => box("valora")?.click());
    await submit();
    expect(calls[1]).toEqual({ command: "card_set_projects", args: { cardId: "c1", projects: [] } });
  });

  it("isn't offered on a part, which is already one project's share", () => {
    show(card({ jointChangeId: "plan", part: { changeId: "plan", position: 1, total: 2, waitsFor: null, warnings: [], contractChanged: [] } }));
    expect(page()).not.toContain("Also touches");
  });
});

describe("the board of every project", () => {
  let root: Root;
  let calls: { command: string; args: Record<string, unknown> }[];

  const PROJECTS = [
    { id: "p1", name: "app" },
    { id: "p2", name: "valora" },
  ];
  const CARDS = [card({ id: "c1", projectId: "p1", title: "Fix the login" }), card({ id: "c2", projectId: "p2", title: "Add a webhook" })];

  const settle = () =>
    act(async () => {
      for (let i = 0; i < 4; i++) await new Promise((resolve) => setTimeout(resolve));
    });
  const show = async (projects: { id: string; name: string }[], ui = <BoardView />) => {
    useApp.setState({ overview: { ...useApp.getState().overview, projects: projects as never }, overviewLoaded: true, view: { name: "board" } });
    await act(async () => root.render(ui));
    await settle();
  };
  const called = (command: string) => calls.filter((call) => call.command === command);
  const projectSelect = () => document.querySelector<HTMLSelectElement>("select")!;
  const choose = (select: HTMLSelectElement, value: string) =>
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")?.set?.call(select, value);
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
  const cardText = (id: string) => document.querySelector(`[data-card="${id}"]`)?.textContent ?? "";

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    calls = [];
    mockIPC((command, args) => {
      calls.push({ command, args: (args ?? {}) as Record<string, unknown> });
      const { projectId } = (args ?? {}) as { projectId?: string | null };
      if (command === "board_get") return { cards: CARDS.filter((c) => projectId == null || c.projectId === projectId), working: 0, maxAgents: 2, defaultWorkflow: null, paused: null };
      if (command === "card_create") return "new";
      if (command === "issues_list") return [{ number: 7, title: "Webhooks drop events", author: "ana", url: "https://github.com/acme/valora/issues/7", body: "" }];
      return null;
    });
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  it("is where the board starts with several projects, and each card says whose it is", async () => {
    await show(PROJECTS);
    expect(called("board_get").map((call) => call.args)).toEqual([{ projectId: null }]);
    expect(projectSelect().value).toBe("");
    expect(projectSelect().options[0].textContent).toBe("All projects");
    expect(cardText("c1")).toContain("app");
    expect(cardText("c2")).toContain("valora");

    // One project's board has only its cards, which don't need to say whose they are.
    await choose(projectSelect(), "p2");
    await settle();
    expect(called("board_get").at(-1)?.args).toEqual({ projectId: "p2" });
    expect(useApp.getState().view).toEqual({ name: "board", projectId: "p2" });
    expect(document.querySelector('[data-card="c1"]')).toBeNull();
    expect(cardText("c2")).not.toContain("valora");

    await choose(projectSelect(), "");
    await settle();
    expect(called("board_get").at(-1)?.args).toEqual({ projectId: null });
    expect(useApp.getState().view).toEqual({ name: "board" });
  });

  it("isn't offered with only one project, which the board shows", async () => {
    await show(PROJECTS.slice(0, 1));
    expect(projectSelect().value).toBe("p1");
    expect([...projectSelect().options].map((option) => option.textContent)).toEqual(["app"]);
    expect(called("board_get").map((call) => call.args)).toEqual([{ projectId: "p1" }]);
  });

  it("asks which project a new task is for, and only then offers the others it can touch", async () => {
    await show(PROJECTS, <TaskDialog projectId="" card={null} onClose={() => {}} onSaved={() => {}} />);
    const title = document.querySelector("input:not([type])") as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(title, "Add a webhook");
      title.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const create = [...document.querySelectorAll("button")].find((button) => button.textContent === "Create") as HTMLButtonElement;
    expect(create.disabled).toBe(true);
    expect(document.body.textContent).not.toContain("Also touches");

    await choose(projectSelect(), "p2");
    expect(create.disabled).toBe(false);
    const offered = [...document.querySelectorAll("fieldset label")].map((label) => label.textContent);
    expect(offered).toEqual(["app"]);
    await act(async () => document.querySelector("form")?.requestSubmit());
    expect(called("card_create")[0].args).toEqual({ projectId: "p2", title: "Add a webhook", body: "" });
  });

  it("asks whose issues to import, and an issue another project imported doesn't count", async () => {
    // app already has its own #7.
    CARDS[0].issue = { number: 7, title: "Login loops", author: "ana", url: "https://github.com/acme/app/issues/7" };
    try {
      await show(PROJECTS);
      const importButton = [...document.querySelectorAll("button")].find((button) => button.textContent === "Import issue") as HTMLButtonElement;
      await act(async () => importButton.click());
      await settle();
      const dialog = document.querySelector('[role="dialog"]') as HTMLElement;
      expect(called("issues_list")).toEqual([]);
      await choose(dialog.querySelector("select")!, "p2");
      await settle();
      expect(called("issues_list")[0].args).toEqual({ projectId: "p2" });
      expect(dialog.textContent).toContain("Webhooks drop events");
      expect(dialog.textContent).not.toContain("Imported");
    } finally {
      CARDS[0].issue = null;
    }
  });

  it("edits a card in its own project, without asking", async () => {
    await show(PROJECTS, <TaskDialog projectId="" card={CARDS[1]} onClose={() => {}} onSaved={() => {}} />);
    expect(document.querySelector("select")).toBeNull();
    expect([...document.querySelectorAll("fieldset label")].map((label) => label.textContent)).toEqual(["app"]);
  });
});
