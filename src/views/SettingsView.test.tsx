import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { NoticeSettings } from "../lib/api";
import { holdKeymap } from "../commands/keymapHold";
import { openClicked, openNotice, useApp } from "../lib/store";
import { SettingsView } from "./SettingsView";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe("notifications", () => {
  let root: Root;
  let saved: NoticeSettings[];
  let refuse: boolean;

  const settle = () =>
    act(async () => {
      for (let i = 0; i < 4; i++) await new Promise((resolve) => setTimeout(resolve));
    });
  const box = (label: string) => [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith(label))?.querySelector("input") as HTMLInputElement;

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    saved = [];
    refuse = false;
    mockIPC((command, args) => {
      if (command === "notices_get") return { waiting: true, finished: true, pullRequests: false, paused: true };
      if (command === "notices_set") {
        if (refuse) throw { code: "store" };
        saved.push((args as { settings: NoticeSettings }).settings);
      }
      if (command === "max_agents_get") return 2;
      if (command === "mcp_overview") return { settings: { enabled: false, port: 7413 }, status: { running: false, port: null, url: null, error: null }, clients: [], calls: [], projectChanges: [] };
      return null;
    });
    root = createRoot(document.body.appendChild(document.createElement("div")));
    await act(async () => root.render(<SettingsView />));
    await settle();
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  it("shows which kinds are on, and saves each change at once", async () => {
    expect(document.body.textContent).toContain("Notifications");
    expect(box("Something waits for you").checked).toBe(true);
    expect(box("A pull request changes").checked).toBe(false);
    await act(async () => box("A run finishes").click());
    expect(saved).toEqual([{ waiting: true, finished: false, pullRequests: false, paused: true }]);
    expect(box("A run finishes").checked).toBe(false);
  });

  it("goes back to what it was when saving fails, and says why", async () => {
    refuse = true;
    await act(async () => box("Todo pauses").click());
    await settle();
    expect(box("Todo pauses").checked).toBe(true);
    expect(document.querySelector('[role="alert"]')).not.toBeNull();
  });
});

describe("a click on a notice", () => {
  it("opens its run, and Back goes where the person was", () => {
    useApp.setState({ view: { name: "board", projectId: "p1" } });
    openNotice("r1");
    expect(useApp.getState().view).toEqual({ name: "run", runId: "r1", backTo: { name: "board", projectId: "p1" } });

    // From another run, Back still goes where the person was before the runs.
    openNotice("r2");
    expect(useApp.getState().view).toEqual({ name: "run", runId: "r2", backTo: { name: "board", projectId: "p1" } });
    openNotice("r2");
    expect(useApp.getState().view).toEqual({ name: "run", runId: "r2", backTo: { name: "board", projectId: "p1" } });
  });

  const tick = () => new Promise((resolve) => setTimeout(resolve));

  it("waits for an open dialog, so nothing written in it is lost", async () => {
    const board = { name: "board", projectId: "p1" } as const;
    useApp.setState({ view: board });
    const close = holdKeymap();
    openNotice("r1");
    await tick();
    expect(useApp.getState().view).toBe(board);
    close();
    await tick();
    expect(useApp.getState().view).toEqual({ name: "run", runId: "r1", backTo: board });
    // Once: another dialog that closes later opens nothing.
    useApp.setState({ view: board });
    holdKeymap()();
    await tick();
    expect(useApp.getState().view).toBe(board);
  });

  it("waits for the dialog that one opens as it closes", async () => {
    const board = { name: "board" } as const;
    useApp.setState({ view: board });
    const closeFirst = holdKeymap();
    openNotice("r1");
    closeFirst();
    const closeSecond = holdKeymap();
    await tick();
    expect(useApp.getState().view).toBe(board);
    closeSecond();
    await tick();
    expect(useApp.getState().view).toMatchObject({ name: "run", runId: "r1" });
  });

  it("opens the last one clicked, and none if the person went somewhere else from the dialog", async () => {
    useApp.setState({ view: { name: "home" } });
    let close = holdKeymap();
    openNotice("r1");
    openNotice("r2");
    close();
    await tick();
    expect(useApp.getState().view).toMatchObject({ name: "run", runId: "r2" });

    useApp.setState({ view: { name: "home" } });
    close = holdKeymap();
    openNotice("r1");
    useApp.getState().navigate({ name: "workflows" });
    close();
    await tick();
    expect(useApp.getState().view).toEqual({ name: "workflows" });
  });

  it("that opened Keel is asked for, since nothing was listening when it came", async () => {
    useApp.setState({ view: { name: "home" } });
    let left: { runId: string | null } | null = { runId: "r1" };
    mockIPC((command) => {
      if (command !== "notice_take") return null;
      const taken = left;
      left = null;
      return taken;
    });
    await openClicked();
    expect(useApp.getState().view).toMatchObject({ name: "run", runId: "r1" });
    useApp.setState({ view: { name: "home" } });
    await openClicked();
    expect(useApp.getState().view).toEqual({ name: "home" });
  });

  it("about Todo pausing, opens the board", () => {
    useApp.setState({ view: { name: "home" } });
    openNotice(null);
    expect(useApp.getState().view).toEqual({ name: "board" });
  });
});
