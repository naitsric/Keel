import { describe, expect, it } from "vitest";
import type { Command } from "../commands/registry";
import { buildMenuModel, menuAccelerator, toAccelerator, type MenuEntry, type MenuSpec } from "./menuModel";

const NAVIGATE = "shortcuts.categories.navigate";
const RUN = "shortcuts.categories.run";
const BOARD = "shortcuts.categories.board";

const command = (id: string, category: string, extra: Partial<Command> = {}): Command => ({ id, title: `title of ${id}`, category, run: () => {}, ...extra });

const COMMANDS = [
  command("search.everywhere", NAVIGATE, { displayKey: "Shift Shift" }),
  command("navigate.board", NAVIGATE, { keybinding: "$mod+Digit2" }),
  command("navigate.settings", NAVIGATE, { displayKey: "$mod+Comma" }),
  command("run.nextWaiting", NAVIGATE, { keybinding: "$mod+KeyG" }),
  command("run.new", RUN, { keybinding: "$mod+KeyN" }),
  command("run.stopStep", RUN, { keybinding: "$mod+F2", context: "run" }),
];

const titles = (key: string) => key;
const model = (commands: Command[] = COMMANDS) => buildMenuModel(commands, titles);
const menu = (menus: MenuSpec[], title: string) => menus.find((m) => m.title === title) as MenuSpec;
const shape = (spec: MenuSpec) => spec.items.map((item: MenuEntry) => (item.kind === "command" ? item.commandId : item.kind === "predefined" ? item.name : "---"));
const item = (menus: MenuSpec[], id: string) => menus.flatMap((m) => m.items).find((i) => i.kind === "command" && i.commandId === id);

describe("toAccelerator", () => {
  it.each([
    ["$mod+KeyO", "CmdOrCtrl+O"],
    ["$mod+Shift+KeyA", "CmdOrCtrl+Shift+A"],
    ["$mod+Digit1", "CmdOrCtrl+1"],
    ["$mod+Comma", "CmdOrCtrl+Comma"],
    ["$mod+BracketLeft", "CmdOrCtrl+BracketLeft"],
    ["$mod+F2", "CmdOrCtrl+F2"],
    ["Control+KeyG", "Ctrl+G"],
    ["F4", "F4"],
  ])("%s → %s", (binding, accelerator) => {
    expect(toAccelerator(binding)).toBe(accelerator);
  });

  it("refuses sequences, unknown modifiers and odd keys", () => {
    expect(toAccelerator("Shift Shift")).toBeNull();
    expect(toAccelerator("Hyper+KeyA")).toBeNull();
    expect(toAccelerator("$mod+a")).toBeNull();
  });
});

describe("menuAccelerator", () => {
  const all = [
    command("global", NAVIGATE, { keybinding: "$mod+KeyO" }),
    command("shown", NAVIGATE, { displayKey: "$mod+Comma" }),
    command("shared.one", RUN, { keybinding: "$mod+KeyN" }),
    command("shared.two", BOARD, { keybinding: "$mod+KeyN", context: "board" }),
    command("page", RUN, { keybinding: "$mod+KeyD", context: "run" }),
    command("field", NAVIGATE, { keybinding: "$mod+KeyA" }),
    command("steps", NAVIGATE, { keybinding: "$mod+KeyG" }),
    command("back", NAVIGATE, { keybinding: "$mod+ArrowUp" }),
    command("chord", NAVIGATE, { displayKey: "Shift Shift" }),
    command("none", NAVIGATE),
  ];
  const byId = (id: string) => menuAccelerator(all.find((c) => c.id === id) as Command, all);

  it("gives one to a key that means one thing everywhere", () => {
    expect(byId("global")).toBe("CmdOrCtrl+O");
  });

  it("takes the one only shown, since this is what handles it", () => {
    expect(byId("shown")).toBe("CmdOrCtrl+Comma");
  });

  it("gives none to a key shared with another command, or bound to a page", () => {
    expect(byId("shared.one")).toBeNull();
    expect(byId("shared.two")).toBeNull();
    expect(byId("page")).toBeNull();
  });

  it("gives none to what text fields need", () => {
    expect(byId("field")).toBeNull();
    expect(byId("back")).toBeNull();
  });

  it("gives none to ⌘G, where a held key must do one thing", () => {
    expect(byId("steps")).toBeNull();
    expect(menuAccelerator(command("x", NAVIGATE, { keybinding: "$mod+Shift+KeyG" }), [])).toBeNull();
  });

  it("gives none to what can't be one, or isn't bound", () => {
    expect(byId("chord")).toBeNull();
    expect(byId("none")).toBeNull();
  });
});

