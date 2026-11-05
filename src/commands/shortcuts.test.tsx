import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import { useApp } from "../lib/store";
import { nextWaitingRun, openRun } from "./runs";
import { useSearchEverywhere } from "./SearchEverywhere";
import { button, callsTo, hideApp, note, paletteOpen, paletteRow, paletteRows, pressEnter, pressKey, pressMod, PROJECTS, row, settle, showApp, tapShift, type, view } from "./testApp";

const RUNS = [row("r1", "running"), row("r2", "waitingApproval"), row("r3", "succeeded"), row("r4", "blocked"), row("r5", "queued")];

describe("the keys", () => {
  beforeEach(() => void vi.useFakeTimers());

  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  it.each([
    ["1", "Digit1", "home"],
    ["2", "Digit2", "board"],
    ["3", "Digit3", "workflows"],
    ["4", "Digit4", "brain"],
  ])("⌘%s goes to its page", async (key, code, page) => {
    await showApp(RUNS, { name: "settings" });
    pressMod(key, code);
    await settle();
    expect(view().name).toBe(page);
  });

  it("works while typing in a field", async () => {
    await showApp(RUNS, { name: "run", runId: "r2" });
    expect(document.activeElement).toBe(note());
    pressMod("3", "Digit3");
    await settle();
    expect(view().name).toBe("workflows");
  });

  it("does nothing while a dialog is open", async () => {
    await showApp(RUNS);
    useApp.setState({ newRunOpen: true });
    await settle();
    pressMod("3", "Digit3");
    pressMod("o", "KeyO");
    tapShift();
    tapShift();
    await settle();
    expect(view().name).toBe("home");
    expect(paletteOpen()).toBe(false);
  });

  it("Esc closes the dialog, and then the keys work again", async () => {
    await showApp(RUNS);
    pressMod("n", "KeyN");
    await settle();
    expect(useApp.getState().newRunOpen).toBe(true);
    pressKey("Escape", "Escape", {}, document.body);
    await settle();
    expect(useApp.getState().newRunOpen).toBe(false);
    pressMod("3", "Digit3");
    await settle();
    expect(view().name).toBe("workflows");
  });

  describe("⌘N", () => {
    it("opens New run, and starts nothing", async () => {
      await showApp(RUNS);
      pressMod("n", "KeyN");
      await settle();
      expect(useApp.getState().newRunOpen).toBe(true);
      expect(callsTo("run_start")).toEqual([]);
    });

    it("is New task on the board", async () => {
      await showApp(RUNS, { name: "board" });
      pressMod("n", "KeyN");
      await settle();
      expect(useApp.getState().newRunOpen).toBe(false);
      expect(document.querySelector("form")).not.toBeNull();
      expect(document.body.textContent).toContain(i18next.t("board.task.newTitle"));
      expect(callsTo("card_create")).toEqual([]);
    });
  });

  describe("Back", () => {
    it("⌘[ goes where the run was opened from", async () => {
      await showApp(RUNS, { name: "run", runId: "r2", backTo: { name: "board", projectId: "p2" } });
      pressMod("[", "BracketLeft");
      await settle();
      expect(view()).toEqual({ name: "board", projectId: "p2" });
    });

    it("⌘[ goes home otherwise, and leaves the key alone there", async () => {
      await showApp(RUNS, { name: "brain" });
      pressMod("[", "BracketLeft");
      await settle();
      expect(view()).toEqual({ name: "home" });
      expect(pressMod("[", "BracketLeft").defaultPrevented).toBe(false);
    });

    it("⌘[ works from a text field, and ⌘↑ leaves it alone", async () => {
      await showApp(RUNS, { name: "run", runId: "r2" });
      expect(document.activeElement).toBe(note());
      expect(pressMod("ArrowUp", "ArrowUp").defaultPrevented).toBe(false);
      await settle();
      expect(view().name).toBe("run");

      pressMod("[", "BracketLeft");
      await settle();
      expect(view().name).toBe("home");
    });

    it("⌘↑ goes back outside text fields", async () => {
      await showApp(RUNS, { name: "workflows" });
      (document.activeElement as HTMLElement | null)?.blur?.();
      expect(pressMod("ArrowUp", "ArrowUp").defaultPrevented).toBe(true);
      await settle();
      expect(view().name).toBe("home");
    });
  });

  describe("⌘G", () => {
    it("goes through the runs waiting, both ways, going round", async () => {
      await showApp(RUNS);
      pressMod("g", "KeyG");
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      pressMod("g", "KeyG");
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r4" });
      pressMod("g", "KeyG");
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      pressMod("G", "KeyG", { shiftKey: true });
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r4" });
    });

    it("passes the key when nothing is waiting", async () => {
      await showApp([row("r1", "running"), row("r3", "succeeded")]);
      expect(pressMod("g", "KeyG").defaultPrevented).toBe(false);
      expect(view()).toEqual({ name: "home" });
    });

    it("Back goes where the person was, not through the runs they passed", async () => {
      await showApp(RUNS, { name: "workflows" });
      pressMod("g", "KeyG");
      await settle();
      pressMod("g", "KeyG");
      await settle();
      pressMod("[", "BracketLeft");
      await settle();
      expect(view()).toEqual({ name: "workflows" });
    });
  });

  describe("⌘D", () => {
    it("shows the run's changes", async () => {
      await showApp(RUNS, { name: "run", runId: "r2" });
      pressMod("d", "KeyD");
      await settle();
      expect(callsTo("run_diff")).toHaveLength(1);
      expect(document.body.textContent).toContain(i18next.t("run.changesTitle"));
    });

    it("isn't bound off the run page", async () => {
      await showApp(RUNS);
      expect(pressMod("d", "KeyD").defaultPrevented).toBe(false);
      expect(callsTo("run_diff")).toEqual([]);
    });

    it("passes when the run changed nothing, as there's no button for it either", async () => {
      await showApp([row("empty", "running")], { name: "run", runId: "empty" });
      expect(pressMod("d", "KeyD").defaultPrevented).toBe(false);
      expect(callsTo("run_diff")).toEqual([]);
    });
  });

  describe("⌘F2", () => {
    it("stops the step while one runs", async () => {
      await showApp([row("r1", "running")], { name: "run", runId: "r1" });
      pressMod("F2", "F2");
      await settle();
      expect(callsTo("attempt_stop")).toEqual([{ command: "attempt_stop", args: { attemptId: "r1-builder" } }]);
    });

    it("passes on a run that isn't working, and off the run page", async () => {
      await showApp(RUNS, { name: "run", runId: "r2" });
      expect(pressMod("F2", "F2").defaultPrevented).toBe(false);
      await hideApp();
      await showApp(RUNS);
      expect(pressMod("F2", "F2").defaultPrevented).toBe(false);
      expect(callsTo("attempt_stop")).toEqual([]);
    });

    it("doesn't repeat while held", async () => {
      await showApp([row("r1", "running")], { name: "run", runId: "r1" });
      pressMod("F2", "F2", { repeat: true });
      await settle();
      expect(callsTo("attempt_stop")).toEqual([]);
    });
  });

  describe("the palette", () => {
    const rowTexts = () => paletteRows().map((r) => r.textContent);
    const headings = () => [...document.querySelectorAll("[cmdk-group-heading]")].map((h) => h.textContent);

    it("⇧⇧ searches runs, projects and actions", async () => {
      await showApp(RUNS);
      tapShift();
      tapShift();
      expect(headings()).toEqual(["Runs", "Projects", "Navigate", "Run"]);
      expect(paletteRow("Run r2")?.textContent).toContain("Waiting for you");
      expect(paletteRow("payments")).toBeDefined();
      expect(paletteRow("Go to Brain")?.textContent).toBe("Go to Brain⌘4");
    });

    it("⇧⌘A only has actions", async () => {
      await showApp(RUNS);
      pressMod("A", "KeyA", { shiftKey: true });
      expect(headings()).toEqual(["Navigate", "Run"]);
      expect(paletteRow("Run r2")).toBeUndefined();
      expect(paletteRow("payments")).toBeUndefined();
    });

    it("⌘O only has runs, every one of them", async () => {
      const many = Array.from({ length: 30 }, (_, i) => row(`run${i}`, "succeeded"));
      await showApp(many);
      pressMod("o", "KeyO");
      expect(headings()).toEqual(["Runs"]);
      expect(paletteRows()).toHaveLength(30);
    });

    it("with nothing typed, ⇧⇧ doesn't let the runs push everything else away, and typing finds them all", async () => {
      const many = Array.from({ length: 30 }, (_, i) => row(`run${i}`, "succeeded"));
      await showApp(many);
      tapShift();
      tapShift();
      expect(paletteRows().filter((r) => r.textContent?.startsWith("Run run"))).toHaveLength(8);
      type("run29");
      await settle();
      expect(paletteRow("Run run29")).toBeDefined();
    });

    it("finds a run by its title, and not by its id", async () => {
      await showApp([row("deadbeef", "succeeded", { title: "Fix the invoices" }), row("feedface", "succeeded", { title: "Rename the module" })]);
      pressMod("o", "KeyO");
      type("invoices");
      await settle();
      expect(rowTexts().map((text) => text?.split("demo")[0])).toEqual(["Fix the invoices"]);
      type("deadbeef");
      await settle();
      expect(rowTexts()).toEqual([]);
      expect(document.body.textContent).toContain("Nothing found");
    });

    it("finds a project by name, and opens its board", async () => {
      await showApp(RUNS);
      tapShift();
      tapShift();
      type("payments");
      await settle();
      expect(paletteRow("payments")).toBeDefined();
      pressEnter();
      await settle();
      expect(view()).toEqual({ name: "board", projectId: "p2" });
    });

    it("opens a project's board while the board is showing another, and Back from a run stays on it", async () => {
      await showApp(RUNS, { name: "board" });
      await settle();
      expect(document.querySelector("select")?.value).toBe(PROJECTS[0].id);
      tapShift();
      tapShift();
      type("payments");
      await settle();
      pressEnter();
      await settle();
      expect(document.querySelector("select")?.value).toBe("p2");
      expect(callsTo("board_get").at(-1)?.args).toEqual({ projectId: "p2" });

      act(() => openRun("r2"));
      await settle();
      pressMod("[", "BracketLeft");
      await settle();
      expect(document.querySelector("select")?.value).toBe("p2");
    });

    it("Back from a run lands on the project picked in the selector", async () => {
      await showApp(RUNS, { name: "board" });
      const select = document.querySelector("select") as HTMLSelectElement;
      act(() => {
        Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")?.set?.call(select, "p2");
        select.dispatchEvent(new Event("change", { bubbles: true }));
      });
      await settle();
      expect(view()).toEqual({ name: "board", projectId: "p2" });
    });

    it("finds Settings, and shows the key the native menu handles", async () => {
      await showApp(RUNS);
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("Go to Settings")?.textContent).toBe("Go to Settings⌘,");
      await act(async () => paletteRow("Go to Settings")?.click());
      await settle();
      expect(view()).toEqual({ name: "settings" });
    });

    it("shows ⌘N on the row it runs: New task on the board, New run elsewhere", async () => {
      await showApp(RUNS, { name: "board" });
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("New task")?.textContent).toBe("New task⌘N");
      expect(paletteRow("New run")?.textContent).toBe("New run");
      await hideApp();
      await showApp(RUNS);
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("New run")?.textContent).toBe("New run⌘N");
      expect(paletteRow("New task")).toBeUndefined();
    });

    it("New run from the board still works from its row", async () => {
      await showApp(RUNS, { name: "board" });
      pressMod("A", "KeyA", { shiftKey: true });
      await act(async () => paletteRow("New run")?.click());
      await settle();
      expect(useApp.getState().newRunOpen).toBe(true);
    });

    it("a page's commands come and go with it, and a disabled one is shown but inert", async () => {
      await showApp(RUNS, { name: "run", runId: "r2" });
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("Stop the step")?.getAttribute("aria-disabled")).toBe("true");
      await act(async () => paletteRow("Stop the step")?.click());
      expect(paletteOpen()).toBe(true);
      expect(callsTo("attempt_stop")).toEqual([]);
      await hideApp();
      await showApp(RUNS);
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("Stop the step")).toBeUndefined();
    });

    it("holds the keymap while open, so keys don't act behind it", async () => {
      await showApp([row("r1", "running")], { name: "run", runId: "r1" });
      pressMod("A", "KeyA", { shiftKey: true });
      pressMod("F2", "F2");
      pressMod("3", "Digit3");
      await settle();
      expect(callsTo("attempt_stop")).toEqual([]);
      expect(view().name).toBe("run");
    });

    it("Esc closes it, gives focus back, and the note is still there", async () => {
      await showApp(RUNS, { name: "run", runId: "r2" });
      type("Half a thought");
      tapShift();
      tapShift();
      expect(paletteOpen()).toBe(true);
      expect(document.activeElement).not.toBe(note());
      pressKey("Escape", "Escape", {}, document.body);
      await settle();
      expect(paletteOpen()).toBe(false);
      expect(document.activeElement).toBe(note());
      expect((note() as HTMLTextAreaElement).value).toBe("Half a thought");
    });

    it("a note survives going elsewhere by key and coming back", async () => {
      await showApp(RUNS, { name: "run", runId: "r2" });
      type("Half a thought");
      pressMod("1", "Digit1");
      await settle();
      expect(note()).toBeNull();
      pressMod("g", "KeyG");
      await settle();
      expect((note() as HTMLTextAreaElement).value).toBe("Half a thought");
    });

    it("comes in Spanish, and doesn't need the accents typed", async () => {
      await showApp(RUNS);
      await act(() => i18next.changeLanguage("es"));
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("Ir a Brain")?.textContent).toBe("Ir a Brain⌘4");
      expect(button("Nueva ejecución")).toBeDefined();
      type("ejecucion");
      await settle();
      expect(paletteRow("Nueva ejecución")).toBeDefined();
      await act(() => i18next.changeLanguage("en"));
    });

    it("reopens empty, whatever was typed", async () => {
      await showApp(RUNS);
      tapShift();
      tapShift();
      type("payments");
      pressKey("Escape", "Escape", {}, document.body);
      await settle();
      tapShift();
      tapShift();
      expect(useSearchEverywhere.getState().query).toBe("");
      expect(paletteRow("Run r1")).toBeDefined();
    });
  });

});

