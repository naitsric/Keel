import { useLayoutEffect } from "react";

/**
 * A dialog holds the keymap while it shows. Then no keybinding runs its command (⌘N opens nothing
 * behind it) and Shift Shift opens nothing; the dialog's own keys (Tab, Enter, Space, Escape) reach
 * it as usual.
 *
 * Checked where keys are dispatched (see keymap.ts), not by each command.
 */
const holds = new Set<symbol>();

/** Holds the keymap until the returned function is called. */
export function holdKeymap(): () => void {
  const hold = Symbol("keymap hold");
  holds.add(hold);
  return () => void holds.delete(hold);
}

export function keymapHeld(): boolean {
  return holds.size > 0;
}

/** Holds the keymap while `active`: some dialogs stay mounted when they're closed. */
export function useKeymapHold(active: boolean) {
  // Before paint, so no key gets through between showing and holding.
  useLayoutEffect(() => (active ? holdKeymap() : undefined), [active]);
}
