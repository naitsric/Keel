import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextWaitingRun } from "../commands/runs";
import { hideApp, pressMod, row, settle, showApp, view } from "../commands/testApp";
import i18next from "../i18n";
import type { AttemptState, McpOverview, Preflight } from "../lib/api";
import { useApp, type View } from "../lib/store";

const STATES: AttemptState[] = ["queued", "preparing", "running", "stalled", "succeeded", "failed", "stopped", "interrupted", "blocked", "waitingApproval", "rejected", "skipped", "abandoned"];
const RUNS = [row("r1", "running"), row("r2", "waitingApproval"), row("r3", "succeeded"), row("r4", "blocked"), row("r5", "queued")];

const bar = () => document.querySelector("footer") as HTMLElement;
const waiting = () => bar().querySelector("button");
const click = async () => {
  act(() => waiting()?.click());
  await settle();
};
const setRuns = (runs: ReturnType<typeof row>[]) => act(() => useApp.setState({ overview: { ...useApp.getState().overview, runs } }));
const setPreflight = (fields: Partial<Preflight>) => act(() => useApp.setState({ preflight: { claudeFound: true, gitFound: true, ghFound: true, version: { status: "supported", found: "2.1.318" }, auth: { loggedIn: true, email: "you@example.com", subscription: "max" }, ignoredEnv: [], blocker: null, ...fields } as Preflight }));
const setMcp = (status: Partial<McpOverview["status"]>) => act(() => useApp.setState({ mcp: { running: false, port: null, url: null, error: null, ...status } }));

