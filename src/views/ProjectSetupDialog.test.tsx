import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { Attempt, GithubStatus, Guess, Project, ProjectAdded } from "../lib/api";
import { useApp } from "../lib/store";
import { ProjectSetupDialog } from "./ProjectSetupDialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const PROJECT: Project = {
  id: "p1",
  name: "demo",
  path: "/Users/you/www/demo",
  githubAccount: null,
  githubRepo: "acme/app",
  setupCommand: null,
  checkCommand: null,
  allowedDomains: ["pypi.org"],
  allowedRead: ["~/.nvm"],
  services: [{ type: "bridge", name: "db", port: 5432, socket: ".s.PGSQL.5432" }],
  env: [{ name: "DB_HOST", value: "{{socketDir:db}}" }],
  defaultWorkflow: "mine",
};
const NO_GUESS: Guess = { check: null, setup: null, from: [], setupOptions: [], checkOptions: [] };
const PNPM: Guess = { check: "pnpm test", setup: "pnpm install --frozen-lockfile", from: ["package.json", "pnpm-lock.yaml"], setupOptions: [], checkOptions: ["pnpm run lint", "pnpm test"] };
const UV: Guess = { check: "uv run --frozen pytest", setup: "uv sync --frozen --group test", from: ["uv.lock", "pyproject.toml"], setupOptions: ["--group stubs", "--group test"], checkOptions: [] };
const ACCOUNT = (login: string) => ({ login, active: false, ok: true, scopes: ["repo"] });

interface Scene {
  project?: Project;
  guess?: Guess;
  /** Each account and its permission in the repo. */
  accounts?: Record<string, "ADMIN" | "WRITE" | "READ">;
  gh?: "missing";
  added?: ProjectAdded | null;
  attempts?: Partial<Attempt>[];
  events?: unknown[];
}

