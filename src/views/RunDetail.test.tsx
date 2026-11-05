import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import type { Attempt, Run } from "../lib/api";
import type { CommandRun } from "../lib/audit";
import { useApp } from "../lib/store";
import { ApprovalCard } from "./RunDetail";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const attempt = (fields: Partial<Attempt>) => ({ kind: "agent", role: "worker", tools: "readOnly", state: "succeeded", round: 0, output: null, resultText: null, filesChanged: null, runFiles: null, ...fields }) as Attempt;
const human = attempt({ id: "human", stepId: "human", kind: "approval", tools: "none", state: "waitingApproval" });
const builder = (files: string[], resultText = "I fixed add in calc.py.") => attempt({ id: "builder", stepId: "builder", tools: "build", resultText, filesChanged: files, runFiles: files });
const check = attempt({ id: "check", stepId: "check", kind: "check", tools: "check", prompt: "make test" });
const reviewer = attempt({ id: "code", stepId: "code", role: "reviewer", output: { verdict: "approved", findings: [], summary: "Meets each criterion." } as unknown as Attempt["output"] });

const deliverNext = [
  { type: "agent", id: "builder" },
  { type: "approval", id: "human" },
  { type: "deliver", id: "pr" },
];
const runWith = (workflow: unknown[]) => ({ id: "r1", projectId: "p1", branch: "keel/abc12345", round: 0, workflow }) as Run;

