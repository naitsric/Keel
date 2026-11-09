// The run page around its diagram: what a node shows, what it can't do, and whose focus it isn't.
import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import type { Attempt, AttemptState, AuditEvent, Run, WorkflowStep } from "../lib/api";
import { useApp } from "../lib/store";
import { RunDetail } from "./RunDetail";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let serial = 0;
const attempt = (stepId: string, state: AttemptState, fields: Partial<Attempt> = {}) => ({ id: `${stepId}-${serial++}`, runId: "r1", stepId, kind: "agent", role: "worker", tools: "readOnly", state, round: 0, number: 1, output: null, reason: null, resultText: null, filesChanged: null, runFiles: null, modelRequested: "sonnet", modelActual: null, prompt: `The prompt of ${stepId}`, settings: {}, ...fields }) as Attempt;

const agent = (id: string, role?: object) => ({ type: "agent", id, model: "sonnet", tools: "readOnly", prompt: "From the workflow", ...(role ? { role } : {}) });
const WORKFLOW = [
  agent("spec"),
  agent("builder"),
  { type: "check", id: "check", run: "make test" },
  { type: "parallel", id: "reviews", steps: [{ id: "security", model: "sonnet", role: { kind: "reviewer" } }, { id: "ux", model: "sonnet", role: { kind: "reviewer" } }] },
  agent("approver", { kind: "approver", reworkTo: "builder", maxRounds: 2 }),
  { type: "approval", id: "human" },
  { type: "deliver", id: "pr" },
] as unknown as WorkflowStep[];

const built = () => [attempt("spec", "succeeded"), attempt("builder", "succeeded", { filesChanged: ["calc.py"], runFiles: ["calc.py"] })];
const human = () => attempt("human", "waitingApproval", { kind: "approval", tools: "none", modelRequested: "" });

/** What's allowed to be called by looking around a run. Anything else would be doing something. */
const READS = ["run_get", "run_look_at_pr", "run_attempts", "attempt_events", "attempt_raw_check", "run_adds_secret", "run_proposals", "attempt_diff"];

