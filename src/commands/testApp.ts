// The whole app on made-up data, for tests that press keys. Not part of the app: nothing it ships
// imports this.
import { mockIPC } from "@tauri-apps/api/mocks";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { vi } from "vitest";
import App from "../App";
import i18next from "../i18n";
import type { Attempt, AttemptState, BoardCard, CiGap, IssueWatch, McpProjectChange, Project, Run, RunRow, ToolFolder } from "../lib/api";
import { useApp, type View } from "../lib/store";
import { useCommands } from "./registry";
import { useSearchEverywhere } from "./SearchEverywhere";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

export const row = (id: string, state: AttemptState, extra: Partial<RunRow> = {}) => ({ id, title: `Run ${id}`, projectId: "p1", projectName: "demo", state, attemptId: `${id}-step`, stepId: "builder", attemptNumber: 1, reason: null, model: "sonnet", turns: null, startedAt: "2026-11-05T10:00:00Z", finishedAt: null, lastActivityAt: null, startedBy: null, prUrl: null, ...extra }) as RunRow;

const project = (id: string, name: string) => ({ id, name, path: `/Users/you/${name}`, githubAccount: "you-work", githubRepo: `acme/${name}`, setupCommand: null, checkCommand: null, allowedDomains: [], allowedRead: [], services: [], env: [], defaultWorkflow: null, sensitivePaths: [], allowLocalhost: false, issueLabel: null }) as Project;
export const PROJECTS = [project("p1", "demo"), project("p2", "payments")];

const attempt = (fields: Partial<Attempt>) => ({ kind: "agent", role: "worker", tools: "build", state: "succeeded", round: 0, number: 1, output: null, resultText: null, filesChanged: null, runFiles: null, startedAt: "2026-11-05T10:00:00Z", ...fields }) as Attempt;
const DELIVERS = [
  { type: "agent", id: "builder" },
  { type: "approval", id: "human" },
  { type: "deliver", id: "pr" },
];

/** A run's steps. A run whose id starts with "empty" changed nothing, so its card closes instead of approving. */
function attemptsOf(row: RunRow): Attempt[] {
  const files = row.id.startsWith("empty") ? [] : ["calc.py"];
  const builder = attempt({ id: `${row.id}-builder`, runId: row.id, stepId: "builder", state: row.state === "waitingApproval" ? "succeeded" : row.state, filesChanged: files, runFiles: files });
  if (row.state !== "waitingApproval") return [builder];
  return [builder, attempt({ id: `${row.id}-human`, runId: row.id, stepId: "human", kind: "approval", tools: "none", state: "waitingApproval" })];
}

/** Every call the app makes to the backend, in order. */
export let calls: { command: string; args: unknown }[] = [];

let root: Root | null = null;
let shownRuns: RunRow[] = [];
let ciGaps: Record<string, CiGap> = {};
let projectChanges: McpProjectChange[] = [];

/** What MCP clients added or configured, until `hideApp`. */
export const setProjectChanges = (changes: McpProjectChange[]) => {
  projectChanges = changes;
};

let issueWatch: IssueWatch | null = null;
let lastLook: IssueWatch | null = null;

/** What a look at a project's issues finds, until `hideApp`. Also what the last look found, unless `stored` is false. */
export const setIssueWatch = (watch: IssueWatch | null, { stored = true } = {}) => {
  issueWatch = watch;
  lastLook = stored ? watch : null;
};

/** What a project's CI checks and its check doesn't, until `hideApp`. */
export const setCiGap = (projectId: string, gap: CiGap) => {
  ciGaps = { ...ciGaps, [projectId]: gap };
};

export const callsTo = (command: string) => calls.filter((call) => call.command === command);

/** What Keel finds a project's commands need from the home folder. Nothing unless a test says. */
export let toolFolders: ToolFolder[] = [];
export const setToolFolders = (folders: ToolFolder[]) => {
  toolFolders = folders;
};
export const answers = () => callsTo("approval_answer");

