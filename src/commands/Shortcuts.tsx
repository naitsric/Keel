import { useEffect } from "react";
import { useNativeMenu } from "../menu/nativeMenu";
import { registerAppCommands } from "./appCommands";
import { useKeymap } from "./keymap";
import { SearchEverywhere, useSearchEverywhere } from "./SearchEverywhere";

const searchEverything = () => useSearchEverywhere.getState().show("all");

/** The keys, the palette and the native menu. It adds nothing to the screen until they're used. */
export function Shortcuts() {
  useEffect(() => registerAppCommands(), []);
  useKeymap(searchEverything);
  useNativeMenu();
  return <SearchEverywhere />;
}
