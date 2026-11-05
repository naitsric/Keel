import { create } from "zustand";
import { useApp } from "../lib/store";
import { keymapHeld } from "./keymapHold";

/**
 * The page a key means something on: ⌘N is New task on the board and New run everywhere else. One
 * page shows at a time and nothing has to be focused on it, so this follows the page, not the DOM.
 */
export type FocusContext = "run" | "board";

/**
 * An action the keymap and the palette can run, with no extra wiring, and the native menu too.
 *
 * Plain Enter runs whatever is registered, so what decides or spends never is: approving, rejecting,
 * answering the rework limit, retrying, resuming, abandoning, the Brain's accept, reject and retire,
 * moving a card to Todo. Those stay buttons on their own screens. safety.test.tsx checks it.
 */
export interface Command {
  /** Stable id, `<area>.<action>`, e.g. `run.showChanges`. */
  id: string;
  /** Translation key. */
  title: string;
  /** Translation key of the group shown in the palette. */
  category: string;
  /** tinykeys syntax, e.g. `$mod+Shift+KeyA`. */
  keybinding?: string;
  /** A second key for the same thing, that leaves text fields alone: there ⌘↑ moves the caret. */
  keybindingOutsideFields?: string;
  /** Shown in the palette but handled elsewhere, like ⌘, which the native menu takes first. See menuModel.ts. */
  displayKey?: string;
  /**
   * Limits the keybinding to this page. Commands may share a binding: see {@link pickCommand}. The
   * palette ignores it; `enabled` alone decides there.
   */
  context?: FocusContext;
  /** Whether the command can run right now. Disabled commands stay visible but inert. */
  enabled?: () => boolean;
  run: () => void | Promise<void>;
}

interface CommandRegistry {
  commands: Record<string, Command>;
}

export const useCommands = create<CommandRegistry>(() => ({ commands: {} }));

/** Registers commands and returns a function that unregisters them. */
export function registerCommands(commands: Command[]): () => void {
  useCommands.setState((s) => ({
    commands: { ...s.commands, ...Object.fromEntries(commands.map((c) => [c.id, c])) },
  }));
  return () =>
    useCommands.setState((s) => {
      const next = { ...s.commands };
      for (const c of commands) if (next[c.id] === c) delete next[c.id];
      return { commands: next };
    });
}

export function isEnabled(command: Command): boolean {
  return command.enabled?.() ?? true;
}

export function focusContext(): FocusContext | undefined {
  const { name } = useApp.getState().view;
  return name === "run" || name === "board" ? name : undefined;
}

/**
 * Which of the commands sharing a key runs: those bound to this page first, then context-free ones,
 * in registration order; the first enabled one wins. A command bound to another page never runs.
 */
export function pickCommand(candidates: readonly Command[], context: FocusContext | undefined): Command | undefined {
  const contextual = candidates.filter((c) => c.context !== undefined && c.context === context);
  const global = candidates.filter((c) => c.context === undefined);
  return [...contextual, ...global].find(isEnabled);
}

export function executeCommand(id: string): void {
  const command = useCommands.getState().commands[id];
  if (!command || !isEnabled(command)) return;
  void Promise.resolve(command.run()).catch((e) => console.error(`command ${id} failed`, e));
}

/**
 * For what's clicked rather than pressed. The bars stay clickable during a board's keyboard drag,
 * and a dialog opened then would drop the card with the first Space typed in it, starting its run.
 * So a hold stops clicks as it stops keys.
 */
export function runUnlessHeld(id: string): void {
  if (!keymapHeld()) executeCommand(id);
}
