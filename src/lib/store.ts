import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";
import i18next, { type Language, systemLanguage } from "../i18n";
import { api, type Overview, type Preflight } from "./api";

export type View = { name: "home" } | { name: "run"; runId: string } | { name: "settings" } | { name: "workflows" } | { name: "brain" };

interface AppState {
  view: View;
  preflight: Preflight | null;
  overview: Overview;
  /** Whether `overview` has real data yet, so an empty list means there's really nothing. */
  overviewLoaded: boolean;
  newRunOpen: boolean;
  quitOpen: boolean;
  /** Bumps whenever the backend reports a change, so open views can refetch. */
  revision: number;
  navigate: (view: View) => void;
  setNewRunOpen: (open: boolean) => void;
  setQuitOpen: (open: boolean) => void;
  refreshOverview: () => Promise<void>;
  refreshSession: () => Promise<void>;
  setLanguage: (language: Language) => Promise<void>;
}

export const useApp = create<AppState>((set) => ({
  view: { name: "home" },
  preflight: null,
  overview: { runs: [], projects: [], pendingProposals: 0, totals: { running: 0, retrying: 0, blocked: 0, tokens: 0, durationMs: 0, costUsd: 0 } },
  overviewLoaded: false,
  newRunOpen: false,
  quitOpen: false,
  revision: 0,
  navigate: (view) => set({ view }),
  setNewRunOpen: (newRunOpen) => set({ newRunOpen }),
  setQuitOpen: (quitOpen) => set({ quitOpen }),
  refreshOverview: async () => set({ overview: await api.overview(), overviewLoaded: true }),
  refreshSession: async () => set({ preflight: await api.refreshSession() }),
  setLanguage: async (language) => {
    await i18next.changeLanguage(language);
    await api.languageSet(language);
    await syncMenu();
  },
}));

const MENU_KEYS = [
  "about",
  "settings",
  "hide",
  "hideOthers",
  "showAll",
  "quit",
  "edit",
  "undo",
  "redo",
  "cut",
  "copy",
  "paste",
  "selectAll",
  "window",
  "minimize",
  "zoom",
  "close",
];

/** The native menu takes its labels from the translations, so it follows the language. */
async function syncMenu() {
  await api.menuSet(Object.fromEntries(MENU_KEYS.map((key) => [key, i18next.t(`menu.${key}`)])));
}

/** Loads what the app needs and starts listening to the backend. */
export async function startApp() {
  const saved = await api.languageGet().catch(() => null);
  await i18next.changeLanguage(saved === "es" || saved === "en" ? saved : systemLanguage());
  await syncMenu().catch(() => undefined);

  const { refreshOverview } = useApp.getState();
  void refreshOverview();
  void api.preflight().then((preflight) => useApp.setState({ preflight }));

  // Steps can report many changes a second. One refresh per burst is enough.
  let pending = false;
  await listen("keel://changed", () => {
    if (pending) return;
    pending = true;
    setTimeout(() => {
      pending = false;
      void refreshOverview();
      useApp.setState((state) => ({ revision: state.revision + 1 }));
    }, 150);
  });
  await listen("keel://quit-requested", () => useApp.setState({ quitOpen: true }));
  await listen("keel://open-settings", () => useApp.setState({ view: { name: "settings" } }));
}
