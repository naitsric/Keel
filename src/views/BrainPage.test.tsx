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

  it("that came from an approval reads as it always has", async () => {
    await open();
    const page = document.body.textContent ?? "";
    expect(page).toContain("Use the existing helper");
    expect(page).not.toContain("You were asked: Use");
    expect(page).not.toContain("You answered: Use");
  });
});
