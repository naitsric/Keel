import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { AuthStatus, Preflight, Project, Workflow, WorkflowStep } from "../lib/api";
import { useApp } from "../lib/store";
import { NewRunDialog } from "./NewRunDialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const SIGNED_IN = { loggedIn: true, email: "you@example.com", subscription: "max" } as AuthStatus;
const CONNECTED = { id: "p1", name: "demo", githubAccount: "you-work", githubRepo: "acme/app" } as Project;
const DELIVERS: WorkflowStep[] = [
  { type: "approval", id: "human" },
  { type: "deliver", id: "pr" },
];
const STAYS_LOCAL: WorkflowStep[] = [{ type: "check", id: "check", run: "make test" }];

describe("the accounts in the new run dialog", () => {
  let root: Root;

  const show = async (auth: AuthStatus | null, project: Project, steps: WorkflowStep[]) => {
    const workflow: Workflow = { id: "builtin:full", name: "Full", builtin: true, steps };
    mockIPC((command) => (command === "workflows_list" ? [workflow] : null));
    useApp.setState({ newRunOpen: true, preflight: { auth } as Preflight, overview: { ...useApp.getState().overview, projects: [project] } });
    await act(async () => root.render(<NewRunDialog />));
  };
  const page = () => document.body.textContent ?? "";

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ newRunOpen: false, preflight: null });
  });

  it("says who it runs as and where it delivers, in one line", async () => {
    await show(SIGNED_IN, CONNECTED, DELIVERS);
    expect(page()).toContain("Claude Code: you@example.com (max) · GitHub: you-work on acme/app");
  });

  it("leaves GitHub out when the workflow doesn't deliver", async () => {
    await show(SIGNED_IN, CONNECTED, STAYS_LOCAL);
    expect(page()).toContain("Claude Code: you@example.com (max)");
    expect(page()).not.toContain("you-work");
    expect(page()).not.toContain("acme/app");
  });

  it("leaves the missing account to the warning that's already there", async () => {
    await show(SIGNED_IN, { ...CONNECTED, githubAccount: null }, DELIVERS);
    expect(page()).toContain("Claude Code: you@example.com (max)");
    expect(page()).not.toContain("GitHub: ");
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(i18next.t("newRun.needsGithub"));
  });

  it("says nothing about a Claude Code account when nobody is signed in", async () => {
    await show({ loggedIn: false } as AuthStatus, CONNECTED, STAYS_LOCAL);
    expect(page()).not.toContain("Claude Code:");
    await show(null, CONNECTED, STAYS_LOCAL);
    expect(page()).not.toContain("Claude Code:");
  });

  it("still says it's signed in when there's no email to show", async () => {
    await show({ loggedIn: true, email: null, subscription: null } as AuthStatus, CONNECTED, STAYS_LOCAL);
    expect(page()).toContain(i18next.t("session.accountNoEmail"));
  });

  it("comes in Spanish too", async () => {
    await i18next.changeLanguage("es");
    await show(SIGNED_IN, CONNECTED, DELIVERS);
    expect(page()).toContain("GitHub: you-work en acme/app");
  });
});
