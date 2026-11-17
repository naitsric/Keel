import { mockIPC } from "@tauri-apps/api/mocks";
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { Preflight, Project, ProjectAdded, Workflow } from "../lib/api";
import { useApp } from "../lib/store";
import { BoardView } from "./BoardView";
import { BrainPage } from "./BrainPage";
import { Home } from "./Home";
import { NewRunDialog } from "./NewRunDialog";
import { Onboarding } from "./Onboarding";
import { ProjectSetupDialog } from "./ProjectSetupDialog";
import { ProjectsRow } from "./ProjectsRow";
import { RemoveProjectDialog } from "./RemoveProjectDialog";
import { ProjectsView } from "./ProjectsView";
import { WorkflowsView } from "./WorkflowsView";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const project = (id: string, fields: Partial<Project> = {}): Project => ({
  id,
  name: id,
  path: `/Users/you/${id}`,
  githubAccount: null,
  githubRepo: null,
  setupCommand: null,
  checkCommand: null,
  allowedDomains: [],
  allowedRead: [],
  services: [],
  env: [],
  defaultWorkflow: null,
  sensitivePaths: [],
  allowLocalhost: false,
  issueLabel: null,
  ...fields,
});
const READY = project("ready", { checkCommand: "make test" });
const NEW = project("new-one");
const ADDED: ProjectAdded = { project: NEW, outcome: "new", map: { status: "started" } };
const WORKFLOW: Workflow = { id: "builtin:full", name: "Full", builtin: true, steps: [{ type: "check", id: "check", run: "make test" }] };
const ADD = "Add project…";

