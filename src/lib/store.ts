import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";
import i18next, { type Language, systemLanguage } from "../i18n";
import { api, type McpOverview, type Overview, type Preflight, type ProjectAdded } from "./api";
import type { QuestionPick } from "./questions";

export type View = { name: "home" } | { name: "run"; runId: string; backTo?: View } | { name: "settings" } | { name: "workflows" } | { name: "brain" } | { name: "board"; projectId?: string };

export interface ProjectSetupRequest {
  projectId: string;
  /** What adding it did. Null when it's being set up later, from a "Finish setup". */
  added: ProjectAdded | null;
  /** New Run closes for this, and opens again when it's done, with what was in it. */
  returnToNewRun: boolean;
}

interface AppState {
  view: View;
  preflight: Preflight | null;
  overview: Overview;
  /** Whether `overview` has real data yet, so an empty list means there's really nothing. */
  overviewLoaded: boolean;
  newRunOpen: boolean;
  projectSetup: ProjectSetupRequest | null;
  /** The project the Brain page opens on, when something sent the person there for it. */
  brainProjectId: string | null;
  quitOpen: boolean;
  /** Notes on approvals not answered yet, per attempt, so leaving the page doesn't drop them. */
  approvalDrafts: Record<string, string>;
  /** What's chosen on questions not answered yet, per attempt and question, for the same reason. */
  questionDrafts: Record<string, Record<string, QuestionPick>>;
  /** Bumps whenever the backend reports a change, so open views can refetch. */
  revision: number;
  /** Whether the MCP server is up, for the status bar. Null until the first answer. */
  mcp: McpOverview["status"] | null;
  navigate: (view: View) => void;
  setNewRunOpen: (open: boolean) => void;
  openProjectSetup: (request: ProjectSetupRequest) => void;
  closeProjectSetup: () => void;
  setQuitOpen: (open: boolean) => void;
  setApprovalDraft: (attemptId: string, text: string) => void;
  setQuestionDraft: (attemptId: string, questionId: string, pick: QuestionPick) => void;
  clearQuestionDrafts: (attemptId: string) => void;
  refreshOverview: () => Promise<void>;
  refreshSession: () => Promise<void>;
  refreshMcp: () => Promise<void>;
  setLanguage: (language: Language) => Promise<void>;
}

export const useApp = create<AppState>((set, get) => ({
  view: { name: "home" },
  preflight: null,
  overview: { runs: [], projects: [], pendingProposals: 0, workflowDrafts: 0, totals: { running: 0, retrying: 0, blocked: 0, tokens: 0, durationMs: 0, costUsd: 0 } },
  overviewLoaded: false,
  newRunOpen: false,
  projectSetup: null,
  brainProjectId: null,
  quitOpen: false,
  approvalDrafts: {},
  questionDrafts: {},
  revision: 0,
  mcp: null,
  navigate: (view) => set({ view }),
  setNewRunOpen: (newRunOpen) => set({ newRunOpen }),
  // One dialog at a time: two swapping places in one go fight over focus.
  openProjectSetup: (projectSetup) => {
    set({ newRunOpen: false });
    setTimeout(() => set({ projectSetup }));
  },
  closeProjectSetup: () => {
    const back = get().projectSetup?.returnToNewRun;
    set({ projectSetup: null });
    if (back) setTimeout(() => set({ newRunOpen: true }));
  },
  setQuitOpen: (quitOpen) => set({ quitOpen }),
  setApprovalDraft: (attemptId, text) =>
    set((s) => {
      const approvalDrafts = { ...s.approvalDrafts, [attemptId]: text };
      if (!text) delete approvalDrafts[attemptId];
      return { approvalDrafts };
    }),
  setQuestionDraft: (attemptId, questionId, pick) => set((s) => ({ questionDrafts: { ...s.questionDrafts, [attemptId]: { ...s.questionDrafts[attemptId], [questionId]: pick } } })),
  clearQuestionDrafts: (attemptId) =>
    set((s) => {
      const questionDrafts = { ...s.questionDrafts };
      delete questionDrafts[attemptId];
      return { questionDrafts };
    }),
  refreshOverview: async () => set({ overview: await api.overview(), overviewLoaded: true }),
  refreshSession: async () => set({ preflight: await api.refreshSession() }),
  refreshMcp: async () => set({ mcp: (await api.mcpOverview()).status }),
  setLanguage: async (language) => {
    await i18next.changeLanguage(language);
    await api.languageSet(language);
  },
}));

/** Loads what the app needs and starts listening to the backend. */
export async function startApp() {
  const saved = await api.languageGet().catch(() => null);
  await i18next.changeLanguage(saved === "es" || saved === "en" ? saved : systemLanguage());

  const { refreshOverview, refreshMcp } = useApp.getState();
  void refreshOverview();
  void api.preflight().then((preflight) => useApp.setState({ preflight }));
  void refreshMcp().catch(() => undefined);

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
  await listen("keel://mcp-changed", () => void refreshMcp().catch(() => undefined));
  await listen("keel://quit-requested", () => useApp.setState({ quitOpen: true }));
}