describe("the approval card", () => {
  let root: Root;
  let answers: unknown[];
  let addsSecret: boolean;

  const show = (attempts: Attempt[], files: string[], workflow: unknown[] = deliverNext, commands: CommandRun[] = []) => {
    act(() => root.render(<ApprovalCard attempt={human} run={runWith(workflow)} attempts={attempts} files={files} commands={commands} rawCheck="intact" onShowChanges={() => {}} onError={() => {}} />));
  };
  const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);
  const press = (text: string) => act(() => button(text)?.click());
  const page = () => document.body.textContent ?? "";
  const write = (text: string) => {
    const textarea = document.querySelector("textarea") as HTMLTextAreaElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(textarea, text);
      textarea.dispatchEvent(new Event("input", { bubbles: true }));
    });
  };

  beforeEach(async () => {
    vi.useFakeTimers();
    await i18next.changeLanguage("en");
    answers = [];
    addsSecret = false;
    mockIPC((command, args) => {
      if (command === "approval_answer") answers.push(args);
      if (command === "run_adds_secret") return addsSecret;
      return null;
    });
    useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ id: "p1", githubRepo: "acme/app", githubAccount: "you-work" }] as never } });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    vi.useRealTimers();
  });

  it("starts on the note, and only lets you approve after a moment", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    expect(document.activeElement).toBe(document.querySelector("textarea"));
    expect(button("Approve and open a draft")?.disabled).toBe(true);
    act(() => vi.advanceTimersByTime(800));
    expect(button("Approve and open a draft")?.disabled).toBe(false);
  });

  it("a click that brings the window back can't approve", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    act(() => vi.advanceTimersByTime(800));
    expect(button("Approve and open a draft")?.disabled).toBe(false);

    // The person goes to another app, and it stays off however long they're away.
    act(() => void window.dispatchEvent(new Event("blur")));
    expect(button("Approve and open a draft")?.disabled).toBe(true);
    act(() => vi.advanceTimersByTime(60_000));
    expect(button("Approve and open a draft")?.disabled).toBe(true);

    // They come back with a click on the button: too soon to count.
    act(() => void window.dispatchEvent(new Event("focus")));
    press("Approve and open a draft");
    expect(answers).toEqual([]);

    act(() => vi.advanceTimersByTime(799));
    expect(button("Approve and open a draft")?.disabled).toBe(true);
    act(() => vi.advanceTimersByTime(1));
    press("Approve and open a draft");
    expect(answers).toEqual([{ attemptId: "human", approve: true, text: "" }]);
  });

  it("a card that appears while the window is away waits again when it's back", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    act(() => vi.advanceTimersByTime(800));
    act(() => void window.dispatchEvent(new Event("focus")));
    expect(button("Approve and open a draft")?.disabled).toBe(true);
  });

  it("warns when the change looks like it holds a secret, without blocking anything", async () => {
    addsSecret = true;
    show([builder(["calc.py"]), human], ["calc.py"]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(800);
    });
    expect(page()).toContain("might hold a secret");
    expect(button("Approve and open a draft")?.disabled).toBe(false);
  });

  it("says nothing about secrets when it finds none", async () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(800);
    });
    expect(page()).not.toContain("might hold a secret");
  });

  it("won't reject without a reason, and blank doesn't count as one", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    expect(button("Reject")?.disabled).toBe(true);
    expect(page()).toContain("Say why above first");
    write("  \n ");
    expect(button("Reject")?.disabled).toBe(true);
    press("Reject");
    expect(answers).toEqual([]);
    write("Use the retry helper.");
    expect(button("Reject")?.disabled).toBe(false);
    expect(page()).not.toContain("Say why above first");
    press("Reject");
    expect(answers).toEqual([{ attemptId: "human", approve: false, text: "Use the retry helper." }]);
  });

  it("approves with no note", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    act(() => vi.advanceTimersByTime(800));
    press("Approve and open a draft");
    expect(answers).toEqual([{ attemptId: "human", approve: true, text: "" }]);
  });

  it("never approves on Enter", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    act(() => vi.advanceTimersByTime(800));
    const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    act(() => button("Approve and open a draft")?.dispatchEvent(enter));
    expect(enter.defaultPrevented).toBe(true);
    expect(answers).toEqual([]);
  });

  it("says what's being approved and where it goes", () => {
    show([builder(["calc.py"]), check, human], ["calc.py"]);
    for (const text of ["I fixed add in calc.py.", "calc.py", "Keel ran `make test` and it passed.", "acme/app", "keel/abc12345", "GitHub: you-work", "Merges nothing"]) {
      expect(page()).toContain(text);
    }
    expect(page()).not.toContain("ran no check");
  });

  it("says so when Keel ran no check, and that the reviews only read the code", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    expect(page()).toContain("Keel ran no check of its own");
    expect(page()).not.toContain("only read the code");
    show([builder(["calc.py"]), reviewer, human], ["calc.py"]);
    expect(page()).toContain("only read the code");
  });

  it("doesn't count a review that was skipped as one that read the code", () => {
    show([builder(["calc.py"]), attempt({ id: "ux", stepId: "ux", role: "reviewer", state: "skipped" }), human], ["calc.py"]);
    expect(page()).not.toContain("only read the code");
    expect(page()).toContain("ux: skipped");
  });

  it("warns about workflow files even when the list of files is cut", () => {
    const files = [...Array.from({ length: 8 }, (_, i) => `f${i}.py`), ".github/workflows/ci.yml"];
    show([builder(files), human], files);
    expect(page()).toContain("and 4 more");
    expect(page()).not.toContain("f7.py");
    expect(page()).toContain("Read them first");
  });

  it("keeps long text out of the card, and one click away", () => {
    const long = `I fixed add. I added a test.\n\n${"More detail nobody needs on this card. ".repeat(40)}`;
    show([builder(["calc.py"], long), human], ["calc.py"]);
    expect(page()).not.toContain("More detail");
    press("Read all");
    expect(page()).toContain("More detail");
  });

  it("offers nothing to approve when the run changed nothing, and closes it as a rejection with no note", () => {
    show([builder([], "I changed nothing: it's already fixed on main."), human], []);
    act(() => vi.advanceTimersByTime(800));
    expect(page()).toContain("The agent changed nothing");
    expect(page()).toContain("I changed nothing: it's already fixed on main.");
    expect(button("Approve and open a draft")).toBeUndefined();
    expect(page()).not.toContain("Goes to");
    expect(document.querySelector("textarea")).toBeNull();
    expect(document.activeElement).toBe(button("Close this run"));
    press("Close this run");
    expect(answers).toEqual([{ attemptId: "human", approve: false, text: "" }]);
  });

  it("isn't empty yet when the approval comes before the building", () => {
    const workflow = [{ type: "approval", id: "human" }, { type: "agent", id: "builder" }, { type: "deliver", id: "pr" }];
    show([human], [], workflow);
    act(() => vi.advanceTimersByTime(800));
    expect(page()).not.toContain("The agent changed nothing");
    expect(button("Reject")?.disabled).toBe(true);
    press("Approve and continue");
    expect(answers).toEqual([{ attemptId: "human", approve: true, text: "" }]);
  });

  it("doesn't promise a pull request a workflow won't open", () => {
    show([builder(["calc.py"]), human], ["calc.py"], [{ type: "agent", id: "builder" }, { type: "approval", id: "human" }]);
    expect(page()).toContain("so nothing gets pushed");
    expect(page()).not.toContain("Goes to");
    expect(page()).not.toContain("goes in the pull request");
  });

  it("says when the project can't receive a pull request yet", () => {
    useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ id: "p1", githubRepo: "acme/app", githubAccount: null }] as never } });
    show([builder(["calc.py"]), human], ["calc.py"]);
    expect(page()).toContain("Choose which GitHub account this project uses.");
  });

  it("sends a note with an approval too", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    act(() => vi.advanceTimersByTime(800));
    write("Keep changes small.");
    press("Approve and open a draft");
    expect(answers).toEqual([{ attemptId: "human", approve: true, text: "Keep changes small." }]);
  });
});