describe("the run page's flow", () => {
  let root: Root;
  let attempts: Attempt[];
  let events: Record<string, AuditEvent[]>;
  let run: Partial<Run>;
  let calls: string[];
  let lookFails: boolean;

  const settle = (ms = 20) => act(() => vi.advanceTimersByTimeAsync(ms));
  const open = async () => {
    await act(async () => root.render(<RunDetail runId="r1" />));
    await settle();
  };
  /** What the backend reporting a change does. */
  const update = async (next?: Attempt[]) => {
    if (next) attempts = next;
    act(() => useApp.setState((state) => ({ revision: state.revision + 1 })));
    await settle();
  };
  const node = (id: string) => document.querySelector<HTMLButtonElement>(`button[data-flow-id="${id}"]`) as HTMLButtonElement;
  const nodes = () => [...document.querySelectorAll<HTMLButtonElement>("button[data-flow-item]")];
  const selected = () => nodes().filter((n) => n.getAttribute("aria-current") === "true").map((n) => n.dataset.flowId);
  const page = () => document.body.textContent ?? "";
  const radio = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="radio"]')].find((r) => r.textContent?.startsWith(text));
  const choose = async (button: HTMLElement | undefined) => {
    act(() => button?.click());
    await settle();
  };
  const showPrompt = () => choose(radio("Prompt"));
  const note = () => document.querySelector("textarea");
  const pressEnterOn = async (button: HTMLButtonElement) => {
    act(() => button.focus());
    const event = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    act(() => void button.dispatchEvent(event));
    if (!event.defaultPrevented) act(() => button.click());
    await settle();
  };

  beforeEach(async () => {
    vi.useFakeTimers();
    await i18next.changeLanguage("en");
    attempts = [...built(), human()];
    events = {};
    run = { id: "r1", projectId: "p1", title: "Fix add", round: 0, workflow: WORKFLOW, branch: "keel/abc", finishedAt: null, prUrl: null };
    calls = [];
    lookFails = false;
    mockIPC((command, args) => {
      calls.push(command);
      if (command === "run_look_at_pr" && lookFails) throw { code: "internal", reason: null, detail: "no" };
      if (command === "run_get") return run;
      if (command === "run_attempts") return attempts;
      if (command === "attempt_events") return events[(args as { attemptId: string }).attemptId] ?? [];
      if (command === "attempt_raw_check") return "intact";
      if (command === "run_adds_secret") return false;
      if (command === "run_proposals") return [];
      if (command === "attempt_diff") return "";
      return null;
    });
    useApp.setState({ approvalDrafts: {}, revision: 0, overview: { ...useApp.getState().overview, projects: [{ id: "p1", githubRepo: "acme/app", githubAccount: "you-work" }] as never } });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    document.body.replaceChildren();
    vi.useRealTimers();
  });

  describe("what became of the pull request", () => {
    const delivered = (fields: Partial<Run>) => {
      attempts = [attempt("spec", "succeeded"), attempt("builder", "succeeded"), attempt("human", "succeeded"), attempt("pr", "succeeded")];
      run = { ...run, prUrl: "https://github.com/acme/app/pull/12", prNumber: 12, finishedAt: "2026-11-06T09:00:00Z", prState: null, prStateAt: null, ...fields } as Run;
    };

    it("gets asked about when a person opens the run, once, whatever else changes", async () => {
      await open();
      expect(calls.filter((c) => c === "run_look_at_pr")).toEqual(["run_look_at_pr"]);
      await update();
      expect(calls.filter((c) => c === "run_look_at_pr")).toEqual(["run_look_at_pr"]);
    });

    it("stays quiet if asking fails", async () => {
      lookFails = true;
      await open();
      expect(calls).toContain("run_look_at_pr");
      expect(document.querySelector('[role="alert"][class*="text-danger"]')).toBeNull();
      expect(page()).toContain("Fix add");
    });

    it("says nothing until Keel knows, and goes on saying what it did before", async () => {
      delivered({});
      await open();
      expect(page()).not.toContain("merged");
      expect(page()).toContain("Review and merge the draft on GitHub");
    });

    it("says when it was merged, and that nothing's left to decide", async () => {
      delivered({ prState: "merged", prStateAt: "2026-11-06T10:00:00Z" });
      await open();
      expect(page()).toMatch(/Pull request merged on .*2026/);
      expect(page()).toContain("Nothing. The pull request was merged.");
      expect(page()).not.toContain("Review and merge");
    });

    it("says when it was closed without merging", async () => {
      delivered({ prState: "closed", prStateAt: "2026-11-06T10:00:00Z" });
      await open();
      expect(page()).toMatch(/Pull request closed without merging on .*2026/);
      expect(page()).toContain("Nothing. The pull request was closed without merging.");
    });

    it("doesn't call one that's ready for review a draft", async () => {
      delivered({ prState: "open", prStateAt: "2026-11-06T10:00:00Z" });
      await open();
      expect(page()).toContain("Pull request open");
      expect(page()).toContain("Review and merge it on GitHub");
      expect(page()).not.toContain("the draft");
    });

    it("says so while it's still a draft", async () => {
      delivered({ prState: "draft", prStateAt: "2026-11-06T10:00:00Z" });
      await open();
      expect(page()).toContain("Pull request still a draft");
      expect(page()).toContain("Review and merge the draft on GitHub");
    });
  });

  describe("a workflow that fits the change", () => {
    const decided = (attemptId: string, data: Record<string, unknown>): AuditEvent => ({ seq: 50, attemptId, at: "", kind: "sizeDecided", data });
    const fitting = () => {
      run = { ...run, workflow: WORKFLOW };
      const asSmall = { reason: { code: "treatedAsSmall", lines: 3, files: 1 } };
      return [...built(), attempt("check", "succeeded", { kind: "check", tools: "check", prompt: "make test" }), attempt("security", "skipped", { role: "reviewer", ...asSmall }), attempt("ux", "skipped", { role: "reviewer", ...asSmall }), attempt("approver", "skipped", { role: "approver", ...asSmall }), human()];
    };

    it("offers what was skipped, and asking for it is the one thing that changes anything", async () => {
      attempts = fitting();
      await open();
      expect(page()).toContain("Keel treated this as a small change: 3 changed lines in 1 file.");
      expect(node("security").textContent).toContain("Small change");
      await settle(900);
      calls = [];
      const runNow = [...document.querySelectorAll("button")].find((b) => b.textContent === "Run them now");
      await act(async () => runNow?.click());
      await settle();
      expect(calls).toContain("approval_run_skipped");
      expect(calls).not.toContain("approval_answer");
    });

    it("finds why it went to full review on whichever step it was recorded", async () => {
      attempts = [...built(), attempt("check", "succeeded", { kind: "check", tools: "check", prompt: "make test" }), attempt("security", "succeeded", { id: "security-a", role: "reviewer" }), attempt("ux", "succeeded", { role: "reviewer" }), attempt("approver", "succeeded", { role: "approver" }), human()];
      events = { "security-a": [decided("security-a", { small: false, because: [{ code: "sensitiveFiles", files: [".github/workflows/ci.yml"], total: 1 }], round: 0 })] };
      await open();
      await settle();
      expect(page()).toContain("It went to full review because:");
      expect(page()).toContain("it touches .github/workflows/ci.yml");
    });

    it("says what it found in the step's own activity too", async () => {
      attempts = fitting();
      const reviewer = attempts.find((a) => a.stepId === "security") as Attempt;
      events = { [reviewer.id]: [{ seq: 49, attemptId: reviewer.id, at: "", kind: "skipped", data: { reason: { code: "treatedAsSmall" } } }, decided(reviewer.id, { small: true, lines: 3, files: 1, round: 0 })] };
      await open();
      await act(async () => node("security").click());
      await settle();
      expect(page()).toContain("Skipped: Keel treated this as a small change");
      expect(page()).toContain("Keel treated this as a small change: 3 changed lines in 1 file");
    });

    it("once a person has asked, says what's going on and offers nothing", async () => {
      attempts = [...built(), attempt("security", "skipped", { role: "reviewer", reason: { code: "treatedAsSmall", lines: 3, files: 1 } }), attempt("human", "postponed", { kind: "approval", tools: "none", modelRequested: "" }), attempt("security", "running", { role: "reviewer", number: 2 })];
      await open();
      expect(document.querySelector('[role="alert"]')).toBeNull();
      expect(node("human").textContent).toContain("Asked for more");
      expect(page()).not.toContain("Run them now");
      expect(page()).not.toContain("so these didn't run");
      expect([...document.querySelectorAll("button")].some((b) => b.textContent === "Retry")).toBe(false);
    });
  });

  describe("what opens", () => {
    it("is the step that needs the person", async () => {
      await open();
      expect(selected()).toEqual(["human"]);
    });

    it("is what's working, if nothing needs the person", async () => {
      attempts = [attempt("spec", "succeeded"), attempt("builder", "running")];
      await open();
      expect(selected()).toEqual(["builder"]);
    });

    it("shows steps that haven't started, as not started", async () => {
      attempts = [attempt("spec", "running")];
      await open();
      expect(node("builder").getAttribute("aria-label")).toContain("Not started");
      expect(node("pr").getAttribute("aria-label")).toContain("Not started");
    });

    it("shows the steps of a group and of the rest, all of them", async () => {
      await open();
      expect(nodes().map((n) => n.dataset.flowId)).toEqual(["spec", "builder", "check", "security", "ux", "approver", "human", "pr"]);
    });
  });

  describe("selecting a node", () => {
    it("shows that step's detail", async () => {
      await open();
      await choose(node("spec"));
      await showPrompt();
      expect(selected()).toEqual(["spec"]);
      expect(page()).toContain("The prompt of spec");
    });

    it("works the same from the keyboard", async () => {
      await open();
      await pressEnterOn(node("builder"));
      await showPrompt();
      expect(selected()).toEqual(["builder"]);
      expect(page()).toContain("The prompt of builder");
    });

    it("says so when the step hasn't started, without any tabs to read", async () => {
      await open();
      await choose(node("pr"));
      expect(selected()).toEqual(["pr"]);
      expect(page()).toContain("This step hasn't started yet.");
      expect(radio("Activity")).toBeUndefined();
    });

    it("stays on that step as the run moves on, and shows its new attempt", async () => {
      attempts = [attempt("spec", "succeeded"), attempt("builder", "succeeded")];
      await open();
      await choose(node("spec"));
      await update([...attempts, attempt("check", "running", { kind: "check" })]);
      expect(selected()).toEqual(["spec"]);
    });

    it("shows a step's first attempt once it starts, if it was selected before that", async () => {
      attempts = [attempt("spec", "running")];
      await open();
      await choose(node("builder"));
      expect(page()).toContain("This step hasn't started yet.");
      await update([attempt("spec", "succeeded"), attempt("builder", "running")]);
      expect(selected()).toEqual(["builder"]);
      await showPrompt();
      expect(page()).toContain("The prompt of builder");
    });

    it("keeps the same tab across steps when that step has it", async () => {
      await open();
      await choose(node("spec"));
      await showPrompt();
      await choose(node("builder"));
      expect(page()).toContain("The prompt of builder");
    });
  });

  describe("live", () => {
    it("follows the run while nobody selected anything", async () => {
      attempts = [attempt("spec", "running")];
      await open();
      expect(selected()).toEqual(["spec"]);
      await update([attempt("spec", "succeeded"), attempt("builder", "running")]);
      expect(selected()).toEqual(["builder"]);
      expect(node("builder").getAttribute("aria-label")).toContain("Running");
      await update([attempt("spec", "succeeded"), attempt("builder", "succeeded"), human()]);
      expect(selected()).toEqual(["human"]);
      expect(node("human").getAttribute("aria-label")).toContain("Waiting for you");
    });
  });

  describe("what comes first", () => {
    const summary = () => [...document.querySelectorAll("*")].find((e) => e.children.length === 0 && e.textContent === "What it did") as Element;
    const flowIsFirst = () => Boolean(node("spec").compareDocumentPosition(summary()) & Node.DOCUMENT_POSITION_FOLLOWING);

    it("is the flow and its detail while the run is going", async () => {
      attempts = [...built(), attempt("check", "running", { kind: "check", tools: "check" })];
      await open();
      expect(flowIsFirst()).toBe(true);
    });

    it("stays the flow between two steps, and while a step is stalled", async () => {
      attempts = built();
      await open();
      expect(flowIsFirst()).toBe(true);
      await update([...built(), attempt("check", "stalled", { kind: "check", tools: "check" })]);
      expect(flowIsFirst()).toBe(true);
    });

    it("is what happened, once the run ended or needs the person", async () => {
      attempts = built();
      await open();
      for (const state of ["failed", "blocked", "stopped", "interrupted"] as AttemptState[]) {
        await update(built());
        expect(flowIsFirst()).toBe(true);
        await update([...built(), attempt("check", state, { kind: "check", tools: "check" })]);
        expect(flowIsFirst()).toBe(false);
      }
      await update(built());
      expect(flowIsFirst()).toBe(true);
      run = { ...run, finishedAt: "2026-01-01T00:10:00Z" };
      await update();
      expect(flowIsFirst()).toBe(false);
    });

    it("is the approval card when it asks, with the flow right below", async () => {
      attempts = [...built(), human()];
      await open();
      expect(note()?.compareDocumentPosition(node("human")) ?? 0).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
      // The card says what it did itself. The summary would say it twice.
      expect(page().split("What it did").length - 1).toBe(1);
    });

    it("doesn't take the flow away from whoever is on it when the run ends", async () => {
      attempts = [...built(), attempt("check", "running", { kind: "check", tools: "check" })];
      await open();
      const before = node("builder");
      act(() => before.focus());
      run = { ...run, finishedAt: "2026-01-01T00:10:00Z" };
      await update([...built(), attempt("check", "succeeded", { kind: "check", tools: "check" })]);
      expect(node("builder")).toBe(before);
      expect(document.activeElement).toBe(before);
      expect(flowIsFirst()).toBe(false);
    });
  });

  describe("nothing about an attempt becomes unreachable", () => {
    const rounds = () => {
      const first = attempt("builder", "succeeded", { prompt: "Round one prompt" });
      const second = attempt("builder", "running", { round: 1, number: 2, prompt: "Round two prompt" });
      return [attempt("spec", "succeeded"), first, attempt("approver", "succeeded", { role: "approver", output: { decision: "rework" } as Attempt["output"] }), second];
    };

    it("offers each round of a step, starting on the latest", async () => {
      attempts = rounds();
      run = { ...run, round: 1 };
      await open();
      await showPrompt();
      expect(page()).toContain("Round two prompt");
      expect(radio("Round 2")?.getAttribute("aria-checked")).toBe("true");
      expect(radio("Round 1")?.getAttribute("aria-checked")).toBe("false");
    });

    it("shows the earlier round when it's chosen, and keeps showing it through updates", async () => {
      attempts = rounds();
      run = { ...run, round: 1 };
      await open();
      await showPrompt();
      await choose(radio("Round 1"));
      expect(page()).toContain("Round one prompt");
      expect(page()).not.toContain("Round two prompt");
      await update();
      expect(page()).toContain("Round one prompt");
      expect(selected()).toEqual(["builder"]);
    });

    it("goes back to the latest when the node is clicked again", async () => {
      attempts = rounds();
      run = { ...run, round: 1 };
      await open();
      await showPrompt();
      await choose(radio("Round 1"));
      await choose(node("builder"));
      expect(page()).toContain("Round two prompt");
    });

    it("offers retries the same way, and asks for each one's own audit", async () => {
      const first = attempt("builder", "failed", { prompt: "First try" });
      const second = attempt("builder", "succeeded", { number: 2, prompt: "Second try" });
      attempts = [attempt("spec", "succeeded"), first, second];
      await open();
      await choose(node("builder"));
      await showPrompt();
      expect(page()).toContain("Second try");
      await choose(radio("Attempt 1"));
      expect(page()).toContain("First try");
      expect(calls).toContain("attempt_events");
    });

    it("names the model of the attempt on show, not only the latest's", async () => {
      attempts = [attempt("builder", "failed", { modelActual: "claude-first" }), attempt("builder", "running", { number: 2, modelActual: "claude-second" })];
      await open();
      expect(page()).toContain("claude-second");
      await choose(radio("Attempt 1"));
      expect(page()).toContain("claude-first");
    });

    it("tells the state of each choice in words", async () => {
      attempts = rounds();
      run = { ...run, round: 1 };
      await open();
      expect(radio("Round 1")?.textContent).toContain("Finished");
      expect(radio("Round 2")?.textContent).toContain("Running");
    });

    it("offers no choice with only one attempt", async () => {
      await open();
      await choose(node("spec"));
      expect(document.querySelector('[role="radiogroup"][aria-label="Which attempt to show"]')).toBeNull();
    });

    it("reaches every attempt of a messy run from some node", async () => {
      const stray = attempt("from-an-older-workflow", "failed", { prompt: "The stray prompt" });
      attempts = [...rounds(), attempt("builder", "failed", { round: 1, number: 3 }), stray];
      run = { ...run, round: 1 };
      await open();
      await choose(node("from-an-older-workflow"));
      await showPrompt();
      expect(page()).toContain("The stray prompt");
      await choose(node("builder"));
      expect(document.querySelector('[role="radiogroup"][aria-label="Which attempt to show"]')?.querySelectorAll('[role="radio"]')).toHaveLength(3);
    });
  });

  describe("runs that aren't the usual shape", () => {
    it("shows a Try it run: setup, then the check", async () => {
      run = { ...run, workflow: [{ type: "setup", id: "setup", run: "pnpm i" }, { type: "check", id: "try-check", run: "pnpm test" }] };
      attempts = [attempt("setup", "succeeded", { kind: "setup", modelRequested: "" }), attempt("try-check", "failed", { kind: "check", modelRequested: "" })];
      await open();
      expect(nodes().map((n) => n.dataset.flowId)).toEqual(["setup", "try-check"]);
      expect(selected()).toEqual(["try-check"]);
      expect(radio("Output")).toBeDefined();
    });

    it("shows a single step", async () => {
      run = { ...run, workflow: [agent("builder")] as unknown as WorkflowStep[] };
      attempts = [attempt("builder", "running")];
      await open();
      expect(nodes().map((n) => n.dataset.flowId)).toEqual(["builder"]);
      expect(selected()).toEqual(["builder"]);
    });

    it.each([
      ["null, as a bad column comes back", null],
      ["an old run's empty list", []],
      ["something else", { what: "is this" }],
      ["a step it doesn't know", [{ type: "teleport", id: "x" }]],
    ])("lists the attempts when the workflow is %s", async (_, workflow) => {
      run = { ...run, workflow: workflow as unknown as WorkflowStep[] };
      attempts = [attempt("first", "succeeded", { prompt: "First prompt" }), attempt("second", "running", { prompt: "Second prompt" }), attempt("second", "failed", { number: 2, prompt: "Retry prompt" })];
      await open();
      expect(nodes().map((n) => n.dataset.flowId)).toEqual(["first", "second"]);
      expect(page()).toContain("can't read this run's workflow");
      await choose(node("first"));
      await showPrompt();
      expect(page()).toContain("First prompt");
      await choose(node("second"));
      expect(page()).toContain("Retry prompt");
      await choose(radio("Attempt 1"));
      expect(page()).toContain("Second prompt");
    });

    it("shows only the top bar before there are any attempts, as before", async () => {
      attempts = [];
      await open();
      expect(nodes()).toEqual([]);
    });
  });

  describe("safety", () => {
    it("never answers anything or does anything, clicked or pressed, whichever node", async () => {
      attempts = [attempt("spec", "succeeded"), attempt("builder", "failed"), human()];
      await open();
      await settle(800);
      for (const button of nodes()) {
        await choose(button);
        await pressEnterOn(button);
      }
      expect(calls.filter((command) => !READS.includes(command))).toEqual([]);
    });

    it("doesn't touch what the card is waiting on", async () => {
      await open();
      act(() => (note() as HTMLTextAreaElement).focus());
      await pressEnterOn(node("human"));
      await pressEnterOn(node("spec"));
      expect(calls).not.toContain("approval_answer");
      // The card still needs its own answer, and it's still there.
      expect(page()).toContain("Should this become a pull request?");
    });

    it("keeps the card above the diagram, where nobody has to look for it", async () => {
      await open();
      const card = document.querySelector('[role="alert"]') as HTMLElement;
      expect(card.compareDocumentPosition(node("spec")) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });

    it("doesn't arm Approve any sooner", async () => {
      await open();
      await choose(node("human"));
      const approve = [...document.querySelectorAll("button")].find((b) => b.textContent === "Approve and open a draft");
      await settle(100);
      expect(approve?.disabled).toBe(true);
    });

    it("never asks for the unmasked workflows", async () => {
      await open();
      for (const button of nodes()) await choose(button);
      expect(calls).not.toContain("workflows_list");
    });

    it("shows the prompt as it came, masked", async () => {
      attempts = [attempt("builder", "succeeded", { prompt: "Use the key ****masked****" })];
      await open();
      await showPrompt();
      expect(page()).toContain("Use the key ****masked****");
    });
  });

  describe("focus", () => {
    let focused: Element[];
    let scrollIntoView: ReturnType<typeof vi.fn>;

    beforeEach(() => {
      focused = [];
      const realFocus = HTMLElement.prototype.focus;
      vi.spyOn(HTMLElement.prototype, "focus").mockImplementation(function (this: HTMLElement, options?: FocusOptions) {
        focused.push(this);
        realFocus.call(this, options);
      });
      scrollIntoView = vi.fn();
      Element.prototype.scrollIntoView = scrollIntoView as unknown as Element["scrollIntoView"];
    });

    afterEach(() => void vi.restoreAllMocks());

    const fromTheFlow = () => focused.filter((element) => element.hasAttribute("data-flow-item"));

    it("is on the note when a waiting run opens, not on the diagram", async () => {
      await open();
      expect(document.activeElement).toBe(note());
      expect(fromTheFlow()).toEqual([]);
    });

    it("stays on the note through a live update", async () => {
      await open();
      await update();
      await settle(1000);
      await update([...attempts]);
      expect(document.activeElement).toBe(note());
      expect(fromTheFlow()).toEqual([]);
    });

    it("stays on the note while what's around it changes", async () => {
      await open();
      const before = note();
      await update([...built(), attempt("security", "skipped"), attempts[attempts.length - 1]]);
      expect(note()).toBe(before);
      expect(document.activeElement).toBe(before);
      expect(fromTheFlow()).toEqual([]);
    });

    it("goes to the note when a card appears on a run that was working", async () => {
      attempts = [attempt("spec", "running")];
      await open();
      expect(document.activeElement).toBe(document.body);
      await update([...built(), human()]);
      expect(document.activeElement).toBe(note());
      expect(fromTheFlow()).toEqual([]);
    });

    it("takes none at all on a run with no card, loading or updating", async () => {
      attempts = [attempt("spec", "running")];
      await open();
      await update([attempt("spec", "succeeded"), attempt("builder", "running")]);
      expect(document.activeElement).toBe(document.body);
      expect(fromTheFlow()).toEqual([]);
    });

    it("never scrolls the page to find a node", async () => {
      await open();
      await update([attempt("spec", "succeeded"), attempt("builder", "running")]);
      await update([attempt("spec", "succeeded"), attempt("builder", "succeeded"), human()]);
      expect(scrollIntoView).not.toHaveBeenCalled();
    });

    it("stays on a node the person went to, through an update", async () => {
      attempts = [attempt("spec", "running")];
      await open();
      act(() => node("spec").focus());
      await update([attempt("spec", "succeeded"), attempt("builder", "running")]);
      expect(document.activeElement).toBe(node("spec"));
    });
  });

  it("says it in Spanish too", async () => {
    await i18next.changeLanguage("es");
    attempts = [attempt("spec", "succeeded"), attempt("builder", "running", { round: 1, number: 2 })];
    run = { ...run, round: 1 };
    await open();
    expect(node("builder").getAttribute("aria-label")).toContain("Ronda 2");
    expect(page()).toContain("Devuelto una vez");
    await choose(node("pr"));
    expect(page()).toContain("Este paso aún no empieza.");
  });
  describe("a step that checks what was asked", () => {
    const output = {
      summary: "A test proves the sum, and nothing covers the rest.",
      criteria: [
        { id: "C1", when: "add(1, 2)", then: "it returns 3", result: "met", evidence: "test_calc.py:1" },
        { id: "C2", when: "one is negative", then: "it still adds", result: "unverified", evidence: "" },
        { id: "C3", when: "it is a decimal", then: "it rounds as today", result: "unverified", evidence: "test_calc.py:9", corrected: "noCheckPassed" },
      ],
    };
    const verifier = (state: AttemptState, fields: Partial<Attempt> = {}) => attempt("verify", state, { role: "verifier", output: output as unknown as Attempt["output"], ...fields });

    it("reads as text in its details, and not as raw JSON", async () => {
      attempts = [verifier("succeeded"), attempt("builder", "running")];
      await open();
      await choose(node("verify"));
      await choose(radio("What it found"));
      expect(page()).toContain("1 of 3 met, 2 not verified");
      expect(page()).toContain("A test proves the sum, and nothing covers the rest.");
      expect(page()).toContain("add(1, 2) → it returns 3");
      expect(page()).toContain("test_calc.py:1");
      expect(page()).toContain("It found nothing that proves this.");
      expect(page()).toContain("It said this was met, but no check passed in this round.");
      expect(page()).not.toContain('"criteria"');
      expect(page()).not.toContain("C1");
    });

    it("says it gave its answer in the activity, and what Keel changed in words", async () => {
      const verify = verifier("succeeded");
      attempts = [verify, attempt("builder", "running")];
      events = {
        [verify.id]: [
          { seq: 1, attemptId: verify.id, at: "2026-11-06T10:00:00Z", kind: "toolCall", data: { id: "t1", name: "StructuredOutput", input: output } },
          { seq: 2, attemptId: verify.id, at: "2026-11-06T10:00:00Z", kind: "verificationCorrected", data: { corrected: [output.criteria[2]], dropped: ["C9"], check: { state: "notRun" } } },
        ],
      };
      await open();
      await choose(node("verify"));
      await choose(radio("Activity"));
      expect(page()).toContain("It wrote down what it found");
      expect(page()).toContain("Keel didn't take its word for one thing");
      expect(page()).toContain("it is a decimal → it rounds as today: It said this was met, but no check passed in this round.");
      expect(page()).toContain("It also answered about something that wasn't asked");
      expect(page()).not.toContain('"summary"');
      expect(page()).not.toContain("C9");
    });

    it("one that couldn't be read says so and offers to try again, with nothing passed off as found", async () => {
      attempts = [verifier("failed", { reason: { code: "noStructuredOutput" }, output: { verdict: "approved" } as unknown as Attempt["output"] })];
      await open();
      expect(page()).toContain("It finished without the answer it was asked for.");
      expect(page()).toContain("Try again");
      expect(page()).not.toContain("What it found");
    });

    it("an attempt stopped with an answer in hand shows nothing it said, since nobody corrected it", async () => {
      attempts = [verifier("stopped")];
      await open();
      expect(page()).not.toContain("What it found");
      expect(page()).not.toContain("1 of 3 met");
    });

    it("the summary of a finished run says how it went", async () => {
      attempts = [verifier("succeeded")];
      run = { ...run, finishedAt: "2026-11-06T10:00:00Z" };
      await open();
      expect(page()).toContain("What proves it");
      expect(page()).toContain("1 of 3 met, 2 not verified");
    });
  });
  describe("a step that works out what's asked", () => {
    const output = { summary: "add returns the sum", criteria: [{ id: "C1", when: "add(1, 2)", then: "it returns 3" }], changes: [], limits: ["Its signature stays"], decided: [{ text: "Keeps the rounding" }], questions: [] };
    const specifier = (state: AttemptState, fields: Partial<Attempt> = {}) => attempt("spec", state, { role: "specifier", output: output as unknown as Attempt["output"], ...fields });

    it("reads as text in its details, and not as raw JSON", async () => {
      attempts = [specifier("succeeded"), attempt("builder", "running")];
      await open();
      await choose(node("spec"));
      await choose(radio("What it understood"));
      expect(page()).toContain("add(1, 2) → it returns 3");
      expect(page()).toContain("Its signature stays");
      expect(page()).not.toContain('"criteria"');
    });

    it("says it gave its answer in the activity, instead of printing it", async () => {
      const spec = specifier("succeeded");
      attempts = [spec, attempt("builder", "running")];
      events = { [spec.id]: [{ seq: 1, attemptId: spec.id, at: "2026-11-06T10:00:00Z", kind: "toolCall", data: { id: "t1", name: "StructuredOutput", input: output } }] };
      await open();
      await choose(node("spec"));
      expect(page()).toContain("It wrote down what it understood");
      expect(page()).not.toContain('"summary"');
    });

    it("asks with its card in place of what a finished step would show", async () => {
      attempts = [specifier("waitingAnswers", { output: { ...output, questions: [{ id: "Q1", text: "What then?", options: [{ label: "Yes", recommended: true }, { label: "No", recommended: false }] }] } as unknown as Attempt["output"] })];
      await open();
      expect(page()).toContain("Keel needs a decision to continue");
      expect(page()).toContain("What then?");
      expect(page()).not.toContain("What you need to decide");
    });

    it("finishing with nothing built says what it understood, not that it explained nothing", async () => {
      attempts = [specifier("succeeded")];
      run = { ...run, finishedAt: "2026-11-06T10:00:00Z" };
      await open();
      expect(page()).toContain("add returns the sum");
      expect(page()).not.toContain("It finished without an explanation.");
    });

    it("a person's return reads as that in the activity", async () => {
      const returned = attempt("human", "returned", { kind: "approval", tools: "none", output: { instructions: "Return 0" } as unknown as Attempt["output"] });
      attempts = [...built(), returned];
      events = { [returned.id]: [{ seq: 1, attemptId: returned.id, at: "2026-11-06T10:00:00Z", kind: "returned", data: { instructions: "Return 0", round: 1 } }] };
      await open();
      await choose(node("human"));
      expect(page()).toContain("You sent it back for round 2");
      expect(page()).toContain("Return 0");
    });
  });

  describe("what an assistant did in the person's place", () => {
    const event = (attemptId: string, kind: string, data: Record<string, unknown>, seq = 1): AuditEvent => ({ seq, attemptId, at: "2026-11-06T10:00:00Z", kind, data });
    const returned = (by?: string) => attempt("human", "returned", { kind: "approval", tools: "none", output: { instructions: "Return 0", ...(by ? { by } : {}) } as unknown as Attempt["output"] });
    const left = () => attempt("human", "abandoned", { kind: "approval", tools: "none" });

    it("says which assistant sent the work back, with what it asked", async () => {
      const back = returned("Claude Code");
      attempts = [...built(), back];
      events = { [back.id]: [event(back.id, "returned", { instructions: "Return 0", round: 1, by: "Claude Code" })] };
      await open();
      await choose(node("human"));
      expect(page()).toContain("Claude Code sent it back for round 2");
      expect(page()).toContain("Return 0");
      expect(page()).not.toContain("You sent it back");
    });

    it("says which assistant closed the run, in its activity and in what's left to decide", async () => {
      const closed = left();
      attempts = [...built(), closed];
      run = { ...run, finishedAt: "2026-11-06T10:00:00Z" };
      events = { [closed.id]: [event(closed.id, "abandoned", { by: "Claude Code" })] };
      await open();
      expect(page()).toContain("Claude Code closed this run");
      expect(page()).toContain("Nothing. Claude Code closed this run.");
      expect(page()).not.toContain("You left this run");
      expect(page()).not.toContain("You closed this run");
    });

    it("says which assistant tried a step again, on the attempt it tried again", async () => {
      const failed = attempt("builder", "failed");
      attempts = [attempt("spec", "succeeded"), failed, attempt("builder", "running", { number: 2 })];
      events = { [failed.id]: [event(failed.id, "finished", { state: "failed" }), event(failed.id, "retryRequested", { by: "Claude Code", resume: true }, 2)] };
      await open();
      await choose(radio("Attempt 1"));
      expect(page()).toContain("Claude Code tried it again");
    });

    it("looks just as before when it was the person", async () => {
      const failed = attempt("builder", "failed");
      const closed = left();
      attempts = [attempt("spec", "succeeded"), failed, attempt("builder", "succeeded", { number: 2 }), closed];
      run = { ...run, finishedAt: "2026-11-06T10:00:00Z" };
      events = { [failed.id]: [event(failed.id, "finished", { state: "failed" })], [closed.id]: [event(closed.id, "abandoned", {})] };
      await open();
      expect(page()).toContain("You left this run");
      expect(page()).toContain("Nothing. You closed this run.");
      await choose(node("builder"));
      await choose(radio("Attempt 1"));
      expect(page()).toContain("Final state: Failed");
      expect(page()).not.toContain("tried it again");
    });

    it("says it in Spanish too", async () => {
      await i18next.changeLanguage("es");
      const back = returned("Claude Code");
      attempts = [...built(), back];
      events = { [back.id]: [event(back.id, "returned", { instructions: "Devuelve 0", round: 1, by: "Claude Code" })] };
      await open();
      await choose(node("human"));
      expect(page()).toContain("Claude Code lo devolvió para la ronda 2");
    });

    describe("a task it started only to read", () => {
      const task = { workflow: [agent("task")] as unknown as WorkflowStep[], workflowId: null, startedBy: "Claude Code" };

      it("says it's read-only", async () => {
        run = { ...run, ...task };
        attempts = [attempt("task", "running")];
        await open();
        expect(page()).toContain("Read-only");
        expect(page()).toContain("Started by Claude Code");
      });

      it.each([
        ["a person's single step", { ...task, startedBy: null }],
        ["a run from a saved workflow", { ...task, workflowId: "builtin:full" }],
        ["a single step that builds", { ...task, workflow: [{ ...agent("task"), tools: "build" }] as unknown as WorkflowStep[] }],
      ])("doesn't say so for %s", async (_, fields) => {
        run = { ...run, ...fields };
        attempts = [attempt("task", "running")];
        await open();
        expect(page()).not.toContain("Read-only");
      });
    });
  });
});
