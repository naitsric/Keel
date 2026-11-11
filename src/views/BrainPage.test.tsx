import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { BrainView } from "../lib/api";
import { useApp } from "../lib/store";
import { BrainPage } from "./BrainPage";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const brain: BrainView = {
  map: [],
  learnings: [],
  proposals: [],
  decisions: [
    { seq: 2, at: "2026-11-05T10:00:00Z", answer: "answered", question: "If `whole` is 0, what should happen?", text: "Show an error, as today", included: true },
    { seq: 1, at: "2026-11-04T10:00:00Z", answer: "reject", question: null, text: "Use the existing helper", included: true },
  ],
  used: 100,
  limit: 4096,
  mapUsed: 3000,
  mapLimit: 3072,
  prompt: "",
  mapRun: null,
};

describe("a decision on the Brain page", () => {
  let root: Root;

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC((command) => (command === "brain_get" ? brain : null));
    useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ id: "p1", name: "demo" }] as never } });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  const open = () => act(async () => root.render(<BrainPage />));

  it("that came from answering says what was asked and what was answered", async () => {
    await open();
    const page = document.body.textContent ?? "";
    expect(page).toContain("You were asked: If `whole` is 0, what should happen?");
    expect(page).toContain("You answered: Show an error, as today");
  });

  it("shares its room with learnings, and the map has its own", async () => {
    await open();
    const page = document.body.textContent ?? "";
    expect(page).toContain("Learned and decisions: 0.1 KB of 4 KB");
    expect(page).toContain("Map: 2.9 KB of 3 KB");
    expect(page).not.toContain("Not everything fits");
  });

  it("that came from an approval reads as it always has", async () => {
    await open();
    const page = document.body.textContent ?? "";
    expect(page).toContain("Use the existing helper");
    expect(page).not.toContain("You were asked: Use");
    expect(page).not.toContain("You answered: Use");
  });
});

describe("the project the Brain page shows", () => {
  let root: Root;
  const brainOf = (projectId: string): BrainView => ({ ...brain, decisions: [{ seq: 1, at: "2026-11-04T10:00:00Z", answer: "reject", question: null, text: `Decided in ${projectId}`, included: true }] });
  const answer = (_: string, args: unknown) => brainOf((args as { projectId: string }).projectId);

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    useApp.setState({
      overview: { ...useApp.getState().overview, projects: [{ id: "p1", name: "demo" }, { id: "p2", name: "other" }] as never },
      brainProjectId: "p1",
    });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ brainProjectId: null });
  });

  const open = () => act(async () => root.render(<BrainPage />));
  const shown = () => document.querySelector("select")?.value;
  const page = () => document.body.textContent ?? "";
  // What "Open the Brain" in a project's setup does.
  const sendTo = (projectId: string) =>
    act(async () => {
      useApp.setState({ brainProjectId: projectId });
      useApp.getState().navigate({ name: "brain" });
    });
  const pick = (projectId: string) =>
    act(async () => {
      const select = document.querySelector("select") as HTMLSelectElement;
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")?.set?.call(select, projectId);
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });

  it("moves to another project sent to it while it's open", async () => {
    mockIPC(answer);
    await open();
    expect(shown()).toBe("p1");
    expect(page()).toContain("Decided in p1");
    await sendTo("p2");
    expect(shown()).toBe("p2");
    expect(page()).toContain("Decided in p2");
    expect(page()).not.toContain("Decided in p1");
  });

  it("moves back to a project sent to it before, after the person picked another", async () => {
    mockIPC(answer);
    await open();
    await pick("p2");
    expect(page()).toContain("Decided in p2");
    await sendTo("p1");
    expect(shown()).toBe("p1");
    expect(page()).toContain("Decided in p1");
  });

  it("ignores a project's answer that arrives after another project is on screen", async () => {
    let release!: () => void;
    const held = new Promise<void>((resolve) => (release = resolve));
    mockIPC(async (command, args) => {
      if ((args as { projectId: string }).projectId === "p1") await held;
      return answer(command, args);
    });
    await open();
    await sendTo("p2");
    expect(page()).toContain("Decided in p2");
    await act(async () => release());
    expect(shown()).toBe("p2");
    expect(page()).toContain("Decided in p2");
    expect(page()).not.toContain("Decided in p1");
  });
});

describe("writing the map", () => {
  let root: Root;

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ id: "p1", name: "demo" }] as never }, brainProjectId: "p1" });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ brainProjectId: null });
  });

  const write = () => act(async () => [...document.querySelectorAll("button")].find((b) => b.textContent === "Write the map")?.click());

  it("starts one run on a double click, and stays off until the page shows that run", async () => {
    const overview = useApp.getState().overview;
    let started = 0;
    let mapRun: string | null = null;
    let finish!: (run: string) => void;
    let reloaded: Promise<void> | null = null;
    let release!: () => void;
    mockIPC(async (command) => {
      if (command === "overview") return overview;
      if (command === "brain_get") {
        if (reloaded) await reloaded;
        return { ...brain, mapRun };
      }
      if (command === "brain_generate_map") {
        started += 1;
        return new Promise<string>((resolve) => (finish = (run) => resolve((mapRun = run))));
      }
      return null;
    });
    await act(async () => root.render(<BrainPage />));

    await write();
    await write();
    expect(started).toBe(1);

    // The run exists, but the page hasn't loaded it yet.
    reloaded = new Promise<void>((resolve) => (release = resolve));
    await act(async () => finish("r9"));
    await write();
    expect(started).toBe(1);

    await act(async () => release());
    expect(document.body.textContent).toContain(i18next.t("brain.mapRunning"));
    await write();
    expect(started).toBe(1);
  });
});

describe("the Brain page in a wide window", () => {
  let root: Root;

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC((command) => (command === "brain_get" ? brain : null));
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  // The same centered column as Settings, so the pages don't jump from side to side.
  const column = () => document.querySelector("main > .mx-auto.w-full.max-w-3xl");

  it.each([
    ["with projects", [{ id: "p1", name: "demo" }], "brain.learnings"],
    ["without projects", [], "brain.noProjects"],
  ])("sits in the middle, like Settings, %s", async (_, projects, inside) => {
    useApp.setState({ overview: { ...useApp.getState().overview, projects: projects as never } });
    await act(async () => root.render(<BrainPage />));
    expect(column()).not.toBeNull();
    expect(column()?.textContent).toContain(i18next.t("brain.subtitle"));
    expect(column()?.textContent).toContain(i18next.t(inside));
  });
});