describe("the setup dialog", () => {
  let root: Root;
  let calls: { command: string; args: Record<string, unknown> }[];

  const settle = () => act(async () => {
    for (let i = 0; i < 4; i++) await new Promise((resolve) => setTimeout(resolve));
  });

  const show = async ({ project = PROJECT, guess = NO_GUESS, accounts = {}, gh, added = null, attempts = [], events = [] }: Scene) => {
    const logins = Object.keys(accounts);
    mockIPC((command, payload) => {
      const args = (payload ?? {}) as Record<string, unknown>;
      calls.push({ command, args });
      switch (command) {
        case "project_setup":
          return { project, guess };
        case "overview":
          return { ...useApp.getState().overview, projects: [project] };
        case "github_status": {
          const status: GithubStatus = { repo: project.githubRepo, accounts: gh ? [] : logins.map(ACCOUNT), info: null, excessScopes: [], error: gh ? { code: "ghMissing" } : null };
          const login = args.account as string | null;
          if (login) status.info = { defaultBranch: "main", permission: accounts[login], private: true, defaultBranchProtected: true };
          return status;
        }
        case "project_update":
          return project;
        case "project_try_check":
          return "r1";
        case "run_attempts":
          return attempts;
        case "attempt_events":
          return events;
        default:
          return null;
      }
    });
    useApp.setState({
      overview: { ...useApp.getState().overview, projects: [project] },
      projectSetup: { projectId: project.id, added, returnToNewRun: false },
    });
    await act(async () => root.render(<ProjectSetupDialog />));
    await settle();
  };

  const page = () => document.body.textContent ?? "";
  const called = (command: string) => calls.filter((call) => call.command === command);
  const button = (name: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === name) as HTMLButtonElement;
  const click = async (name: string) => {
    await act(async () => button(name).click());
    await settle();
  };
  const select = () => document.querySelector("select") as HTMLSelectElement;
  const inputs = () => [...document.querySelectorAll("input")] as HTMLInputElement[];
  const type = async (input: HTMLInputElement, value: string) => {
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
  };
  const choose = async (value: string) => {
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")?.set?.call(select(), value);
      select().dispatchEvent(new Event("change", { bubbles: true }));
    });
  };

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    calls = [];
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ projectSetup: null, newRunOpen: false });
  });

  describe("choosing the account", () => {
    it("preselects the only one that can push", async () => {
      await show({ accounts: { "you-work": "WRITE", "you-other": "READ" } });
      expect(select().value).toBe("you-work");
      expect(page()).toContain("can push");
    });

    it("chooses nobody when several can push, and Save asks for one", async () => {
      await show({ accounts: { "you-work": "WRITE", "you-admin": "ADMIN" } });
      expect(select().value).toBe("__none__");
      expect(page()).toContain(i18next.t("setup.several", { repo: "acme/app" }));

      await click("Save");
      expect(called("project_update")).toEqual([]);
      expect(document.querySelector('[role="alert"]')?.textContent).toBe(i18next.t("reasons.githubAccountMissing"));
      expect(useApp.getState().projectSetup).not.toBeNull();

      await choose("you-admin");
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).githubAccount).toBe("you-admin");
      expect(useApp.getState().projectSetup).toBeNull();
    });

    it("keeps an account already saved", async () => {
      await show({ project: { ...PROJECT, githubAccount: "you-admin" }, accounts: { "you-work": "WRITE", "you-admin": "ADMIN" } });
      expect(select().value).toBe("you-admin");
    });

    it("says so when none can push, and still lets the commands be saved", async () => {
      await show({ accounts: { "you-other": "READ" }, guess: PNPM });
      expect(select().value).toBe("__none__");
      expect(page()).toContain(i18next.t("setup.noPusher", { repo: "acme/app" }));
      expect(button(i18next.t("settings.githubGiveAccess"))).toBeDefined();
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).checkCommand).toBe("pnpm test");
    });

    it("explains an account that can't push once it's chosen, with the way to fix it", async () => {
      await show({ accounts: { "you-other": "READ", "you-work": "WRITE" } });
      await choose("you-other");
      expect(page()).toContain("can only read");
      expect(page()).toContain(i18next.t("settings.githubReadOnly", { account: "you-other" }));
      expect(button(i18next.t("settings.githubGiveAccess"))).toBeDefined();
      expect(button(i18next.t("settings.githubAcceptInvite"))).toBeDefined();
    });

    it("explains no gh, with where to get it", async () => {
      await show({ gh: "missing" });
      expect(page()).toContain(i18next.t("setup.noGh"));
      expect(select()).toBeNull();
      expect(button(i18next.t("onboarding.install"))).toBeDefined();
    });

    it("explains no account, with the way to connect one", async () => {
      await show({ accounts: {} });
      expect(page()).toContain(i18next.t("setup.noAccount"));
      expect(select()).toBeNull();
      expect(button(i18next.t("settings.githubConnect"))).toBeDefined();
    });

    it("explains no remote, and has no account to ask for", async () => {
      await show({ project: { ...PROJECT, githubRepo: null }, accounts: { "you-work": "WRITE" }, guess: PNPM });
      expect(page()).toContain(i18next.t("setup.noRemote"));
      expect(select()).toBeNull();
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).githubAccount).toBeNull();
    });

    it("recognizes a repo published since it was added", async () => {
      await show({ project: { ...PROJECT, githubRepo: "acme/published" }, accounts: { "you-work": "WRITE" } });
      expect(page()).toContain("~/www/demo · acme/published");
      expect(calls.findIndex((c) => c.command === "project_setup")).toBeLessThan(calls.findIndex((c) => c.command === "github_status"));
    });
  });

  describe("the guess", () => {
    it("is labelled as one, fills the fields and saves nothing until Save", async () => {
      await show({ guess: PNPM, accounts: { "you-work": "WRITE" } });
      expect(inputs().map((input) => input.value)).toEqual(["pnpm test", "pnpm install --frozen-lockfile"]);
      expect(page()).toContain("Guessed from package.json, pnpm-lock.yaml.");
      expect(called("project_update")).toEqual([]);
    });

    it("leaves the fields empty for an unknown stack, and says so", async () => {
      await show({ accounts: { "you-work": "WRITE" } });
      expect(inputs().map((input) => input.value)).toEqual(["", ""]);
      expect(page()).toContain(i18next.t("setup.noGuess"));
      expect(page()).not.toContain("Guessed from");
    });

    it("never replaces what's saved", async () => {
      await show({ project: { ...PROJECT, checkCommand: "make test", setupCommand: "make deps" }, guess: PNPM });
      expect(inputs().map((input) => input.value)).toEqual(["make test", "make deps"]);
      expect(page()).not.toContain("Guessed from");
    });

    it("stops calling it a guess once it's been changed", async () => {
      await show({ guess: { ...PNPM, setup: null } });
      expect(page()).toContain("Guessed from");
      await type(inputs()[0], "pnpm test:unit");
      expect(page()).not.toContain("Guessed from");
    });
  });

  describe("what the repo offers to add", () => {
    const option = (text: string) => [...document.querySelectorAll("button")].find((button) => button.textContent === text);

    it("offers only what the command doesn't have yet, and adds what's chosen", async () => {
      await show({ guess: UV });
      expect(option("--group test")).toBeUndefined();
      await act(async () => option("--group stubs")!.click());
      expect(inputs()[1].value).toBe("uv sync --frozen --group test --group stubs");
      expect(option("--group stubs")).toBeUndefined();
    });

    it("offers a saved project the group it's missing and changes nothing until it's chosen", async () => {
      await show({ project: { ...PROJECT, checkCommand: "uv run --frozen pytest", setupCommand: "uv sync --frozen" }, guess: UV });
      expect(inputs()[1].value).toBe("uv sync --frozen");
      await act(async () => option("--group test")!.click());
      expect(inputs()[1].value).toBe("uv sync --frozen --group test");
    });

    it("offers no flags for a setup command that isn't uv's", async () => {
      await show({ project: { ...PROJECT, setupCommand: "make deps" }, guess: UV });
      expect(option("--group stubs")).toBeUndefined();
    });

    it("makes a check that was added have to pass too", async () => {
      await show({ guess: PNPM });
      expect(option("pnpm test")).toBeUndefined();
      await act(async () => option("pnpm run lint")!.click());
      expect(inputs()[0].value).toBe("pnpm test && pnpm run lint");
    });

    it("adds nothing to a project with nothing to offer", async () => {
      await show({ guess: NO_GUESS });
      expect(page()).not.toContain("You can also add");
    });

    it("forgets a trial, since it only speaks for the commands it ran", async () => {
      await show({ guess: PNPM, attempts: [{ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null }] });
      await act(async () => button("Try it")!.click());
      await settle();
      expect(page()).toContain("It passed");
      await act(async () => option("pnpm run lint")!.click());
      expect(page()).not.toContain("It passed");
    });
  });

  describe("saving", () => {
    it("keeps every other field of the project", async () => {
      await show({ guess: PNPM, accounts: { "you-work": "WRITE" } });
      await click("Save");
      const [{ args }] = called("project_update");
      expect(args.projectId).toBe("p1");
      expect(args.settings).toEqual({ ...PROJECT, githubAccount: "you-work", checkCommand: "pnpm test", setupCommand: "pnpm install --frozen-lockfile" });
    });

    it("starts from the project as it is by then, not as it was when this opened", async () => {
      await show({ accounts: { "you-work": "WRITE" } });
      useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ ...PROJECT, allowedDomains: ["changed.example"] }] } });
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowedDomains).toEqual(["changed.example"]);
    });

    it("says why when it can't save, and stays open", async () => {
      await show({ accounts: { "you-work": "WRITE" } });
      mockIPC((command) => {
        if (command === "project_update") throw { code: "blocked", reason: { code: "invalidSetting", field: "x", value: "y" }, detail: null };
        return null;
      });
      await click("Save");
      expect(document.querySelector('[role="alert"]')?.textContent).toContain("y");
      expect(useApp.getState().projectSetup).not.toBeNull();
    });
  });

  describe("trying it", () => {
    const check = (fields: Partial<Attempt>): Partial<Attempt> => ({ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null, ...fields });

    it("runs what's on screen, saves nothing, and shows the answer with its output", async () => {
      await show({ guess: PNPM, attempts: [check({})], events: [{ seq: 1, attemptId: "a", at: "", kind: "checkResult", data: { stdout: "all good", stderr: "" } }] });
      await type(inputs()[0], "pnpm test:unit");
      await click("Try it");
      expect(called("project_try_check")[0].args).toEqual({ projectId: "p1", setup: "pnpm install --frozen-lockfile", check: "pnpm test:unit", language: "en" });
      expect(called("project_update")).toEqual([]);
      expect(page()).toContain(i18next.t("setup.passed"));

      await click("See the output");
      expect(page()).toContain("all good");
      await click("Hide the output");
      expect(page()).not.toContain("all good");
    });

    it("says how it failed", async () => {
      await show({ guess: PNPM, attempts: [check({ state: "failed", exitCode: 1, reason: { code: "checkFailed", exitCode: 1 } })] });
      await click("Try it");
      expect(page()).toContain("The check failed with code 1.");
      expect(page()).not.toContain(i18next.t("setup.notFound"));
    });

    it("says which step failed when it's the setup", async () => {
      await show({ guess: PNPM, attempts: [check({ kind: "setup", state: "failed", exitCode: 3, reason: { code: "setupFailed", exitCode: 3 } })] });
      await click("Try it");
      expect(page()).toContain("Preparing the project failed with code 3.");
    });

    it("explains a command the sandbox can't find", async () => {
      await show({ guess: PNPM, attempts: [check({ state: "failed", exitCode: 127, reason: { code: "checkFailed", exitCode: 127 } })] });
      await click("Try it");
      expect(page()).toContain(i18next.t("setup.notFound"));
    });

    it("shows a step that's still going", async () => {
      await show({ guess: PNPM, attempts: [check({ kind: "setup", state: "running" })] });
      await click("Try it");
      expect(page()).toContain(i18next.t("setup.runningSetup"));
      expect(button("Try it").disabled).toBe(true);
      expect(called("project_try_check")).toHaveLength(1);
    });

    it("can be tried again once it has an answer", async () => {
      await show({ guess: PNPM, attempts: [check({ state: "failed", exitCode: 1, reason: { code: "checkFailed", exitCode: 1 } })] });
      await click("Try it");
      expect(button("Try it").disabled).toBe(false);
    });

    it("says it's waiting when it is", async () => {
      await show({ guess: PNPM, attempts: [check({ state: "queued", reason: { code: "waitingForProject" } })] });
      await click("Try it");
      expect(page()).toContain(i18next.t("reasons.waitingForProject"));
    });

    it("forgets the answer when the command changes, since it no longer speaks for it", async () => {
      await show({ guess: PNPM, attempts: [check({})] });
      await click("Try it");
      expect(page()).toContain(i18next.t("setup.passed"));
      await type(inputs()[0], "pnpm other");
      expect(page()).not.toContain(i18next.t("setup.passed"));
    });

    it("can't try nothing", async () => {
      await show({});
      expect(button("Try it").disabled).toBe(true);
    });

    it("goes to the run when asked, without going back to New Run", async () => {
      await show({ guess: PNPM, attempts: [check({})] });
      useApp.setState((s) => ({ projectSetup: { ...s.projectSetup!, returnToNewRun: true } }));
      await settle();
      await click("Try it");
      await click("Open the run");
      expect(useApp.getState().view).toEqual({ name: "run", runId: "r1" });
      expect(useApp.getState().projectSetup).toBeNull();
      await settle();
      expect(useApp.getState().newRunOpen).toBe(false);
      useApp.setState({ view: { name: "home" } });
    });
  });

  describe("the map line", () => {
    const added = (map: ProjectAdded["map"], outcome: ProjectAdded["outcome"] = "new"): ProjectAdded => ({ project: PROJECT, outcome, map });

    it("says it started only when it did", async () => {
      await show({ added: added({ status: "started" }) });
      expect(page()).toContain(i18next.t("setup.mapStarted"));
      expect(page()).toContain("demo is added");
    });

    it("says Claude Code is signed out, and points to the Brain for this project", async () => {
      await show({ added: added({ status: "notStarted", reason: { code: "claudeLoggedOut" } }) });
      expect(page()).not.toContain(i18next.t("setup.mapStarted"));
      expect(page()).toContain(i18next.t("setup.mapSignedOut"));
      await click(i18next.t("setup.openBrain"));
      expect(useApp.getState().view).toEqual({ name: "brain" });
      expect(useApp.getState().brainProjectId).toBe("p1");
      expect(useApp.getState().projectSetup).toBeNull();
      useApp.setState({ view: { name: "home" }, brainProjectId: null });
    });

    it("says why when anything else kept it from starting", async () => {
      await show({ added: added({ status: "notStarted", reason: { code: "gitMissing" } }) });
      expect(page()).toContain(i18next.t("reasons.gitMissing"));
    });

    it("makes no claim about a folder that was already there", async () => {
      await show({ added: added(null, "existing") });
      expect(page()).toContain("demo is already in Keel");
      expect(page()).not.toContain(i18next.t("setup.mapStarted"));
      expect(page()).not.toContain(i18next.t("setup.openBrain"));
    });

    it("says a removed project came back with its history, and makes no claim about a map", async () => {
      await show({ added: added(null, "restored") });
      expect(page()).toContain("demo is back");
      expect(page()).toContain(i18next.t("setup.restored"));
      expect(page()).not.toContain(i18next.t("setup.mapStarted"));
    });

    it("says nothing about a map when reopened to finish setup", async () => {
      await show({});
      expect(page()).toContain("Finish setting up demo");
      expect(page()).not.toContain(i18next.t("setup.mapStarted"));
    });
  });

  describe("Later", () => {
    it("closes and saves nothing", async () => {
      await show({ guess: PNPM, accounts: { "you-work": "WRITE" } });
      await click("Later");
      expect(useApp.getState().projectSetup).toBeNull();
      expect(called("project_update")).toEqual([]);
    });
  });

  it("comes in Spanish too", async () => {
    await i18next.changeLanguage("es");
    await show({ guess: PNPM, accounts: { "you-work": "WRITE" }, added: { project: PROJECT, outcome: "new", map: { status: "started" } } });
    expect(page()).toContain("demo quedó agregado");
    expect(page()).toContain("Deducido de package.json, pnpm-lock.yaml.");
    expect(page()).toContain("puede subir cambios");
    expect(button("Probarlo")).toBeDefined();
    expect(button("Después")).toBeDefined();
  });
});
