import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import type { AttemptState, StepRole, StepSpec, WorkflowStep } from "../lib/api";
import { type Flow, type FlowAttempt, flowOf } from "../lib/flow";
import { FlowDiagram } from "./FlowDiagram";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const spec = (id: string, model = "sonnet", role?: StepRole): StepSpec => ({ id, model, effort: null, tools: "readOnly", prompt: "p", maxTurns: null, maxBudgetUsd: null, ...(role ? { role } : {}) });
const STEPS: WorkflowStep[] = [
  { type: "agent", ...spec("spec", "opus") },
  { type: "agent", ...spec("builder") },
  { type: "check", id: "check", run: "make test" },
  { type: "parallel", id: "reviews", steps: [spec("security", "sonnet", { kind: "reviewer" }), spec("ux", "sonnet", { kind: "reviewer" })] },
  { type: "agent", ...spec("approver", "opus", { kind: "approver", reworkTo: "builder", maxRounds: 2 }) },
  { type: "approval", id: "human" },
  { type: "deliver", id: "pr" },
  { type: "agent", ...spec("learn", "sonnet", { kind: "learner" }) },
];

let serial = 0;
const attempt = (stepId: string, state: AttemptState, fields: Partial<FlowAttempt> = {}): FlowAttempt => ({ id: `a${serial++}`, stepId, kind: "agent", role: "worker", round: 0, number: 1, state, modelRequested: "sonnet", modelActual: null, output: null, ...fields });

