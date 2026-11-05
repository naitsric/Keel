// Approving publishes work, and plain Enter runs whatever is in the palette. These hold the line.
import { emit } from "@tauri-apps/api/event";
import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import en from "../i18n/en.json";
import es from "../i18n/es.json";
import { useApp, type View } from "../lib/store";
import { buildMenuModel, type MenuSpec, type PredefinedItem } from "../menu/menuModel";
import { runMenuCommand } from "../menu/nativeMenu";
import { KEYS } from "./keys";
import { useCommands, type Command } from "./registry";
import { answers, button, callsTo, calls, hideApp, note, paletteOpen, paletteRow, paletteRows, pressEnter, pressKey, pressMod, row, settle, showApp, tapShift, type, view } from "./testApp";

/** The backend calls that decide, spend or can't be taken back. No command may ever reach one. */
const DECIDES = ["approval_answer", "rework_limit_answer", "run_start", "attempt_retry", "attempt_abandon", "brain_accept", "brain_dismiss", "brain_retire", "brain_generate_map", "decision_retire", "card_move", "card_delete", "project_add", "project_remove", "project_try_check"];
/** These arrive with the add-project work, so they may not be in api.ts yet. */
const NOT_ALWAYS_THERE = ["project_remove", "project_try_check"];

/** Words that mean the same, in the ids and in what the person reads. "New run" opens a dialog, so it's allowed. */
const DECIDING_WORDS = /approve|reject|retry|resume|abandon|accept|retire|dismiss|\bto ?do\b|\bstart|deliver|merge|remove|delete|another round/i;
const DECIDING_WORDS_ES = /aprob|rechaz|reintent|reanud|abandon|acept|retir|descart|inici|entreg|fusion|elimin|otra ronda/i;
/** The same calls, as the code writes them. */
const DECIDING_CALL = /api\.(approvalAnswer|reworkLimitAnswer|runStart|attemptRetry|attemptAbandon|brainAccept|brainDismiss|brainRetire|brainGenerateMap|decisionRetire|cardMove|cardDelete|projectAdd|projectRemove|projectTryCheck)\b/;

/** The app's own code as text, by path. */
const SOURCES = import.meta.glob<string>(["/src/**/*.{ts,tsx}", "!/src/**/*.test.{ts,tsx}"], { query: "?raw", import: "default", eager: true });

const translated = (language: object, key: string) => key.split(".").reduce<unknown>((value, part) => (value as Record<string, unknown>)?.[part], language) as string | undefined;

const WAITING = "r1";
const PAGES: { name: string; view: View }[] = [
  { name: "home", view: { name: "home" } },
  { name: "a run waiting for approval", view: { name: "run", runId: WAITING } },
  { name: "a run that's working", view: { name: "run", runId: "working" } },
  { name: "a run that's blocked", view: { name: "run", runId: "stuck" } },
  { name: "the board", view: { name: "board" } },
  { name: "workflows", view: { name: "workflows" } },
  { name: "brain", view: { name: "brain" } },
  { name: "settings", view: { name: "settings" } },
];
const RUNS = [row(WAITING, "waitingApproval"), row("r2", "waitingApproval"), row("empty", "waitingApproval"), row("working", "running"), row("stuck", "blocked")];

