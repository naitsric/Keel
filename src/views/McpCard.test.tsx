// What the person lets each assistant do over MCP, client by client.
import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { McpClient, McpOverview, McpProjectChange } from "../lib/api";
import { McpCard } from "./McpCard";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const client = (fields: Partial<McpClient> = {}): McpClient => ({ id: "c1", name: "Claude Code", tokenPrefix: "keel_Ab3", canRun: true, canSteer: false, canManageProjects: false, maxOpenRuns: 3, createdAt: "2026-11-01T00:00:00Z", lastSeenAt: null, revokedAt: null, ...fields });

const change = (fields: Partial<McpProjectChange> = {}): McpProjectChange => ({
  seq: 1,
  clientId: "c2",
  clientName: "Bot",
  projectId: "p",
  projectName: "storefront",
  projectPath: "/Users/you/work/storefront",
  githubRepo: "acme/storefront",
  action: "configured",
  changes: [{ field: "checkCommand", before: "pnpm test", after: "pnpm test && pnpm lint" }],
  at: "2026-11-20T00:00:00Z",
  ...fields,
});

describe("the MCP card", () => {
  let root: Root;
  let clients: McpClient[];
  let projectChanges: McpProjectChange[];
  let running: boolean;
  let calls: { command: string; args: unknown }[];

  const overview = (): McpOverview => ({ settings: { enabled: running, port: 7413 }, status: { running, port: running ? 7413 : null, url: running ? "http://127.0.0.1:7413/mcp" : null, error: null }, clients, calls: [], projectChanges });
  const settle = () => act(async () => {});
  const open = async () => {
    await act(async () => root.render(<McpCard />));
    await settle();
  };
  const page = () => document.body.textContent ?? "";
  const checkbox = (text: string) => [...document.querySelectorAll("label")].find((label) => label.textContent?.includes(text))?.querySelector<HTMLInputElement>('input[type="checkbox"]') as HTMLInputElement;
  const feedback = () => checkbox("Can give feedback");
  const limit = () => [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Open runs at once"))?.querySelector("input") as HTMLInputElement;
  const type = (input: HTMLInputElement, text: string) =>
    act(() => {
      input.focus();
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, text);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
  const leave = async (input: HTMLInputElement) => {
    act(() => input.blur());
    await settle();
  };
  const sent = (command: string) => calls.filter((call) => call.command === command).map((call) => call.args);

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    clients = [client()];
    projectChanges = [];
    running = true;
    calls = [];
    mockIPC((command, args) => {
      calls.push({ command, args });
      if (command === "mcp_overview") return overview();
      if (command === "mcp_client_set_can_manage_projects") clients = clients.map((c) => ({ ...c, canManageProjects: (args as { canManageProjects: boolean }).canManageProjects }));
      if (command === "mcp_client_set_can_steer") clients = clients.map((c) => ({ ...c, canSteer: (args as { canSteer: boolean }).canSteer }));
      if (command === "mcp_client_set_max_open_runs") clients = clients.map((c) => ({ ...c, maxOpenRuns: (args as { max: number }).max }));
      return null;
    });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    document.body.replaceChildren();
  });

  it("says what an assistant can do if allowed, and what it never can", async () => {
    await open();
    expect(page()).toContain(
      "If you allow it, it can send work back, try a step again, close the runs it started, and add a note to their cards or put them back in Todo while they wait.",
    );
    expect(page()).toContain("It can't approve or reject a step, answer the questions a step asks, merge");
    // Closing its own runs comes with starting them, so it can let go of one it won't take further.
    expect(page()).toContain("Can start and stop runs, close the ones it started");
    // A note changes what a run is asked, so it's feedback.
    expect(page()).toContain("Can give feedback: send work back, try again and add notes to its cards");
  });

  describe("giving feedback", () => {
    it("is off until the person turns it on, and turning it on asks for just that", async () => {
      await open();
      expect(feedback().checked).toBe(false);
      expect(feedback().disabled).toBe(false);
      await act(async () => feedback().click());
      await settle();
      expect(sent("mcp_client_set_can_steer")).toEqual([{ id: "c1", canSteer: true }]);
      expect(sent("mcp_client_set_can_run")).toEqual([]);
      expect(feedback().checked).toBe(true);
    });

    it("can be turned off again", async () => {
      clients = [client({ canSteer: true })];
      await open();
      expect(feedback().checked).toBe(true);
      await act(async () => feedback().click());
      expect(sent("mcp_client_set_can_steer")).toEqual([{ id: "c1", canSteer: false }]);
    });

    it("can't be turned on for a client that can only read, and says why", async () => {
      clients = [client({ canRun: false, canSteer: true })];
      await open();
      expect(feedback().disabled).toBe(true);
      expect(feedback().checked).toBe(false);
      expect(page()).toContain("Turn on the one above first.");
      await act(async () => feedback().click());
      expect(sent("mcp_client_set_can_steer")).toEqual([]);
    });

    it("says nothing about it for a client that can run", async () => {
      await open();
      expect(page()).not.toContain("Turn on the one above first.");
    });
  });

  describe("adding and configuring projects", () => {
    const manage = () => checkbox("Can add and configure projects");

    it("is off until the person turns it on, says the commands run here, and asks for just that", async () => {
      await open();
      expect(manage().checked).toBe(false);
      expect(manage().disabled).toBe(false);
      expect(page()).toContain("setup and check commands, which run on this Mac");
      await act(async () => manage().click());
      await settle();
      expect(sent("mcp_client_set_can_manage_projects")).toEqual([{ id: "c1", canManageProjects: true }]);
      expect(sent("mcp_client_set_can_run")).toEqual([]);
      expect(sent("mcp_client_set_can_steer")).toEqual([]);
      expect(manage().checked).toBe(true);
    });

    it("can be turned off again", async () => {
      clients = [client({ canManageProjects: true })];
      await open();
      expect(manage().checked).toBe(true);
      await act(async () => manage().click());
      expect(sent("mcp_client_set_can_manage_projects")).toEqual([{ id: "c1", canManageProjects: false }]);
    });

    it("can't be turned on for a client that can only read", async () => {
      clients = [client({ canRun: false, canManageProjects: true })];
      await open();
      expect(manage().disabled).toBe(true);
      expect(manage().checked).toBe(false);
      await act(async () => manage().click());
      expect(sent("mcp_client_set_can_manage_projects")).toEqual([]);
    });
  });

  describe("what a client changed in a project", () => {
    it("shows who, in which project, and each value before and after", async () => {
      projectChanges = [change()];
      await open();
      expect(page()).toContain("Bot configured storefront");
      expect(page()).toContain("Check command");
      expect(page()).toContain("Before: pnpm test");
      expect(page()).toContain("After: pnpm test && pnpm lint");
    });

    it("says it added one, and when there was or is no value", async () => {
      projectChanges = [change({ action: "added", changes: [{ field: "githubAccount", before: null, after: "alice" }] }), change({ seq: 2, action: "restored", projectName: "api", changes: [{ field: "setupCommand", before: "make deps", after: null }] })];
      await open();
      expect(page()).toContain("Bot added storefront");
      expect(page()).toContain("GitHub account");
      expect(page()).toContain("Before: none");
      expect(page()).toContain("After: alice");
      expect(page()).toContain("Bot added api back");
      expect(page()).toContain("Setup command");
      expect(page()).toContain("After: none");
    });

    it("shows the domains before and after, one a line", async () => {
      projectChanges = [change({ changes: [{ field: "allowedDomains", before: "pypi.org", after: "pypi.org\nregistry.npmjs.org" }] })];
      await open();
      expect(page()).toContain("Allowed domains");
      expect(page()).toContain("Before: pypi.org");
      expect(page()).toContain("After: pypi.org\nregistry.npmjs.org");
    });

    it("names the folder and repo, which two projects with one name don't share", async () => {
      projectChanges = [change({ action: "added" }), change({ seq: 2, projectPath: "/Users/you/other/storefront", githubRepo: null })];
      await open();
      expect(page()).toContain("/Users/you/work/storefront · acme/storefront");
      expect(page()).toContain("/Users/you/other/storefront");
    });

    it("can be scrolled from the keyboard, and the calls below it have their own heading", async () => {
      projectChanges = [change()];
      await open();
      const list = document.querySelector<HTMLElement>("ul[aria-labelledby='mcp-project-changes']");
      expect(list?.tabIndex).toBe(0);
      expect(page()).toContain("Changes to projects");
      expect(page()).toContain("Calls");
    });

    it("is still there with the server off", async () => {
      running = false;
      projectChanges = [change()];
      await open();
      expect(page()).toContain("Activity");
      expect(page()).toContain("Bot configured storefront");
    });

    it("is said in Spanish too", async () => {
      await i18next.changeLanguage("es");
      projectChanges = [change()];
      await open();
      expect(page()).toContain("Bot configuró storefront");
      expect(page()).toContain("Antes: pnpm test");
      expect(page()).toContain("Después: pnpm test && pnpm lint");
      expect(page()).toContain("Puede agregar y configurar proyectos");
    });
  });

  describe("the runs it can keep open", () => {
    it("starts at what the client has", async () => {
      clients = [client({ maxOpenRuns: 5 })];
      await open();
      expect(limit().value).toBe("5");
    });

    it("saves the number once the person is done typing it", async () => {
      await open();
      type(limit(), "1");
      type(limit(), "10");
      expect(sent("mcp_client_set_max_open_runs")).toEqual([]);
      await leave(limit());
      expect(sent("mcp_client_set_max_open_runs")).toEqual([{ id: "c1", max: 10 }]);
      expect(limit().value).toBe("10");
    });

    it("saves on Enter too", async () => {
      await open();
      type(limit(), "7");
      await act(async () => void limit().dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
      expect(sent("mcp_client_set_max_open_runs")).toEqual([{ id: "c1", max: 7 }]);
    });

    it("keeps it between 1 and 10", async () => {
      await open();
      type(limit(), "25");
      await leave(limit());
      type(limit(), "0");
      await leave(limit());
      expect(sent("mcp_client_set_max_open_runs")).toEqual([
        { id: "c1", max: 10 },
        { id: "c1", max: 1 },
      ]);
    });

    it("takes only digits, and goes back to what it was when left empty or unchanged", async () => {
      await open();
      type(limit(), "abc");
      expect(limit().value).toBe("");
      await leave(limit());
      expect(limit().value).toBe("3");
      type(limit(), "3");
      await leave(limit());
      expect(sent("mcp_client_set_max_open_runs")).toEqual([]);
    });

    it("can't be changed for a client that can only read", async () => {
      clients = [client({ canRun: false })];
      await open();
      expect(limit().disabled).toBe(true);
    });
  });

  it("says it in Spanish too", async () => {
    await i18next.changeLanguage("es");
    await open();
    expect(page()).toContain("Puede lanzar y detener ejecuciones, cerrar las que lanzó");
    expect(page()).toContain("Puede dar feedback: devolver trabajo, reintentar y agregar notas a sus tarjetas");
    expect(page()).toContain("Ejecuciones abiertas a la vez");
    expect(page()).toContain("Si lo permites, puede devolver trabajo");
  });

  it("shows none of it for a client that was revoked", async () => {
    clients = [client({ revokedAt: "2026-11-02T00:00:00Z" })];
    await open();
    expect(feedback()).toBeUndefined();
    expect(limit()).toBeUndefined();
  });
});
