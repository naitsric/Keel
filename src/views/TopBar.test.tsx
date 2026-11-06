import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSearchEverywhere } from "../commands/SearchEverywhere";
import { callsTo, hideApp, note, paletteOpen, paletteRow, pressKey, pressMod, row, settle, showApp, view } from "../commands/testApp";
import i18next from "../i18n";
import { useApp, type View } from "../lib/store";

const RUNS = [row("r1", "running"), row("r2", "succeeded")];

const header = () => document.querySelector("header") as HTMLElement;
const nav = () => header().querySelector("nav");
const headerButton = (text: string) => [...header().querySelectorAll("button")].find((b) => b.textContent?.startsWith(text));
const settings = () => header().querySelector<HTMLButtonElement>('button[aria-label="Settings"]');
const currentOnes = () => [...header().querySelectorAll('[aria-current="page"]')].map((b) => b.textContent || b.getAttribute("aria-label"));
const click = async (button: HTMLElement | null | undefined) => {
  act(() => button?.click());
  await settle();
};

describe("the top bar", () => {
  beforeEach(() => void vi.useFakeTimers());

  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  it.each([
    ["Runs", "home"],
    ["Board", "board"],
    ["Workflows", "workflows"],
    ["Brain", "brain"],
  ])("%s goes to its page", async (label, page) => {
    await showApp(RUNS, { name: "settings" });
    await click(headerButton(label));
    expect(view().name).toBe(page);
  });

  it("goes to Settings from the gear", async () => {
    await showApp(RUNS);
    await click(settings());
    expect(view().name).toBe("settings");
  });

  it.each<[string, View, string]>([
    ["home", { name: "home" }, "Runs"],
    ["the board", { name: "board" }, "Board"],
    ["workflows", { name: "workflows" }, "Workflows"],
    ["brain", { name: "brain" }, "Brain"],
    ["settings", { name: "settings" }, "Settings"],
    ["a run opened from home", { name: "run", runId: "r1" }, "Runs"],
    ["a run opened from the board", { name: "run", runId: "r1", backTo: { name: "board", projectId: "p2" } }, "Board"],
    ["a run opened from the brain", { name: "run", runId: "r1", backTo: { name: "brain" } }, "Brain"],
  ])("on %s, says which page is current, and only that one", async (_name, at, label) => {
    await showApp(RUNS, at);
    expect(currentOnes()).toEqual([label]);
  });

  it("shows how many proposals wait, on the Brain", async () => {
    await showApp(RUNS);
    act(() => useApp.setState({ overview: { ...useApp.getState().overview, pendingProposals: 2 } }));
    expect(headerButton("Brain")?.textContent).toBe("Brain2");
    expect(headerButton("Brain")?.querySelector("span")?.getAttribute("aria-label")).toBe("2 proposals waiting");
  });

  it("shows how many drafts wait, on Workflows", async () => {
    await showApp(RUNS);
    expect(headerButton("Workflows")?.textContent).toBe("Workflows");
    act(() => useApp.setState({ overview: { ...useApp.getState().overview, workflowDrafts: 2 } }));
    expect(headerButton("Workflows")?.textContent).toBe("Workflows2");
    expect(headerButton("Workflows")?.querySelector("span")?.getAttribute("aria-label")).toBe("2 workflow drafts to review");
  });

  it("is in Spanish too", async () => {
    await showApp(RUNS);
    await act(() => i18next.changeLanguage("es"));
    const labels = [...(nav() as Element).querySelectorAll("button")].map((b) => b.textContent);
    expect(labels).toEqual(["Ejecuciones", "Tablero", "Workflows", "Brain"]);
    await act(() => i18next.changeLanguage("en"));
  });

  describe("Back", () => {
    it.each(["home", "board", "workflows", "brain", "settings"] as const)("isn't on %s", async (page) => {
      await showApp(RUNS, { name: page });
      expect(headerButton("Back")).toBeUndefined();
      expect(header().textContent).toContain("Keel");
    });

    it("is on a run, in place of the name, and goes home by default", async () => {
      await showApp(RUNS, { name: "run", runId: "r1" });
      expect(header().textContent).not.toContain("Keel");
      await click(headerButton("Back"));
      expect(view()).toEqual({ name: "home" });
    });

    it("goes back to the board's project a run was opened from", async () => {
      await showApp(RUNS, { name: "run", runId: "r1", backTo: { name: "board", projectId: "p2" } });
      await click(headerButton("Back"));
      expect(view()).toEqual({ name: "board", projectId: "p2" });
    });

    it("shows while a run loads, so there's always a way out", async () => {
      await showApp([], { name: "run", runId: "not-loaded" });
      await click(headerButton("Back"));
      expect(view()).toEqual({ name: "home" });
    });
  });

  it("from a run opened on the board, Board goes back to that project", async () => {
    await showApp(RUNS, { name: "run", runId: "r1", backTo: { name: "board", projectId: "p2" } });
    await click(headerButton("Board"));
    expect(view()).toEqual({ name: "board", projectId: "p2" });
  });

  it("a page's button on that page changes nothing, so the board keeps its project", async () => {
    await showApp(RUNS, { name: "board", projectId: "p2" });
    await click(headerButton("Board"));
    expect(view()).toEqual({ name: "board", projectId: "p2" });
  });

  it("drags the window from everything except what can be clicked", async () => {
    await showApp(RUNS, { name: "run", runId: "r1" });
    expect(header().hasAttribute("data-tauri-drag-region")).toBe(true);
    expect(nav()?.hasAttribute("data-tauri-drag-region")).toBe(true);
    const buttons = [...header().querySelectorAll("button")];
    expect(buttons.length).toBeGreaterThan(0);
    for (const button of buttons) expect(button.closest("[data-tauri-drag-region]")).not.toBe(button);
    expect(buttons.filter((button) => button.hasAttribute("data-tauri-drag-region"))).toEqual([]);
    expect(header().className).toContain("pl-[var(--traffic-lights-inset)]");
  });

  describe("the search field", () => {
    const field = () => headerButton("Search") as HTMLButtonElement;

    it("is a button that looks like a field, and shows ⇧⇧", async () => {
      await showApp(RUNS);
      expect(field().tagName).toBe("BUTTON");
      expect(field().textContent).toBe("Search⇧⇧");
      expect(header().querySelector("input")).toBeNull();
    });

    it("opens the palette on everything, empty, from any page", async () => {
      for (const at of [{ name: "home" }, { name: "board" }, { name: "workflows" }, { name: "brain" }, { name: "settings" }, { name: "run", runId: "r1" }] as View[]) {
        await showApp(RUNS, at);
        await click(field());
        expect(paletteOpen()).toBe(true);
        expect(useSearchEverywhere.getState()).toMatchObject({ scope: "all", query: "" });
        expect(view()).toEqual(at);
        await hideApp();
      }
      await showApp(RUNS);
    });

    it("leaves focus alone when clicked, so Esc comes back to the note", async () => {
      await showApp([row("r1", "waitingApproval")], { name: "run", runId: "r1" });
      const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true });
      act(() => void field().dispatchEvent(down));
      expect(down.defaultPrevented).toBe(true);
      expect(document.activeElement).toBe(note());
      await click(field());
      expect(paletteOpen()).toBe(true);
      pressKey("Escape", "Escape", {}, document.body);
      await settle();
      expect(paletteOpen()).toBe(false);
      expect(document.activeElement).toBe(note());
    });

    it("says Buscar in Spanish", async () => {
      await showApp(RUNS);
      await act(() => i18next.changeLanguage("es"));
      expect(field()).toBeUndefined();
      expect(headerButton("Buscar")?.textContent).toBe("Buscar⇧⇧");
      await act(() => i18next.changeLanguage("en"));
    });
  });

  describe("the primary button", () => {
    it.each<[string, View]>([
      ["home", { name: "home" }],
      ["workflows", { name: "workflows" }],
      ["brain", { name: "brain" }],
      ["settings", { name: "settings" }],
      ["a run", { name: "run", runId: "r1" }],
    ])("is New run on %s: it opens the dialog and starts nothing", async (_name, at) => {
      await showApp(RUNS, at);
      expect(headerButton("New run")?.title).toBe("New run  ⌘N");
      await click(headerButton("New run"));
      expect(useApp.getState().newRunOpen).toBe(true);
      expect(view()).toEqual(at);
      expect(callsTo("run_start")).toEqual([]);
    });

    it("is New task on the board, as ⌘N is there", async () => {
      await showApp(RUNS, { name: "board" });
      expect(headerButton("New run")).toBeUndefined();
      expect(headerButton("New task")?.title).toBe("New task  ⌘N");
      await click(headerButton("New task"));
      expect(useApp.getState().newRunOpen).toBe(false);
      expect(document.querySelector("form")).not.toBeNull();
      expect(document.body.textContent).toContain(i18next.t("board.task.newTitle"));
      expect(callsTo("card_create")).toEqual([]);
    });

    it("says what ⌘N would run, everywhere", async () => {
      for (const at of [{ name: "home" }, { name: "board" }, { name: "run", runId: "r1" }] as View[]) {
        await showApp(RUNS, at);
        pressMod("A", "KeyA", { shiftKey: true });
        const label = headerButton("New")?.textContent as string;
        expect(paletteRow(label)?.textContent, label).toBe(`${label}⌘N`);
        await hideApp();
      }
      await showApp(RUNS);
    });

    it("isn't there without a project to run in, on any page", async () => {
      for (const at of [{ name: "home" }, { name: "board" }, { name: "settings" }] as View[]) {
        await showApp(RUNS, at, []);
        expect(headerButton("New")).toBeUndefined();
        await hideApp();
      }
      await showApp(RUNS);
    });

    it("fits Nueva ejecución and Nueva tarea in Spanish", async () => {
      await showApp(RUNS);
      await act(() => i18next.changeLanguage("es"));
      expect(headerButton("Nueva")?.textContent).toBe("Nueva ejecución");
      await click(headerButton("Tablero"));
      expect(headerButton("Nueva")?.textContent).toBe("Nueva tarea");
      await act(() => i18next.changeLanguage("en"));
    });
  });

  describe("with nothing to go to yet", () => {
    it("has no pages on the welcome screen, and keeps Settings", async () => {
      await showApp([], { name: "home" }, []);
      expect(nav()).toBeNull();
      expect(headerButton("Search")).toBeUndefined();
      expect(headerButton("New run")).toBeUndefined();
      expect(settings()).not.toBeNull();
      expect(document.body.textContent).toContain(i18next.t("onboarding.title"));
    });

    it("has them on any other page, as the way back", async () => {
      await showApp([], { name: "home" }, []);
      await click(settings());
      expect(view().name).toBe("settings");
      expect(nav()).not.toBeNull();
      await click(headerButton("Runs"));
      expect(view().name).toBe("home");
      expect(nav()).toBeNull();
    });

    it("has them with every project removed and history to look at, without New run", async () => {
      await showApp(RUNS, { name: "home" }, []);
      expect(nav()).not.toBeNull();
      expect(headerButton("New run")).toBeUndefined();
      expect(document.body.textContent).toContain(i18next.t("onboarding.title"));
      expect(document.body.textContent).toContain("Run r1");
    });

    it("waits for the first answer, so someone with projects doesn't see it flash", async () => {
      await showApp([], { name: "home" });
      act(() => useApp.setState({ overviewLoaded: false, overview: { ...useApp.getState().overview, projects: [] } }));
      expect(nav()).not.toBeNull();
    });
  });
});
