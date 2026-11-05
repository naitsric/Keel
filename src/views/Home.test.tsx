import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { AuthStatus, Preflight, Project } from "../lib/api";
import { useApp } from "../lib/store";
import { Home } from "./Home";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const DAY = 24 * 60 * 60 * 1000;
const inDays = (days: number) => new Date(Date.now() + days * DAY).toISOString();

describe("the session's warning on the home page", () => {
  let root: Root;

  const show = (auth: Partial<AuthStatus>) => {
    const preflight = { auth: { loggedIn: true, email: "you@example.com", ...auth }, ignoredEnv: [], blocker: null, version: null } as unknown as Preflight;
    useApp.setState({ preflight, overviewLoaded: true, overview: { ...useApp.getState().overview, projects: [{ id: "p1", checkCommand: "make test" } as Project] } });
    act(() => root.render(<Home />));
  };
  const warning = () => [...document.querySelectorAll(".text-warning")].map((node) => node.textContent).join("");

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC(() => null);
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ preflight: null, overviewLoaded: false });
  });

  it("warns when the session ends within a week, with the date", () => {
    show({ expiresAt: inDays(3) });
    expect(warning()).toContain("Your session expires on ");
  });

  it("says so in Spanish too", async () => {
    await i18next.changeLanguage("es");
    show({ expiresAt: inDays(3) });
    expect(warning()).toContain("Tu sesión vence el ");
  });

  it("says when it has already ended", () => {
    show({ expiresAt: inDays(-1) });
    expect(warning()).toContain("Your session expired on ");
  });

  it("stays quiet when there's time, or when the date is missing or can't be read", () => {
    for (const expiresAt of [inDays(30), null, "garbage 1", "1801234567"]) {
      show({ expiresAt });
      expect(warning(), String(expiresAt)).toBe("");
    }
  });

  it("leaves a signed-out session to the blocker", () => {
    show({ loggedIn: false, expiresAt: inDays(3) });
    expect(warning()).toBe("");
  });
});
