import { useLayoutEffect } from "react";

/**
 * A dialog holds the keymap while it shows. Then no keybinding runs its command (⌘N opens nothing
 * behind it) and Shift Shift opens nothing; the dialog's own keys (Tab, Enter, Space, Escape) reach
 * it as usual.
 *
 * Checked where keys are dispatched (see keymap.ts) and for the bars and the native menu (see
 * `runUnlessHeld`), not by each command.
 */
const holds = new Set<symbol>();
const watchers = new Set<() => void>();

const changed = () => {
  for (const watcher of [...watchers]) watcher();
};

/** Holds the keymap until the returned function is called. */
export function holdKeymap(): () => void {
  const hold = Symbol("keymap hold");
  holds.add(hold);
  changed();
  return () => {
    holds.delete(hold);
    changed();
  };
}

export function keymapHeld(): boolean {
  return holds.size > 0;
}

/** Calls `watcher` whenever a hold starts or ends. Returns a function that stops it. */
export function watchKeymapHold(watcher: () => void): () => void {
  watchers.add(watcher);
  return () => void watchers.delete(watcher);
}

/** Holds the keymap while `active`: some dialogs stay mounted when they're closed. */
export function useKeymapHold(active: boolean) {
  // Before paint, so no key gets through between showing and holding.
  useLayoutEffect(() => (active ? holdKeymap() : undefined), [active]);
}
