import { useEffect, useRef } from "react";
import { KEYS } from "./keys";
import { registerCommands } from "./registry";

/** Binds a page's commands once, not on every render. They read what the page can do right now from here. */
function useLatest<T>(value: T) {
  const latest = useRef(value);
  latest.current = value;
  return latest;
}

/** What the run page can do right now. Null means not now, and the key goes to whoever else handles it. */
export interface RunPageActions {
  showChanges: (() => void) | null;
  stopStep: (() => void) | null;
}

export function useRunCommands(actions: RunPageActions) {
  const page = useLatest(actions);
  useEffect(
    () =>
      registerCommands([
        { id: "run.showChanges", title: "shortcuts.showChanges", category: "shortcuts.categories.run", keybinding: KEYS.showChanges, context: "run", enabled: () => page.current.showChanges !== null, run: () => page.current.showChanges?.() },
        { id: "run.stopStep", title: "shortcuts.stopStep", category: "shortcuts.categories.run", keybinding: KEYS.stopStep, context: "run", enabled: () => page.current.stopStep !== null, run: () => page.current.stopStep?.() },
      ]),
    [page],
  );
}

/** `newTask` is null until the board can take one, and then ⌘N is New run as everywhere else. */
export function useBoardCommands(newTask: (() => void) | null) {
  const page = useLatest(newTask);
  useEffect(
    () => registerCommands([{ id: "board.newTask", title: "shortcuts.newTask", category: "shortcuts.categories.board", keybinding: KEYS.newRun, context: "board", enabled: () => page.current !== null, run: () => page.current?.() }]),
    [page],
  );
}