describe("the flow diagram", () => {
  let root: Root;
  let chosen: string[];
  let added: number[];
  let addedTo: number[];
  let scrollIntoView: ReturnType<typeof vi.fn>;

  const show = (flow: Flow<FlowAttempt>, props: Partial<Parameters<typeof FlowDiagram<FlowAttempt>>[0]> = {}) =>
    act(() =>
      root.render(
        <div>
          <button id="outside" type="button" />
          <FlowDiagram flow={flow} label="Steps" selected={null} onSelect={(key) => chosen.push(key)} onAdd={(gap) => added.push(gap)} onAddMember={(stage) => addedTo.push(stage)} {...props} />
        </div>,
      ),
    );
  const node = (id: string) => document.querySelector<HTMLButtonElement>(`button[data-flow-id="${id}"]`) as HTMLButtonElement;
  const items = () => [...document.querySelectorAll<HTMLElement>("[data-flow-item]")];
  const press = (key: string, init: KeyboardEventInit = {}) => {
    const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init });
    act(() => void document.activeElement?.dispatchEvent(event));
    return event;
  };
  const scroller = () => document.querySelector('[role="group"][aria-label="Steps"]') as HTMLElement;

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    chosen = [];
    added = [];
    addedTo = [];
    scrollIntoView = vi.fn();
    Element.prototype.scrollIntoView = scrollIntoView as unknown as Element["scrollIntoView"];
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    vi.restoreAllMocks();
  });

  it("makes every node a real button, in the order they run", () => {
    show(flowOf(STEPS, []));
    const ids = items().map((item) => item.dataset.flowId);
    expect(ids).toEqual(["spec", "builder", "check", "security", "ux", "approver", "human", "pr", "learn"]);
    expect(items().every((item) => item.tagName === "BUTTON" && item.getAttribute("type") === "button")).toBe(true);
  });

  it("selects on a click", () => {
    show(flowOf(STEPS, []));
    act(() => node("builder").click());
    expect(chosen).toEqual(["1"]);
    act(() => node("ux").click());
    expect(chosen).toEqual(["1", "3.1"]);
  });

  it("marks the selected node for screen readers, and only that one", () => {
    show(flowOf(STEPS, []), { selected: "1" });
    expect(node("builder").getAttribute("aria-current")).toBe("true");
    expect(node("spec").hasAttribute("aria-current")).toBe(false);
  });

  describe("in words", () => {
    it("gives each step's name and state", () => {
      show(flowOf(STEPS, [attempt("spec", "succeeded"), attempt("builder", "running", { modelActual: "claude-sonnet-5-5" }), attempt("ux", "skipped")]));
      expect(node("spec").getAttribute("aria-label")).toBe("spec, Does the work, sonnet, Finished");
      expect(node("builder").getAttribute("aria-label")).toBe("builder, Does the work, claude-sonnet-5-5, Running");
      expect(node("ux").getAttribute("aria-label")).toBe("ux, Reviews, sonnet, Skipped");
      expect(node("human").getAttribute("aria-label")).toBe("human, Your approval, Not started");
      expect(node("check").getAttribute("aria-label")).toBe("check, Check, Not started");
      expect(node("learn").getAttribute("aria-label")).toBe("learn, Proposes learnings, sonnet, Not started");
    });

    it("says it's waiting for the person", () => {
      show(flowOf(STEPS, [attempt("human", "waitingApproval", { kind: "approval" })]));
      expect(node("human").getAttribute("aria-label")).toContain("Waiting for you");
    });

    it("says the round, and when a state is from an earlier one", () => {
      const flow = flowOf(STEPS, [attempt("builder", "succeeded"), attempt("check", "succeeded", { kind: "check" }), attempt("builder", "running", { round: 1, number: 2 })]);
      show(flow);
      expect(node("builder").getAttribute("aria-label")).toContain("Running, Round 2");
      expect(node("check").getAttribute("aria-label")).toBe("check, Check, Finished, Last ran in round 1");
    });

    it("says how many attempts when it's only retries", () => {
      show(flowOf(STEPS, [attempt("builder", "failed"), attempt("builder", "running", { number: 2 })]));
      expect(node("builder").getAttribute("aria-label")).toContain("2 attempts");
    });

    it("puts the group in words too", () => {
      show(flowOf(STEPS, []));
      expect(document.querySelector('[role="group"][aria-label="reviews, at once"]')?.querySelector('[data-flow-id="security"]')).not.toBeNull();
    });

    it("hides the drawing from them", () => {
      show(flowOf(STEPS, []));
      expect(document.querySelector("svg")?.getAttribute("aria-hidden")).toBe("true");
    });

    it("says the times it went back, in a sentence you can read", () => {
      show(flowOf(STEPS, [attempt("builder", "running", { round: 1, number: 2 })]));
      expect(document.body.textContent).toContain("Sent back once");
      show(flowOf(STEPS, [attempt("builder", "running", { round: 2, number: 3 })]));
      expect(document.body.textContent).toContain("Sent back 2 times");
    });

    it("says nothing about going back when it hasn't", () => {
      show(flowOf(STEPS, [attempt("builder", "running")]));
      expect(document.body.textContent).not.toContain("Sent back");
    });

    it("does all of it in Spanish", async () => {
      await act(() => i18next.changeLanguage("es"));
      show(flowOf(STEPS, [attempt("builder", "succeeded"), attempt("check", "succeeded", { kind: "check" }), attempt("builder", "running", { round: 1, number: 2 })]));
      expect(node("builder").getAttribute("aria-label")).toBe("builder, Hace el trabajo, sonnet, En curso, Ronda 2");
      expect(node("check").getAttribute("aria-label")).toBe("check, Verificación, Terminado, Última vez: ronda 1");
      expect(node("human").getAttribute("aria-label")).toBe("human, Tu aprobación, Sin empezar");
      expect(document.body.textContent).toContain("Devuelto una vez");
      expect(document.querySelector('[role="group"][aria-label="reviews, a la vez"]')).not.toBeNull();
    });
  });

  describe("the keyboard", () => {
    it("is one stop for Tab, on the selected node", () => {
      show(flowOf(STEPS, []), { selected: "2" });
      expect(items().filter((item) => item.tabIndex === 0).map((item) => item.dataset.flowId)).toEqual(["check"]);
    });

    it("starts on the first node with nothing selected", () => {
      show(flowOf(STEPS, []));
      expect(items().filter((item) => item.tabIndex === 0).map((item) => item.dataset.flowId)).toEqual(["spec"]);
    });

    it("moves focus with the arrow keys, and selects nothing by moving", () => {
      show(flowOf(STEPS, []));
      act(() => node("spec").focus());
      press("ArrowRight");
      expect(document.activeElement).toBe(node("builder"));
      press("ArrowLeft");
      expect(document.activeElement).toBe(node("spec"));
      expect(chosen).toEqual([]);
    });

    it("stays the one Tab stop where the arrows left it", () => {
      show(flowOf(STEPS, []));
      act(() => node("spec").focus());
      press("ArrowRight");
      expect(items().filter((item) => item.tabIndex === 0).map((item) => item.dataset.flowId)).toEqual(["builder"]);
    });

    it("goes up and down in a group", () => {
      show(flowOf(STEPS, []));
      act(() => node("security").focus());
      press("ArrowDown");
      expect(document.activeElement).toBe(node("ux"));
      press("ArrowUp");
      expect(document.activeElement).toBe(node("security"));
    });

    it("goes to both ends", () => {
      show(flowOf(STEPS, []));
      act(() => node("check").focus());
      press("End");
      expect(document.activeElement).toBe(node("learn"));
      press("Home");
      expect(document.activeElement).toBe(node("spec"));
    });

    it("keeps the arrows from scrolling the page while they're moving focus", () => {
      show(flowOf(STEPS, []));
      act(() => node("spec").focus());
      expect(press("ArrowRight").defaultPrevented).toBe(true);
      expect(press("ArrowDown").defaultPrevented).toBe(true);
    });

    it("leaves other keys and combinations alone", () => {
      show(flowOf(STEPS, []));
      act(() => node("spec").focus());
      expect(press("Enter").defaultPrevented).toBe(false);
      expect(press("ArrowRight", { metaKey: true }).defaultPrevented).toBe(false);
      expect(document.activeElement).toBe(node("spec"));
    });
  });

  describe("taking focus", () => {
    it("takes none when it appears", () => {
      show(flowOf(STEPS, []), { selected: "1" });
      expect(document.activeElement).toBe(document.body);
    });

    it("leaves it where it is when it appears", () => {
      show(flowOf(STEPS, []));
      const outside = document.querySelector("#outside") as HTMLElement;
      act(() => outside.focus());
      show(flowOf(STEPS, [attempt("spec", "running")]), { selected: "0" });
      show(flowOf(STEPS, [attempt("spec", "succeeded"), attempt("builder", "running")]), { selected: "1" });
      expect(document.activeElement).toBe(outside);
    });

    it("keeps a node focused across updates", () => {
      show(flowOf(STEPS, []));
      act(() => node("builder").focus());
      show(flowOf(STEPS, [attempt("spec", "running")]));
      expect(document.activeElement).toBe(node("builder"));
    });

    it("only asks when asked", () => {
      show(flowOf(STEPS, []));
      show(flowOf(STEPS, []), { focusRequest: { key: "1" } });
      expect(document.activeElement).toBe(node("builder"));
    });
  });

  describe("scrolling", () => {
    const narrow = (width: number) => vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(width);

    it("brings the selected node into view, sideways", () => {
      narrow(400);
      show(flowOf(STEPS, []), { selected: "6" });
      expect(scroller().scrollLeft).toBeGreaterThan(0);
    });

    it("follows the selection as it moves", () => {
      narrow(400);
      show(flowOf(STEPS, []), { selected: "6" });
      const far = scroller().scrollLeft;
      show(flowOf(STEPS, []), { selected: "0" });
      expect(scroller().scrollLeft).toBeLessThan(far);
    });

    it("doesn't move if the node already shows", () => {
      narrow(5000);
      show(flowOf(STEPS, []), { selected: "6" });
      expect(scroller().scrollLeft).toBe(0);
    });

    it("leaves the person's scroll alone on a live update of the same selection", () => {
      narrow(400);
      show(flowOf(STEPS, [attempt("spec", "running")]), { selected: "0" });
      scroller().scrollLeft = 300;
      show(flowOf(STEPS, [attempt("spec", "succeeded")]), { selected: "0" });
      expect(scroller().scrollLeft).toBe(300);
    });

    it("never scrolls the page around it, which would take a waiting card out of view", () => {
      narrow(400);
      show(flowOf(STEPS, []), { selected: "6" });
      show(flowOf(STEPS, []), { selected: "1" });
      expect(scrollIntoView).not.toHaveBeenCalled();
    });
  });

  describe("how it looks", () => {
    const classes = (id: string) => node(id).className;

    it("stands out when it's waiting for the person", () => {
      show(flowOf(STEPS, [attempt("human", "waitingApproval", { kind: "approval" }), attempt("spec", "succeeded")]));
      expect(classes("human")).toContain("border-warning");
      expect(classes("spec")).not.toContain("border-warning");
    });

    it("shows a failed step as such, and a running one", () => {
      show(flowOf(STEPS, [attempt("builder", "failed"), attempt("spec", "running")]));
      expect(classes("builder")).toContain("border-danger");
      expect(classes("spec")).toContain("border-accent");
    });

    it("keeps the learner quiet even when it fails, since nothing depends on it", () => {
      show(flowOf(STEPS, [attempt("learn", "failed", { role: "learner" })]));
      expect(classes("learn")).not.toContain("border-danger");
      expect(node("learn").textContent).toContain("Failed");
    });

    it("stops pulsing for a person who asked for less motion", () => {
      show(flowOf(STEPS, [attempt("spec", "running")]));
      const badge = node("spec").querySelector(".animate-pulse");
      expect(badge).not.toBeNull();
      expect(badge?.closest('[class*="motion-reduce:[&_*]:animate-none"]')).not.toBeNull();
    });

    it("shows a long name and model whole on hover, not breaking the node", () => {
      const long = "a-step-with-a-very-long-name-that-goes-on-and-on-and-on";
      show(flowOf([{ type: "agent", ...spec(long, "claude-with-an-equally-long-model-name") }], []));
      const button = node(long);
      expect(button.title).toContain(long);
      expect(button.title).toContain("claude-with-an-equally-long-model-name");
      expect(button.style.width).toBe("148px");
      expect(button.querySelectorAll(".truncate").length).toBeGreaterThan(0);
    });

    it("has a name to show for a step that has none yet", () => {
      show(flowOf([{ type: "agent", ...spec("") }]), { editing: true });
      expect(document.body.textContent).toContain("No name");
    });
  });

  describe("attempts that aren't in the workflow", () => {
    it("are still there to select, under a label", () => {
      show(flowOf(STEPS.slice(0, 1), [attempt("stray", "failed")]));
      expect(document.body.textContent).toContain("Not in this workflow");
      act(() => node("stray").click());
      expect(chosen).toEqual(["x0"]);
    });

    it("are what's shown when the workflow can't be read, and it says why", () => {
      show(flowOf([], [attempt("a", "succeeded"), attempt("b", "failed")]));
      expect(document.body.textContent).toContain("can't read this run's workflow");
      expect(document.body.textContent).not.toContain("Not in this workflow");
      expect(items().map((item) => item.dataset.flowId)).toEqual(["a", "b"]);
    });
  });

  describe("in the editor", () => {
    const plain = flowOf(STEPS.slice(0, 5));
    const plus = (name: string) => document.querySelector<HTMLButtonElement>(`button[aria-label="${name}"]`) as HTMLButtonElement;

    it("shows no state at all, because nothing has run", () => {
      show(plain, { editing: true });
      expect(document.body.textContent).not.toContain("Not started");
      expect(node("builder").getAttribute("aria-label")).toBe("builder, Does the work, sonnet");
    });

    it("has a + for each gap, named for where it adds", () => {
      show(plain, { editing: true });
      act(() => plus("Add a step before spec").click());
      act(() => plus("Add a step before approver").click());
      act(() => plus("Add a step at the end").click());
      expect(added).toEqual([0, 4, 5]);
    });

    it("says which + is open", () => {
      show(plain, { editing: true, adding: 1 });
      expect(plus("Add a step before builder").getAttribute("aria-expanded")).toBe("true");
      expect(plus("Add a step before spec").getAttribute("aria-expanded")).toBe("false");
    });

    it("adds inside a group", () => {
      show(plain, { editing: true });
      act(() => plus("Add a step to reviews").click());
      expect(addedTo).toEqual([3]);
    });

    it("selects a group by its name, and each of its steps", () => {
      show(plain, { editing: true, selected: "3" });
      const name = document.querySelector<HTMLButtonElement>('button[data-flow-item="3"]') as HTMLButtonElement;
      expect(name.getAttribute("aria-current")).toBe("true");
      act(() => name.click());
      act(() => node("ux").click());
      expect(chosen).toEqual(["3", "3.1"]);
    });

    it("gets to the + and the group's name with the arrows too", () => {
      show(plain, { editing: true });
      act(() => node("spec").focus());
      press("ArrowRight");
      expect(document.activeElement).toBe(plus("Add a step before builder"));
      press("Home");
      expect(document.activeElement).toBe(plus("Add a step before spec"));
    });

    it("shows a problem on its node, in words and to the eye", () => {
      show(plain, { editing: true, problems: { "1": "Write what “builder” should do." } });
      expect(node("builder").getAttribute("aria-label")).toContain("Write what “builder” should do.");
      expect(node("builder").title).toContain("Write what “builder” should do.");
      expect(node("builder").className).toContain("border-danger");
      expect(node("spec").className).not.toContain("border-danger");
    });

    it("shows a problem on a group's name", () => {
      show(plain, { editing: true, problems: { "3": "The group “reviews” has no steps." } });
      const name = document.querySelector('button[data-flow-item="3"]') as HTMLElement;
      expect(name.getAttribute("aria-label")).toContain("has no steps");
      expect(name.className).toContain("text-danger");
    });

    it("invites the first step when there are none", () => {
      show(flowOf([]), { editing: true });
      expect(document.body.textContent).toContain("Add the first step");
      act(() => plus("Add the first step").click());
      expect(added).toEqual([0]);
    });

    it("shows an empty group, ready to be filled", () => {
      show(flowOf([{ type: "parallel", id: "reviews", steps: [] }]), { editing: true });
      expect(document.body.textContent).toContain("No steps yet");
      act(() => plus("Add a step to reviews").click());
      expect(addedTo).toEqual([0]);
    });

    it("says where the approver sends work back, and how many times it can", () => {
      show(plain, { editing: true });
      expect(document.body.textContent).toContain("Back to builder, 2 rounds at most");
    });
  });
});
