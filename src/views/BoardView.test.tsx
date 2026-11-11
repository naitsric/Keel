import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { BoardCard } from "../lib/api";
import { useApp } from "../lib/store";
import { TaskDialog } from "./BoardView";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const card = (extra: Partial<BoardCard> = {}): BoardCard => ({ id: "c1", projectId: "p1", title: "Log in through Valora", body: "", issue: null, column: "backlog", position: 0, runId: null, runState: null, reason: null, prUrl: null, prState: null, prStateAt: null, prCi: null, prCiFailed: [], rejection: null, startBlocked: null, createdAt: "2026-11-10T00:00:00Z", jointChangeId: null, alsoTouches: [], part: null, ...extra });

describe("a card that touches other projects", () => {
  let root: Root;
  let calls: { command: string; args: unknown }[];
  let saved: number;

  const show = (shown: BoardCard | null) => act(() => root.render(<TaskDialog projectId="p1" card={shown} onClose={() => {}} onSaved={() => saved++} />));
  const page = () => document.body.textContent ?? "";
  const box = (name: string) => [...document.querySelectorAll("label")].find((label) => label.textContent === name)?.querySelector("input") as HTMLInputElement | undefined;
  const submit = () => act(async () => document.querySelector("form")?.requestSubmit());

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    calls = [];
    saved = 0;
    mockIPC((command, args) => {
      calls.push({ command, args });
      return command === "card_create" ? "new" : null;
    });
    useApp.setState({
      overview: {
        ...useApp.getState().overview,
        projects: [
          { id: "p1", name: "app" },
          { id: "p2", name: "valora" },
          { id: "p3", name: "dupplaBack" },
        ] as never,
      },
    });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
  });

  it("offers every other project, and a new card that touches one says so once it exists", async () => {
    show(null);
    expect(page()).toContain("Also touches");
    expect(box("app")).toBeUndefined();
    const title = document.querySelector("input:not([type])") as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(title, "Log in through Valora");
      title.dispatchEvent(new Event("input", { bubbles: true }));
    });
    act(() => box("valora")?.click());
    await submit();
    expect(calls.map((call) => call.command)).toEqual(["card_create", "card_set_projects"]);
    expect(calls[1].args).toEqual({ cardId: "new", projects: ["p2"] });
    expect(saved).toBe(1);
  });

  it("only says what changed, and taking every project away makes it an ordinary card", async () => {
    show(card({ jointChangeId: "c1", alsoTouches: ["p2"] }));
    expect(box("valora")?.checked).toBe(true);
    await submit();
    expect(calls.map((call) => call.command)).toEqual(["card_update"]);

    // Saving closes the dialog, so the next change is made in a new one.
    act(() => root.unmount());
    root = createRoot(document.body.appendChild(document.createElement("div")));
    calls = [];
    show(card({ jointChangeId: "c1", alsoTouches: ["p2"] }));
    act(() => box("valora")?.click());
    await submit();
    expect(calls[1]).toEqual({ command: "card_set_projects", args: { cardId: "c1", projects: [] } });
  });

  it("isn't offered on a part, which is already one project's share", () => {
    show(card({ jointChangeId: "plan", part: { changeId: "plan", position: 1, total: 2, waitsFor: null, warnings: [] } }));
    expect(page()).not.toContain("Also touches");
  });
});
