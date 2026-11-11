import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { callsTo, hideApp, paletteRow, pressKey, pressMod, row, setCiGap, setToolFolders, settle, showApp, view } from "../commands/testApp";
import i18next from "../i18n";
import type { Project } from "../lib/api";
import { useApp } from "../lib/store";

const RUNS = [row("r1", "running")];
const project = (id: string, fields: Partial<Project> = {}): Project => ({ id, name: id, path: `/Users/you/${id}`, githubAccount: "you", githubRepo: `acme/${id}`, setupCommand: null, checkCommand: "make test", allowedDomains: [], allowedRead: [], services: [], env: [], defaultWorkflow: null, sensitivePaths: [], ...fields });
const THREE = [project("alpha"), project("beta", { checkCommand: null }), project("gamma", { githubRepo: "other/thing" })];

const header = () => document.querySelector("header") as HTMLElement;
const main = () => document.querySelector("main") as HTMLElement;
const list = () => document.querySelector('[aria-label="Projects"]') as HTMLElement;
const listText = () => list().textContent ?? "";
const click = async (el: Element | null | undefined) => {
  act(() => (el as HTMLElement | null)?.click());
  await settle();
};
const projectButton = (name: string) => [...list().querySelectorAll("button")].find((b) => b.textContent?.startsWith(name));
const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);
const heading = () => main().querySelector("h2")?.textContent;
const typeInto = (el: HTMLInputElement | HTMLTextAreaElement, value: string) =>
  act(async () => {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value")?.set?.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
const envBox = () => [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("settings.env")))?.querySelector("textarea") as HTMLTextAreaElement;

describe("the Projects page", () => {
  beforeEach(() => void vi.useFakeTimers());
  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  it("opens from the top bar, ⌘5 and the palette, and the bar marks it", async () => {
    await showApp(RUNS, { name: "home" }, THREE);
    await click([...header().querySelectorAll("button")].find((b) => b.textContent === "Projects"));
    expect(view().name).toBe("projects");
    expect([...header().querySelectorAll('[aria-current="page"]')].map((b) => b.textContent)).toEqual(["Projects"]);
    await hideApp();
    await showApp(RUNS, { name: "home" }, THREE);
    pressMod("5", "Digit5");
    await settle();
    expect(view().name).toBe("projects");
    await hideApp();
    await showApp(RUNS, { name: "home" }, THREE);
    pressMod("A", "KeyA", { shiftKey: true });
    await act(async () => paletteRow("Go to Projects")?.click());
    await settle();
    expect(view().name).toBe("projects");
  });

  it("lists every project with its repo and state, but shows the form of the selected one only", async () => {
    await showApp(RUNS, { name: "projects" }, THREE);
    expect(listText()).toContain("alpha");
    expect(listText()).toContain("acme/alpha");
    expect(listText()).toContain("other/thing");
    expect(projectButton("alpha")?.textContent).toContain(i18next.t("projects.ready"));
    expect(projectButton("beta")?.textContent).toContain(i18next.t("projects.needsSetup"));
    expect(heading()).toBe("alpha");
    expect(main().querySelectorAll("textarea").length).toBeGreaterThan(0);
    expect(document.querySelectorAll(`input[placeholder="${i18next.t("settings.checkCommandPlaceholder")}"]`)).toHaveLength(1);
    await click(projectButton("beta"));
    expect(heading()).toBe("beta");
  });

  it("opens on the project it was sent to, or the first if that one is gone or none was given", async () => {
    await showApp(RUNS, { name: "projects", projectId: "gamma" }, THREE);
    expect(heading()).toBe("gamma");
    await hideApp();
    await showApp(RUNS, { name: "projects", projectId: "gone" }, THREE);
    expect(heading()).toBe("alpha");
  });

  it("only offers the filter from 6 projects, and keeps those whose name or repo has the text", async () => {
    await showApp(RUNS, { name: "projects" }, THREE);
    expect(document.querySelector('input[type="search"]')).toBeNull();
    await hideApp();
    const six = ["a", "b", "c", "d", "e", "f"].map((id) => project(`p-${id}`, { githubRepo: id === "e" ? "acme/needle" : `acme/${id}` }));
    await showApp(RUNS, { name: "projects" }, six);
    const filter = document.querySelector('input[type="search"]') as HTMLInputElement;
    await typeInto(filter, "needle");
    expect(listText()).toContain("p-e");
    expect(listText()).not.toContain("p-a");
    await typeInto(filter, "P-B");
    expect(listText()).toContain("p-b");
    expect(listText()).not.toContain("p-e");
  });

  describe("the folders a project's commands need", () => {
    const readBox = () => [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("settings.readPaths")))?.querySelector("textarea") as HTMLTextAreaElement;
    const checkBox = () => document.querySelector(`input[placeholder="${i18next.t("settings.checkCommandPlaceholder")}"]`) as HTMLInputElement;
    const offer = (folder: string) => [...main().querySelectorAll("button")].find((b) => b.textContent === folder) ?? null;

    it("offers what Keel found that the project doesn't read yet, and adds it to the list", async () => {
      setToolFolders([
        { folder: "~/.nvm", programs: ["node"] },
        { folder: "~/Library/pnpm", programs: ["pnpm"] },
      ]);
      await showApp(RUNS, { name: "projects" }, [project("alpha", { allowedRead: ["~/.nvm/"] })]);
      await settle(500);
      expect(callsTo("project_tool_folders").at(-1)?.args).toEqual({ projectId: "alpha", setup: "", check: "make test" });
      expect(main().textContent).toContain(i18next.t("settings.foundFolders"));
      expect(offer("~/.nvm")).toBeNull();
      await click(offer("~/Library/pnpm"));
      expect(readBox().value).toBe("~/.nvm/\n~/Library/pnpm");
      expect(offer("~/Library/pnpm")).toBeNull();
      expect(callsTo("project_update")).toEqual([]);

      await click(button(i18next.t("settings.save")));
      expect((callsTo("project_update")[0].args as { settings: Project }).settings.allowedRead).toEqual(["~/.nvm/", "~/Library/pnpm"]);
    });

    it("starts over when the setup dialog adds folders, so saving here doesn't take them back out", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha", { allowedRead: ["~/.nvm"] })]);
      act(() => useApp.setState({ overview: { ...useApp.getState().overview, projects: [project("alpha", { allowedRead: ["~/.nvm", "~/Library/pnpm"] })] } }));
      await settle();
      expect(readBox().value).toBe("~/.nvm\n~/Library/pnpm");
    });

    it("looks again when the commands change", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha")]);
      await settle(500);
      await typeInto(checkBox(), "pnpm test");
      await settle(500);
      expect(callsTo("project_tool_folders").at(-1)?.args).toEqual({ projectId: "alpha", setup: "", check: "pnpm test" });
      expect(main().textContent).not.toContain(i18next.t("settings.foundFolders"));
    });
  });

  it("refuses an env line without = and saves a valid one, saying so", async () => {
    await showApp(RUNS, { name: "projects" }, THREE);
    await typeInto(envBox(), "nope");
    await click(button(i18next.t("settings.save")));
    expect(main().textContent).toContain(i18next.t("settings.envInvalid", { line: "nope" }));
    expect(callsTo("project_update")).toEqual([]);
    await typeInto(envBox(), "A=1");
    await click(button(i18next.t("settings.save")));
    expect(callsTo("project_update")).toHaveLength(1);
    expect(main().textContent).toContain(i18next.t("settings.saved"));
  });

  it("says what the project's CI runs that its check doesn't, and only adds it when asked", async () => {
    setCiGap("alpha", { commands: ["uv run ruff check .", "uv run pyright"], from: [".github/workflows/ci.yml"] });
    await showApp(RUNS, { name: "projects" }, THREE);
    const checkBox = () => document.querySelector(`input[placeholder="${i18next.t("settings.checkCommandPlaceholder")}"]`) as HTMLInputElement;
    expect(callsTo("project_ci_gap").map((call) => call.args)).toEqual([{ projectId: "alpha" }]);
    expect(main().textContent).toContain(i18next.t("projects.ciMissing"));
    expect(main().textContent).toContain("uv run ruff check .");
    expect(main().textContent).toContain("uv run pyright");
    expect(checkBox().value).toBe("make test");

    await typeInto(checkBox(), "make test && uv run pyright");
    expect(main().textContent).toContain("uv run ruff check .");
    expect(main().textContent).not.toContain("uv run pyright");
    await click(button(i18next.t("projects.ciMissingAdd")));
    expect(checkBox().value).toBe("make test && uv run pyright && uv run ruff check .");
    expect(main().textContent).not.toContain(i18next.t("projects.ciMissing"));
    expect(callsTo("project_update")).toEqual([]);

    await click(projectButton("beta"));
    expect(main().textContent).not.toContain(i18next.t("projects.ciMissing"));
  });

  describe("the hint under a service's socket file", () => {
    const postgres = () => i18next.t("settings.serviceSocketHintPostgres");
    const general = () => i18next.t("settings.serviceSocketHint");
    const portBox = () => [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("settings.servicePort")))?.querySelector("input") as HTMLInputElement;

    it("shows no Postgres hint on a command service, only the general one", async () => {
      const services = [{ type: "command" as const, name: "cache", start: "redis-server", socket: null }];
      await showApp(RUNS, { name: "projects" }, [project("alpha", { services })]);
      expect(main().textContent).not.toContain("PGSQL");
      expect(main().textContent).toContain(general());
    });

    it("shows the Postgres hint only on a bridge to 5432, and follows the port as it is typed", async () => {
      const services = [{ type: "bridge" as const, name: "db", port: 6379, socket: null }];
      await showApp(RUNS, { name: "projects" }, [project("alpha", { services })]);
      expect(main().textContent).not.toContain(postgres());
      expect(main().textContent).toContain(general());
      await typeInto(portBox(), "5432");
      expect(main().textContent).toContain(postgres());
      expect(main().textContent).not.toContain(general());
      await typeInto(portBox(), "6379");
      expect(main().textContent).not.toContain(postgres());
      expect(main().textContent).toContain(general());
    });
  });

  it("drops the unsaved changes when another project is chosen", async () => {
    await showApp(RUNS, { name: "projects" }, THREE);
    await typeInto(envBox(), "A=1");
    await click(projectButton("beta"));
    await click(projectButton("alpha"));
    expect(envBox().value).toBe("");
  });

  it("selects the next project when the selected one is removed, and says when none is left", async () => {
    await showApp(RUNS, { name: "projects" }, [project("only")]);
    await click(button(i18next.t("remove.confirm")));
    const dialog = document.querySelector('[role="dialog"]') as HTMLElement;
    useApp.setState({ overview: { ...useApp.getState().overview, projects: [] } });
    await act(async () => [...dialog.querySelectorAll("button")].find((b) => b.textContent === i18next.t("remove.confirm"))?.click());
    await settle();
    expect(main().textContent).toContain(i18next.t("projects.none"));
  });

  it("moves to another project when the selected one disappears from the list", async () => {
    await showApp(RUNS, { name: "projects", projectId: "beta" }, THREE);
    await act(async () => useApp.setState({ overview: { ...useApp.getState().overview, projects: THREE.filter((p) => p.id !== "beta") } }));
    await settle();
    expect(heading()).toBe("alpha");
  });

  it("puts the settings of the app in Settings, and no project", async () => {
    await showApp(RUNS, { name: "settings" }, THREE);
    const text = main().textContent ?? "";
    expect(text).toContain(i18next.t("settings.language"));
    expect(text).toContain(i18next.t("settings.maxAgents"));
    expect(text).not.toContain("alpha");
    expect(text).not.toContain(i18next.t("projects.add"));
    expect(main().querySelector(".mx-auto")).not.toBeNull();
  });
});