function backend(command: string, args: unknown) {
  calls.push({ command, args });
  const { runId, projectId } = (args ?? {}) as { runId?: string; projectId?: string };
  const shown = shownRuns.find((r) => r.id === runId);
  switch (command) {
    case "run_get":
      return shown ? ({ id: shown.id, projectId: "p1", title: shown.title, branch: "keel/abc", round: 0, workflow: DELIVERS } as unknown as Run) : null;
    case "run_attempts":
      return shown ? attemptsOf(shown) : [];
    case "run_diff":
      return "diff --git a/calc.py b/calc.py";
    case "run_adds_secret":
      return false;
    case "attempt_events":
    case "run_proposals":
    case "workflows_list":
    case "workflow_results":
    case "workflow_drafts_list":
    case "decisions_list":
    case "issues_list":
    case "github_accounts":
      return [];
    case "attempt_raw_check":
      return "intact";
    case "board_get":
      // No project is every project's cards, which are p1's.
      return { cards: (projectId ?? "p1") === "p1" ? [card("c1", "A task in the backlog", "p1"), card("c2", "Another task", "p1")] : [], working: 0, maxAgents: 2, defaultWorkflow: null, paused: null };
    case "brain_get":
      return { map: [], learnings: [], proposals: [], decisions: [], used: 0, limit: 4096, mapUsed: 0, mapLimit: 3072, prompt: "", mapRun: null };
    case "mcp_overview":
      return { settings: { enabled: false, port: 7413 }, status: { running: false, port: null, url: null, error: null }, clients: [], calls: [], projectChanges };
    case "github_status":
      return { repo: "acme/demo", accounts: [], info: null, excessScopes: [], error: null };
    case "project_tool_folders":
      return toolFolders;
    case "issues_watch":
      return lastLook;
    case "issues_look":
      lastLook = issueWatch;
      return issueWatch;
    case "project_update": {
      // Saved as Keel saves it: a new label forgets what the old one found.
      const { settings } = args as { settings: Project };
      const overview = useApp.getState().overview;
      const before = overview?.projects.find((p) => p.id === projectId);
      if (overview && before) {
        if (before.issueLabel !== settings.issueLabel) lastLook = null;
        const saved = { ...before, ...settings, id: before.id, name: before.name, path: before.path, githubRepo: before.githubRepo };
        useApp.setState({ overview: { ...overview, projects: overview.projects.map((p) => (p.id === before.id ? saved : p)) } });
        return saved;
      }
      return null;
    }
    case "project_ci_gap":
      return ciGaps[projectId ?? ""] ?? { commands: [], leftOut: [], from: [] };
    case "overview":
      return useApp.getState().overview;
    case "max_agents_get":
      return 2;
    default:
      return null;
  }
}

const card = (id: string, title: string, projectId?: string) => ({ id, projectId, title, body: "", issue: null, column: "backlog", position: 0, runId: null, runState: null, reason: null, prUrl: null, rejection: null, startBlocked: null, createdAt: "2026-11-05T10:00:00Z" }) as unknown as BoardCard;

/** Lets what's waiting happen: answers from the backend, a palette closing, a card mounting. */
export const settle = (ms = 20) => act(() => vi.advanceTimersByTimeAsync(ms));

export async function showApp(runs: RunRow[], view: View = { name: "home" }, projects: Project[] = PROJECTS) {
  calls = [];
  shownRuns = runs;
  mockIPC(backend, { shouldMockEvents: true });
  await i18next.changeLanguage("en");
  useApp.setState({ view, preflight: null, mcp: null, approvalDrafts: {}, newRunOpen: false, quitOpen: false, overviewLoaded: true, overview: { runs, projects, pendingProposals: 0, proposalsByProject: {}, workflowDrafts: 0, projectProposals: 0, totals: { running: 0, retrying: 0, blocked: 0, tokens: 0, durationMs: 0, costUsd: 0 } } });
  useSearchEverywhere.setState({ open: false, scope: "all", query: "" });
  const container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => root?.render(createElement(App)));
  await settle();
}

export async function hideApp() {
  await act(async () => root?.unmount());
  root = null;
  toolFolders = [];
  ciGaps = {};
  projectChanges = [];
  issueWatch = null;
  lastLook = null;
  await settle();
  document.body.replaceChildren();
  // Radix leaves this for a dialog that closes with the page; the next test starts clean.
  document.body.removeAttribute("style");
  useCommands.setState({ commands: {} });
}

const MOD: KeyboardEventInit = /Mac|iPhone|iPad/.test(navigator.platform) ? { metaKey: true } : { ctrlKey: true };

export function pressKey(key: string, code: string, init: KeyboardEventInit = {}, on?: Element) {
  const event = new KeyboardEvent("keydown", { key, code, bubbles: true, cancelable: true, ...init });
  act(() => void (on ?? document.activeElement ?? document.body).dispatchEvent(event));
  return event;
}

/** ⌘ on a Mac. tinykeys reads `$mod` from the platform, and happy-dom says Linux. */
export const pressMod = (key: string, code: string, init: KeyboardEventInit = {}) => pressKey(key, code, { ...MOD, ...init });

/** What a browser does with Enter: the page hears it, and then a focused button is pressed unless it was prevented. */
export function pressEnter(init: KeyboardEventInit = {}) {
  const focused = document.activeElement;
  const event = pressKey("Enter", "Enter", init);
  if (!event.defaultPrevented && focused instanceof HTMLButtonElement) act(() => focused.click());
}

export function tapShift() {
  act(() => {
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Shift", code: "ShiftLeft" }));
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Shift", code: "ShiftLeft" }));
  });
}

/** Types into what has focus, the way React hears it. */
export function type(text: string) {
  const field = document.activeElement as HTMLInputElement | HTMLTextAreaElement;
  const proto = field instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  act(() => {
    Object.getOwnPropertyDescriptor(proto, "value")?.set?.call(field, text);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

export const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);
export const note = () => document.querySelector("textarea");
export const paletteRows = () => [...document.querySelectorAll<HTMLElement>("[cmdk-item]")];
export const paletteRow = (text: string) => paletteRows().find((row) => row.textContent?.startsWith(text));
export const paletteOpen = () => document.querySelector("[cmdk-root]") !== null;
export const view = () => useApp.getState().view;
