import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import type { Attempt, Run, SpecOutput } from "../lib/api";
import type { CommandRun } from "../lib/audit";
import { useApp } from "../lib/store";
import type { SizeDecision } from "../lib/size";
import { ApprovalCard, QuestionsCard } from "./RunDetail";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const attempt = (fields: Partial<Attempt>) => ({ kind: "agent", role: "worker", tools: "readOnly", state: "succeeded", round: 0, output: null, resultText: null, filesChanged: null, runFiles: null, ...fields }) as Attempt;
const human = attempt({ id: "human", stepId: "human", kind: "approval", tools: "none", state: "waitingApproval" });
const builder = (files: string[], resultText = "I fixed add in calc.py.") => attempt({ id: "builder", stepId: "builder", tools: "build", resultText, filesChanged: files, runFiles: files });
const check = attempt({ id: "check", stepId: "check", kind: "check", tools: "check", prompt: "make test" });
const spec = (resultText: string) => attempt({ id: "spec", stepId: "spec", resultText });
const specOutput = (fields: Partial<SpecOutput> = {}): SpecOutput => ({ summary: "add returns the sum of its arguments.", criteria: [{ id: "C1", when: "add(1, 2)", then: "it returns 3" }], changes: [{ kind: "changes", text: "add stops subtracting" }], limits: ["Its signature stays"], decided: [], questions: [], ...fields });
const specifier = (fields: Partial<SpecOutput> = {}, state: Attempt["state"] = "succeeded") => attempt({ id: "spec", stepId: "spec", role: "specifier", state, resultText: "", output: specOutput(fields) as unknown as Attempt["output"] });
const said = (id: string, result: string, evidence = "") => ({ id, when: `when ${id.toLowerCase()}`, then: `then ${id.toLowerCase()}`, result, evidence });
const verifier = (criteria: unknown[], fields: Partial<Attempt> = {}) => attempt({ id: "verify", stepId: "verify", role: "verifier", output: { criteria, summary: "What it found" } as unknown as Attempt["output"], ...fields });
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
  let sentBack: unknown[];
  let addsSecret: boolean;
  let answerFails: boolean;
  let askedToRun: unknown[];
  let runFails: boolean;
  let errors: string[];

  const show = (attempts: Attempt[], files: string[], workflow: unknown[] = deliverNext, commands: CommandRun[] = [], asked: Attempt = human, sizeDecision: SizeDecision | null = null) => {
    act(() => root.render(<ApprovalCard attempt={asked} run={runWith(workflow)} attempts={attempts} files={files} commands={commands} rawCheck="intact" sizeDecision={sizeDecision} onShowChanges={() => {}} onError={(message) => errors.push(message)} />));
  };
  const textarea = () => document.querySelector("textarea") as HTMLTextAreaElement;
  const settle = () => act(() => vi.advanceTimersByTimeAsync(0));
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
    sentBack = [];
    addsSecret = false;
    answerFails = false;
    askedToRun = [];
    runFails = false;
    errors = [];
    mockIPC((command, args) => {
      if (command === "approval_run_skipped") {
        if (runFails) throw { code: "blocked", reason: { code: "nothingSkippedToRun" }, detail: null };
        askedToRun.push(args);
      }
      if (command === "approval_answer") {
        if (answerFails) throw { code: "invalidState", reason: null, detail: null };
        answers.push(args);
      }
      if (command === "approval_send_back") {
        if (answerFails) throw { code: "invalidState", reason: null, detail: null };
        sentBack.push(args);
      }
      if (command === "run_adds_secret") return addsSecret;
      return null;
    });
    useApp.setState({ approvalDrafts: {}, overview: { ...useApp.getState().overview, projects: [{ id: "p1", githubRepo: "acme/app", githubAccount: "you-work" }] as never } });
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

  it("never rejects on Enter either", () => {
    show([builder(["calc.py"]), human], ["calc.py"]);
    write("Use the retry helper.");
    const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    act(() => button("Reject")?.dispatchEvent(enter));
    expect(enter.defaultPrevented).toBe(true);
  });

  it("never closes an empty run on Enter, though the focus starts on that button", () => {
    show([builder([], "I changed nothing."), human], []);
    expect(document.activeElement).toBe(button("Close this run"));
    const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    act(() => button("Close this run")?.dispatchEvent(enter));
    expect(enter.defaultPrevented).toBe(true);
  });

  describe("the note", () => {
    const leave = () => act(() => root.render(null));

    it("survives leaving the page and coming back", () => {
      show([builder(["calc.py"]), human], ["calc.py"]);
      write("Use the retry helper.");
      leave();
      show([builder(["calc.py"]), human], ["calc.py"]);
      expect(textarea().value).toBe("Use the retry helper.");
      expect(button("Reject")?.disabled).toBe(false);
    });

    it("belongs to its attempt", () => {
      show([builder(["calc.py"]), human], ["calc.py"]);
      write("Use the retry helper.");
      leave();
      const other = { ...human, id: "other" };
      show([builder(["calc.py"]), other], ["calc.py"], deliverNext, [], other);
      expect(textarea().value).toBe("");
      expect(button("Reject")?.disabled).toBe(true);
    });

    it("is gone once the attempt is answered", async () => {
      show([builder(["calc.py"]), human], ["calc.py"]);
      write("Use the retry helper.");
      press("Reject");
      await settle();
      expect(answers).toEqual([{ attemptId: "human", approve: false, text: "Use the retry helper." }]);
      expect(useApp.getState().approvalDrafts).toEqual({});
      // The card is still there until the backend reports the change, and it doesn't blink empty.
      expect(textarea().value).toBe("Use the retry helper.");
    });

    it("stays when answering fails, ready to try again", async () => {
      answerFails = true;
      show([builder(["calc.py"]), human], ["calc.py"]);
      write("Use the retry helper.");
      press("Reject");
      await settle();
      expect(useApp.getState().approvalDrafts).toEqual({ human: "Use the retry helper." });
      expect(textarea().value).toBe("Use the retry helper.");
      expect(button("Reject")?.disabled).toBe(false);
    });
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

  describe("what was asked", () => {
    it("is there at the final approval, beside what the agent did", () => {
      show([spec("add returns the sum of its arguments."), builder(["calc.py"]), human], ["calc.py"]);
      expect(page()).toContain("What was asked");
      expect(page()).toContain("add returns the sum of its arguments.");
      expect(page()).toContain("I fixed add in calc.py.");
    });

    it("keeps long text out of the card, one click away, under its own name", () => {
      const long = `add returns the sum of its arguments, including negative ones and decimals.\n\n${"A detail nobody needs on this card. ".repeat(40)}`;
      show([spec(long), builder(["calc.py"]), human], ["calc.py"]);
      expect(page()).toContain("add returns the sum");
      expect(page()).not.toContain("A detail nobody");
      const readAll = document.querySelector<HTMLButtonElement>('button[aria-label="Read all of what was asked"]');
      act(() => readAll?.click());
      expect(page()).toContain("A detail nobody");
      expect(document.querySelector('[role="dialog"]')?.textContent).toContain("What was asked");
    });

    it("has nothing more to read when it all fits", () => {
      show([spec("add returns the sum of its arguments, including negative ones and decimals."), builder(["calc.py"]), human], ["calc.py"]);
      expect(document.querySelector('button[aria-label="Read all of what was asked"]')).toBeNull();
    });

    it("isn't repeated at an approval right after the spec, where the card already shows it", () => {
      const workflow = [{ type: "agent", id: "spec" }, { type: "approval", id: "human" }, { type: "agent", id: "builder" }, { type: "deliver", id: "pr" }];
      show([spec("add returns the sum of its arguments."), human], [], workflow);
      expect(page()).not.toContain("What was asked");
      expect(page().match(/add returns the sum/g)).toHaveLength(1);
    });

    it("isn't there if nothing says it", () => {
      show([builder(["calc.py"]), human], ["calc.py"]);
      expect(page()).not.toContain("What was asked");
      show([spec("   "), builder(["calc.py"]), human], ["calc.py"]);
      expect(page()).not.toContain("What was asked");
    });

    it("scrolls with the rest of what's being approved, so it never pushes the buttons away", () => {
      show([spec("add returns the sum."), builder(["calc.py"]), human], ["calc.py"]);
      const list = document.querySelector("dl") as HTMLElement;
      expect(list.textContent).toContain("What was asked");
      expect(list.className).toContain("overflow-y-auto");
      expect(list.contains(button("Reject") as HTMLElement)).toBe(false);
    });
  });

  describe("with a verifier in the workflow", () => {
    const proof = () => document.querySelector("details");
    const greenCheck = () => proof()?.querySelector("summary svg.text-success") ?? null;
    const met = said("C1", "met", "test_calc.py:1");
    const open = said("C2", "unverified");

    it("says everything was met, with a check, when that's so", () => {
      show([builder(["calc.py"]), check, verifier([met, said("C2", "met", "test_calc.py:2")]), human], ["calc.py"]);
      expect(proof()?.querySelector("summary")?.textContent).toBe("2 of 2 met");
      expect(greenCheck()).not.toBeNull();
    });

    it("never shows a green check over something not verified, and says what it is", () => {
      show([builder(["calc.py"]), check, verifier([met, met, met, open]), human], ["calc.py"]);
      expect(proof()?.querySelector("summary")?.textContent).toBe("3 of 4 met, 1 not verified");
      expect(greenCheck()).toBeNull();
    });

    it("opens to each one in its own words, without bare ids or JSON", () => {
      show([builder(["calc.py"]), check, verifier([met, open, { ...said("C3", "unverified", "test_calc.py:9"), corrected: "noCheckPassed" }, { ...said("C4", "unverified"), corrected: "noEvidence" }, { ...said("C5", "unverified"), corrected: "leftOut" }, said("C6", "unmet", "calc.py:3")]), human], ["calc.py"]);
      const list = proof()?.querySelector("ul")?.textContent ?? "";
      expect(list).toContain("Metwhen c1 → then c1");
      expect(list).toContain("test_calc.py:1");
      expect(list).toContain("Not verifiedwhen c2 → then c2");
      expect(list).toContain("It found nothing that proves this.");
      expect(list).toContain("It said this was met, but no check passed in this round.");
      expect(list).toContain("test_calc.py:9");
      expect(list).toContain("It said this was met, but not what proves it.");
      expect(list).toContain("It said nothing about this.");
      expect(list).toContain("Not metwhen c6 → then c6");
      expect(list).toContain("calc.py:3");
      expect(page()).not.toMatch(/\bC[1-6]\b/);
      expect(page()).not.toContain("{");
      expect(page()).not.toMatch(/criteria|acceptance/i);
    });

    it("stands out when something isn't met", () => {
      show([builder(["calc.py"]), check, verifier([met, said("C2", "unmet", "calc.py:3")]), human], ["calc.py"]);
      const line = proof()?.querySelector("summary span");
      expect(line?.textContent).toBe("1 of 2 met, 1 not met");
      expect(line?.className).toContain("text-warning");
      expect(greenCheck()).toBeNull();
    });

    it("says plainly when nothing could be verified, as a normal thing and not as an error", () => {
      show([builder(["calc.py"]), check, verifier([open, said("C3", "unverified")]), human], ["calc.py"]);
      const line = proof()?.querySelector("summary span");
      expect(line?.textContent).toBe("None of the 2 could be verified");
      expect(line?.className).not.toContain("text-danger");
      expect(line?.className).not.toContain("text-warning");
      expect(greenCheck()).toBeNull();
    });

    it("is in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, verifier([met, met, met, open]), human], ["calc.py"]);
      expect(proof()?.querySelector("summary")?.textContent).toBe("3 de 4 se cumplen, 1 sin verificar");
      expect(proof()?.textContent).toContain("Sin verificarwhen c2 → then c2");
    });

    it("says nothing about an earlier round, which looked at code that has changed since", () => {
      const later = { id: "r1", projectId: "p1", branch: "keel/abc12345", round: 1, workflow: deliverNext } as Run;
      act(() => root.render(<ApprovalCard attempt={human} run={later} attempts={[builder(["calc.py"]), verifier([met]), human]} files={["calc.py"]} commands={[]} rawCheck="intact" onShowChanges={() => {}} onError={() => {}} />));
      expect(proof()).toBeNull();
      expect(page()).not.toContain("1 of 1 met");
    });

    it("says nothing when the workflow has no verifier", () => {
      show([builder(["calc.py"]), check, human], ["calc.py"]);
      expect(proof()).toBeNull();
    });

    it("keeps its buttons where they were: opening it is not answering", () => {
      show([builder(["calc.py"]), check, verifier([met, open]), human], ["calc.py"]);
      act(() => proof()?.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true })));
      expect(answers).toEqual([]);
    });
  });

  describe("when the workflow fits the change", () => {
    const small = { code: "treatedAsSmall", lines: 12, files: 2 };
    const skip = (id: string, role: Attempt["role"]) => attempt({ id, stepId: id, role, state: "skipped", reason: small });
    const skipped = [skip("security", "reviewer"), skip("code", "reviewer"), skip("approver", "approver")];
    const went = (because: { code: string; [key: string]: unknown }[]): SizeDecision => ({ small: false, because, round: 0 });
    const row = (label: string) => [...document.querySelectorAll("dt")].find((dt) => dt.textContent?.startsWith(label))?.nextElementSibling?.textContent ?? null;
    const JARGON = ["spec", "ODD", "organic", "escalat", "route", "size", "criteria", "regression"];

    it("says it treated this as small, how big it was, and what didn't run", () => {
      show([builder(["calc.py"]), check, ...skipped, human], ["calc.py"]);
      const said = row("Small change");
      expect(said).toContain("Keel treated this as a small change: 12 changed lines in 2 files.");
      expect(said).toContain("It touches nothing sensitive, and the check passed.");
      expect(said).toContain("So these didn't run: security, code, approver.");
      expect(said).toContain("Nothing already built is done again");
      expect(row("Reviews")).toBeNull();
      expect(page()).not.toContain("nothing they cover changed");
    });

    it("counts one line and one file in the singular", () => {
      show([builder(["calc.py"]), check, attempt({ id: "code", stepId: "code", role: "reviewer", state: "skipped", reason: { code: "treatedAsSmall", lines: 1, files: 1 } }), human], ["calc.py"]);
      expect(row("Small change")).toContain("1 changed line in 1 file.");
    });

    it("reads the same way in Spanish, in the person's words", async () => {
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, ...skipped, human], ["calc.py"]);
      const said = row("Cambio pequeño");
      expect(said).toContain("Keel lo trató como un cambio pequeño: 12 líneas cambiadas en 2 archivos.");
      expect(said).toContain("Por eso no corrieron: security, code, approver.");
      expect(button("Correrlos ahora")).toBeDefined();
    });

    it("runs them when asked, only after a moment, and neither approves nor rejects", async () => {
      show([builder(["calc.py"]), check, ...skipped, human], ["calc.py"]);
      expect(button("Run them now")?.disabled).toBe(true);
      press("Run them now");
      expect(askedToRun).toEqual([]);
      act(() => vi.advanceTimersByTime(800));
      expect(button("Run them now")?.disabled).toBe(false);
      press("Run them now");
      await settle();
      expect(askedToRun).toEqual([{ attemptId: "human" }]);
      expect(answers).toEqual([]);
      expect(sentBack).toEqual([]);
    });

    it("Enter on it answers nothing", () => {
      show([builder(["calc.py"]), check, ...skipped, human], ["calc.py"]);
      act(() => vi.advanceTimersByTime(800));
      const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
      act(() => void button("Run them now")?.dispatchEvent(enter));
      expect(enter.defaultPrevented).toBe(true);
      expect(askedToRun).toEqual([]);
    });

    it("says so if it's refused, and lets them answer", async () => {
      runFails = true;
      show([builder(["calc.py"]), check, ...skipped, human], ["calc.py"]);
      act(() => vi.advanceTimersByTime(800));
      press("Run them now");
      await settle();
      expect(errors).toEqual(["Nothing was skipped for being a small change, so there's nothing to run."]);
      expect(button("Approve and open a draft")?.disabled).toBe(false);
    });

    it("has nothing to offer when nothing was skipped, or everything it asked for has run", () => {
      show([builder(["calc.py"]), check, reviewer, human], ["calc.py"]);
      expect(button("Run them now")).toBeUndefined();
      expect(row("Small change")).toBeNull();

      const ranAfter = attempt({ ...skipped[1], state: "succeeded", reason: null, number: 2, output: { verdict: "approved", findings: [], summary: "Fine." } as unknown as Attempt["output"] });
      show([builder(["calc.py"]), check, skipped[1], ranAfter, human], ["calc.py"]);
      expect(button("Run them now")).toBeUndefined();
      expect(row("Reviews")).toContain("No problems");
    });

    it("keeps apart what had nothing to look at", () => {
      const nothing = attempt({ id: "ux", stepId: "ux", role: "reviewer", state: "skipped", reason: { code: "noMatchingPaths", paths: ["**/*.tsx"] } });
      show([builder(["calc.py"]), check, reviewer, nothing, human], ["calc.py"]);
      expect(row("Reviews")).toContain("ux: skipped, nothing they cover changed");
      expect(row("Small change")).toBeNull();
    });

    it("says every reason it went to full review, in plain words", () => {
      const every = [
        { code: "specNotSmall" },
        { code: "specNamesRisks", risks: ["security", "somethingNew"] },
        { code: "keelChoseAnAnswer" },
        { code: "noCheck" },
        { code: "checkNotPassed" },
        { code: "issueFromSomeoneElse", author: "mallory" },
        { code: "issueFromUnknownAuthor" },
        { code: "issueWithNoAccount" },
        { code: "startedByAssistant", client: "Claude Code" },
        { code: "sensitiveFiles", files: [".github/workflows/ci.yml"], total: 1 },
        { code: "badSensitivePattern", pattern: "a[" },
        { code: "tooManyLines", lines: 512, limit: 400 },
        { code: "filesWithoutLines", files: ["logo.png"], total: 1 },
        { code: "diffUnreadable" },
        { code: "verifierFoundUnmet", count: 1 },
        { code: "verifierHasNotRun" },
        { code: "personAsked" },
      ];
      show([builder(["calc.py"]), check, reviewer, human], ["calc.py"], deliverNext, [], human, went(every));
      const said = row("Full review") ?? "";
      expect(said).toContain("It went to full review because:");
      for (const line of ["it could affect security, something it couldn't name", "this workflow has no check", "it comes from an issue by mallory, who isn't this project's account", "it touches .github/workflows/ci.yml", "it changes 512 lines, and a small change stays under 400", "you asked for it"]) expect(said).toContain(line);
      expect([...document.querySelectorAll("dt")].find((dt) => dt.textContent === "Full review")?.nextElementSibling?.querySelectorAll("li").length).toBe(every.length);
      for (const word of JARGON) expect(said.toLowerCase()).not.toContain(word.toLowerCase());
    });

    it("says it in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, reviewer, human], ["calc.py"], deliverNext, [], human, went([{ code: "sensitiveFiles", files: [".github/ci.yml"], total: 3 }]));
      const said = row("Revisión completa") ?? "";
      expect(said).toContain("Pasó a revisión completa porque:");
      expect(said).toContain("toca .github/ci.yml y 2 más");
    });

    it("says which round those reasons are from, once it's a later one", () => {
      const later = { ...runWith(deliverNext), round: 1 };
      act(() => root.render(<ApprovalCard attempt={human} run={later} attempts={[builder(["calc.py"]), check, human]} files={["calc.py"]} commands={[]} rawCheck="intact" sizeDecision={went([{ code: "verifierFoundUnmet", count: 1 }])} onShowChanges={() => {}} onError={() => {}} />));
      expect([...document.querySelectorAll("dt")].find((dt) => dt.textContent?.startsWith("Full review"))?.textContent).toBe("Full reviewRound 1");
    });

    it("says nothing about it for a workflow that doesn't depend on the change", () => {
      show([builder(["calc.py"]), check, reviewer, human], ["calc.py"]);
      expect(row("Full review")).toBeNull();
    });
  });

  describe("with a structured answer from the specifier", () => {
    const building = [{ type: "agent", id: "spec", tools: "readOnly", role: { kind: "specifier" } }, { type: "agent", id: "builder", tools: "build" }, { type: "approval", id: "human" }, { type: "deliver", id: "pr" }];
    const gate = [{ type: "agent", id: "spec", tools: "readOnly", role: { kind: "specifier" } }, { type: "approval", id: "human" }, { type: "agent", id: "builder", tools: "build" }, { type: "deliver", id: "pr" }];

    it("sums up what was asked, with the rest one click away and never as JSON", () => {
      show([specifier(), builder(["calc.py"]), human], ["calc.py"], building);
      expect(page()).toContain("What was asked");
      expect(page()).toContain("add returns the sum of its arguments.");
      expect(page()).not.toContain("add stops subtracting");
      const readAll = document.querySelector<HTMLButtonElement>('button[aria-label="Read all of what was asked"]');
      act(() => readAll?.click());
      const dialog = document.querySelector('[role="dialog"]')?.textContent ?? "";
      expect(dialog).toContain("What has to be true");
      expect(dialog).toContain("add(1, 2) → it returns 3");
      expect(dialog).toContain("Changes: add stops subtracting");
      expect(dialog).toContain("What stays as it is");
      expect(dialog).not.toContain("{");
    });

    it("shows what was asked and nothing false at an approval right after it", () => {
      show([specifier(), human], [], gate);
      expect(page()).toContain("What was asked");
      expect(page()).toContain("add returns the sum of its arguments.");
      expect(page()).not.toContain("What it did");
      expect(page()).not.toContain("without an explanation");
    });

    it("still says it, once a worker has something to say", () => {
      show([specifier(), builder(["calc.py"]), human], ["calc.py"], building);
      expect(page()).toContain("What it did");
      expect(page()).toContain("I fixed add in calc.py.");
    });

    it("still says when a worker did something and explained nothing", () => {
      show([specifier(), attempt({ id: "builder", stepId: "builder", tools: "build", resultText: null, filesChanged: ["calc.py"], runFiles: ["calc.py"] }), human], ["calc.py"], building);
      expect(page()).toContain("What it did");
      expect(page()).toContain("It finished without an explanation.");
    });

    describe("what it decided alone", () => {
      const decided = [{ text: "Keeps the rounding as it is" }, { text: "If it's 0? Show an error", taken: "person" as const }, { text: "Round up? Leave it", taken: "roundsLimit" as const }];
      const change = (text: string) => document.querySelector<HTMLButtonElement>(`button[aria-label="Change: ${text}"]`);
      const open = () => {
        show([specifier({ decided }), builder(["calc.py"]), human], ["calc.py"], building);
        act(() => change("Keeps the rounding as it is")?.click());
      };

      it("is a short list, each line with a way to change it, saying who settled what", () => {
        show([specifier({ decided }), builder(["calc.py"]), human], ["calc.py"], building);
        expect(page()).toContain("What it decided alone");
        for (const item of decided) {
          expect(page()).toContain(item.text);
          expect(change(item.text)).not.toBeNull();
        }
        expect(page()).toContain("You chose this.");
        expect(page()).toContain("The rounds of questions ran out, so Keel stopped asking and took the recommended option.");
      });

      it("isn't there if it decided nothing", () => {
        show([specifier(), builder(["calc.py"]), human], ["calc.py"], building);
        expect(page()).not.toContain("What it decided alone");
      });

      it("offers no way to change it where nothing could do the work again", () => {
        show([specifier({ decided }), human], [], gate);
        expect(page()).toContain("Keeps the rounding as it is");
        expect(change("Keeps the rounding as it is")).toBeNull();
      });

      it("asks what should change, and won't go without it", () => {
        open();
        expect(page()).toContain("What should change?");
        expect(document.activeElement).toBe(document.querySelector("li textarea"));
        act(() => vi.advanceTimersByTime(800));
        expect(button("Send it back")?.disabled).toBe(true);
      });

      it("sends the person's words, with which line they're about, and doesn't read as an approval", async () => {
        open();
        act(() => vi.advanceTimersByTime(800));
        write("Round it to two decimals");
        press("Send it back");
        await settle();
        expect(sentBack).toEqual([{ attemptId: "human", instructions: "About “Keeps the rounding as it is”: Round it to two decimals" }]);
        expect(answers).toEqual([]);
      });

      it("never sends on Enter, and a window coming back can't send it either", () => {
        open();
        act(() => vi.advanceTimersByTime(800));
        write("Round it");
        const send = button("Send it back") as HTMLButtonElement;
        expect(send.onkeydown).toBeNull();
        const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
        act(() => void send.dispatchEvent(enter));
        expect(enter.defaultPrevented).toBe(true);
        act(() => void window.dispatchEvent(new Event("blur")));
        expect(button("Send it back")?.disabled).toBe(true);
      });

      it("can be left as it is, keeping what was written for later", () => {
        open();
        write("Round it");
        press("Leave it");
        expect(page()).not.toContain("What should change?");
        act(() => change("Keeps the rounding as it is")?.click());
        expect((document.querySelector("li textarea") as HTMLTextAreaElement).value).toBe("Round it");
      });

      it("keeps the words when sending fails, and says why", async () => {
        const errors: string[] = [];
        answerFails = true;
        act(() => root.render(<ApprovalCard attempt={human} run={runWith(building)} attempts={[specifier({ decided }), builder(["calc.py"]), human]} files={["calc.py"]} commands={[]} rawCheck="intact" onShowChanges={() => {}} onError={(message) => errors.push(message)} />));
        act(() => change("Keeps the rounding as it is")?.click());
        act(() => vi.advanceTimersByTime(800));
        write("Round it");
        press("Send it back");
        await settle();
        expect(errors).toHaveLength(1);
        expect((document.querySelector("li textarea") as HTMLTextAreaElement).value).toBe("Round it");
        expect(button("Send it back")?.disabled).toBe(false);
      });
    });
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

describe("the card that asks", () => {
  let root: Root;
  let calls: { command: string; args: unknown }[];
  let errors: string[];
  let answerFails: boolean;

  const question = (id: string, text: string) => ({ id, text, options: [{ label: "Show an error", recommended: true }, { label: "Return 0", recommended: false }] });
  const waiting = (questions = [question("Q1", "If `whole` is 0, what should happen?")]) => specifier({ questions }, "waitingAnswers");
  const show = (asking = waiting(), attempts: Attempt[] = [asking]) => {
    act(() => root.render(<QuestionsCard attempt={asking} attempts={attempts} onError={(message) => errors.push(message)} />));
  };
  const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);
  const press = (text: string) => act(() => button(text)?.click());
  const page = () => document.body.textContent ?? "";
  const label = (fieldset: number, text: string) => [...document.querySelectorAll("fieldset")[fieldset].querySelectorAll("label")].find((l) => l.textContent?.startsWith(text)) as HTMLLabelElement;
  const input = (fieldset: number, text: string) => label(fieldset, text).querySelector("input") as HTMLInputElement;
  const pick = (fieldset: number, text: string) => act(() => input(fieldset, text).click());
  const write = (text: string) => {
    const textarea = document.querySelector("textarea") as HTMLTextAreaElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(textarea, text);
      textarea.dispatchEvent(new Event("input", { bubbles: true }));
    });
  };
  const arm = () => act(() => vi.advanceTimersByTime(800));
  const settle = () => act(() => vi.advanceTimersByTimeAsync(0));
  const answered = () => calls.filter((c) => c.command === "questions_answer").map((c) => c.args);

  beforeEach(async () => {
    vi.useFakeTimers();
    await i18next.changeLanguage("en");
    calls = [];
    errors = [];
    answerFails = false;
    mockIPC((command, args) => {
      calls.push({ command, args });
      if (answerFails && command === "questions_answer") throw { code: "blocked", reason: { code: "invalidAnswer", problem: "unanswered", question: "Q1" }, detail: null };
      return null;
    });
    useApp.setState({ questionDrafts: {} });
    const container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    document.body.replaceChildren();
    vi.useRealTimers();
  });

  it("says what's going on, what will change, and what it needs to know", () => {
    show();
    expect(page()).toContain("Keel needs a decision to continue");
    expect(page()).toContain("add returns the sum of its arguments.");
    expect(page()).toContain("If `whole` is 0, what should happen?");
    expect(page()).not.toMatch(/\bspec\b|criteria|\[TBC\]/i);
  });

  it("starts with the recommended option selected, marked and focused", () => {
    show();
    expect(input(0, "Show an error").checked).toBe(true);
    expect(input(0, "Return 0").checked).toBe(false);
    expect(document.activeElement).toBe(input(0, "Show an error"));
    expect(label(0, "Show an error").textContent).toContain("Recommended");
    expect(label(0, "Return 0").textContent).not.toContain("Recommended");
  });

  it("has an Other that takes the person's own words, and a way to remember each answer", () => {
    show(waiting([question("Q1", "One?"), question("Q2", "Two?")]));
    expect(document.querySelector("textarea")).toBeNull();
    pick(1, "Other");
    expect(document.activeElement).toBe(document.querySelector("textarea"));
    expect(document.querySelectorAll('input[type="checkbox"]')).toHaveLength(2);
    expect(label(0, "Remember for this project")).toBeTruthy();
  });

  describe("using the recommended", () => {
    it("is free, and says so", () => {
      show();
      expect(page()).toContain("Free and instant: no agent runs.");
    });

    it("only goes once it has armed, and a window coming back can't press it", () => {
      show();
      expect(button("Use the recommended")?.disabled).toBe(true);
      arm();
      expect(button("Use the recommended")?.disabled).toBe(false);
      act(() => void window.dispatchEvent(new Event("blur")));
      expect(button("Use the recommended")?.disabled).toBe(true);
      act(() => void window.dispatchEvent(new Event("focus")));
      press("Use the recommended");
      expect(answered()).toEqual([]);
    });

    it("sends the recommended option for every question", async () => {
      show(waiting([question("Q1", "One?"), question("Q2", "Two?")]));
      pick(1, "Return 0");
      arm();
      press("Use the recommended");
      await settle();
      expect(answered()).toEqual([
        {
          attemptId: "spec",
          answers: [
            { question: "Q1", choice: { kind: "option", label: "Show an error" }, remember: false },
            { question: "Q2", choice: { kind: "option", label: "Show an error" }, remember: false },
          ],
        },
      ]);
    });

    it("keeps Remember where the person had it on the answer that goes", async () => {
      show();
      act(() => input(0, "Remember").click());
      arm();
      press("Use the recommended");
      await settle();
      expect(answered()).toEqual([{ attemptId: "spec", answers: [{ question: "Q1", choice: { kind: "option", label: "Show an error" }, remember: true }] }]);
    });
  });

  describe("answering", () => {
    it("has nothing to send until the person chooses something else, and says so", () => {
      show();
      arm();
      expect(button("Answer")?.disabled).toBe(true);
      expect(page()).toContain("Choose another option to answer differently.");
      pick(0, "Return 0");
      expect(button("Answer")?.disabled).toBe(false);
      expect(page()).not.toContain("Choose another option");
    });

    it("sends the option chosen, with what to remember", async () => {
      show();
      arm();
      pick(0, "Return 0");
      act(() => input(0, "Remember").click());
      press("Answer");
      await settle();
      expect(answered()).toEqual([{ attemptId: "spec", answers: [{ question: "Q1", choice: { kind: "option", label: "Return 0" }, remember: true }] }]);
    });

    it("won't send Other with nothing written, and says what's missing", () => {
      show();
      arm();
      pick(0, "Other");
      expect(button("Answer")?.disabled).toBe(true);
      expect(page()).toContain("Write your answer, or choose one of the options.");
      write("   ");
      expect(button("Answer")?.disabled).toBe(true);
      press("Answer");
      expect(answered()).toEqual([]);
      write("Neither");
      expect(button("Answer")?.disabled).toBe(false);
      expect(page()).not.toContain("Write your answer");
    });

    it("sends the person's own words, trimmed", async () => {
      show();
      arm();
      pick(0, "Other");
      write("  Neither of those  ");
      press("Answer");
      await settle();
      expect(answered()).toEqual([{ attemptId: "spec", answers: [{ question: "Q1", choice: { kind: "own", text: "Neither of those" }, remember: false }] }]);
    });

    it("sends once however fast it's pressed", async () => {
      show();
      arm();
      pick(0, "Return 0");
      press("Answer");
      press("Answer");
      await settle();
      expect(answered()).toHaveLength(1);
    });

    it("never answers on Enter", () => {
      show();
      arm();
      pick(0, "Return 0");
      for (const text of ["Answer", "Use the recommended", "Cancel"]) {
        const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
        act(() => void button(text)?.dispatchEvent(enter));
        expect(enter.defaultPrevented).toBe(true);
      }
      expect(answered()).toEqual([]);
    });

    it("says what went wrong and can be tried again", async () => {
      answerFails = true;
      show();
      arm();
      pick(0, "Return 0");
      press("Answer");
      await settle();
      expect(errors).toEqual(["Question Q1 still needs an answer."]);
      expect(button("Answer")?.disabled).toBe(false);
    });
  });

  describe("cancelling", () => {
    it("can't go from a click that brings the window back", () => {
      show();
      expect(button("Cancel")?.disabled).toBe(true);
      press("Cancel");
      expect(calls.filter((c) => c.command === "attempt_abandon")).toEqual([]);
    });

    it("needs no reason and sends nothing else", async () => {
      show();
      arm();
      press("Cancel");
      await settle();
      expect(calls.filter((c) => c.command === "attempt_abandon").map((c) => c.args)).toEqual([{ attemptId: "spec" }]);
      expect(answered()).toEqual([]);
    });
  });

  describe("what's been chosen", () => {
    it("survives leaving the page and coming back", () => {
      show();
      pick(0, "Other");
      write("Neither");
      act(() => root.unmount());
      root = createRoot(document.body.firstElementChild as HTMLElement);
      show();
      expect(input(0, "Other").checked).toBe(true);
      expect((document.querySelector("textarea") as HTMLTextAreaElement).value).toBe("Neither");
    });

    it("is gone once it's answered", async () => {
      show();
      arm();
      pick(0, "Return 0");
      press("Answer");
      await settle();
      expect(useApp.getState().questionDrafts).toEqual({});
    });
  });

  it("warns when this is the last time it can ask, and says why there's a round after that", () => {
    const earlier = (id: string) => attempt({ id, stepId: "spec", role: "specifier", output: specOutput({ answered: [{ question: "Q1", asked: "?", answer: "!", recommended: false, remember: false }] }) as unknown as Attempt["output"] });
    show();
    expect(page()).not.toContain("the last round");
    const last = waiting();
    show(last, [earlier("first"), last]);
    expect(page()).toContain("This is the last round of questions.");
    const extra = waiting();
    show(extra, [earlier("first"), earlier("second"), extra]);
    expect(page()).toContain("An extra round, because something has no recommended option.");
    expect(page()).not.toContain("This is the last round");
  });

  describe("with a question nothing recommends an answer to", () => {
    const onlyTheyKnow = { id: "Q2", text: "What discount do frequent customers get?", options: [{ label: "5 %", recommended: false }, { label: "10 %", recommended: false }] };
    const both = () => waiting([question("Q1", "If `whole` is 0, what should happen?"), onlyTheyKnow]);

    it("starts with nothing chosen there, says it's the person's to decide, and focuses the first question as before", () => {
      show(both());
      expect(input(1, "5 %").checked).toBe(false);
      expect(input(1, "10 %").checked).toBe(false);
      expect(page()).toContain("None recommended: neither the code nor the request says.");
      expect(page()).not.toContain("Recommended5 %");
      expect(document.activeElement).toBe(input(0, "Show an error"));
    });

    it("focuses its first option without choosing it when it comes first", () => {
      show(waiting([onlyTheyKnow]));
      expect(document.activeElement).toBe(input(0, "5 %"));
      expect(input(0, "5 %").checked).toBe(false);
    });

    it("can't be answered with the recommended, and says so", () => {
      show(both());
      arm();
      expect(button("Use the recommended")?.disabled).toBe(true);
      expect(page()).toContain("Not here: a question has no recommended option.");
      expect(page()).not.toContain("Free and instant");
    });

    it("needs every question answered, and then goes with the agent", async () => {
      show(both());
      arm();
      expect(button("Answer")?.disabled).toBe(true);
      expect(page()).toContain("Answer every question to go on.");
      expect(page()).not.toContain("Choose another option to answer differently.");
      pick(1, "10 %");
      expect(button("Answer")?.disabled).toBe(false);
      expect(page()).not.toContain("Answer every question to go on.");
      press("Answer");
      await settle();
      expect(answered()).toEqual([
        {
          attemptId: "spec",
          answers: [
            { question: "Q1", choice: { kind: "option", label: "Show an error" }, remember: false },
            { question: "Q2", choice: { kind: "option", label: "10 %" }, remember: false },
          ],
        },
      ]);
    });
  });

  it("keeps its buttons in view: the questions scroll, and the buttons sit outside them", () => {
    const three = waiting([question("Q1", "One?"), question("Q2", "Two?"), question("Q3", "Three?")]);
    show(three);
    const list = document.querySelector("fieldset")?.parentElement as HTMLElement;
    expect(list.className).toContain("overflow-y-auto");
    expect(list.className).not.toContain("shrink-0");
    expect(list.contains(button("Answer") as HTMLElement)).toBe(false);
    expect(list.querySelectorAll("fieldset")).toHaveLength(3);
  });

  it("is all in Spanish too", async () => {
    await i18next.changeLanguage("es");
    show();
    for (const text of ["Keel necesita una decisión para continuar", "Recomendada", "Otra", "Recordar para este proyecto", "Usar lo recomendado", "Responder", "Cancelar"]) {
      expect(page()).toContain(text);
    }
  });
});
