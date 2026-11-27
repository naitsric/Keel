import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { callsTo, hideApp, paletteRow, pressKey, pressMod, row, setCiGap, setIssueWatch, setProjectChanges, setToolFolders, settle, showApp, view } from "../commands/testApp";
import i18next from "../i18n";
import { api, type IssueWatch, type McpProjectChange, type Project, type ProjectSettings } from "../lib/api";
import { useApp } from "../lib/store";

const RUNS = [row("r1", "running")];
const project = (id: string, fields: Partial<Project> = {}): Project => ({ id, name: id, path: `/Users/you/${id}`, githubAccount: "you", githubRepo: `acme/${id}`, setupCommand: null, checkCommand: "make test", allowedDomains: [], allowedRead: [], services: [], env: [], defaultWorkflow: null, sensitivePaths: [], allowLocalhost: false, issueLabel: null, ...fields });
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

  describe("the page of one project", () => {
    const section = (id: string) => main().querySelector(`[data-section="${id}"]`) as HTMLElement;
    const index = () => main().querySelector(`nav[aria-label="${i18next.t("projects.sections")}"]`) as HTMLElement;

    it("has a card for each part, in the order of the index above them", async () => {
      await showApp(RUNS, { name: "projects" }, THREE);
      expect([...index().querySelectorAll("button")].map((b) => b.textContent)).toEqual(["Commands", "GitHub", "Workflow", "Access", "Services"]);
      expect([...main().querySelectorAll("[data-section]")].map((card) => card.getAttribute("data-section"))).toEqual(["commands", "github", "workflow", "access", "services"]);
      expect(section("commands").textContent).toContain(i18next.t("settings.setupCommand"));
      expect(section("commands").textContent).toContain(i18next.t("settings.checkCommand"));
      expect(section("github").textContent).toContain(i18next.t("settings.githubAccount"));
      expect(section("access").textContent).toContain(i18next.t("settings.domains"));
      expect(section("services").textContent).toContain(i18next.t("settings.env"));
    });

    it("keeps the files too sensitive for a small change with the default workflow, not with what the sandbox reaches", async () => {
      await showApp(RUNS, { name: "projects" }, THREE);
      expect(section("workflow").textContent).toContain(i18next.t("settings.defaultWorkflow"));
      expect(section("workflow").textContent).toContain(i18next.t("settings.sensitive"));
      expect(section("access").textContent).not.toContain(i18next.t("settings.sensitive"));
    });

    it("marks the first part in the index, and a click on another goes there and puts the focus in it", async () => {
      await showApp(RUNS, { name: "projects" }, THREE);
      const current = () => [...index().querySelectorAll('[aria-current="location"]')].map((b) => b.textContent);
      expect(current()).toEqual(["Commands"]);
      await click([...index().querySelectorAll("button")].find((b) => b.textContent === "Access"));
      expect(current()).toEqual(["Access"]);
      expect(document.activeElement).toBe(section("access"));
      // The list still says which project this is: the index marks a place, not a page.
      expect(main().querySelector('[aria-current="true"]')?.textContent).toContain("alpha");
    });

    it("says a project is set up beside its name, and nothing more", async () => {
      await showApp(RUNS, { name: "projects" }, THREE);
      const top = main().querySelector("h2")?.parentElement as HTMLElement;
      expect(top.textContent).toContain(i18next.t("projects.ready"));
      expect(button(i18next.t("projects.finishSetup"))).toBeUndefined();
      expect(main().textContent).not.toContain(i18next.t("projects.missing.check"));
    });

    it("says what a project's setup still lacks, and Finish setup opens the same dialog as on Home", async () => {
      await showApp(RUNS, { name: "projects", projectId: "beta" }, [project("alpha"), project("beta", { checkCommand: null, githubAccount: null })]);
      const top = main().querySelector("h2")?.parentElement as HTMLElement;
      expect(top.textContent).toContain(i18next.t("projects.needsSetup"));
      expect(main().textContent).toContain(i18next.t("projects.missing.check"));
      expect(main().textContent).toContain(i18next.t("projects.missing.account"));
      await click(button(i18next.t("projects.finishSetup")));
      expect(view().name).toBe("projects");
      expect(useApp.getState().projectSetup).toEqual({ projectId: "beta", added: null, returnToNewRun: false });
      pressKey("Escape", "Escape");
    });

    it("says only what's missing: a project with a check and no account chosen for its repo", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha", { githubAccount: null })]);
      expect(main().textContent).toContain(i18next.t("projects.missing.account"));
      expect(main().textContent).not.toContain(i18next.t("projects.missing.check"));
    });

    it("offers both ways to add a service where it says there are none, and below the list once there's one", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha")]);
      const empty = () => [...section("services").querySelectorAll("p")].find((p) => p.textContent === i18next.t("settings.servicesEmpty"));
      expect(empty()?.parentElement?.textContent).toContain(i18next.t("settings.addBridge"));
      expect(empty()?.parentElement?.textContent).toContain(i18next.t("settings.addCommand"));
      await click(button(i18next.t("settings.addBridge")));
      expect(empty()).toBeUndefined();
      expect(section("services").querySelectorAll("li")).toHaveLength(1);
      expect(button(i18next.t("settings.addCommand"))).toBeDefined();
      await click(button(i18next.t("settings.serviceRemove")));
      expect(empty()).toBeDefined();
    });

    it("says that Save saves every part, until there's an error to say instead", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha")]);
      expect(main().textContent).toContain(i18next.t("projects.savesAll"));
      await typeInto(envBox(), "nope");
      await click(button(i18next.t("settings.save")));
      expect(main().textContent).toContain(i18next.t("settings.envInvalid", { line: "nope" }));
      expect(main().textContent).not.toContain(i18next.t("projects.savesAll"));
    });
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

  describe("a change to the project while its form is open", () => {
    const START = project("alpha", { setupCommand: "make deps", checkCommand: "make test" });
    const input = (label: string) => [...main().querySelectorAll("label")].find((l) => l.textContent?.startsWith(label))?.querySelector("input") as HTMLInputElement;
    const setupBox = () => input(i18next.t("settings.setupCommand"));
    const checkBox = () => input(i18next.t("settings.checkCommand"));
    const notice = () => main().querySelector('[role="status"]')?.textContent ?? "";
    const saved = () => (callsTo("project_update").at(-1)?.args as { settings: Project }).settings;
    const record = (seq: number, at = new Date().toISOString()): McpProjectChange => ({ seq, clientId: "c1", clientName: "Bot", projectId: "alpha", projectName: "alpha", projectPath: "/Users/you/alpha", githubRepo: "acme/alpha", action: "configured", changes: [], at });
    const change = async (fields: Partial<Project>) => {
      act(() => useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ ...useApp.getState().overview.projects[0], ...fields }] } }));
      await settle();
    };

    it("keeps what the person hadn't saved, takes a client's values in the rest, and says a client changed it", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(envBox(), "A=1");
      await typeInto(setupBox(), "make mine");
      expect(notice()).toBe("");

      setProjectChanges([record(1)]);
      await change({ githubAccount: "bot-account", setupCommand: "make theirs", checkCommand: "make check" });

      expect(envBox().value).toBe("A=1");
      expect(setupBox().value).toBe("make mine");
      expect(checkBox().value).toBe("make check");
      expect(notice()).toContain(i18next.t("projects.clientChanged"));
      expect(notice()).toContain(i18next.t("projects.changedKept"));
      // What's saved now, also for the field that still shows what the person typed.
      for (const value of ["bot-account", "make theirs", "make check"]) expect(notice()).toContain(value);
      // The CI hint is about the saved check.
      expect(callsTo("project_ci_gap")).toHaveLength(2);
      expect(callsTo("project_update")).toEqual([]);

      await click(button(i18next.t("settings.save")));
      expect(saved()).toMatchObject({ githubAccount: "bot-account", setupCommand: "make mine", checkCommand: "make check", env: [{ name: "A", value: "1" }] });
      expect(notice()).toBe("");
    });

    const domainsBox = () => [...main().querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("settings.domains")))?.querySelector("textarea") as HTMLTextAreaElement;

    it("takes the domains a client changed, so saving doesn't write over them", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      setProjectChanges([{ ...record(1), changes: [{ field: "allowedDomains", before: null, after: "registry.npmjs.org\npypi.org" }] }]);
      await change({ allowedDomains: ["registry.npmjs.org", "pypi.org"] });
      expect(domainsBox().value).toBe("registry.npmjs.org\npypi.org");
      expect(notice()).toContain(i18next.t("projects.clientChanged"));
      expect(notice()).toContain(i18next.t("mcp.field.allowedDomains"));
      expect(notice()).toContain("registry.npmjs.org");

      await click(button(i18next.t("settings.save")));
      expect(saved().allowedDomains).toEqual(["registry.npmjs.org", "pypi.org"]);
    });

    it("keeps the domains the person was editing, and says what a client saved", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(domainsBox(), "mine.example");
      setProjectChanges([record(1)]);
      await change({ allowedDomains: ["theirs.example"] });
      expect(domainsBox().value).toBe("mine.example");
      expect(notice()).toContain("theirs.example");
      expect(notice()).toContain(i18next.t("projects.yoursKept"));
    });

    // Keel keeps, as it saves, any domain the project has that isn't among those the form saw.
    const seen = () => (callsTo("project_update").at(-1)?.args as { settings: ProjectSettings }).settings.domainsSeen;

    it("doesn't count as seen a domain the person allowed from a waiting step while they edited the box", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(domainsBox(), "mine.example");
      // Allowed from a run, which is on no client's record.
      await change({ allowedDomains: ["registry.npmjs.org"] });
      expect(domainsBox().value).toBe("mine.example");

      await click(button(i18next.t("settings.save")));
      expect(saved().allowedDomains).toEqual(["mine.example"]);
      expect(seen()).toEqual([]);
    });

    it("says it saw the domains the box was filled with, so the person can take one away", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await change({ allowedDomains: ["registry.npmjs.org"] });
      expect(domainsBox().value).toBe("registry.npmjs.org");
      await typeInto(domainsBox(), "");
      await click(button(i18next.t("settings.save")));
      expect(saved().allowedDomains).toEqual([]);
      expect(seen()).toEqual(["registry.npmjs.org"]);
    });

    it("counts as seen what a client added, once the person has been told and saves their own", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(domainsBox(), "mine.example");
      setProjectChanges([{ ...record(1), changes: [{ field: "allowedDomains", before: null, after: "theirs.example" }] }]);
      await change({ allowedDomains: ["theirs.example"] });
      await click(button(i18next.t("settings.save")));
      expect(saved().allowedDomains).toEqual(["mine.example"]);
      expect(seen()).toEqual(["theirs.example"]);
    });

    it("doesn't count as seen one a client added and took away again", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(domainsBox(), "mine.example");
      setProjectChanges([{ ...record(1), changes: [{ field: "allowedDomains", before: null, after: "theirs.example" }] }]);
      await change({ allowedDomains: ["theirs.example"] });
      setProjectChanges([
        { ...record(2), changes: [{ field: "allowedDomains", before: "theirs.example", after: null }] },
        { ...record(1), changes: [{ field: "allowedDomains", before: null, after: "theirs.example" }] },
      ]);
      await change({ allowedDomains: [] });
      // The person allows it from a run: it's theirs now, and saving mustn't take it away.
      await change({ allowedDomains: ["theirs.example"] });
      await click(button(i18next.t("settings.save")));
      expect(seen()).toEqual([]);
    });

    it("says in both languages that domains can be why nothing was saved", () => {
      expect(i18next.t("projects.notSaved", { lng: "en" })).toContain("domains");
      expect(i18next.t("projects.notSaved", { lng: "es" })).toContain("dominios");
    });

    it("doesn't save over a change it hasn't heard of yet, and says nothing was saved", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      // Saved by a client and on record, and the app not told yet.
      setProjectChanges([record(1)]);
      const fresh = { ...useApp.getState().overview, projects: [{ ...START, checkCommand: "make check" }] };
      vi.spyOn(api, "overview").mockResolvedValueOnce(fresh);
      await click(button(i18next.t("settings.save")));
      expect(callsTo("project_update")).toEqual([]);
      expect(checkBox().value).toBe("make check");
      expect(main().textContent).toContain(i18next.t("projects.notSaved"));

      await click(button(i18next.t("settings.save")));
      expect(saved().checkCommand).toBe("make check");
    });

    it("saves on the second try when a client changed a value and put it back before the app heard", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(envBox(), "A=1");
      setProjectChanges([record(2), record(1)]);
      await click(button(i18next.t("settings.save")));
      expect(callsTo("project_update")).toEqual([]);
      expect(main().textContent).toContain(i18next.t("projects.notSaved"));
      // No field differs from what the form had, so none is marked.
      expect(notice()).toBe("");

      await click(button(i18next.t("settings.save")));
      expect(callsTo("project_update")).toHaveLength(1);
      expect(saved()).toMatchObject({ setupCommand: "make deps", checkCommand: "make test", env: [{ name: "A", value: "1" }] });
    });

    it("doesn't count as told a change on record that the form doesn't hold yet", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      // The app heard of the first change, and the record already has a second one.
      setProjectChanges([
        { ...record(2), changes: [{ field: "checkCommand", before: "make check", after: "make other" }] },
        { ...record(1), changes: [{ field: "checkCommand", before: "make test", after: "make check" }] },
      ]);
      await change({ checkCommand: "make check" });
      await click(button(i18next.t("settings.save")));
      expect(callsTo("project_update")).toEqual([]);
      expect(main().textContent).toContain(i18next.t("projects.notSaved"));
    });

    it("looks for CI again when the setup command changes", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      const before = callsTo("project_ci_gap").length;
      await change({ setupCommand: "make other-deps" });
      expect(callsTo("project_ci_gap")).toHaveLength(before + 1);
    });

    it("says the fields already show a change when the person had edited none of them", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      setProjectChanges([record(1)]);
      await change({ checkCommand: "make check" });
      expect(notice()).toContain(i18next.t("projects.changedTaken"));
      expect(notice()).not.toContain(i18next.t("projects.changedKept"));
      expect(notice()).not.toContain(i18next.t("projects.yoursKept"));
    });

    it("stays on the project shown when a client adds one ahead of it", async () => {
      await showApp(RUNS, { name: "projects" }, [project("beta")]);
      await typeInto(envBox(), "A=1");
      act(() => useApp.setState({ overview: { ...useApp.getState().overview, projects: [project("aardvark"), ...useApp.getState().overview.projects] } }));
      await settle();
      expect(envBox().value).toBe("A=1");
      expect(main().querySelector('[aria-current="true"]')?.textContent).toContain("beta");
    });

    it("takes a command the client removed", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      setProjectChanges([record(1)]);
      await change({ setupCommand: null });
      expect(setupBox().value).toBe("");
      expect(notice()).toContain(i18next.t("mcp.noValue"));
    });

    it("says nothing of a client when none made the change", async () => {
      // One from before the form opened says nothing about what changes now.
      setProjectChanges([record(1, "2026-01-01T00:00:00.000Z")]);
      await showApp(RUNS, { name: "projects" }, [START]);
      await typeInto(envBox(), "A=1");
      await change({ checkCommand: "make check" });
      expect(checkBox().value).toBe("make check");
      expect(envBox().value).toBe("A=1");
      expect(notice()).toBe("");
    });

    it("doesn't announce a client's change again when something else changes the project", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      setProjectChanges([record(1)]);
      await change({ checkCommand: "make check" });
      await click(button(i18next.t("settings.save")));
      await change({ checkCommand: "make other" });
      expect(checkBox().value).toBe("make other");
      expect(notice()).toBe("");
    });

    it("shows what was saved once the person saves, as Keel wrote it", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      const write = useApp.subscribe((state) => {
        const [now] = state.overview.projects;
        if (now.checkCommand !== now.checkCommand?.trim()) useApp.setState({ overview: { ...state.overview, projects: [{ ...now, checkCommand: now.checkCommand?.trim() ?? null }] } });
      });
      await typeInto(checkBox(), "make all  ");
      await click(button(i18next.t("settings.save")));
      write();
      expect(checkBox().value).toBe("make all");
    });

    it("still starts over when the setup dialog opens localhost, so saving here doesn't close it", async () => {
      await showApp(RUNS, { name: "projects" }, [START]);
      await change({ allowLocalhost: true });
      await click(button(i18next.t("settings.save")));
      expect(saved().allowLocalhost).toBe(true);
    });

    it("has its words in both languages", () => {
      for (const key of ["projects.clientChanged", "projects.changedKept", "setup.changed"]) {
        for (const lng of ["en", "es"]) expect(i18next.exists(key, { lng, fallbackLng: [] }), `${lng} ${key}`).toBe(true);
      }
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
    setCiGap("alpha", { commands: ["uv run --frozen ruff check .", "uv run --frozen pyright"], leftOut: [{ command: "semgrep ci", why: "offline" }], from: [".github/workflows/ci.yml"] });
    await showApp(RUNS, { name: "projects" }, THREE);
    const checkBox = () => document.querySelector(`input[placeholder="${i18next.t("settings.checkCommandPlaceholder")}"]`) as HTMLInputElement;
    expect(callsTo("project_ci_gap").map((call) => call.args)).toEqual([{ projectId: "alpha" }]);
    expect(main().textContent).toContain(i18next.t("projects.ciMissing"));
    expect(main().textContent).toContain("uv run --frozen ruff check .");
    expect(main().textContent).toContain("uv run --frozen pyright");
    expect(main().textContent).toContain(i18next.t("projects.ciLeftOut"));
    expect(main().textContent).toContain("semgrep ci");
    expect(main().textContent).toContain(i18next.t("projects.pyrightNode"));
    expect(checkBox().value).toBe("make test");

    await typeInto(checkBox(), "make test && uv run --frozen pyright");
    expect(main().textContent).toContain("uv run --frozen ruff check .");
    expect(main().textContent).not.toContain("uv run --frozen pyright");
    await click(button(i18next.t("projects.ciMissingAdd")));
    // Only what Keel can run: what it can't stays a note.
    expect(checkBox().value).toBe("make test && uv run --frozen pyright && uv run --frozen ruff check .");
    expect(main().textContent).not.toContain(i18next.t("projects.ciMissing"));
    expect(button(i18next.t("projects.ciMissingAdd"))).toBeUndefined();
    expect(main().textContent).toContain(i18next.t("projects.ciLeftOut"));
    expect(callsTo("project_update")).toEqual([]);

    await click(projectButton("beta"));
    expect(main().textContent).not.toContain(i18next.t("projects.ciMissing"));
    expect(main().textContent).not.toContain(i18next.t("projects.ciLeftOut"));
  });

  it("says why each command of the CI is left out, and never offers to add it", async () => {
    const why = ["offline", "mayFail", "env", "sometimes", "installs", "template"] as const;
    setCiGap("alpha", { commands: [], leftOut: why.map((reason) => ({ command: `run-${reason}`, why: reason })), from: [".github/workflows/ci.yml"] });
    await showApp(RUNS, { name: "projects" }, THREE);
    expect(main().textContent).toContain(i18next.t("projects.ciLeftOut"));
    for (const reason of why) {
      const row = [...main().querySelectorAll("li")].find((item) => item.textContent?.includes(`run-${reason}`));
      expect(row?.textContent).toContain(i18next.t(`projects.ciWhy.${reason}`));
    }
    expect(button(i18next.t("projects.ciMissingAdd"))).toBeUndefined();
  });

  describe("watching a project's issues", () => {
    const field = () => [...main().querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("projects.issueLabel")))?.querySelector("input") as HTMLInputElement | undefined;
    const saved = () => (callsTo("project_update").at(-1)?.args as { settings: Project }).settings;
    const WATCH: IssueWatch = { label: "keel", at: new Date().toISOString(), open: 3, brought: 2, removed: 0, problem: null, commentFailed: null };

    it("saves the label, looks right away and says what it brought", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha")]);
      expect(field()?.value).toBe("");
      expect(main().textContent).toContain(i18next.t("projects.issueLabelHint"));
      setIssueWatch(WATCH, { stored: false });
      await typeInto(field() as HTMLInputElement, "  keel ");
      await click(button(i18next.t("settings.save")));
      expect(saved().issueLabel).toBe("keel");
      expect(callsTo("issues_look")).toHaveLength(1);
      expect(main().textContent).toContain(i18next.t("projects.issueWatch.brought", { count: 2 }));
    });

    it("still looks and shows it when the same save makes the form start over", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha")]);
      setIssueWatch(WATCH, { stored: false });
      const read = [...main().querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t("settings.readPaths")))?.querySelector("textarea") as HTMLTextAreaElement;
      await typeInto(read, "~/.nvm");
      await typeInto(field() as HTMLInputElement, "keel");
      await click(button(i18next.t("settings.save")));
      expect(saved().allowedRead).toEqual(["~/.nvm"]);
      expect(field()?.value).toBe("keel");
      expect(callsTo("issues_look")).toHaveLength(1);
      expect(main().textContent).toContain(i18next.t("projects.issueWatch.brought", { count: 2 }));
    });

    it("looks on opening a watching project the background hasn't looked at yet", async () => {
      setIssueWatch(WATCH, { stored: false });
      await showApp(RUNS, { name: "projects" }, [project("alpha", { issueLabel: "keel" })]);
      expect(callsTo("issues_look")).toHaveLength(1);
      expect(main().textContent).toContain(i18next.t("projects.issueWatch.brought", { count: 2 }));
    });

    it("stops watching when the label is emptied, and doesn't look", async () => {
      setIssueWatch(WATCH);
      await showApp(RUNS, { name: "projects" }, [project("alpha", { issueLabel: "keel" })]);
      expect(field()?.value).toBe("keel");
      await typeInto(field() as HTMLInputElement, "");
      await click(button(i18next.t("settings.save")));
      expect(saved().issueLabel).toBeNull();
      expect(callsTo("issues_look")).toEqual([]);
      expect(main().textContent).not.toContain(i18next.t("projects.issueWatch.brought", { count: 2 }));
    });

    it("says when the repo has no such label, or a comment on an issue failed", async () => {
      setIssueWatch({ ...WATCH, brought: 0, problem: { kind: "labelMissing" }, commentFailed: { number: 7, detail: "HTTP 403" } });
      await showApp(RUNS, { name: "projects" }, [project("alpha", { issueLabel: "keel" })]);
      expect(main().textContent).toContain(i18next.t("projects.issueWatch.labelMissing", { label: "keel" }));
      expect(main().textContent).toContain(i18next.t("projects.issueWatch.commentFailed", { number: 7, detail: "HTTP 403" }));
    });

    it("isn't offered for a project that isn't on GitHub", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha", { githubRepo: null })]);
      expect(field()).toBeUndefined();
    });
  });

  describe("localhost for a project's tests", () => {
    const box = () => [...main().querySelectorAll("label")].find((l) => l.textContent?.includes(i18next.t("settings.localhost")))?.querySelector<HTMLInputElement>('input[type="checkbox"]') as HTMLInputElement;
    const saved = () => (callsTo("project_update").at(-1)?.args as { settings: Project }).settings;

    it("is closed unless the project opened it, says what opening it risks, and saves what's chosen", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha")]);
      expect(box().checked).toBe(false);
      expect(main().textContent).toContain(i18next.t("settings.localhostHint"));
      await click(box());
      expect(box().checked).toBe(true);
      expect(callsTo("project_update")).toEqual([]);
      await click(button(i18next.t("settings.save")));
      expect(saved().allowLocalhost).toBe(true);
      expect(saved().allowedRead).toEqual([]);
    });

    it("closes again", async () => {
      await showApp(RUNS, { name: "projects" }, [project("alpha", { allowLocalhost: true })]);
      expect(box().checked).toBe(true);
      await click(box());
      await click(button(i18next.t("settings.save")));
      expect(saved().allowLocalhost).toBe(false);
    });

    it("tells the risk in plain words, in both languages", () => {
      for (const lng of ["en", "es"]) {
        const hint = i18next.t("settings.localhostHint", { lng });
        expect(hint).toMatch(lng === "en" ? /any service running on this Mac/ : /cualquier servicio que corra en este Mac/);
        expect(hint).toMatch(lng === "en" ? /local network/ : /red local/);
        expect(hint).not.toMatch(/Seatbelt|sandbox-exec|allowLocalBinding|0\.0\.0\.0|interface/i);
      }
    });
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
        return { commands: [], leftOut: [], from: [] };
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

  it("a check whose tests were refused localhost says so and opens the run's project", async () => {
    const { mockIPC } = await import("@tauri-apps/api/mocks");
    const refused = { code: "checkFailed", exitCode: 1, localhostRefused: true };
    const runs = [row("r2", "failed", { projectId: "beta" })];
    await showApp(runs, { name: "home" }, THREE);
    mockIPC(
      (command, args) => {
        if (command === "run_get") return { ...(handler(command, args) as object), projectId: "beta", workflow: [{ type: "check", id: "check", run: "npm test" }] };
        if (command === "run_attempts") return [{ id: "a1", runId: "r2", stepId: "check", kind: "check", role: "worker", tools: "check", prompt: "npm test", state: "failed", round: 0, number: 1, output: null, reason: refused, exitCode: 1, resultText: null, filesChanged: null, runFiles: null, startedAt: "2026-11-05T10:00:00Z" }];
        return handler(command, args);
      },
      { shouldMockEvents: true },
    );
    act(() => useApp.getState().navigate({ name: "run", runId: "r2" }));
    await settle();
    expect(main().textContent).toContain(i18next.t("settings.localhost"));
    await click(button(open("newRun.openProject")));
    expect(view()).toEqual({ name: "projects", projectId: "beta" });
  });
});
