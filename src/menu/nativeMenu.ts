import { listen } from "@tauri-apps/api/event";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { runUnlessHeld, useCommands } from "../commands/registry";
import { api } from "../lib/api";
import { buildMenuModel } from "./menuModel";

/** A page's commands come and go together: one rebuild for the burst. */
const REBUILD_DELAY_MS = 150;

/**
 * A key with an accelerator reaches either the webview or the menu, depending on who AppKit asks
 * first. Should both ever fire for one press, the menu's run is dropped if the keymap just ran it.
 */
const DOUBLE_FIRE_WINDOW_MS = 250;
const keyboardRuns = new Map<string, number>();

/** Called by the keymap for every command it runs. */
export function noteKeyboardRun(commandId: string) {
  keyboardRuns.set(commandId, performance.now());
}

/**
 * Runs what the menu picked, by click or accelerator, through the same command as the key. AppKit
 * knows nothing of a dialog and keeps the menu bar live, so a hold stops it here.
 */
export function runMenuCommand(commandId: string) {
  const ranAt = keyboardRuns.get(commandId);
  if (ranAt !== undefined && performance.now() - ranAt < DOUBLE_FIRE_WINDOW_MS) return;
  runUnlessHeld(commandId);
}

/** Keeps the native menu bar in step with the registry and the language, and runs what it picks. */
export function useNativeMenu() {
  const { t, i18n } = useTranslation();
  const commands = useCommands((s) => s.commands);

  useEffect(() => {
    const timer = setTimeout(() => {
      api.menuSet(buildMenuModel(Object.values(commands), (key) => t(key))).catch((e) => console.error("menu update failed", e));
    }, REBUILD_DELAY_MS);
    return () => clearTimeout(timer);
  }, [commands, t, i18n.language]);

  useEffect(() => {
    const unlisten = listen<string>("keel://menu-command", ({ payload }) => runMenuCommand(payload));
    return () => void unlisten.then((stop) => stop());
  }, []);
}