describe("nextWaitingRun", () => {
  const runs = [row("a", "running"), row("b", "failed"), row("c", "succeeded"), row("d", "stalled"), row("e", "waitingApproval")];
  const next = (id: string | null, step: 1 | -1 = 1) => nextWaitingRun(runs, id, step)?.id;

  it("starts at the newest going forward, and the oldest going back", () => {
    expect(next(null)).toBe("b");
    expect(next(null, -1)).toBe("e");
    expect(next("gone")).toBe("b");
  });

  it("goes round from where the person is, even on a run that isn't waiting", () => {
    expect(next("b")).toBe("d");
    expect(next("e")).toBe("b");
    expect(next("b", -1)).toBe("e");
    expect(next("c")).toBe("d");
    expect(next("c", -1)).toBe("b");
  });

  it("has nothing to offer when nothing waits", () => {
    expect(nextWaitingRun([row("a", "running")], null, 1)).toBeNull();
  });

  it("means what the board's Human review means, for a run still open", () => {
    const waiting = (state: Parameters<typeof row>[1]) => nextWaitingRun([row("x", state)], null, 1) !== null;
    for (const state of ["waitingApproval", "blocked", "stalled", "failed", "stopped", "interrupted"] as const) expect(waiting(state), state).toBe(true);
    for (const state of ["queued", "preparing", "running", "succeeded", "skipped", "rejected", "abandoned"] as const) expect(waiting(state), state).toBe(false);
  });
});