describe("safety", () => {
  beforeEach(() => void vi.useFakeTimers());

  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  describe("what can be a command", () => {
    /** Every command registered on every page. A page's own come and go with it. */
    async function everyCommand() {
      const all = new Map<string, Command>();
      for (const page of PAGES) {
        await showApp(RUNS, page.view);
        for (const command of Object.values(useCommands.getState().commands)) all.set(command.id, command);
        await hideApp();
      }
      return [...all.values()];
    }

    it("sees every command there is, so the walks below mean something", async () => {
      const ids = (await everyCommand()).map((command) => command.id);
      expect(ids).toEqual(expect.arrayContaining(["search.everywhere", "search.findAction", "navigate.goToRun", "navigate.runs", "navigate.board", "navigate.workflows", "navigate.brain", "navigate.settings", "navigate.back", "run.nextWaiting", "run.previousWaiting", "run.new", "run.showChanges", "run.stopStep", "board.newTask"]));
    });

    it("no command is about deciding, in any language", async () => {
      for (const command of await everyCommand()) {
        expect(command.id).not.toMatch(DECIDING_WORDS);
        expect(translated(en, command.title), command.id).toBeTruthy();
        expect(translated(es, command.title), command.id).toBeTruthy();
        expect(translated(en, command.title)).not.toMatch(DECIDING_WORDS);
        expect(translated(es, command.title)).not.toMatch(DECIDING_WORDS_ES);
      }
    });

    it.each(PAGES)("no command reaches a deciding call when it runs, on $name", async (page) => {
      await showApp(RUNS, page.view);
      const commands = Object.values(useCommands.getState().commands);
      expect(commands.length).toBeGreaterThan(0);
      for (const command of commands) {
        // Enabled or not: `enabled` can change, and what a command does doesn't.
        await act(async () => void (await command.run()));
        await settle();
        pressEsc();
        await settle();
      }
      expect(calls.filter((call) => DECIDES.includes(call.command))).toEqual([]);
    });

    it("the code that registers commands never mentions a deciding call", () => {
      const registering = Object.keys(SOURCES).filter((file) => !file.endsWith("/registry.ts") && SOURCES[file].includes("registerCommands("));
      expect(registering.sort()).toEqual(["/src/commands/appCommands.ts", "/src/commands/pageCommands.ts"]);
      for (const file of registering) expect(SOURCES[file], file).not.toMatch(DECIDING_CALL);
    });

    it("the bars don't call the backend at all: they only run commands", () => {
      for (const file of ["/src/views/TopBar.tsx", "/src/views/StatusBar.tsx"]) {
        expect(SOURCES[file], file).not.toMatch(DECIDING_CALL);
        expect(SOURCES[file], file).not.toMatch(/\bapi\b/);
      }
    });

    it("the deciding calls are still called what this test says", () => {
      const api = SOURCES["/src/lib/api.ts"];
      for (const command of DECIDES.filter((name) => !NOT_ALWAYS_THERE.includes(name))) expect(api, command).toContain(`"${command}"`);
    });
  });

  describe("what the keys leave alone", () => {
    const boundKeys = (commands: Command[]) => commands.flatMap((c) => [c.keybinding, c.keybindingOutsideFields]).filter(Boolean);

    it("Esc is no command's key, or dialogs would stop closing under a hold", async () => {
      for (const page of PAGES) {
        await showApp(RUNS, page.view);
        const keys = boundKeys(Object.values(useCommands.getState().commands));
        expect(keys.length).toBeGreaterThan(0);
        expect(keys.filter((key) => /escape|esc\b/i.test(key as string))).toEqual([]);
        await hideApp();
      }
    });

    it("binds nothing the native menu takes first, or main.tsx blocks", async () => {
      // src-tauri/src/menu.rs: Quit, Hide, Hide Others, Close, Minimize, Settings, and the Edit menu.
      const MENU = ["$mod+KeyQ", "$mod+KeyH", "$mod+Alt+KeyH", "$mod+KeyW", "$mod+KeyM", "$mod+Comma", "$mod+KeyZ", "$mod+Shift+KeyZ", "$mod+KeyX", "$mod+KeyC", "$mod+KeyV", "$mod+KeyA"];
      // src/main.tsx: reload and zoom.
      const BLOCKED = ["$mod+KeyR", "$mod+Equal", "$mod+Shift+Equal", "$mod+Minus", "$mod+Digit0"];
      for (const page of PAGES) {
        await showApp(RUNS, page.view);
        const keys = boundKeys(Object.values(useCommands.getState().commands));
        for (const taken of [...MENU, ...BLOCKED]) expect(keys, taken).not.toContain(taken);
        await hideApp();
      }
    });

    it("⌘, stays the native menu's: it's shown on the row, and no listener takes it", async () => {
      await showApp(RUNS);
      const settings = useCommands.getState().commands["navigate.settings"];
      expect(settings.keybinding).toBeUndefined();
      expect(settings.displayKey).toBe(KEYS.settings);
      expect(pressMod(",", "Comma").defaultPrevented).toBe(false);
    });

    it("leaves the keys main.tsx blocks for it to prevent", async () => {
      await showApp(RUNS);
      for (const [key, code] of [["r", "KeyR"], ["=", "Equal"], ["-", "Minus"], ["0", "Digit0"]]) {
        expect(pressMod(key, code).defaultPrevented, code).toBe(false);
      }
    });
  });

  describe("the approval card after ⌘G", () => {
    const APPROVE = "Approve and open a draft";

    it("keeps Approve off for 800 ms, with focus on the note", async () => {
      await showApp(RUNS);
      pressMod("g", "KeyG");
      await settle(0);
      expect(view()).toMatchObject({ name: "run", runId: WAITING });
      expect(button(APPROVE)?.disabled).toBe(true);
      expect(document.activeElement).toBe(note());

      await settle(799);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(1);
      expect(button(APPROVE)?.disabled).toBe(false);
    });

    it("starts the wait over on the next run, though the last card was armed", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      expect(button(APPROVE)?.disabled).toBe(false);

      pressMod("g", "KeyG");
      await settle(0);
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      expect(button(APPROVE)?.disabled).toBe(true);
      expect(document.activeElement).toBe(note());
      await settle(799);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(1);
      expect(button(APPROVE)?.disabled).toBe(false);
      expect(answers()).toEqual([]);
    });

    it("does nothing on the only run waiting, so the card isn't disturbed", async () => {
      await showApp([row(WAITING, "waitingApproval")], { name: "run", runId: WAITING });
      type("Half a thought");
      pressMod("g", "KeyG");
      await settle(0);
      expect((note() as HTMLTextAreaElement).value).toBe("Half a thought");
      expect(document.activeElement).toBe(note());
    });
  });

  describe("Enter", () => {
    const ENTERS = 6;
    const pressEnters = async () => {
      for (let i = 0; i < ENTERS; i++) {
        pressEnter();
        await settle(300);
      }
    };

    it("never answers, from ⌘O through to a card long since armed", async () => {
      await showApp(RUNS);
      pressMod("o", "KeyO");
      expect(paletteOpen()).toBe(true);
      await pressEnters();
      expect(paletteOpen()).toBe(false);
      expect(view()).toMatchObject({ name: "run", runId: WAITING });
      expect(document.activeElement).toBe(note());
      expect(button("Approve and open a draft")?.disabled).toBe(false);
      expect(answers()).toEqual([]);
    });

    it("never answers, with a note written that makes Reject possible", async () => {
      await showApp(RUNS);
      pressMod("o", "KeyO");
      pressEnter();
      await settle(800);
      type("A reason");
      expect(button("Reject")?.disabled).toBe(false);
      await pressEnters();
      expect(answers()).toEqual([]);
    });

    it("never answers on a card that starts with its focus on Close this run", async () => {
      const runs = [row("empty", "waitingApproval")];
      await showApp(runs);
      pressMod("o", "KeyO");
      expect(paletteRow("Run empty")).toBeDefined();
      await pressEnters();
      expect(view()).toMatchObject({ name: "run", runId: "empty" });
      expect(document.activeElement).toBe(button("Close this run"));
      expect(answers()).toEqual([]);
      // It's still there for whoever means it.
      act(() => button("Close this run")?.click());
      await settle();
      expect(answers()).toHaveLength(1);
    });

    it("never answers from a button focus could have gone back to, when the palette picks the same run", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      type("A reason");
      for (const text of ["Reject", "Approve and open a draft"]) {
        act(() => button(text)?.focus());
        expect(document.activeElement).toBe(button(text));
        tapShift();
        tapShift();
        expect(paletteOpen()).toBe(true);
        await pressEnters();
        expect(paletteOpen()).toBe(false);
        expect(answers()).toEqual([]);
      }
      expect(callsTo("approval_answer")).toEqual([]);
    });

    it("doesn't press what had focus when the palette closes, like a button that starts a run", async () => {
      await showApp(RUNS, { name: "board" });
      const move = button("Move to Todo") as HTMLButtonElement;
      act(() => move.focus());
      tapShift();
      tapShift();
      // A row that stays on the board, so the button is still there to press.
      type("Go to Board");
      await settle();
      pressEnter();
      await settle();
      expect(paletteOpen()).toBe(false);
      expect(view().name).toBe("board");
      expect(move.isConnected).toBe(true);
      expect(document.activeElement).not.toBe(move);
      pressEnter();
      await settle();
      expect(callsTo("card_move")).toEqual([]);
    });

    it("from ⌘N doesn't start a run", async () => {
      await showApp(RUNS);
      pressMod("n", "KeyN");
      await settle();
      expect(document.body.textContent).toContain("Start");
      await pressEnters();
      expect(callsTo("run_start")).toEqual([]);
    });

    it("held down doesn't choose a row", async () => {
      await showApp(RUNS);
      pressMod("o", "KeyO");
      pressEnter({ repeat: true });
      await settle();
      expect(paletteOpen()).toBe(true);
      expect(view()).toEqual({ name: "home" });
    });
  });

  describe("the palette's rows", () => {
    it.each(PAGES)("can't answer anything or start anything, whichever is picked, on $name", async (page) => {
      await showApp(RUNS, page.view);
      tapShift();
      tapShift();
      const labels = paletteRows().map((r) => r.textContent ?? "");
      expect(labels.length).toBeGreaterThan(0);
      for (const label of labels) {
        expect(label).not.toMatch(/approve|reject|retry|resume|abandon/i);
      }
      pressKey("Escape", "Escape");
      await settle();

      for (const label of labels) {
        await hideApp();
        await showApp(RUNS, page.view);
        tapShift();
        tapShift();
        await act(async () => paletteRows().find((r) => r.textContent === label)?.click());
        await settle();
        pressEsc();
        expect(calls.filter((call) => DECIDES.includes(call.command)), label).toEqual([]);
      }
    });

    it("opening a run that's waiting answers nothing, and its card starts from nothing", async () => {
      await showApp(RUNS);
      pressMod("o", "KeyO");
      await act(async () => paletteRow("Run r2")?.click());
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      expect(button("Approve and open a draft")?.disabled).toBe(true);
      expect(answers()).toEqual([]);
    });

    it("with nothing typed, the row Enter would run only goes somewhere, even where Stop is on offer", async () => {
      await showApp([row("working", "running")], { name: "run", runId: "working" });
      pressMod("A", "KeyA", { shiftKey: true });
      expect(paletteRow("Stop the step")?.getAttribute("aria-disabled")).toBe("false");
      expect(document.querySelector('[cmdk-item][aria-selected="true"]')?.textContent).toBe("Search everywhere⇧⇧");
    });
  });

  describe("the native menu", () => {
    const entries = (menus: MenuSpec[]) => menus.flatMap((menu) => menu.items);
    const commandItems = (menus: MenuSpec[]) => entries(menus).flatMap((entry) => (entry.kind === "command" ? [entry] : []));
    const inLanguage = (language: object) => (key: string) => translated(language, key) as string;
    const modelHere = (language: object = en) => buildMenuModel(Object.values(useCommands.getState().commands), inLanguage(language));

    /** The only things in the menu that aren't commands. None of them answers a run. */
    const SYSTEM_ITEMS: PredefinedItem[] = ["about", "hide", "hideOthers", "showAll", "quit", "undo", "redo", "cut", "copy", "paste", "selectAll", "minimize", "zoom", "close"];

    it.each(PAGES)("has only registered commands and the system's own items, on $name", async (page) => {
      await showApp(RUNS, page.view);
      const registered = Object.keys(useCommands.getState().commands);
      const items = commandItems(modelHere());
      expect(items.length).toBeGreaterThan(0);
      for (const item of items) expect(registered).toContain(item.commandId);
      for (const entry of entries(modelHere())) {
        expect(["command", "predefined", "separator"]).toContain(entry.kind);
        if (entry.kind === "predefined") expect(SYSTEM_ITEMS).toContain(entry.name);
      }
    });

    it.each(PAGES)("no item is about deciding, in any language, on $name", async (page) => {
      await showApp(RUNS, page.view);
      for (const [language, words] of [[en, DECIDING_WORDS], [es, DECIDING_WORDS_ES]] as const) {
        const menus = modelHere(language);
        for (const menu of menus) expect(menu.title).toBeTruthy();
        expect(menus.map((menu) => menu.title).join(" ")).not.toMatch(words);
        for (const entry of entries(menus)) {
          if (entry.kind === "separator") continue;
          expect(entry.title, JSON.stringify(entry)).toBeTruthy();
          expect(entry.title).not.toMatch(words);
        }
      }
    });

    it.each(PAGES)("no item reaches a deciding call when chosen, on $name", async (page) => {
      await showApp(RUNS, page.view);
      for (const item of commandItems(modelHere())) {
        act(() => runMenuCommand(item.commandId));
        await settle();
        pressEsc();
        await settle();
      }
      expect(calls.filter((call) => DECIDES.includes(call.command))).toEqual([]);
    });

    it("gives an accelerator only to a key that means one thing everywhere", async () => {
      await showApp(RUNS);
      const keys = Object.fromEntries(commandItems(modelHere()).map((item) => [item.commandId, item.accelerator]));
      expect(keys).toEqual({
        "search.everywhere": null,
        "search.findAction": "CmdOrCtrl+Shift+A",
        "navigate.goToRun": "CmdOrCtrl+O",
        "navigate.runs": "CmdOrCtrl+1",
        "navigate.board": "CmdOrCtrl+2",
        "navigate.workflows": "CmdOrCtrl+3",
        "navigate.brain": "CmdOrCtrl+4",
        "navigate.settings": "CmdOrCtrl+Comma",
        "navigate.back": "CmdOrCtrl+BracketLeft",
        "run.nextWaiting": null,
        "run.previousWaiting": null,
        "run.new": "CmdOrCtrl+N",
      });
    });

    it("leaves the keys bound to a page to the keymap, as AppKit would take them first", async () => {
      await showApp(RUNS, { name: "run", runId: "working" });
      const onRun = Object.fromEntries(commandItems(modelHere()).map((item) => [item.commandId, item.accelerator]));
      expect(onRun["run.showChanges"]).toBeNull();
      expect(onRun["run.stopStep"]).toBeNull();
      await hideApp();
      await showApp(RUNS, { name: "board" });
      const onBoard = Object.fromEntries(commandItems(modelHere()).map((item) => [item.commandId, item.accelerator]));
      expect(onBoard["run.new"]).toBeNull();
      expect(onBoard["board.newTask"]).toBeNull();
    });

    it("never gives an accelerator to a key without a modifier, on any page", async () => {
      for (const page of PAGES) {
        await showApp(RUNS, page.view);
        for (const { accelerator } of commandItems(modelHere())) if (accelerator) expect(accelerator).toMatch(/^CmdOrCtrl\+/);
        await hideApp();
      }
      await showApp(RUNS);
    });

    it("runs nothing while a dialog holds the keymap: ⌘, doesn't go to Settings behind it", async () => {
      await showApp(RUNS);
      act(() => useApp.setState({ newRunOpen: true }));
      await settle();
      for (const { commandId } of commandItems(modelHere())) {
        act(() => runMenuCommand(commandId));
        await settle();
      }
      expect(view()).toEqual({ name: "home" });
      expect(paletteOpen()).toBe(false);
    });

    it("⌘, works once the dialog is gone, through the event Rust sends", async () => {
      await showApp(RUNS);
      act(() => useApp.setState({ newRunOpen: true }));
      await settle();
      await act(() => emit("keel://menu-command", "navigate.settings"));
      await settle();
      expect(view()).toEqual({ name: "home" });

      pressKey("Escape", "Escape", {}, document.body);
      await settle();
      await act(() => emit("keel://menu-command", "navigate.settings"));
      await settle();
      expect(view()).toEqual({ name: "settings" });
    });

    it("runs nothing during a board's keyboard drag", async () => {
      await showApp(RUNS, { name: "board" });
      const card = document.querySelector<HTMLElement>("[data-card]") as HTMLElement;
      act(() => card.focus());
      pressKey(" ", "Space");
      await settle(0);
      for (const { commandId } of commandItems(modelHere())) {
        act(() => runMenuCommand(commandId));
        await settle(0);
      }
      expect(view().name).toBe("board");
      expect(paletteOpen()).toBe(false);
      expect(document.querySelector("form")).toBeNull();
      expect(callsTo("card_move")).toEqual([]);
    });

    it("ignores a command it doesn't know, whatever the event says", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      for (const name of ["run.approve", "approval_answer", "card_move", ""]) await act(() => emit("keel://menu-command", name));
      await settle();
      expect(calls.filter((call) => DECIDES.includes(call.command))).toEqual([]);
      expect(view()).toEqual({ name: "run", runId: WAITING });
    });

    it("from a run, a menu item opening another one starts the card's wait over", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      expect(button("Approve and open a draft")?.disabled).toBe(false);
      await act(() => emit("keel://menu-command", "run.nextWaiting"));
      await settle(0);
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      expect(button("Approve and open a draft")?.disabled).toBe(true);
      expect(answers()).toEqual([]);
    });

    it("doesn't run a command twice when the key and the menu both fire for one press", async () => {
      await showApp(RUNS);
      pressMod("n", "KeyN");
      await settle();
      pressEsc();
      await settle();
      expect(useApp.getState().newRunOpen).toBe(false);
      act(() => runMenuCommand("run.new"));
      await settle();
      expect(useApp.getState().newRunOpen).toBe(false);
      await settle(300);
      act(() => runMenuCommand("run.new"));
      await settle();
      expect(useApp.getState().newRunOpen).toBe(true);
    });

    it("is what's sent to Rust, in the language showing, and again when it changes", async () => {
      await showApp(RUNS);
      await settle(200);
      const sent = () => (callsTo("menu_set").at(-1)?.args as { menus: MenuSpec[] }).menus;
      expect(sent().map((menu) => menu.title)).toEqual(["Keel", "Edit", "Navigate", "Run", "Window"]);
      await act(() => i18next.changeLanguage("es"));
      await settle(200);
      expect(sent().map((menu) => menu.title)).toEqual(["Keel", "Edición", "Navegar", "Ejecución", "Ventana"]);
      expect(commandItems(sent()).find((item) => item.commandId === "run.new")?.title).toBe("Nueva ejecución");
      await act(() => i18next.changeLanguage("en"));
    });

    it("the menu's code never mentions a deciding call", () => {
      const files = Object.keys(SOURCES).filter((file) => file.startsWith("/src/menu/"));
      expect(files.sort()).toEqual(["/src/menu/menuModel.ts", "/src/menu/nativeMenu.ts"]);
      for (const file of files) expect(SOURCES[file], file).not.toMatch(DECIDING_CALL);
    });
  });

  describe("the status bar's click", () => {
    const APPROVE = "Approve and open a draft";
    const waiting = () => document.querySelector<HTMLButtonElement>("footer button");

    it("is the only thing to click there", async () => {
      await showApp(RUNS);
      expect(document.querySelectorAll("footer button")).toHaveLength(1);
      expect(document.querySelectorAll("footer a, footer input, footer [tabindex], footer [role='button']")).toHaveLength(0);
    });

    it.each(PAGES)("answers nothing, however many times, on $name", async (page) => {
      await showApp(RUNS, page.view);
      for (let i = 0; i < RUNS.length * 2; i++) {
        act(() => waiting()?.click());
        await settle(300);
      }
      expect(view().name).toBe("run");
      expect(calls.filter((call) => DECIDES.includes(call.command))).toEqual([]);
    });

    it("keeps Approve off for 800 ms where it lands, with focus on the note", async () => {
      await showApp(RUNS);
      act(() => waiting()?.click());
      await settle(0);
      expect(view()).toMatchObject({ name: "run", runId: WAITING });
      expect(button(APPROVE)?.disabled).toBe(true);
      expect(document.activeElement).toBe(note());
      await settle(799);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(1);
      expect(button(APPROVE)?.disabled).toBe(false);
    });

    it("starts the wait over on the next run, though the last card was armed", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      expect(button(APPROVE)?.disabled).toBe(false);
      act(() => waiting()?.click());
      await settle(0);
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      expect(button(APPROVE)?.disabled).toBe(true);
    });

    it("does nothing on the only run waiting, so the card and its note aren't disturbed", async () => {
      await showApp([row(WAITING, "waitingApproval")], { name: "run", runId: WAITING });
      await settle(800);
      type("Half a thought");
      act(() => waiting()?.click());
      await settle(0);
      expect((note() as HTMLTextAreaElement).value).toBe("Half a thought");
      expect(button(APPROVE)?.disabled).toBe(false);
      expect(answers()).toEqual([]);
    });

    it("from the keyboard, Enter opens the run and the next ones answer nothing", async () => {
      await showApp(RUNS);
      act(() => waiting()?.focus());
      for (let i = 0; i < 6; i++) {
        pressEnter();
        await settle(300);
      }
      expect(view()).toMatchObject({ name: "run", runId: WAITING });
      expect(document.activeElement).toBe(note());
      expect(answers()).toEqual([]);
    });

    it("from the keyboard, Enter answers nothing on a card that starts with its focus on Close this run", async () => {
      await showApp([row("empty", "waitingApproval")]);
      act(() => waiting()?.focus());
      for (let i = 0; i < 6; i++) {
        pressEnter();
        await settle(300);
      }
      expect(document.activeElement).toBe(button("Close this run"));
      expect(answers()).toEqual([]);
    });

    it("held Enter doesn't go through the runs", async () => {
      await showApp(RUNS);
      act(() => waiting()?.focus());
      pressEnter({ repeat: true });
      await settle();
      expect(view()).toEqual({ name: "home" });
    });

    it("does nothing during a board's keyboard drag or under a dialog", async () => {
      await showApp(RUNS, { name: "board" });
      const card = document.querySelector<HTMLElement>("[data-card]") as HTMLElement;
      act(() => card.focus());
      pressKey(" ", "Space");
      await settle(0);
      act(() => waiting()?.click());
      await settle(0);
      expect(view().name).toBe("board");
      pressKey("Escape", "Escape", {}, document.body);
      await settle(0);

      act(() => useApp.setState({ newRunOpen: true }));
      await settle(0);
      act(() => waiting()?.click());
      await settle(0);
      expect(view().name).toBe("board");
      expect(callsTo("card_move")).toEqual([]);
    });
  });

  describe("the approval card's height", () => {
    it("leaves room for both bars, so its buttons show without scrolling", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      const card = document.querySelector('section[role="alert"]') as HTMLElement;
      const reserved = Number(/max-h-\[calc\(100vh-(\d+)px\)\]/.exec(card.className)?.[1]);
      const top = Number(/h-\[(\d+)px\]/.exec((document.querySelector("header") as HTMLElement).className)?.[1]);
      expect((document.querySelector("footer") as HTMLElement).className).toContain("h-6");
      // Above and below the card: the page's padding, the run's title and the gap under it.
      const page = 102;
      expect(reserved).toBeGreaterThanOrEqual(top + 24 + page);
    });
  });

  describe("Approve around the palette", () => {
    const APPROVE = "Approve and open a draft";

    it("is off while the palette covers the card, and waits again after it closes", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      expect(button(APPROVE)?.disabled).toBe(false);

      tapShift();
      tapShift();
      expect(paletteOpen()).toBe(true);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(5000);
      expect(button(APPROVE)?.disabled).toBe(true);

      pressKey("Escape", "Escape");
      await settle(0);
      expect(paletteOpen()).toBe(false);
      act(() => button(APPROVE)?.click());
      expect(answers()).toEqual([]);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(800);
      expect(button(APPROVE)?.disabled).toBe(false);
    });

    it("is off when the palette opens from the top bar's field too", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      expect(button(APPROVE)?.disabled).toBe(false);
      act(() => button("Search⇧⇧")?.click());
      await settle(0);
      expect(paletteOpen()).toBe(true);
      expect(button(APPROVE)?.disabled).toBe(true);
    });

    it("is off under New run, which is a click away on the run page, and waits again after it closes", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      expect(button(APPROVE)?.disabled).toBe(false);

      act(() => button("New run")?.click());
      await settle(0);
      expect(useApp.getState().newRunOpen).toBe(true);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(5000);
      expect(button(APPROVE)?.disabled).toBe(true);

      pressKey("Escape", "Escape", {}, document.body);
      await settle(0);
      expect(useApp.getState().newRunOpen).toBe(false);
      act(() => button(APPROVE)?.click());
      expect(answers()).toEqual([]);
      await settle(799);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(1);
      expect(button(APPROVE)?.disabled).toBe(false);
    });

    it("waits for a dialog already open when the card appears under it", async () => {
      await showApp(RUNS);
      act(() => useApp.setState({ newRunOpen: true, view: { name: "run", runId: WAITING } }));
      await settle(5000);
      expect(button(APPROVE)?.disabled).toBe(true);
      pressKey("Escape", "Escape", {}, document.body);
      await settle(799);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(1);
      expect(button(APPROVE)?.disabled).toBe(false);
    });

    it("waits under the changes too, even if the window comes back meanwhile", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      pressMod("d", "KeyD");
      await settle();
      expect(button(APPROVE)?.disabled).toBe(true);
      act(() => void window.dispatchEvent(new Event("blur")));
      act(() => void window.dispatchEvent(new Event("focus")));
      await settle(5000);
      expect(button(APPROVE)?.disabled).toBe(true);
    });

    it("goes off with the window even while the card is open, as before", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      await settle(800);
      act(() => void window.dispatchEvent(new Event("blur")));
      expect(button(APPROVE)?.disabled).toBe(true);
      act(() => void window.dispatchEvent(new Event("focus")));
      await settle(799);
      expect(button(APPROVE)?.disabled).toBe(true);
      await settle(1);
      expect(button(APPROVE)?.disabled).toBe(false);
    });
  });

  describe("the hold", () => {
    it("the inline card doesn't hold: keys still work on it", async () => {
      await showApp(RUNS, { name: "run", runId: WAITING });
      pressMod("2", "Digit2");
      await settle();
      expect(view().name).toBe("board");
    });

    it("the dialogs that stay mounted hold only while open", async () => {
      await showApp(RUNS);
      pressMod("1", "Digit1");
      pressMod("3", "Digit3");
      await settle();
      expect(view().name).toBe("workflows");
    });

    it("a board drag holds, so typing in something opened mid-drag can't drop the card on Todo", async () => {
      await showApp(RUNS, { name: "board" });
      const card = document.querySelector<HTMLElement>("[data-card]") as HTMLElement;
      act(() => card.focus());
      pressKey(" ", "Space");
      await settle(0);
      // Picked up: ⇧⇧ and ⌘N open nothing, and the drag goes on.
      tapShift();
      tapShift();
      pressMod("n", "KeyN");
      await settle(0);
      expect(paletteOpen()).toBe(false);
      expect(document.querySelector("form")).toBeNull();

      pressKey("Escape", "Escape", {}, document.body);
      await settle(0);
      pressMod("n", "KeyN");
      await settle(0);
      expect(document.querySelector("form")).not.toBeNull();
      expect(callsTo("card_move")).toEqual([]);
    });

    it("a board drag holds clicks on the top bar too, so nothing opens that typing would drop the card from", async () => {
      await showApp(RUNS, { name: "board" });
      const card = document.querySelector<HTMLElement>("[data-card]") as HTMLElement;
      act(() => card.focus());
      pressKey(" ", "Space");
      await settle(0);
      for (const clickable of document.querySelectorAll<HTMLElement>("header button")) act(() => clickable.click());
      await settle(0);
      expect(paletteOpen()).toBe(false);
      expect(document.querySelector("form")).toBeNull();
      expect(useApp.getState().newRunOpen).toBe(false);
      expect(view().name).toBe("board");

      pressKey("Escape", "Escape", {}, document.body);
      await settle(0);
      act(() => button("Search⇧⇧")?.click());
      await settle(0);
      expect(paletteOpen()).toBe(true);
      expect(callsTo("card_move")).toEqual([]);
    });
  });
});

function pressEsc() {
  act(() => {
    // Whatever a command opened, the next one starts from a closed palette.
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
  });
}
