import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import i18next from "../i18n";
import type { ProjectAdded, ProjectProposal } from "../lib/api";
import { useApp } from "../lib/store";
import { ProjectProposals } from "./ProjectProposals";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const proposal = (id: string, fields: Partial<ProjectProposal> = {}): ProjectProposal => ({
  id,
  path: `/Users/you/www/${id}`,
  name: id,
  githubRepo: `octo/${id}`,
  proposedBy: "Claude Code",
  createdAt: "2026-11-06T15:00:00.000Z",
  returning: false,
  gone: false,
  ...fields,
});
const ADDED = { project: { id: "p9", name: "older" }, outcome: "new", map: { status: "started" } } as unknown as ProjectAdded;

describe("a project an assistant proposed", () => {
  let root: Root;
  let waiting: ProjectProposal[];
  let calls: { command: string; args: Record<string, unknown> }[];
  let refuse: unknown;

  const settle = () =>
    act(async () => {
      for (let i = 0; i < 4; i++) await new Promise((resolve) => setTimeout(resolve));
    });
  const page = () => document.body.textContent ?? "";
  const called = (command: string) => calls.filter((call) => call.command === command);
  const button = (name: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === name) as HTMLButtonElement | undefined;
  const click = async (name: string) => {
    await act(async () => button(name)!.click());
    await settle();
  };
  const show = async (proposals: ProjectProposal[]) => {
    waiting = proposals;
    useApp.setState({ overview: { ...useApp.getState().overview, projectProposals: proposals.length } });
    await act(async () => root.render(<ProjectProposals />));
    await settle();
  };

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    waiting = [];
    calls = [];
    refuse = null;
    mockIPC((command, payload) => {
      const args = (payload ?? {}) as Record<string, unknown>;
      calls.push({ command, args });
      switch (command) {
        case "project_proposals_list":
          if (refuse) throw refuse;
          return waiting;
        case "project_proposal_accept":
          if (refuse) throw refuse;
          waiting = waiting.filter((p) => p.id !== args.proposalId);
          return ADDED;
        case "project_proposal_discard":
          waiting = waiting.filter((p) => p.id !== args.proposalId);
          return null;
        case "overview":
          return { ...useApp.getState().overview, projectProposals: waiting.length };
        default:
          return null;
      }
    });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    document.body.replaceChildren();
    useApp.setState({ projectSetup: null, overview: { ...useApp.getState().overview, projectProposals: 0 } });
  });

  it("shows nothing and asks for nothing when none is waiting", async () => {
    await show([]);
    expect(page()).toBe("");
    expect(called("project_proposals_list")).toEqual([]);
  });

  it("says who proposed what, and the bar only opens the review", async () => {
    await show([proposal("older")]);
    expect(page()).toContain("Claude Code proposes adding older");
    await click("Review");
    expect(called("project_proposal_accept")).toEqual([]);
    expect(page()).toContain("/Users/you/www/older");
    expect(page()).toContain("octo/older");
    expect(page()).toContain("Adding it starts an agent");
  });

  it("starts on the oldest, so one arriving later doesn't change what's under the pointer", async () => {
    await show([proposal("newer"), proposal("older")]);
    expect(page()).toContain("Claude Code proposes adding older, and 1 more");
    await click("Review");
    expect(page()).toContain("/Users/you/www/older");
    expect(page()).not.toContain("/Users/you/www/newer");
  });

  it("starts on Not now, so a stray Enter can't add anything", async () => {
    await show([proposal("older")]);
    await click("Review");
    expect(document.activeElement).toBe(button("Not now"));
    await click("Not now");
    expect(called("project_proposal_accept")).toEqual([]);
    expect(called("project_proposal_discard")).toEqual([]);
    expect(page()).toContain("Claude Code proposes adding older");
  });

  it("adding it opens the same setup dialog as adding one by hand", async () => {
    await show([proposal("older")]);
    await click("Review");
    await click("Add project");
    expect(called("project_proposal_accept")[0].args).toEqual({ proposalId: "older", language: "en" });
    expect(useApp.getState().projectSetup).toMatchObject({ projectId: "p9", added: ADDED });
    expect(page()).toBe("");
  });

  it("discarding it adds nothing and opens nothing", async () => {
    await show([proposal("older")]);
    await click("Review");
    await click("Discard");
    expect(called("project_proposal_discard")[0].args).toEqual({ proposalId: "older" });
    expect(called("project_proposal_accept")).toEqual([]);
    expect(useApp.getState().projectSetup).toBeNull();
    expect(page()).toBe("");
  });

  it("a folder that's no longer a repo can only be discarded", async () => {
    await show([proposal("older", { gone: true })]);
    await click("Review");
    expect(button("Add project")!.disabled).toBe(true);
    expect(button("Discard")!.disabled).toBe(false);
    expect(page()).toContain("isn't a git repository");
    expect(page()).not.toContain("Adding it starts an agent");
  });

  it("one that was in Keel before says it comes back, with no new map", async () => {
    await show([proposal("older", { returning: true })]);
    await click("Review");
    expect(page()).toContain("comes back with its history");
    expect(page()).not.toContain("Adding it starts an agent");
  });

  it("says when it has no GitHub repo", async () => {
    await show([proposal("older", { githubRepo: null })]);
    await click("Review");
    expect(page()).toContain("This folder has no GitHub remote");
  });

  it("a failure says why and leaves the proposal where it was", async () => {
    await show([proposal("older")]);
    await click("Review");
    refuse = { code: "notARepo", detail: "/Users/you/www/older" };
    await click("Add project");
    expect(document.querySelector("[role=alert]")?.textContent).toContain("/Users/you/www/older");
    expect(useApp.getState().projectSetup).toBeNull();
    expect(button("Discard")).toBeDefined();
  });

  it("closes the review when the proposal goes away by itself", async () => {
    await show([proposal("older"), proposal("other")]);
    await click("Review");
    expect(page()).toContain("/Users/you/www/other");
    waiting = [proposal("older")];
    await act(async () => useApp.setState((state) => ({ revision: state.revision + 1 })));
    await settle();
    expect(page()).not.toContain("/Users/you/www/other");
  });

  it("reads in Spanish", async () => {
    await i18next.changeLanguage("es");
    await show([proposal("older")]);
    expect(page()).toContain("Claude Code propone agregar older");
    await click("Revisar");
    expect(document.activeElement).toBe(button("Ahora no"));
    expect(page()).toContain("con tu uso de Claude");
  });
});
