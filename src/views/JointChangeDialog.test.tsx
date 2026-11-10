import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { JointCard, JointChange } from "../lib/api";
import { useApp } from "../lib/store";
import { JointChangeDialog } from "./JointChangeDialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const card = (fields: Partial<JointCard>): JointCard => ({ cardId: "c", projectId: "p", project: "app", title: "Log in through Valora", column: "todo", runId: null, runState: null, prUrl: null, prState: null, part: null, ...fields });
const part = (position: number, extra: Partial<NonNullable<JointCard["part"]>> = {}) => ({ changeId: "plan", position, total: 2, waitsFor: null, warnings: [], ...extra });

const agreed: JointChange = {
  id: "plan",
  agreedIn: card({ cardId: "plan", column: "done", runId: "r0", runState: "succeeded" }),
  projects: [
    { id: "p1", name: "app" },
    { id: "p2", name: "valora" },
    { id: "p3", name: "dupplaBack" },
  ],
  agreed: { summary: "Log in from Valora into back-adm", criteria: [], changes: [], limits: [], decided: [], questions: [], parts: [{ project: "valora", text: "Sends the token" }], together: [] },
  parts: [
    card({ cardId: "one", projectId: "p2", project: "valora", column: "done", runId: "r1", runState: "succeeded", prUrl: "https://github.com/acme/valora/pull/7", prState: "draft", part: part(1) }),
    card({ cardId: "two", projectId: "p1", project: "app", part: part(2, { waitsFor: { cardId: "one", position: 1, project: "valora" }, warnings: [{ kind: "changed", position: 1, project: "valora" }] }) }),
  ],
  readOnly: [{ id: "p3", name: "dupplaBack" }],
  checks: [{ seq: 1, text: "Log in on Valora and land in back-adm", confirmedAt: null }],
};

describe("a change across several projects, in one place", () => {
  let root: Root;
  let shown: JointChange;
  let calls: { command: string; args: unknown }[];
  let closed: number;

  const render = async () => {
    await act(async () => root.render(<JointChangeDialog changeId="plan" backTo={{ name: "board", projectId: "p1" }} onClose={() => closed++} />));
  };
  const page = () => document.body.textContent ?? "";
  const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    shown = agreed;
    calls = [];
    closed = 0;
    mockIPC((command, args) => {
      if (command === "joint_change_get") return shown;
      calls.push({ command, args });
      return null;
    });
    useApp.setState({ view: { name: "board", projectId: "p1" } });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  it("lists each part in order, what it waits for, what became of the one before, and what was only read", async () => {
    await render();
    expect(page()).toContain("Agreed on “Log in through Valora”, in app.");
    const rows = [...document.querySelectorAll("li")].map((row) => row.textContent ?? "");
    expect(rows[0]).toContain("Part 1 of 2");
    expect(rows[0]).toContain("valora");
    expect(rows[0]).toContain("Draft");
    expect(rows[1]).toContain("Part 2 of 2");
    expect(rows[1]).toContain("Waits for part 1, in valora, to have a pull request");
    expect(rows[1]).toContain("Part 1, in valora, has commits this part didn't read.");
    expect(page()).toContain("Only read, not changed: dupplaBack");
    expect(page()).toContain("Log in from Valora into back-adm");
  });

  it("lets a person stop a part waiting, and say what works with every part in place", async () => {
    await render();
    await act(async () => button("Don't wait")?.click());
    const box = document.querySelector<HTMLInputElement>('input[type="checkbox"]');
    await act(async () => box?.click());
    expect(calls).toEqual([
      { command: "card_stop_waiting", args: { cardId: "two" } },
      { command: "joint_check_set", args: { changeId: "plan", seq: 1, works: true } },
    ]);
  });

  it("opens a part's run where Back comes back to the board", async () => {
    await render();
    await act(async () => [...document.querySelectorAll<HTMLButtonElement>("li button")].find((b) => b.textContent === "Open run")?.click());
    expect(closed).toBe(1);
    expect(useApp.getState().view).toEqual({ name: "run", runId: "r1", backTo: { name: "board", projectId: "p1" } });
  });

  it("before it's agreed, says where that happens and which projects it touches", async () => {
    shown = { ...agreed, agreedIn: card({ cardId: "plan", column: "inProgress", runId: "r0", runState: "running" }), agreed: null, parts: [], readOnly: [], checks: [] };
    await render();
    expect(page()).toContain("Not agreed yet. That happens on “Log in through Valora”, in app.");
    expect(page()).toContain("app, valora, dupplaBack");
    expect(page()).not.toContain("To try together");
  });

  it("is all in Spanish too", async () => {
    await i18next.changeLanguage("es");
    await render();
    expect(page()).toContain("Cambio en varios proyectos");
    expect(page()).toContain("Parte 2 de 2");
    expect(page()).toContain("Para probar juntos");
    expect(button("No esperar")).toBeDefined();
  });
});