describe("the ways to a project's page", () => {
  beforeEach(() => void vi.useFakeTimers());
  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  it("Home's project names lead to their page, and Finish setup still opens the assistant", async () => {
    await showApp(RUNS, { name: "home" }, THREE);
    const link = [...document.querySelectorAll("button")].find((b) => b.textContent === "gamma");
    await click(link);
    expect(view()).toEqual({ name: "projects", projectId: "gamma" });
    await hideApp();
    await showApp(RUNS, { name: "home" }, THREE);
    await click(button(i18next.t("projects.finishSetup")));
    expect(view().name).toBe("home");
    expect(useApp.getState().projectSetup === null).toBe(false);
    pressKey("Escape", "Escape");
  });
});

describe("what sends a person to fix a project", () => {
  const BLOCKED = { code: "githubAccountMissing" };
  const handler = (command: string, args: unknown) => {
    const { projectId } = (args ?? {}) as { projectId?: string };
    switch (command) {
      case "board_get":
        return { cards: [{ id: "c1", projectId, title: "Task", body: "", issue: null, column: "todo", position: 0, runId: null, runState: null, reason: null, prUrl: null, rejection: null, startBlocked: BLOCKED, createdAt: "2026-11-05T10:00:00Z" }], working: 0, maxAgents: 2, defaultWorkflow: null, paused: null };
      case "run_get":
        return { id: "r2", projectId: "p2", title: "Run r2", branch: "keel/abc", round: 0, workflow: [{ type: "agent", id: "builder" }] };
      case "run_attempts":
        return [{ id: "a1", runId: "r2", stepId: "builder", kind: "agent", role: "worker", tools: "build", state: "blocked", round: 0, number: 1, output: null, reason: BLOCKED, resultText: null, filesChanged: null, runFiles: null, startedAt: "2026-11-05T10:00:00Z" }];
      case "max_agents_get":
        return 2;
      case "github_status":
        return { repo: null, accounts: [], info: null, excessScopes: [], error: null };
      case "project_ci_gap":
        return { commands: [], from: [] };
      case "attempt_raw_check":
        return "intact";
      case "run_adds_secret":
        return false;
      case "run_diff":
        return "";
      default:
        return [];
    }
  };
  const open = i18next.t.bind(i18next);

  beforeEach(() => void vi.useFakeTimers());
  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  it("the board's button opens that project in Projects", async () => {
    const { mockIPC } = await import("@tauri-apps/api/mocks");
    await showApp(RUNS, { name: "home" }, THREE);
    mockIPC(handler, { shouldMockEvents: true });
    act(() => useApp.getState().navigate({ name: "board", projectId: "gamma" }));
    await settle();
    await click(button(open("newRun.openProject")));
    expect(view()).toEqual({ name: "projects", projectId: "gamma" });
  });

  it("a blocked run's button opens the run's project in Projects", async () => {
    const { mockIPC } = await import("@tauri-apps/api/mocks");
    const runs = [row("r2", "blocked", { projectId: "beta" })];
    await showApp(runs, { name: "home" }, THREE);
    mockIPC((command, args) => (command === "run_get" ? { ...(handler(command, args) as object), projectId: "beta" } : handler(command, args)), { shouldMockEvents: true });
    act(() => useApp.getState().navigate({ name: "run", runId: "r2" }));
    await settle();
    await click(button(open("newRun.openProject")));
    expect(view()).toEqual({ name: "projects", projectId: "beta" });
  });
});
