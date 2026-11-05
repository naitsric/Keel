import type { Command } from "../commands/registry";

/**
 * The native menu bar, built from the command registry: every registered command lands in a menu by
 * its category, so a new one shows up without touching this file. What's registered can't decide
 * anything (see registry.ts), so nothing here can either. Rust only makes it real
 * (src-tauri/src/menu.rs).
 *
 * Items stay enabled: the menu can't ask each command every time it opens, so choosing one that
 * can't run now does nothing, as its key would.
 */

/** What the system does itself. Quit and Close Window end up where Rust asks about active runs. */
export type PredefinedItem = "about" | "hide" | "hideOthers" | "showAll" | "quit" | "undo" | "redo" | "cut" | "copy" | "paste" | "selectAll" | "minimize" | "zoom" | "close";

export type MenuEntry = { kind: "command"; commandId: string; title: string; accelerator: string | null } | { kind: "predefined"; name: PredefinedItem; title: string } | { kind: "separator" };

export interface MenuSpec {
  title: string;
  items: MenuEntry[];
}

type MenuId = "navigate" | "run";

/** Between Edit and Window, in bar order. */
const MENUS: { id: MenuId; title: string }[] = [
  { id: "navigate", title: "shortcuts.categories.navigate" },
  { id: "run", title: "shortcuts.categories.run" },
];

/** Where each command category goes, in section order. Any other goes in Navigate, so nothing registered is ever missing. */
const CATEGORY_MENU: [category: string, menu: MenuId][] = [
  ["shortcuts.categories.navigate", "navigate"],
  ["shortcuts.categories.run", "run"],
  ["shortcuts.categories.board", "run"],
];

/** In the app menu, where a Mac keeps it, under the label it has there. */
const SETTINGS_COMMAND = "navigate.settings";

/**
 * Keys a menu must never take, because AppKit gives it the first go at them:
 *
 * - What text fields need. The Edit menu's own items handle the first few.
 * - ⌘G and ⇧⌘G, which go somewhere new each time. A key held on a menu item repeats, and the keymap
 *   is what makes a held key do one thing. The other keys here open something or go to one place,
 *   so repeating them changes nothing.
 */
const RESERVED_KEYS = new Set(["$mod+KeyA", "$mod+KeyC", "$mod+KeyV", "$mod+KeyX", "$mod+KeyZ", "$mod+Shift+KeyZ", "$mod+Backspace", "$mod+ArrowUp", "$mod+ArrowDown", "$mod+ArrowLeft", "$mod+ArrowRight", "$mod+KeyG", "$mod+Shift+KeyG"]);

const MODIFIERS: Record<string, string> = {
  $mod: "CmdOrCtrl",
  Control: "Ctrl",
  Alt: "Alt",
  Shift: "Shift",
};

/** tinykeys syntax (`$mod+Shift+KeyA`) as a Tauri accelerator (`CmdOrCtrl+Shift+A`), or null for what it can't say, like ⇧⇧. */
export function toAccelerator(binding: string): string | null {
  if (binding.includes(" ")) return null;
  const parts = binding.split("+");
  const key = parts.pop() as string;
  const modifiers = parts.map((part) => MODIFIERS[part]);
  if (modifiers.some((modifier) => !modifier)) return null;
  const name = /^Key[A-Z]$/.test(key) ? key.slice(3) : /^Digit\d$/.test(key) ? key.slice(5) : /^([A-Z][a-z]+)+$|^F\d{1,2}$/.test(key) ? key : null;
  return name ? [...modifiers, name].join("+") : null;
}

/**
 * The accelerator an item gets, if any. Only a key that means one thing everywhere and that the
 * webview doesn't need: none bound to a page, none shared, none reserved. The rest stay with the
 * keymap, which knows the page: ⌘N is New task on the board, so there New run has none.
 */
export function menuAccelerator(command: Command, all: readonly Command[]): string | null {
  // A display key is one that something else handles, and for ⌘, this is what does.
  const binding = command.keybinding ?? command.displayKey;
  if (!binding || command.context || RESERVED_KEYS.has(binding)) return null;
  const shared = all.some((other) => other.id !== command.id && other.keybinding === binding);
  return shared ? null : toAccelerator(binding);
}

export function buildMenuModel(commands: readonly Command[], t: (key: string) => string): MenuSpec[] {
  const entry = (command: Command, title = t(command.title)): MenuEntry => ({ kind: "command", commandId: command.id, title, accelerator: menuAccelerator(command, commands) });
  const predefined = (...names: PredefinedItem[]): MenuEntry[] => names.map((name) => ({ kind: "predefined", name, title: t(`menu.${name}`) }));
  const separator: MenuEntry = { kind: "separator" };

  const menuOf = new Map(CATEGORY_MENU);
  const sections = new Map<MenuId, Map<string, Command[]>>(MENUS.map(({ id }) => [id, new Map(CATEGORY_MENU.filter(([, menu]) => menu === id).map(([category]) => [category, []]))]));
  for (const command of commands) {
    if (command.id === SETTINGS_COMMAND) continue;
    const section = sections.get(menuOf.get(command.category) ?? "navigate") as Map<string, Command[]>;
    section.set(command.category, [...(section.get(command.category) ?? []), command]);
  }
  const itemsOf = (id: MenuId): MenuEntry[] =>
    [...(sections.get(id) as Map<string, Command[]>).values()]
      .filter((section) => section.length > 0)
      .flatMap((section, index) => [...(index > 0 ? [separator] : []), ...section.map((command) => entry(command))]);

  const settings = commands.find((command) => command.id === SETTINGS_COMMAND);
  return [
    {
      title: t("app.name"),
      items: [...predefined("about"), separator, ...(settings ? [entry(settings, t("menu.settings")), separator] : []), ...predefined("hide", "hideOthers", "showAll"), separator, ...predefined("quit")],
    },
    { title: t("menu.edit"), items: [...predefined("undo", "redo"), separator, ...predefined("cut", "copy", "paste", "selectAll")] },
    ...MENUS.map(({ id, title }) => ({ title: t(title), items: itemsOf(id) })).filter((menu) => menu.items.length > 0),
    { title: t("menu.window"), items: [...predefined("minimize", "zoom"), separator, ...predefined("close")] },
  ];
}
