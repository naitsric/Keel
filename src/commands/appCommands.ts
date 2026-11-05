import { useApp, type View } from "../lib/store";
import { KEYS } from "./keys";
import { registerCommands, type Command } from "./registry";
import { nextWaitingRun, openRun } from "./runs";
import { useSearchEverywhere, type SearchScope } from "./SearchEverywhere";

const NAVIGATE = "shortcuts.categories.navigate";
const RUN = "shortcuts.categories.run";

const app = () => useApp.getState();

const showPalette = (scope: SearchScope) => () => useSearchEverywhere.getState().show(scope);

/** Already there, nothing changes: a page keeps what it's showing, like the board's project. */
const goTo = (view: View) => () => {
  if (app().view.name !== view.name) app().navigate(view);
};

/** Where the top bar's Back goes, or null where it doesn't show. */
function backTarget(): View | null {
  const { view } = app();
  if (view.name === "home") return null;
  return (view.name === "run" && view.backTo) || { name: "home" };
}

function goBack() {
  const target = backTarget();
  if (target) app().navigate(target);
}

const waitingRun = (step: 1 | -1) => {
  const { view, overview } = app();
  return nextWaitingRun(overview.runs, view.name === "run" ? view.runId : null, step);
};

const openWaitingRun = (step: 1 | -1) => () => {
  const run = waitingRun(step);
  if (run) openRun(run.id);
};

/** Everything that works on any page. It only moves around and opens dialogs. */
export function registerAppCommands(): () => void {
  const commands: Command[] = [
    { id: "search.everywhere", title: "shortcuts.searchEverywhere", category: NAVIGATE, displayKey: KEYS.searchEverywhere, run: showPalette("all") },
    { id: "search.findAction", title: "shortcuts.findAction", category: NAVIGATE, keybinding: KEYS.findAction, run: showPalette("actions") },
    { id: "navigate.goToRun", title: "shortcuts.goToRun", category: NAVIGATE, keybinding: KEYS.goToRun, run: showPalette("runs") },
    { id: "navigate.runs", title: "shortcuts.runs", category: NAVIGATE, keybinding: KEYS.runs, run: goTo({ name: "home" }) },
    { id: "navigate.board", title: "shortcuts.board", category: NAVIGATE, keybinding: KEYS.board, run: goTo({ name: "board" }) },
    { id: "navigate.workflows", title: "shortcuts.workflows", category: NAVIGATE, keybinding: KEYS.workflows, run: goTo({ name: "workflows" }) },
    { id: "navigate.brain", title: "shortcuts.brain", category: NAVIGATE, keybinding: KEYS.brain, run: goTo({ name: "brain" }) },
    // The native menu takes ⌘, first, so here it's only findable.
    { id: "navigate.settings", title: "shortcuts.settings", category: NAVIGATE, displayKey: KEYS.settings, run: goTo({ name: "settings" }) },
    { id: "navigate.back", title: "shortcuts.back", category: NAVIGATE, keybinding: KEYS.back, keybindingOutsideFields: KEYS.backOutsideFields, enabled: () => backTarget() !== null, run: goBack },
    { id: "run.nextWaiting", title: "shortcuts.nextWaiting", category: NAVIGATE, keybinding: KEYS.nextWaiting, enabled: () => waitingRun(1) !== null, run: openWaitingRun(1) },
    { id: "run.previousWaiting", title: "shortcuts.previousWaiting", category: NAVIGATE, keybinding: KEYS.previousWaiting, enabled: () => waitingRun(-1) !== null, run: openWaitingRun(-1) },
    // Opens the dialog. Starting takes its own button.
    { id: "run.new", title: "shortcuts.newRun", category: RUN, keybinding: KEYS.newRun, run: () => app().setNewRunOpen(true) },
  ];
  return registerCommands(commands);
}
