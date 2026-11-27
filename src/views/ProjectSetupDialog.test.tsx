import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { Attempt, GithubStatus, Guess, Project, ProjectAdded, ToolFolder } from "../lib/api";
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
  sensitivePaths: ["billing/"],
  allowLocalhost: true,
  issueLabel: null,
};
const NO_GUESS: Guess = { check: null, setup: null, from: [], setupOptions: [], checkOptions: [], fromCi: false, localhost: false, ciChecks: [], ciLeftOut: [] };
const PNPM: Guess = { check: "pnpm test", setup: "pnpm install --frozen-lockfile", from: ["package.json", "pnpm-lock.yaml"], setupOptions: [], checkOptions: ["pnpm run lint", "pnpm test"], fromCi: false, localhost: false, ciChecks: [], ciLeftOut: [] };
const UV: Guess = { check: "uv run --frozen pytest", setup: "uv sync --frozen --group test", from: ["uv.lock", "pyproject.toml"], setupOptions: ["--group stubs", "--group test"], checkOptions: [], fromCi: false, localhost: false, ciChecks: [], ciLeftOut: [] };
const CI: Guess = { check: "zig build test", setup: "zig fetch && zig build", from: [".github/workflows/ci.yml"], setupOptions: [], checkOptions: [], fromCi: true, localhost: false, ciChecks: [], ciLeftOut: [] };
const UV_AND_CI: Guess = { ...UV, check: "uv run --frozen pytest && uv run --frozen ruff check . && uv run --frozen pyright", from: ["uv.lock", ".github/workflows/ci.yml"], ciChecks: ["uv run --frozen ruff check .", "uv run --frozen pyright"], ciLeftOut: [{ command: "semgrep ci", why: "offline" }, { command: "uv run mypy .", why: "mayFail" }] };
const GRADLE: Guess = { check: "./gradlew test", setup: "./gradlew test --test-dry-run", from: ["build.gradle.kts", "gradlew"], setupOptions: [], checkOptions: [], fromCi: false, localhost: true, ciChecks: [], ciLeftOut: [] };
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
  /** What Keel finds the commands need, or that looking failed. */
  folders?: ToolFolder[] | "fails";
}

