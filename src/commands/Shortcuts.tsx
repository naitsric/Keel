import { useEffect } from "react";
import { registerAppCommands } from "./appCommands";
import { useKeymap } from "./keymap";
import { SearchEverywhere, useSearchEverywhere } from "./SearchEverywhere";

const searchEverything = () => useSearchEverywhere.getState().show("all");

/** The keys and the palette. It adds nothing to the screen until they're used. */
export function Shortcuts() {
  useEffect(() => registerAppCommands(), []);
  useKeymap(searchEverything);
  return <SearchEverywhere />;
}