describe("buildMenuModel", () => {
  it("orders the bar like a Mac's", () => {
    expect(model().map((m) => m.title)).toEqual(["app.name", "menu.edit", NAVIGATE, RUN, "menu.window"]);
  });

  it("keeps the app menu, with Settings by its label there, and the native Quit last", () => {
    expect(shape(model()[0])).toEqual(["about", "---", "navigate.settings", "---", "hide", "hideOthers", "showAll", "---", "quit"]);
    expect(item(model(), "navigate.settings")).toEqual({ kind: "command", commandId: "navigate.settings", title: "menu.settings", accelerator: "CmdOrCtrl+Comma" });
  });

  it("keeps the Edit menu as it was, so fields keep their keys", () => {
    expect(shape(menu(model(), "menu.edit"))).toEqual(["undo", "redo", "---", "cut", "copy", "paste", "selectAll"]);
  });

  it("keeps Close Window", () => {
    expect(shape(menu(model(), "menu.window"))).toEqual(["minimize", "zoom", "---", "close"]);
  });

  it("translates what it's given", () => {
    const predefined = model()[0].items[0];
    expect(predefined).toEqual({ kind: "predefined", name: "about", title: "menu.about" });
    expect(item(model(), "run.new")).toMatchObject({ title: "title of run.new" });
  });

  it("puts every registered command somewhere, once, an unknown category in Navigate", () => {
    const commands = [...COMMANDS, command("brand.new", "shortcuts.categories.somethingNew")];
    const ids = model(commands).flatMap((m) => m.items.flatMap((i) => (i.kind === "command" ? [i.commandId] : [])));
    expect(ids.sort()).toEqual(commands.map((c) => c.id).sort());
    expect(shape(menu(model(commands), NAVIGATE))).toContain("brand.new");
  });

  it("keeps the board's commands with the run's, in their own section", () => {
    const onBoard = [...COMMANDS, command("board.newTask", BOARD, { keybinding: "$mod+KeyN", context: "board" })];
    expect(shape(menu(model(onBoard), RUN))).toEqual(["run.new", "run.stopStep", "---", "board.newTask"]);
  });

  it("gives up ⌘N on the board, for the keymap to decide, and has it back elsewhere", () => {
    const onBoard = [...COMMANDS, command("board.newTask", BOARD, { keybinding: "$mod+KeyN", context: "board" })];
    expect(item(model(onBoard), "run.new")).toMatchObject({ accelerator: null });
    expect(item(model(onBoard), "board.newTask")).toMatchObject({ accelerator: null });
    expect(item(model(), "run.new")).toMatchObject({ accelerator: "CmdOrCtrl+N" });
  });

  it("leaves out a menu with nothing in it, as with no commands yet", () => {
    expect(model([]).map((m) => m.title)).toEqual(["app.name", "menu.edit", "menu.window"]);
    expect(shape(model([])[0])).toEqual(["about", "---", "hide", "hideOthers", "showAll", "---", "quit"]);
  });
});

describe("what Rust is sent", () => {
  const rust = import.meta.glob<string>("/src-tauri/src/menu.rs", { query: "?raw", import: "default", eager: true })["/src-tauri/src/menu.rs"];

  it("is only what it knows how to make", () => {
    const variants = [...rust.slice(rust.indexOf("pub enum Predefined")).matchAll(/^ {4}([A-Z]\w+),$/gm)].map(([, name]) => name);
    const sent = model().flatMap((m) => m.items.flatMap((i) => (i.kind === "predefined" ? [i.name] : [])));
    expect(sent.length).toBeGreaterThan(0);
    for (const name of sent) expect(variants, name).toContain(name[0].toUpperCase() + name.slice(1));
    expect(variants).toHaveLength(new Set(sent).size);
  });

  it("uses the names it reads", () => {
    expect(rust).toContain('tag = "kind", rename_all = "camelCase"');
    expect(item(model(), "run.new")).toHaveProperty("commandId");
  });
});
