import { useEffect } from "react";
import { tinykeys } from "tinykeys";
import { keymapHeld } from "./keymapHold";
import { executeCommand, focusContext, pickCommand, useCommands, type Command, type FocusContext } from "./registry";

/** Max gap between the two Shift taps of "Shift Shift", as in IntelliJ. */
const DOUBLE_SHIFT_MS = 350;

/**
 * What a key bound to `candidates` does: run one of them, do nothing at all, or go to whoever else
 * handles it.
 *
 * - The command `pickCommand` chooses runs and consumes the key. Without an enabled one, the key
 *   passes untouched.
 * - While a dialog holds the keymap (see keymapHold.ts) nothing runs. A key bound where the person
 *   is (globally, or to this page) is suppressed, enabled or not: its default action is prevented,
 *   though the dialog's handlers still see it. Keys bound only to other pages pass.
 */
export function resolveKey(
  candidates: readonly Command[],
  context: FocusContext | undefined,
  held: boolean,
): { action: "run"; command: Command } | { action: "suppress" } | { action: "pass" } {
  if (held) {
    const bound = candidates.some((c) => c.context === undefined || c.context === context);
    return bound ? { action: "suppress" } : { action: "pass" };
  }
  const command = pickCommand(candidates, context);
  return command ? { action: "run", command } : { action: "pass" };
}

/** Whether what the person types goes into a field, which has its own use for ⌘↑. */
export function typingInField(): boolean {
  return document.activeElement?.matches("input, textarea, select, [contenteditable]") ?? false;
}

/**
 * Binds every registered command's keybinding on the window, in the capture phase so it runs before
 * anything on the page. Rebinds when the registry changes. Unlike tinykeys' default, shortcuts also
 * fire from inputs: all our bindings use a modifier or a function key, so they never collide with
 * typing. Key repeat is ignored, so a held key does one thing.
 *
 * Several commands may share a key (see `pickCommand`). The one that runs consumes the key (no
 * default action, no propagation); when none is enabled, the key goes to whoever else handles it.
 * Nothing runs while a dialog holds the keymap.
 */
export function useKeymap(onDoubleShift: () => void) {
  const commands = useCommands((s) => s.commands);

  useEffect(() => {
    const byBinding = new Map<string, Command[]>();
    const add = (binding: string, command: Command) => byBinding.set(binding, [...(byBinding.get(binding) ?? []), command]);
    for (const c of Object.values(commands)) {
      if (c.keybinding) add(c.keybinding, c);
      if (c.keybindingOutsideFields) add(c.keybindingOutsideFields, c);
    }
    const bindings = Object.fromEntries(
      [...byBinding].map(([binding, candidates]) => [
        binding,
        (event: KeyboardEvent) => {
          // In a field the key is the field's, even under a dialog: nothing runs or is suppressed.
          const applicable = candidates.filter((c) => c.keybindingOutsideFields !== binding || !typingInField());
          const resolved = resolveKey(applicable, focusContext(), keymapHeld());
          if (resolved.action === "pass") return;
          event.preventDefault();
          if (resolved.action === "suppress") return;
          event.stopPropagation();
          executeCommand(resolved.command.id);
        },
      ]),
    );
    try {
      return tinykeys(window, bindings, {
        capture: true,
        ignore: (event) => event.repeat || event.isComposing,
      });
    } catch (error) {
      // tinykeys reads its keys with a regex lookbehind, which WebKit only has from Safari 16.4, and
      // macOS 13.0 to 13.2 are older. Keel works without keys there. Throwing here would blank it.
      console.error("keyboard shortcuts are off", error);
    }
  }, [commands]);

  useEffect(() => detectDoubleShift(() => !keymapHeld() && onDoubleShift()), [onDoubleShift]);
}

/** Fires on two bare Shift taps in quick succession, with no other key in between. */
function detectDoubleShift(onDoubleShift: () => void): () => void {
  let lastTap = -Infinity;
  let clean = false;

  const down = (e: KeyboardEvent) => {
    if (e.key === "Shift" && !e.repeat) {
      clean = !(e.metaKey || e.ctrlKey || e.altKey);
    } else {
      clean = false;
      lastTap = -Infinity;
    }
  };
  const up = (e: KeyboardEvent) => {
    if (e.key !== "Shift" || !clean) return;
    const now = performance.now();
    if (now - lastTap < DOUBLE_SHIFT_MS) {
      lastTap = -Infinity;
      onDoubleShift();
    } else {
      lastTap = now;
    }
  };

  window.addEventListener("keydown", down, true);
  window.addEventListener("keyup", up, true);
  return () => {
    window.removeEventListener("keydown", down, true);
    window.removeEventListener("keyup", up, true);
  };
}
