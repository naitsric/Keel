import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { hideApp, row, settle, showApp, view } from "../commands/testApp";
import i18next from "../i18n";
import { useApp, type View } from "../lib/store";

const RUNS = [row("r1", "running"), row("r2", "succeeded")];

const header = () => document.querySelector("header") as HTMLElement;
const nav = () => header().querySelector("nav");
const headerButton = (text: string) => [...header().querySelectorAll("button")].find((b) => b.textContent?.startsWith(text));
const settings = () => header().querySelector<HTMLButtonElement>('button[aria-label="Settings"]');
const currentOnes = () => [...header().querySelectorAll('[aria-current="page"]')].map((b) => b.textContent || b.getAttribute("aria-label"));
const click = async (button: HTMLElement | null | undefined) => {
  act(() => button?.click());
  await settle();
};

describe("the top bar", () => {
  beforeEach(() => void vi.useFakeTimers());

  afterEach(async () => {
    await hideApp();
    vi.useRealTimers();
  });

  it.each([
    ["Runs", "home"],
    ["Board", "board"],
    ["Workflows", "workflows"],
    ["Brain", "brain"],
  ])("%s goes to its page", async (label, page) => {
    await showApp(RUNS, { name: "settings" });
    await click(headerButton(label));
    expect(view().name).toBe(page);
  });

  it("goes to Settings from the gear", async () => {
    await showApp(RUNS);
    await click(settings());
    expect(view().name).toBe("settings");
  });

  it.each<[string, View, string]>([
    ["home", { name: "home" }, "Runs"],
    ["the board", { name: "board" }, "Board"],
    ["workflows", { name: "workflows" }, "Workflows"],
    ["brain", { name: "brain" }, "Brain"],
    ["settings", { name: "settings" }, "Settings"],
    ["a run opened from home", { name: "run", runId: "r1" }, "Runs"],
    ["a run opened from the board", { name: "run", runId: "r1", backTo: { name: "board", projectId: "p2" } }, "Board"],
    ["a run opened from the brain", { name: "run", runId: "r1", backTo: { name: "brain" } }, "Brain"],
  ])("on %s, says which page is current, and only that one", async (_name, at, label) => {
    await showApp(RUNS, at);
    expect(currentOnes()).toEqual([label]);
  });

  it("shows how many proposals wait, on the Brain", async () => {
    await showApp(RUNS);
    act(() => useApp.setState({ overview: { ...useApp.getState().overview, pendingProposals: 2 } }));
    expect(headerButton("Brain")?.textContent).toBe("Brain2");
    expect(headerButton("Brain")?.querySelector("span")?.getAttribute("aria-label")).toBe("2 proposals waiting");
  });

  it("is in Spanish too", async () => {
    await showApp(RUNS);
    await act(() => i18next.changeLanguage("es"));
    const labels = [...(nav() as Element).querySelectorAll("button")].map((b) => b.textContent);
    expect(labels).toEqual(["Ejecuciones", "Tablero", "Workflows", "Brain"]);
    await act(() => i18next.changeLanguage("en"));
  });

  describe("Back", () => {
    it.each(["home", "board", "workflows", "brain", "settings"] as const)("isn't on %s", async (page) => {
      await showApp(RUNS, { name: page });
      expect(headerButton("Back")).toBeUndefined();
      expect(header().textContent).toContain("Keel");
    });

    it("is on a run, in place of the name, and goes home by default", async () => {
      await showApp(RUNS, { name: "run", runId: "r1" });
      expect(header().textContent).not.toContain("Keel");
      await click(headerButton("Back"));
      expect(view()).toEqual({ name: "home" });
    });

    it("goes back to the board's project a run was opened from", async () => {
      await showApp(RUNS, { name: "run", runId: "r1", backTo: { name: "board", projectId: "p2" } });
      await click(headerButton("Back"));
      expect(view()).toEqual({ name: "board", projectId: "p2" });
    });

    it("shows while a run loads, so there's always a way out", async () => {
      await showApp([], { name: "run", runId: "not-loaded" });
      await click(headerButton("Back"));
      expect(view()).toEqual({ name: "home" });
    });
  });

  it("from a run opened on the board, Board goes back to that project", async () => {
    await showApp(RUNS, { name: "run", runId: "r1", backTo: { name: "board", projectId: "p2" } });
    await click(headerButton("Board"));
    expect(view()).toEqual({ name: "board", projectId: "p2" });
  });

  it("a page's button on that page changes nothing, so the board keeps its project", async () => {
    await showApp(RUNS, { name: "board", projectId: "p2" });
    await click(headerButton("Board"));
    expect(view()).toEqual({ name: "board", projectId: "p2" });
  });

  it("drags the window from everything except what can be clicked", async () => {
    await showApp(RUNS, { name: "run", runId: "r1" });
    expect(header().hasAttribute("data-tauri-drag-region")).toBe(true);
    expect(nav()?.hasAttribute("data-tauri-drag-region")).toBe(true);
    const buttons = [...header().querySelectorAll("button")];
    expect(buttons.length).toBeGreaterThan(0);
    for (const button of buttons) expect(button.closest("[data-tauri-drag-region]")).not.toBe(button);
    expect(buttons.filter((button) => button.hasAttribute("data-tauri-drag-region"))).toEqual([]);
    expect(header().className).toContain("pl-[var(--traffic-lights-inset)]");
  });

  describe("with nothing to go to yet", () => {
    it("has no pages on the welcome screen, and keeps Settings", async () => {
      await showApp([], { name: "home" }, []);
      expect(nav()).toBeNull();
      expect(headerButton("New run")).toBeUndefined();
      expect(settings()).not.toBeNull();
      expect(document.body.textContent).toContain(i18next.t("onboarding.title"));
    });

    it("has them on any other page, as the way back", async () => {
      await showApp([], { name: "home" }, []);
      await click(settings());
      expect(view().name).toBe("settings");
      expect(nav()).not.toBeNull();
      await click(headerButton("Runs"));
      expect(view().name).toBe("home");
      expect(nav()).toBeNull();
    });

    it("has them with every project removed and history to look at, without New run", async () => {
      await showApp(RUNS, { name: "home" }, []);
      expect(nav()).not.toBeNull();
      expect(headerButton("New run")).toBeUndefined();
      expect(document.body.textContent).toContain(i18next.t("onboarding.title"));
      expect(document.body.textContent).toContain("Run r1");
    });

    it("waits for the first answer, so someone with projects doesn't see it flash", async () => {
      await showApp([], { name: "home" });
      act(() => useApp.setState({ overviewLoaded: false, overview: { ...useApp.getState().overview, projects: [] } }));
      expect(nav()).not.toBeNull();
    });
  });
});
