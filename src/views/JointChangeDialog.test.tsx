import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { ContractEdit, JointCard, JointChange } from "../lib/api";
import { useApp } from "../lib/store";
import { JointChangeDialog } from "./JointChangeDialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const card = (fields: Partial<JointCard>): JointCard => ({ cardId: "c", projectId: "p", project: "app", title: "Log in through Valora", column: "todo", runId: null, runState: null, prUrl: null, prState: null, part: null, ...fields });
const part = (position: number, extra: Partial<NonNullable<JointCard["part"]>> = {}) => ({ changeId: "plan", position, total: 2, waitsFor: null, warnings: [], contractChanged: [], ...extra });

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
  contract: [],
  contractNext: 1,
};

const contract: JointChange["contract"] = [
  {
    id: "K1",
    when: "someone logs in on valora",
    then: "valora posts /session with token",
    projects: ["valora"],
    parts: [{ cardId: "one", position: 1, project: "valora", result: "met", verifies: true, earlier: false }],
  },
  {
    id: "K2",
    when: "app gets /session with token",
    then: "it answers 201",
    projects: ["app"],
    parts: [{ cardId: "two", position: 2, project: "app", result: null, verifies: true, earlier: false }],
  },
  {
    id: "K3",
    when: "the token is wrong",
    then: "app answers 401",
    projects: ["app"],
    parts: [{ cardId: "two", position: 2, project: "app", result: "unmet", verifies: true, earlier: false }],
  },
];