describe("adding a project", () => {
  let root: Root;
  let calls: { command: string; args: Record<string, unknown> }[];
  let listed: Project[];
  let refuse: unknown;

  const settle = () =>
    act(async () => {
      for (let i = 0; i < 4; i++) await new Promise((resolve) => setTimeout(resolve));
    });

  const show = async (ui: ReactNode, projects: Project[]) => {
    listed = projects;
    useApp.setState({ overview: { ...useApp.getState().overview, projects }, overviewLoaded: true, preflight: { gitFound: true, ignoredEnv: [], auth: { loggedIn: true }, blocker: null, version: null, ghFound: true } as unknown as Preflight });
    await act(async () => root.render(ui));
    await settle();
  };
  const page = () => document.body.textContent ?? "";
  const called = (command: string) => calls.filter((call) => call.command === command);
  const buttons = (name: string) => [...document.querySelectorAll("button")].filter((b) => b.textContent === name || b.getAttribute("aria-label") === name);
  const click = async (name: string) => {
    await act(async () => buttons(name)[0].click());
    await settle();
  };

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    calls = [];
    listed = [];
    refuse = null;
    mockIPC((command, payload) => {
      const args = (payload ?? {}) as Record<string, unknown>;
      calls.push({ command, args });
      switch (command) {
        case "plugin:dialog|open":
          return "/Users/you/new-one";
        case "project_add":
          listed = [...listed, NEW];
          return ADDED;
        case "project_remove":
          if (refuse) throw refuse;
          listed = listed.filter((p) => p.id !== args.projectId);
          return null;
        case "overview":
          return { ...useApp.getState().overview, projects: listed };
        case "project_setup":
          return { project: listed.find((p) => p.id === args.projectId) ?? NEW, guess: { check: null, setup: null, from: [], setupOptions: [], checkOptions: [], fromCi: false, ciChecks: [], ciLeftOut: [] } };
        case "github_status":
          return { repo: null, accounts: [], info: null, excessScopes: [], error: null };
        case "workflow_drafts_list":
        case "workflow_results":
          return [];
        case "workflows_list":
          return [WORKFLOW];
        case "brain_get":
          return { map: [], learnings: [], proposals: [], decisions: [], used: 0, limit: 4096, mapUsed: 0, mapLimit: 3072, prompt: "", mapRun: null };
        case "board_get":
          return { cards: [], working: 0, maxAgents: 2, defaultWorkflow: null, paused: null };
        case "max_agents_get":
          return 2;
        case "mcp_overview":
          return { settings: { enabled: false, port: 7413 }, status: { running: false, port: null, url: null, error: null }, clients: [], calls: [] };
        default:
          return null;
      }
    });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ projectSetup: null, newRunOpen: false, view: { name: "home" } });
  });

  describe("every way in leads to the same dialog", () => {
    const opensTheDialog = async () => {
      await click(ADD);
      expect(called("project_add")[0].args).toEqual({ path: "/Users/you/new-one", language: "en" });
      expect(useApp.getState().projectSetup).toMatchObject({ projectId: "new-one", added: ADDED });
      expect(page()).toContain("new-one is added");
    };

    it("from Home, one click away with projects already there", async () => {
      await show(
        <>
          <Home />
          <ProjectSetupDialog />
        </>,
        [READY],
      );
      expect(buttons(ADD)).toHaveLength(1);
      await opensTheDialog();
    });

    it("from Projects, by its heading", async () => {
      await show(
        <>
          <ProjectsView />
          <ProjectSetupDialog />
        </>,
        [READY],
      );
      await opensTheDialog();
    });

    it("from an empty Projects", async () => {
      await show(
        <>
          <ProjectsView />
          <ProjectSetupDialog />
        </>,
        [],
      );
      expect(page()).toContain(i18next.t("projects.none"));
      await opensTheDialog();
    });

    it("from an empty Workflows", async () => {
      await show(
        <>
          <WorkflowsView />
          <ProjectSetupDialog />
        </>,
        [],
      );
      expect(page()).not.toContain("when you start a run");
      await opensTheDialog();
    });

    it("from an empty Brain", async () => {
      await show(
        <>
          <BrainPage />
          <ProjectSetupDialog />
        </>,
        [],
      );
      expect(page()).not.toContain("when you start a run");
      await opensTheDialog();
    });

    it("from an empty Board", async () => {
      await show(
        <>
          <BoardView />
          <ProjectSetupDialog />
        </>,
        [],
      );
      expect(page()).not.toContain("when you start a run");
      await opensTheDialog();
    });

    it("from the Board, beside its selector, and lands on the new project", async () => {
      await show(
        <>
          <BoardView />
          <ProjectSetupDialog />
        </>,
        [READY],
      );
      await opensTheDialog();
      await click("Later");
      expect((document.querySelector("select") as HTMLSelectElement).value).toBe("new-one");
    });

    it("from Onboarding, and the dialog outlives it", async () => {
      await show(
        <>
          <Onboarding />
          <ProjectSetupDialog />
        </>,
        [],
      );
      await opensTheDialog();
    });

    it("says nothing when the folder picker is cancelled", async () => {
      mockIPC((command) => (command === "plugin:dialog|open" ? null : null));
      await show(<ProjectsRow />, [READY]);
      await click(ADD);
      expect(useApp.getState().projectSetup).toBeNull();
      expect(document.querySelector('[role="alert"]')).toBeNull();
    });

    it("says why a folder can't be added", async () => {
      mockIPC((command) => {
        if (command === "plugin:dialog|open") return "/Users/you/nope";
        if (command === "project_add") throw { code: "notARepo", reason: null, detail: "/Users/you/nope" };
        return null;
      });
      await show(<ProjectsRow />, [READY]);
      await click(ADD);
      expect(document.querySelector('[role="alert"]')?.textContent).toContain("/Users/you/nope");
      expect(useApp.getState().projectSetup).toBeNull();
    });
  });

  describe("from New Run", () => {
    const ui = (
      <>
        <NewRunDialog />
        <ProjectSetupDialog />
      </>
    );
    const textarea = () => document.querySelector("textarea") as HTMLTextAreaElement;
    const write = async (text: string) => {
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(textarea(), text);
        textarea().dispatchEvent(new Event("input", { bubbles: true }));
      });
    };

    it("closes, shows the dialog, and comes back with the new project and the prompt intact", async () => {
      useApp.setState({ newRunOpen: true });
      await show(ui, [READY]);
      await write("Fix the calendar");
      await click(ADD);
      expect(useApp.getState().newRunOpen).toBe(false);
      expect(textarea()).toBeNull();
      expect(page()).toContain("new-one is added");

      await click("Later");
      expect(useApp.getState().projectSetup).toBeNull();
      expect(useApp.getState().newRunOpen).toBe(true);
      expect(textarea().value).toBe("Fix the calendar");
      expect((document.querySelector("select") as HTMLSelectElement).value).toBe("new-one");
    });

    it("only the dialog is open at any moment", async () => {
      useApp.setState({ newRunOpen: true });
      await show(ui, [READY]);
      await click(ADD);
      expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(1);
      await click("Later");
      expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(1);
    });

    it("warns about a project with no check without stopping Start, and Finish setup comes back", async () => {
      useApp.setState({ newRunOpen: true });
      await show(ui, [NEW]);
      await write("Fix the calendar");
      expect(page()).toContain(i18next.t("newRun.checkNone"));
      expect(document.querySelector('[role="alert"]')).toBeNull();
      expect(buttons("Start")[0].disabled).toBe(false);
      expect(buttons(i18next.t("newRun.openProject"))).toHaveLength(0);

      await click("Finish setup");
      expect(useApp.getState().projectSetup).toMatchObject({ projectId: "new-one", added: null, returnToNewRun: true });
      expect(useApp.getState().newRunOpen).toBe(false);
      await click("Later");
      expect(useApp.getState().newRunOpen).toBe(true);
      expect(textarea().value).toBe("Fix the calendar");
    });

    it("doesn't claim a map it knows nothing about", async () => {
      useApp.setState({ newRunOpen: true });
      await show(ui, [READY]);
      expect(page()).not.toContain("to write its map");
    });

    it("leaves a project that was removed", async () => {
      useApp.setState({ newRunOpen: true });
      await show(ui, [NEW, READY]);
      expect((document.querySelector("select") as HTMLSelectElement).value).toBe("new-one");
      await act(async () => useApp.setState({ overview: { ...useApp.getState().overview, projects: [READY] } }));
      await settle();
      expect((document.querySelector("select") as HTMLSelectElement).value).toBe("ready");
    });
  });

  describe("the marker", () => {
    const row = (
      <>
        <ProjectsRow />
        <ProjectSetupDialog />
      </>
    );

    it("shows on a project that needs it and not on one that's ready", async () => {
      await show(row, [READY, NEW]);
      expect(buttons("Finish setup")).toHaveLength(1);
      expect(page()).toContain("ready");
      expect(document.querySelector('[aria-label="Set up"]')).not.toBeNull();
    });

    it("reopens the dialog with no claim about how it was added", async () => {
      await show(row, [NEW]);
      await click("Finish setup");
      expect(useApp.getState().projectSetup).toMatchObject({ projectId: "new-one", added: null, returnToNewRun: false });
      expect(page()).toContain("Finish setting up new-one");
    });

    it("clears by itself when the project is fixed, from anywhere", async () => {
      await show(row, [NEW]);
      await act(async () => useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ ...NEW, checkCommand: "make test" }] } }));
      expect(buttons("Finish setup")).toHaveLength(0);
    });

    it("stays on a project with no check, even with everything else done", async () => {
      await show(row, [{ ...NEW, githubRepo: "acme/app", githubAccount: "you", setupCommand: "npm ci" }]);
      expect(buttons("Finish setup")).toHaveLength(1);
    });

    it("shows on the board for the project selected", async () => {
      await show(
        <>
          <BoardView />
          <ProjectSetupDialog />
        </>,
        [NEW],
      );
      await click("Finish setup");
      expect(useApp.getState().projectSetup).toMatchObject({ projectId: "new-one", added: null, returnToNewRun: false });
    });

    it("isn't on the board for a project that's ready", async () => {
      await show(<BoardView />, [READY]);
      expect(buttons("Finish setup")).toHaveLength(0);
    });
  });

  describe("the project form", () => {
    const field = (label: string) => [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(label))?.querySelector("input") as HTMLInputElement;

    it("takes what the dialog saved, so its own Save can't put the old value back", async () => {
      await show(<ProjectsView />, [READY]);
      expect(field(i18next.t("settings.checkCommand")).value).toBe("make test");
      await act(async () => useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ ...READY, checkCommand: "pnpm test", setupCommand: "pnpm i" }] } }));
      await settle();
      expect(field(i18next.t("settings.checkCommand")).value).toBe("pnpm test");
      await click(i18next.t("settings.save"));
      expect(called("project_update")[0].args.settings).toMatchObject({ checkCommand: "pnpm test", setupCommand: "pnpm i" });
    });

    it("still says it saved, even though saving can start the card over", async () => {
      await show(<ProjectsView />, [READY]);
      mockIPC((command) => (command === "overview" ? { ...useApp.getState().overview, projects: [{ ...READY, setupCommand: "uv sync" }] } : command === "workflows_list" ? [WORKFLOW] : null));
      await click(i18next.t("settings.save"));
      expect(field(i18next.t("settings.setupCommand")).value).toBe("uv sync");
      expect(page()).toContain(i18next.t("settings.saved"));
    });

    it("saves the files too sensitive for a small change, one pattern per line", async () => {
      await show(<ProjectsView />, [{ ...READY, sensitivePaths: ["billing/"] }]);
      const box = [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("settings.sensitive")))?.querySelector("textarea") as HTMLTextAreaElement;
      expect(box.value).toBe("billing/");
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(box, "billing/\n\n  *.proto \n");
        box.dispatchEvent(new Event("input", { bubbles: true }));
      });
      await click(i18next.t("settings.save"));
      expect(called("project_update")[0].args.settings).toMatchObject({ sensitivePaths: ["billing/", "*.proto"] });
      expect(page()).toContain(".github/ and other CI, build and dependency files, test settings, migrations");
    });

    it("says which pattern it can't read", async () => {
      await show(<ProjectsView />, [READY]);
      mockIPC((command) => {
        if (command === "project_update") throw { code: "blocked", reason: { code: "invalidSetting", field: "sensitivePaths", value: "a[" }, detail: null };
        return command === "workflows_list" ? [WORKFLOW] : null;
      });
      await click(i18next.t("settings.save"));
      expect(page()).toContain("“a[” can't be read as a file pattern.");
    });

    it("carries the default workflow when it saves", async () => {
      await show(<ProjectsView />, [{ ...READY, defaultWorkflow: "builtin:full" }]);
      await click(i18next.t("settings.save"));
      expect(called("project_update")).toHaveLength(1);
    });
  });

  describe("removing", () => {
    it("says what it does and doesn't do, and does nothing until confirmed", async () => {
      await show(<ProjectsView />, [READY]);
      await click(i18next.t("remove.confirm"));
      expect(page()).toContain("Remove ready from Keel?");
      expect(page()).toContain(i18next.t("remove.doesNot"));
      expect(page()).toContain("Your folder isn't touched");
      expect(page()).toContain("Any cards it has in Todo start then");
      expect(called("project_remove")).toEqual([]);
      await click(i18next.t("remove.cancel"));
      expect(called("project_remove")).toEqual([]);
      expect(page()).toContain(READY.path);
    });

    it("removes it from the list once confirmed", async () => {
      await show(<ProjectsView />, [READY]);
      await click(i18next.t("remove.confirm"));
      const dialog = document.querySelector('[role="dialog"]') as HTMLElement;
      await act(async () => [...dialog.querySelectorAll("button")].find((b) => b.textContent === i18next.t("remove.confirm"))?.click());
      await settle();
      expect(called("project_remove")[0].args).toEqual({ projectId: "ready" });
      expect(useApp.getState().overview.projects).toEqual([]);
      expect(document.querySelector('[role="dialog"]')).toBeNull();
      expect(page()).toContain(i18next.t("projects.none"));
    });

    it("says why it's refused, with a way to the run", async () => {
      refuse = { code: "blocked", reason: { code: "projectHasOpenRuns", count: 2, runId: "r9", title: "Fix the calendar" }, detail: null };
      await show(<RemoveProjectDialog project={READY} open onOpenChange={() => undefined} />, [READY]);
      const dialog = document.querySelector('[role="dialog"]') as HTMLElement;
      await act(async () => [...dialog.querySelectorAll("button")].find((b) => b.textContent === i18next.t("remove.confirm"))?.click());
      await settle();
      expect(document.querySelector('[role="alert"]')?.textContent).toContain("“Fix the calendar” and 1 more");
      await click(i18next.t("remove.openRun"));
      expect(useApp.getState().view).toEqual({ name: "run", runId: "r9" });
    });

    it("has no way to a run that doesn't exist yet", async () => {
      refuse = { code: "blocked", reason: { code: "projectHasOpenRuns", count: 1, runId: "", title: "A card" }, detail: null };
      await show(<RemoveProjectDialog project={READY} open onOpenChange={() => undefined} />, [READY]);
      const dialog = document.querySelector('[role="dialog"]') as HTMLElement;
      await act(async () => [...dialog.querySelectorAll("button")].find((b) => b.textContent === i18next.t("remove.confirm"))?.click());
      await settle();
      expect(document.querySelector('[role="alert"]')?.textContent).toContain("A card");
      expect(buttons(i18next.t("remove.openRun"))).toHaveLength(0);
    });

    it("keeps the runs on Home when no projects are left", async () => {
      useApp.setState({ overview: { ...useApp.getState().overview, runs: [{ id: "r1", title: "An old run", projectName: "gone", stepId: "builder", attemptId: "a", attemptNumber: 1, state: "succeeded", reason: null, model: "m", turns: 1, startedAt: "2026-11-05T10:00:00Z", finishedAt: "2026-11-05T10:01:00Z", lastActivityAt: null, startedBy: null, prUrl: null, projectId: "p" }] } });
      await show(<Home />, []);
      expect(page()).toContain("An old run");
      expect(page()).toContain(i18next.t("onboarding.title"));
      useApp.setState({ overview: { ...useApp.getState().overview, runs: [] } });
    });
  });

  it("the onboarding is truthful about GitHub", async () => {
    await show(<Onboarding />, []);
    expect(page()).not.toContain("optional");
    expect(page()).toContain(i18next.t("onboarding.neededForBuiltIn"));
    expect(i18next.t("onboarding.githubMissing")).toContain("only a single step or a custom workflow with no delivery can run");
    expect(i18next.t("onboarding.githubSignedOut")).toContain("only a single step or a custom workflow with no delivery can run");
  });
});