describe("the status bar", () => {
  beforeEach(() => void vi.useFakeTimers());

  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  describe("what's running", () => {
    it("counts runs with a step working, and not queued or stalled ones", async () => {
      await showApp([row("a", "running"), row("b", "preparing"), row("c", "queued"), row("d", "stalled"), row("e", "succeeded")]);
      expect(bar().textContent).toContain("2 running");
    });

    it("says so when nothing is", async () => {
      await showApp([row("a", "succeeded")]);
      expect(bar().textContent).toContain("Nothing running");
    });

    it("follows the overview as runs change", async () => {
      await showApp(RUNS);
      expect(bar().textContent).toContain("1 running");
      expect(bar().textContent).toContain("2 waiting for you");
      setRuns([row("r1", "succeeded"), row("r2", "waitingApproval")]);
      expect(bar().textContent).toContain("Nothing running");
      expect(bar().textContent).toContain("1 waiting for you");
    });
  });

  describe("what's waiting", () => {
    it.each(STATES)("agrees with ⌘G about a run that's %s", async (state) => {
      const runs = [row("only", state)];
      await showApp(runs);
      const keyHasSomewhereToGo = nextWaitingRun(runs, null, 1) !== null;
      expect(waiting() !== null).toBe(keyHasSomewhereToGo);
      expect(pressMod("g", "KeyG").defaultPrevented).toBe(keyHasSomewhereToGo);
      await settle();
      expect(view().name).toBe(keyHasSomewhereToGo ? "run" : "home");
    });

    it("shows the key", async () => {
      await showApp(RUNS);
      expect(waiting()?.textContent).toBe("2 waiting for you⌘G");
      expect(waiting()?.title).toBe("Next run waiting for you  ⌘G");
    });

    it("goes through them as ⌘G does, going round", async () => {
      await showApp(RUNS);
      await click();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      await click();
      expect(view()).toMatchObject({ name: "run", runId: "r4" });
      await click();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
    });

    it("takes turns with ⌘G, from wherever the other one got to", async () => {
      await showApp(RUNS);
      pressMod("g", "KeyG");
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
      await click();
      expect(view()).toMatchObject({ name: "run", runId: "r4" });
      pressMod("g", "KeyG");
      await settle();
      expect(view()).toMatchObject({ name: "run", runId: "r2" });
    });

    it("Back then goes where the person was, not through the runs they passed", async () => {
      await showApp(RUNS, { name: "workflows" });
      await click();
      await click();
      pressMod("[", "BracketLeft");
      await settle();
      expect(view()).toEqual({ name: "workflows" });
    });

    it("counts in Spanish, in the singular too", async () => {
      await showApp(RUNS);
      await act(() => i18next.changeLanguage("es"));
      expect(waiting()?.textContent).toBe("2 te esperan⌘G");
      setRuns([row("r2", "waitingApproval")]);
      expect(waiting()?.textContent).toBe("1 te espera⌘G");
      expect(bar().textContent).toContain("Nada en curso");
      await act(() => i18next.changeLanguage("en"));
    });
  });

  describe("the session", () => {
    it("says which Claude Code and whose", async () => {
      await showApp(RUNS);
      setPreflight({});
      expect(bar().textContent).toContain("Claude Code 2.1.318·you@example.com (max)");
    });

    it("shows the whole account on hover, and cuts it rather than the rest of the bar", async () => {
      await showApp(RUNS);
      setPreflight({});
      const account = [...bar().querySelectorAll("span")].find((span) => span.textContent === "you@example.com (max)");
      expect(account?.title).toBe("you@example.com (max)");
      expect(account?.className).toContain("truncate");
    });

    it("says it's checking until it knows", async () => {
      await showApp(RUNS);
      expect(bar().textContent).toContain(i18next.t("session.checking"));
    });

    it("says signed in without an email, and signed out", async () => {
      await showApp(RUNS);
      setPreflight({ auth: { loggedIn: true } as Preflight["auth"] });
      expect(bar().textContent).toContain("signed in");
      setPreflight({ auth: { loggedIn: false } as Preflight["auth"] });
      expect(bar().textContent).toContain("signed out");
    });

    it("says when Claude Code isn't there", async () => {
      await showApp(RUNS);
      setPreflight({ claudeFound: false, version: null, auth: null });
      expect(bar().textContent).toContain("Claude Code not found");
    });

    it("has moved here from Home, which keeps only what needs attention", async () => {
      await showApp(RUNS);
      setPreflight({ ignoredEnv: ["ANTHROPIC_API_KEY"] });
      const home = document.querySelector("main") as HTMLElement;
      expect(home.textContent).not.toContain("you@example.com");
      expect(home.textContent).not.toContain("2.1.318");
      expect(home.textContent).toContain("ANTHROPIC_API_KEY");
      setPreflight({ auth: { loggedIn: false } as Preflight["auth"], blocker: { code: "claudeLoggedOut" } });
      expect(home.querySelector('[role="alert"]')).not.toBeNull();
    });

    it("Home keeps its cards and projects", async () => {
      await showApp(RUNS);
      const home = document.querySelector("main") as HTMLElement;
      for (const label of [i18next.t("cards.running"), i18next.t("cards.blocked"), i18next.t("cards.tokens"), i18next.t("projects.title")]) expect(home.textContent).toContain(label);
    });
  });

  describe("MCP", () => {
    it("says nothing until it knows", async () => {
      await showApp(RUNS);
      expect(bar().textContent).not.toContain("MCP");
    });

    it("says on and off", async () => {
      await showApp(RUNS);
      setMcp({ running: true, port: 7413 });
      expect(bar().textContent).toContain("MCP on");
      setMcp({ running: false });
      expect(bar().textContent).toContain("MCP off");
    });

    it("says when it couldn't start, and why on hover", async () => {
      await showApp(RUNS);
      setMcp({ error: { code: "portInUse", port: 7413 } });
      const failed = [...bar().querySelectorAll("span")].find((span) => span.textContent === "MCP did not start");
      expect(failed?.title).toBe(i18next.t("mcp.portInUse", { port: 7413 }));
    });

    it("reads the server's status and keeps nothing else, like what clients said", async () => {
      await showApp(RUNS);
      await act(() => useApp.getState().refreshMcp());
      expect(useApp.getState().mcp).toEqual({ running: false, port: null, url: null, error: null });
    });
  });

  describe("on an empty app", () => {
    it("has no counts on the welcome screen, and still says which Claude Code", async () => {
      await showApp([], { name: "home" }, []);
      setPreflight({});
      expect(bar().textContent).not.toContain("Nothing running");
      expect(bar().textContent).toContain("Claude Code 2.1.318");
    });

    it("keeps the counts with every project removed and history to look at", async () => {
      await showApp([row("a", "failed")], { name: "home" }, []);
      expect(bar().textContent).toContain("Nothing running");
      expect(waiting()?.textContent).toContain("1 waiting for you");
    });
  });

  describe("in the window", () => {
    const PAGES: View[] = [{ name: "home" }, { name: "board" }, { name: "workflows" }, { name: "brain" }, { name: "settings" }, { name: "run", runId: "r1" }];

    it.each(PAGES)("is below the page, which scrolls by itself: $name", async (page) => {
      await showApp(RUNS, page);
      await settle();
      const main = document.querySelector("main") as HTMLElement;
      expect(bar().contains(main)).toBe(false);
      expect(bar().className).toContain("h-6");
      expect(bar().className).toContain("shrink-0");
      expect(main.className).toContain("flex-1");
      expect(main.parentElement?.className).toContain("min-h-0");
      expect(main.parentElement?.nextElementSibling).toBe(bar());
    });

    it("stays at the bottom while a run loads", async () => {
      await showApp([], { name: "run", runId: "not-loaded" });
      expect(document.querySelector("main")).toBeNull();
      expect(bar().previousElementSibling?.className).toContain("flex-1");
    });
  });
});