describe("the setup dialog", () => {
  let root: Root;
  let calls: { command: string; args: Record<string, unknown> }[];
  /** The project as Keel has it saved. */
  let kept: Project;

  const settle = () => act(async () => {
    for (let i = 0; i < 4; i++) await new Promise((resolve) => setTimeout(resolve));
  });

  const show = async ({ project = PROJECT, guess = NO_GUESS, accounts = {}, gh, added = null, attempts = [], events = [], folders = [] }: Scene) => {
    const logins = Object.keys(accounts);
    kept = project;
    mockIPC((command, payload) => {
      const args = (payload ?? {}) as Record<string, unknown>;
      calls.push({ command, args });
      switch (command) {
        case "project_setup":
          return { project, guess };
        case "overview":
          return { ...useApp.getState().overview, projects: [kept] };
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
        case "project_tool_folders":
          if (folders === "fails") throw { code: "internal", reason: null, detail: "no shell" };
          return folders;
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

  /** Saved without the dialog, as a client does over MCP. The app hears of it unless `heard` is false. */
  const savedMeanwhile = async (fields: Partial<Project>, { heard = true } = {}) => {
    kept = { ...kept, ...fields };
    if (heard) await act(async () => useApp.setState({ overview: { ...useApp.getState().overview, projects: [kept] } }));
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
  const field = (label: string) => [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(i18next.t(label)))?.querySelector("input") as HTMLInputElement;
  const checkInput = () => field("settings.checkCommand");
  const setupInput = () => field("settings.setupCommand");
  /** Both commands by name, whatever order the dialog shows them in. */
  const commands = () => ({ check: checkInput().value, setup: setupInput().value });
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
      expect(commands()).toEqual({ check: "pnpm test", setup: "pnpm install --frozen-lockfile" });
      expect(page()).toContain("Guessed from package.json, pnpm-lock.yaml.");
      expect(called("project_update")).toEqual([]);
    });

    it("shows the setup before the check, the order they run in, each under the Commands heading", async () => {
      await show({ guess: PNPM, accounts: { "you-work": "WRITE" } });
      expect(setupInput().compareDocumentPosition(checkInput()) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
      const headings = [...document.querySelectorAll("h3")].map((h) => h.textContent);
      expect(headings).toEqual([i18next.t("projects.sectionGithub"), i18next.t("projects.sectionCommands")]);
      // Where the commands came from is said beside them, before anything the dialog offers to open.
      const section = setupInput().closest("section") as HTMLElement;
      expect(section.textContent).toContain("Guessed from package.json, pnpm-lock.yaml.");
    });

    it("leaves the fields empty for an unknown stack, and says so", async () => {
      await show({ accounts: { "you-work": "WRITE" } });
      expect(commands()).toEqual({ check: "", setup: "" });
      expect(page()).toContain(i18next.t("setup.noGuess"));
      expect(page()).not.toContain("Guessed from");
    });

    it("never replaces what's saved", async () => {
      await show({ project: { ...PROJECT, checkCommand: "make test", setupCommand: "make deps" }, guess: PNPM });
      expect(commands()).toEqual({ check: "make test", setup: "make deps" });
      expect(page()).not.toContain("Guessed from");
    });

    it("says when it comes from what the project's CI runs", async () => {
      await show({ guess: CI, accounts: { "you-work": "WRITE" } });
      expect(commands()).toEqual({ check: "zig build test", setup: "zig fetch && zig build" });
      expect(page()).toContain("Taken from your CI: .github/workflows/ci.yml.");
      expect(page()).not.toContain("Guessed from");
      expect(called("project_update")).toEqual([]);
      await type(checkInput(), "zig build test --summary all");
      await type(setupInput(), "zig build");
      expect(page()).not.toContain("Taken from your CI");
    });

    it("says which commands of the check come from the project's CI", async () => {
      await show({ guess: UV_AND_CI, accounts: { "you-work": "WRITE" } });
      expect(checkInput().value).toBe("uv run --frozen pytest && uv run --frozen ruff check . && uv run --frozen pyright");
      expect(page()).toContain("Guessed from uv.lock, .github/workflows/ci.yml.");
      expect(page()).toContain("From your CI: uv run --frozen ruff check ., uv run --frozen pyright.");
      // Each with why, in a few words.
      expect(page()).toContain("Left out of the check, though your CI runs them: semgrep ci (Keel can't run it offline as CI does), uv run mypy . (CI lets it fail).");
      expect(page()).toContain(i18next.t("projects.pyrightNode"));
      expect(called("project_update")).toEqual([]);
      await type(checkInput(), "uv run --frozen pytest");
      expect(page()).not.toContain("From your CI");
    });

    it("stops calling it a guess once it's been changed", async () => {
      await show({ guess: { ...PNPM, setup: null } });
      expect(page()).toContain("Guessed from");
      expect(page()).not.toContain("From your CI");
      expect(page()).not.toContain("Left out of the check");
      expect(page()).not.toContain(i18next.t("projects.pyrightNode"));
      await type(checkInput(), "pnpm test:unit");
      expect(page()).not.toContain("Guessed from");
    });
  });

  describe("what the repo offers to add", () => {
    const option = (text: string) => [...document.querySelectorAll("button")].find((button) => button.textContent === text);

    it("offers only what the command doesn't have yet, and adds what's chosen", async () => {
      await show({ guess: UV });
      expect(option("--group test")).toBeUndefined();
      await act(async () => option("--group stubs")!.click());
      expect(setupInput().value).toBe("uv sync --frozen --group test --group stubs");
      expect(option("--group stubs")).toBeUndefined();
    });

    it("offers a saved project the group it's missing and changes nothing until it's chosen", async () => {
      await show({ project: { ...PROJECT, checkCommand: "uv run --frozen pytest", setupCommand: "uv sync --frozen" }, guess: UV });
      expect(setupInput().value).toBe("uv sync --frozen");
      await act(async () => option("--group test")!.click());
      expect(setupInput().value).toBe("uv sync --frozen --group test");
    });

    it("offers no flags for a setup command that isn't uv's", async () => {
      await show({ project: { ...PROJECT, setupCommand: "make deps" }, guess: UV });
      expect(option("--group stubs")).toBeUndefined();
    });

    it("makes a check that was added have to pass too", async () => {
      await show({ guess: PNPM });
      expect(option("pnpm test")).toBeUndefined();
      await act(async () => option("pnpm run lint")!.click());
      expect(checkInput().value).toBe("pnpm test && pnpm run lint");
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
      await savedMeanwhile({ allowedDomains: ["changed.example"] }, { heard: false });
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowedDomains).toEqual(["changed.example"]);
      expect((called("project_update")[0].args.settings as Project).sensitivePaths).toEqual(["billing/"]);
    });

    it("says why when it can't save, and stays open", async () => {
      await show({ accounts: { "you-work": "WRITE" } });
      mockIPC((command) => {
        if (command === "project_update") throw { code: "blocked", reason: { code: "invalidSetting", field: "x", value: "y" }, detail: null };
        if (command === "overview") return useApp.getState().overview;
        return null;
      });
      await click("Save");
      expect(document.querySelector('[role="alert"]')?.textContent).toContain("y");
      expect(useApp.getState().projectSetup).not.toBeNull();
    });
  });

  describe("a change to the project while this is open", () => {
    const SAVED: Project = { ...PROJECT, githubAccount: "you-work", setupCommand: "make deps", checkCommand: "make test" };
    const BOTH = { "you-work": "WRITE", "you-bot": "WRITE" } as const;
    const notice = () => document.querySelector('[role="status"]')?.textContent ?? "";
    const sent = () => called("project_update")[0].args.settings as Project;
    const values = () => [checkInput().value, setupInput().value];

    it("refreshes what the person hadn't touched, keeps what they had, and says so", async () => {
      await show({ project: SAVED, accounts: BOTH });
      await type(setupInput(), "make mine");
      expect(notice()).toBe("");

      await savedMeanwhile({ githubAccount: "you-bot", setupCommand: "make theirs", checkCommand: "make check" });
      expect(values()).toEqual(["make check", "make mine"]);
      expect(select().value).toBe("you-bot");
      expect(notice()).toContain(i18next.t("setup.changed"));
      expect(notice()).toContain(i18next.t("projects.changedKept"));
      expect(notice()).toContain("make theirs");

      // Looking at the accounts again doesn't bring the old one back.
      await act(async () => void window.dispatchEvent(new Event("focus")));
      await settle();
      expect(select().value).toBe("you-bot");

      await click("Save");
      expect(sent()).toMatchObject({ githubAccount: "you-bot", setupCommand: "make mine", checkCommand: "make check" });
      expect(useApp.getState().projectSetup).toBeNull();
    });

    it("keeps the account the person chose", async () => {
      await show({ project: SAVED, accounts: { ...BOTH, "you-admin": "ADMIN" } });
      await choose("you-admin");
      await savedMeanwhile({ githubAccount: "you-bot" });
      expect(select().value).toBe("you-admin");
      expect(notice()).toContain("you-bot");
    });

    it("replaces a guess with a command saved meanwhile", async () => {
      await show({ guess: PNPM, accounts: BOTH, project: { ...SAVED, setupCommand: null, checkCommand: null } });
      expect(values()).toEqual(["pnpm test", "pnpm install --frozen-lockfile"]);
      await savedMeanwhile({ checkCommand: "make check" });
      expect(values()).toEqual(["make check", "pnpm install --frozen-lockfile"]);
    });

    it("doesn't save over a change it hasn't shown yet", async () => {
      await show({ project: SAVED, accounts: BOTH });
      await savedMeanwhile({ checkCommand: "make check" }, { heard: false });
      await click("Save");
      expect(called("project_update")).toEqual([]);
      expect(useApp.getState().projectSetup).not.toBeNull();
      expect(values()[0]).toBe("make check");
      expect(notice()).toContain(i18next.t("setup.changed"));

      await click("Save");
      expect(sent().checkCommand).toBe("make check");
      expect(useApp.getState().projectSetup).toBeNull();
    });

    it("warns before saving over a change to a field the person had edited", async () => {
      await show({ project: SAVED, accounts: BOTH });
      await type(checkInput(), "make mine");
      await savedMeanwhile({ checkCommand: "make check" }, { heard: false });
      await click("Save");
      expect(called("project_update")).toEqual([]);
      expect(values()[0]).toBe("make mine");
      expect(notice()).toContain("make check");
      await click("Save");
      expect(sent().checkCommand).toBe("make mine");
    });

    it("forgets a trial of a command that was replaced", async () => {
      await show({ project: SAVED, accounts: BOTH, attempts: [{ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null }] });
      await click("Try it");
      expect(page()).toContain(i18next.t("setup.passed"));
      await savedMeanwhile({ checkCommand: "make check" });
      expect(page()).not.toContain(i18next.t("setup.passed"));
    });
  });

  describe("the folders the commands need", () => {
    const NVM = { folder: "~/.nvm", programs: ["node"] };
    const PNPM_HOME = { folder: "~/Library/pnpm", programs: ["pnpm"] };
    const boxes = () => [...document.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')];
    /** Keel looks a moment after the commands stop changing. */
    const lookedUp = async () => {
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 450));
      });
      await settle();
    };

    it("ticks each folder found that the project doesn't read yet, and saves it with the ones it does", async () => {
      await show({ guess: PNPM, folders: [NVM, PNPM_HOME] });
      await lookedUp();
      expect(called("project_tool_folders").at(-1)?.args).toEqual({ projectId: "p1", setup: "pnpm install --frozen-lockfile", check: "pnpm test" });
      expect(page()).toContain(i18next.t("setup.folders"));
      expect(page()).toContain("~/Library/pnpm");
      expect(page()).toContain(i18next.t("setup.folderFor", { programs: "pnpm" }));
      expect(boxes().map((box) => box.checked)).toEqual([true]);
      expect(called("project_update")).toEqual([]);

      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowedRead).toEqual(["~/.nvm", "~/Library/pnpm"]);
    });

    it("tries the commands with the folders ticked, and leaves out one the person unticks", async () => {
      await show({ guess: PNPM, folders: [PNPM_HOME], attempts: [{ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null }] });
      await lookedUp();
      await click("Try it");
      expect(called("project_try_check")[0].args.read).toEqual(["~/Library/pnpm"]);

      await act(async () => boxes()[0].click());
      await settle();
      expect(page()).not.toContain(i18next.t("setup.passed"));
      await click("Try it");
      expect(called("project_try_check")[1].args.read).toEqual([]);
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowedRead).toEqual(["~/.nvm"]);
    });

    it("looks again when a command changes", async () => {
      await show({ guess: PNPM, folders: [PNPM_HOME] });
      await lookedUp();
      await type(checkInput(), "make test");
      await lookedUp();
      expect(called("project_tool_folders").at(-1)?.args).toEqual({ projectId: "p1", setup: "pnpm install --frozen-lockfile", check: "make test" });
    });

    it("shows nothing when the project reads all it needs already", async () => {
      await show({ guess: PNPM, folders: [NVM] });
      await lookedUp();
      expect(called("project_tool_folders")).not.toEqual([]);
      expect(boxes()).toEqual([]);
      expect(page()).not.toContain(i18next.t("setup.folders"));
    });

    it("shows nothing when Keel couldn't look, and saves the list as it was", async () => {
      await show({ guess: PNPM, folders: "fails" });
      await lookedUp();
      expect(boxes()).toEqual([]);
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowedRead).toEqual(["~/.nvm"]);
    });
  });

  describe("opening localhost", () => {
    const CLOSED = { ...PROJECT, allowLocalhost: false };
    const box = () => document.querySelector<HTMLInputElement>('input[type="checkbox"]');

    it("ticks it when the check can't run without it, and tries and saves the commands with it open", async () => {
      await show({ project: CLOSED, guess: GRADLE, attempts: [{ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null }] });
      expect(page()).toContain(i18next.t("setup.localhost"));
      expect(page()).toContain(i18next.t("setup.localhostWhy"));
      expect(box()?.checked).toBe(true);
      expect(called("project_update")).toEqual([]);

      await click("Try it");
      expect(called("project_try_check")[0].args.localhost).toBe(true);
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowLocalhost).toBe(true);
    });

    it("leaves it closed when the person unticks it", async () => {
      await show({ project: CLOSED, guess: GRADLE, attempts: [{ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null }] });
      await act(async () => box()?.click());
      await settle();
      await click("Try it");
      expect(called("project_try_check")[0].args.localhost).toBe(false);
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowLocalhost).toBe(false);
    });

    it("asks nothing when the project has it open already, or the check doesn't need it", async () => {
      await show({ guess: GRADLE });
      expect(box()).toBeNull();
      expect(page()).not.toContain(i18next.t("setup.localhost"));
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowLocalhost).toBe(true);

      await act(async () => root.unmount());
      root = createRoot(document.body.appendChild(document.createElement("div")));
      calls = [];
      await show({ project: CLOSED, guess: PNPM });
      expect(page()).not.toContain(i18next.t("setup.localhost"));
      await click("Save");
      expect((called("project_update")[0].args.settings as Project).allowLocalhost).toBe(false);
    });
  });

  describe("trying it", () => {
    const check = (fields: Partial<Attempt>): Partial<Attempt> => ({ id: "a", kind: "check", state: "succeeded", exitCode: 0, reason: null, ...fields });

    it("runs what's on screen, saves nothing, and shows the answer with its output", async () => {
      await show({ guess: PNPM, attempts: [check({})], events: [{ seq: 1, attemptId: "a", at: "", kind: "checkResult", data: { stdout: "all good", stderr: "" } }] });
      await type(checkInput(), "pnpm test:unit");
      await click("Try it");
      expect(called("project_try_check")[0].args).toEqual({ projectId: "p1", setup: "pnpm install --frozen-lockfile", check: "pnpm test:unit", read: [], localhost: false, language: "en" });
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
      await type(checkInput(), "pnpm other");
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