describe("a change across several projects, in one place", () => {
  let root: Root;
  let shown: JointChange;
  let calls: { command: string; args: unknown }[];
  let closed: number;
  let refuse: unknown;

  const render = async () => {
    await act(async () => root.render(<JointChangeDialog changeId="plan" backTo={{ name: "board", projectId: "p1" }} onClose={() => closed++} />));
  };
  const page = () => document.body.textContent ?? "";
  const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);
  const drafts = () => [...document.querySelectorAll<HTMLElement>("[data-contract-draft]")];
  const write = async (box: HTMLTextAreaElement, text: string) => {
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(box, text);
      box.dispatchEvent(new Event("input", { bubbles: true }));
    });
  };
  const tick = async (draft: HTMLElement, project: string) => {
    const box = [...draft.querySelectorAll("label")].find((label) => label.textContent === project)?.querySelector("input");
    await act(async () => box?.click());
  };
  const corrected = { ...agreed, agreed: { ...agreed.agreed!, parts: [...agreed.agreed!.parts!, { project: "app", text: "Reads the token" }] }, contract, contractNext: 4 };

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    shown = agreed;
    calls = [];
    closed = 0;
    refuse = null;
    mockIPC((command, args) => {
      if (command === "joint_change_get") return shown;
      calls.push({ command, args });
      if (refuse) throw refuse;
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

  it("shows, for each point the parts agree on, what each part that keeps it found", async () => {
    shown = { ...agreed, agreed: { ...agreed.agreed!, contract: [{ id: "K3", when: "the token is wrong", then: "app answers 401", projects: ["app"] }] }, contract };
    await render();
    const points = [...document.querySelectorAll("[data-contract]")].map((point) => point.textContent ?? "");
    expect(points).toHaveLength(3);
    expect(points[0]).toContain("K1");
    expect(points[0]).toContain("someone logs in on valora → valora posts /session with token");
    expect(points[0]).toContain("Part 1, valora: Met");
    expect(points[1]).toContain("Part 2, app: Not checked yet");
    expect(points[2]).toContain("Part 2, app: Not met");
    expect(page()).toContain("What the parts agree on exactly");
    // Once is enough: not again in what was agreed.
    expect(page().match(/app answers 401/g)).toHaveLength(1);
  });

  it("says when a part's workflow doesn't verify, rather than that it hasn't yet", async () => {
    const unverifiable = contract.map((point) => ({ ...point, parts: point.parts.map((part) => (part.project === "app" ? { ...part, result: null, verifies: false } : part)) }));
    shown = { ...agreed, contract: unverifiable };
    await render();
    const points = [...document.querySelectorAll("[data-contract]")].map((point) => point.textContent ?? "");
    expect(points[1]).toContain("Part 2, app: This workflow doesn't verify");
    expect(page()).not.toContain("Not checked yet");
    await i18next.changeLanguage("es");
    await render();
    expect(page()).toContain("Parte 2, app: Este flujo no verifica");
  });

  it("says nothing about it for an agreement without any", async () => {
    await render();
    expect(document.querySelector("[data-contract]")).toBeNull();
    expect(page()).not.toContain("What the parts agree on exactly");
  });

  it("lets a person correct a point, remove one and add one, and saves them with the ids Keel keeps", async () => {
    shown = corrected;
    await render();
    await act(async () => button("Correct")?.click());
    expect(page()).toContain("A part specified already keeps what it was given until it starts again.");
    expect(drafts().map((draft) => draft.dataset.contractDraft)).toEqual(["K1", "K2", "K3"]);
    await write(drafts()[0].querySelectorAll("textarea")[1], "valora posts /session with token and expires");
    await act(async () => [...drafts()[2].querySelectorAll("button")].find((b) => b.textContent === "Remove")?.click());
    await act(async () => button("Add a point")?.click());
    // A new point shows the id Keel will give it, and can't be saved empty.
    expect(drafts().map((draft) => draft.dataset.contractDraft)).toEqual(["K1", "K2", "K4"]);
    expect(drafts()[2].textContent).toContain("new");
    expect(button("Save")?.disabled).toBe(true);
    const [when, then] = drafts()[2].querySelectorAll("textarea");
    await write(when, "the session already exists");
    await write(then, "app answers 409");
    expect(button("Save")?.disabled).toBe(true);
    await tick(drafts()[2], "app");
    await act(async () => button("Save")?.click());
    const saved: ContractEdit[] = [
      { id: "K1", when: "someone logs in on valora", then: "valora posts /session with token and expires", projects: ["valora"] },
      { id: "K2", when: "app gets /session with token", then: "it answers 201", projects: ["app"] },
      { id: null, when: "the session already exists", then: "app answers 409", projects: ["app"] },
    ];
    expect(calls).toEqual([{ command: "joint_contract_set", args: { changeId: "plan", points: saved } }]);
    expect(drafts()).toHaveLength(0);
    expect(document.querySelectorAll("[data-contract]")).toHaveLength(3);
  });

  it("says what's wrong with a correction and keeps it open as the person left it", async () => {
    shown = corrected;
    refuse = { code: "blocked", reason: { code: "contractNotValid", problems: [{ code: "contractUnknownProject", id: "K1", position: 1, project: "back-adm" }] }, detail: null };
    await render();
    await act(async () => button("Correct")?.click());
    await write(drafts()[0].querySelectorAll("textarea")[0], "someone logs in");
    await act(async () => button("Save")?.click());
    expect(page()).toContain("The correction wasn't saved: K1 names back-adm, which isn't one of the projects you chose.");
    expect(drafts()[0].querySelector("textarea")?.value).toBe("someone logs in");
    // Cancelling leaves the contract as it was.
    await act(async () => button("Cancel")?.click());
    expect(drafts()).toHaveLength(0);
    expect(page()).not.toContain("wasn't saved");
    expect(calls).toHaveLength(1);
  });

  it("adds a first point to an agreement without any", async () => {
    shown = { ...corrected, contract: [], contractNext: 1 };
    await render();
    await act(async () => button("Add a point the parts agree on")?.click());
    await act(async () => button("Add a point")?.click());
    expect(drafts().map((draft) => draft.dataset.contractDraft)).toEqual(["K1"]);
    // Each project with a part is there to name.
    expect([...drafts()[0].querySelectorAll("label")].map((label) => label.textContent).slice(2)).toEqual(["valora", "app"]);
  });

  it("doesn't offer to correct anything before it's agreed", async () => {
    shown = { ...agreed, agreed: null, parts: [], readOnly: [], checks: [], contractNext: null };
    await render();
    expect(button("Correct")).toBeUndefined();
    expect(button("Add a point the parts agree on")).toBeUndefined();
  });

  it("marks the parts specified with a point as it no longer is, rather than what they found of it", async () => {
    const earlier = contract.map((point) => (point.id === "K1" ? { ...point, parts: point.parts.map((part) => ({ ...part, result: null, earlier: true })) } : point));
    shown = { ...corrected, contract: earlier, parts: [{ ...corrected.parts[0], part: part(1, { contractChanged: ["K1"] }) }, corrected.parts[1]] };
    await render();
    const points = [...document.querySelectorAll("[data-contract]")].map((point) => point.textContent ?? "");
    expect(points[0]).toContain("Part 1, valora: Specified with an earlier version");
    expect(points[0]).not.toContain("Met");
    expect(document.querySelector("[data-contract-changed]")?.textContent).toBe("Specified with an earlier version of what the parts agree on (K1). It takes the current one when it starts again from Todo.");
    await i18next.changeLanguage("es");
    await render();
    expect(page()).toContain("Parte 1, valora: Se especificó con una versión anterior");
    expect(button("Corregir")).toBeDefined();
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
    shown = { ...agreed, contract };
    await render();
    expect(page()).toContain("Cambio en varios proyectos");
    expect(page()).toContain("Parte 2 de 2");
    expect(page()).toContain("Para probar juntos");
    expect(page()).toContain("Lo que las partes acuerdan entre sí");
    expect(page()).toContain("Parte 1, valora: Se cumple");
    expect(page()).toContain("Parte 2, app: Sin revisar aún");
    expect(button("No esperar")).toBeDefined();
  });
});
