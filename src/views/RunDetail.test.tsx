import { mockIPC } from "@tauri-apps/api/mocks";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18next from "../i18n";
import type { Attempt, AuditEvent, CommandChange, Run, Screenshot, Screenshots, SpecOutput } from "../lib/api";
import { type CommandRun, commandsFrom } from "../lib/audit";
import { useApp } from "../lib/store";
import type { SizeDecision } from "../lib/size";
import { Actions, ActivityTab, ApprovalCard, CommandsTab, OutputTab, QuestionsCard, Summary } from "./RunDetail";

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
  let screenshots: Screenshots | null;

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
    screenshots = { shown: [], leftOut: [], moreLeftOut: 0 };
    mockIPC((command, args) => {
      if (command === "run_screenshots") return screenshots;
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

  describe("screenshots a step left", () => {
    const shot = (name: string, step = "screens", beforeLastChange = false): Screenshot => ({ name, mime: "image/png", data: "iVBORw0KGgo=", step, beforeLastChange });
    const picture = (name: string) => document.querySelector<HTMLImageElement>(`img[alt="${name}"]`);
    const shown = async () => {
      show([builder(["src/views/RunDetail.tsx"]), human], ["src/views/RunDetail.tsx"]);
      await settle();
    };

    it("shows them with the change and the step that saved each, and one bigger on a click", async () => {
      screenshots = { shown: [shot("run.light.png"), shot("run.dark.png", "builder")], leftOut: [], moreLeftOut: 0 };
      await shown();
      expect(page()).toContain("Screenshots");
      expect(picture("run.light.png")?.src).toBe("data:image/png;base64,iVBORw0KGgo=");
      expect(picture("run.dark.png")).not.toBeNull();
      expect(page()).toContain("Saved by screens");
      expect(page()).toContain("Saved by builder");
      expect(page()).not.toContain("may have changed");

      act(() => document.querySelector<HTMLButtonElement>('button[aria-label="See run.dark.png larger"]')?.click());
      const dialog = document.querySelector('[role="dialog"]');
      expect(dialog?.textContent).toContain("run.dark.png");
      expect(dialog?.textContent).toContain("Saved by builder");
      expect(dialog?.querySelector("img")?.getAttribute("src")).toBe("data:image/png;base64,iVBORw0KGgo=");
    });

    it("shows two with the same name from different steps", async () => {
      screenshots = { shown: [shot("screen.png"), shot("screen.png", "builder")], leftOut: [], moreLeftOut: 0 };
      await shown();
      expect(document.querySelectorAll('img[alt="screen.png"]')).toHaveLength(2);
    });

    it("says which the code may have changed after", async () => {
      screenshots = { shown: [shot("old.png", "builder", true), shot("new.png")], leftOut: [], moreLeftOut: 0 };
      await shown();
      expect(page().match(/The code may have changed after it was saved/g)).toHaveLength(1);
    });

    it("says what it left out and why", async () => {
      screenshots = { shown: [], leftOut: ["notes.txt", "view.svg"], moreLeftOut: 0 };
      await shown();
      expect(page()).toContain("Not shown: notes.txt, view.svg.");
      expect(page()).toContain("PNG, JPEG, GIF or WebP");
    });

    it("counts what it left out past the names it lists", async () => {
      screenshots = { shown: [], leftOut: ["notes.txt"], moreLeftOut: 12 };
      await shown();
      expect(page()).toContain("Not shown: notes.txt and 12 more.");
    });

    it("come after what proves it, so in a short window what wasn't proven shows first", async () => {
      screenshots = { shown: [shot("run.light.png"), shot("run.dark.png")], leftOut: [], moreLeftOut: 0 };
      const ux = attempt({ id: "ux", stepId: "ux", role: "reviewer", output: { verdict: "approved", findings: [], summary: "Fine.", unread: [], unchecked: ["How the Save bar looks in dark mode"] } as unknown as Attempt["output"] });
      show([builder(["src/views/RunDetail.tsx"]), ux, human], ["src/views/RunDetail.tsx"]);
      await settle();
      const labels = [...document.querySelectorAll("dt")].map((dt) => dt.textContent);
      const proves = labels.indexOf("What proves it");
      expect(proves).toBeGreaterThan(-1);
      expect(labels.indexOf("Screenshots")).toBe(proves + 1);
      expect(document.querySelectorAll("dt")[proves]?.nextElementSibling?.textContent?.startsWith("Not proven")).toBe(true);
    });

    it("has no row when a step left none", async () => {
      await shown();
      expect(page()).not.toContain("Screenshots");
    });

    it("has no row when there's no answer", async () => {
      screenshots = null;
      await shown();
      expect(page()).not.toContain("Screenshots");
    });
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

  it("says how many tests a check that passed skipped, not just that it passed", async () => {
    show([builder(["calc.py"]), { ...check, skippedTests: 385 }, human], ["calc.py"]);
    expect(page()).toContain("Keel ran `make test` and it passed, but 385 tests were skipped.");
    expect(page()).not.toContain("Keel ran `make test` and it passed.");
    show([builder(["calc.py"]), { ...check, skippedTests: 1 }, human], ["calc.py"]);
    expect(page()).toContain("Keel ran `make test` and it passed, but 1 test was skipped.");
    // None skipped, or nothing Keel could count, reads as before.
    for (const skippedTests of [0, null, undefined]) {
      show([builder(["calc.py"]), { ...check, skippedTests } as Attempt, human], ["calc.py"]);
      expect(page()).toContain("Keel ran `make test` and it passed.");
      expect(page()).not.toContain("was skipped");
      expect(page()).not.toContain("were skipped");
    }
    // A check that failed says that, whatever it skipped.
    show([builder(["calc.py"]), { ...check, state: "failed", skippedTests: 3 }, human], ["calc.py"]);
    expect(page()).toContain("Keel ran `make test` and it didn't pass.");
    expect(page()).not.toContain("were skipped");
    await i18next.changeLanguage("es");
    show([builder(["calc.py"]), { ...check, skippedTests: 385 }, human], ["calc.py"]);
    expect(page()).toContain("Keel corrió `make test` y pasó, pero se saltaron 385 pruebas.");
    show([builder(["calc.py"]), { ...check, skippedTests: 1 }, human], ["calc.py"]);
    expect(page()).toContain("Keel corrió `make test` y pasó, pero se saltó 1 prueba.");
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

    it("says when a test exists but didn't run here, apart from when there's none", async () => {
      show([builder(["calc.py"]), check, verifier([met, { ...said("C2", "unverified", "tests/db.rs:12, needs Postgres"), why: "notRun" }, { ...said("C3", "unverified"), why: "noTest" }]), human], ["calc.py"]);
      const list = proof()?.querySelector("ul")?.textContent ?? "";
      expect(list).toContain("Its test exists, but didn't run here.tests/db.rs:12, needs Postgres");
      expect(list).toContain("Not verifiedwhen c3 → then c3It found nothing that proves this.");
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, verifier([met, { ...said("C2", "unverified", "tests/db.rs:12"), why: "notRun" }]), human], ["calc.py"]);
      expect(proof()?.querySelector("ul")?.textContent).toContain("Su prueba existe, pero no corrió aquí.");
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

  describe("what the reviews couldn't check by reading the code", () => {
    const looked = (stepId: string, unchecked: string[]) => attempt({ id: stepId, stepId, role: "reviewer", output: { verdict: "approved", findings: [], summary: "Fine.", unread: [], unchecked } as unknown as Attempt["output"] });
    const met = said("C1", "met", "test_calc.py:1");
    const proves = (label = "What proves it") => [...document.querySelectorAll("dt")].find((dt) => dt.textContent === label)?.nextElementSibling?.textContent ?? "";
    const greenCheck = () => document.querySelector("details summary svg.text-success");

    it("comes first in what proves it, as not proven, though everything that had to be true was met", () => {
      show([builder(["calc.py"]), check, verifier([met]), looked("ux", ["How the Save bar looks in dark mode"]), looked("code", []), human], ["calc.py"]);
      const row = proves();
      expect(row.startsWith("Not proven")).toBe(true);
      expect(row).toContain("How the Save bar looks in dark mode");
      expect(row).toContain("ux");
      expect(row.indexOf("How the Save bar")).toBeLessThan(row.indexOf("1 of 1 met"));
      // What the verifier found stays as it was, without the check that says it's all proven.
      expect(document.querySelector("details summary")?.textContent).toBe("1 of 1 met");
      expect(greenCheck()).toBeNull();
    });

    it("isn't there when the reviews could check all of it", () => {
      show([builder(["calc.py"]), check, verifier([met]), looked("ux", []), human], ["calc.py"]);
      expect(page()).not.toContain("Not proven");
      expect(greenCheck()).not.toBeNull();
    });

    it("is there without a verifier too", () => {
      show([builder(["calc.py"]), looked("ui", ["The page in a wide window"]), human], ["calc.py"]);
      expect(proves().startsWith("Not proven")).toBe(true);
      expect(proves()).toContain("The page in a wide window");
    });

    it("is in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, verifier([met]), looked("ux", ["Cómo se ve en tema oscuro"]), human], ["calc.py"]);
      expect(proves("Qué lo demuestra").startsWith("Sin probar")).toBe(true);
      expect(proves("Qué lo demuestra")).toContain("Cómo se ve en tema oscuro");
    });

    it("is with what each review said, too", () => {
      show([builder(["calc.py"]), check, verifier([met]), looked("ux", ["How the Save bar looks in dark mode", "  "]), looked("code", []), human], ["calc.py"]);
      press("See the reviews");
      expect(page()).toContain("Couldn't check by reading the code, so not proven: How the Save bar looks in dark mode");
      expect(page().match(/Couldn't check by reading the code/g)).toHaveLength(1);
    });
  });

  describe("what the reviews found", () => {
    const found = (stepId: string, findings: unknown[]) => attempt({ id: stepId, stepId, role: "reviewer", output: { verdict: "changes", findings, summary: "Two things.", unread: [], unchecked: [] } as unknown as Attempt["output"] });
    const asked = specifier({ criteria: [{ id: "C2", when: "one is negative", then: "it still adds" }] });
    const both = () => [asked, builder(["calc.py"]), check, found("security", ["app/api.py:42 doesn't call require_role"]), found("code", [{ file: "calc.py", line: 3, text: "Subtracts when one is negative", breaks: "C2" }, { file: "test_calc.py", line: null, text: "The test name could say more", breaks: null }, { file: "calc.py", line: 9, text: "Names what isn't there", breaks: "C9" }]), human];
    const items = () => [...document.querySelectorAll('[role="dialog"] li li')].map((li) => li.textContent);

    it("shows text from before Keel asked as it was, and what a finding breaks before where it is", () => {
      show(both(), ["calc.py"]);
      press("See the reviews");
      expect(items()).toEqual(["app/api.py:42 doesn't call require_role", "Breaks C2. calc.py:3 Subtracts when one is negative", "test_calc.py The test name could say more", "calc.py:9 Names what isn't there"]);
    });

    it("says it in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show(both(), ["calc.py"]);
      press("Ver las revisiones");
      expect(items()).toContain("Rompe C2. calc.py:3 Subtracts when one is negative");
    });

    // Keel keeps a review that approved what breaks a criterion as asking for changes, and its own summary can still say all is fine.
    const keptAsChanges = () => [
      asked,
      builder(["calc.py"]),
      check,
      found("security", ["app/api.py:42 doesn't call require_role"]),
      attempt({ id: "code", stepId: "code", role: "reviewer", output: { verdict: "changes", findings: [{ file: "calc.py", line: 3, text: "Subtracts when one is negative", breaks: "C2" }], summary: "Minor things, nothing that blocks.", unread: [], unchecked: [] } as unknown as Attempt["output"] }),
      attempt({ id: "ux", stepId: "ux", role: "reviewer", output: { verdict: "approved", findings: [{ file: "calc.py", line: 9, text: "Names what isn't there", breaks: "C9" }], summary: "Fine.", unread: [], unchecked: [] } as unknown as Attempt["output"] }),
      human,
    ];
    const verdicts = () => [...([...document.querySelectorAll("dt")].find((dt) => dt.textContent?.startsWith("Revi"))?.nextElementSibling?.querySelectorAll("li") ?? [])].map((li) => li.textContent);

    it("says at a glance why a review whose summary sounds fine doesn't count as an approval", () => {
      show(keptAsChanges(), ["calc.py"]);
      expect(verdicts()).toEqual(["securityAsks for changes", "codeBreaks C2, so Keel doesn't count it as an approval", "uxNo problems"]);
      expect(page()).toContain("code: Minor things, nothing that blocks.");
      // The same in the full reviews.
      press("See the reviews");
      expect(document.querySelector('[role="dialog"]')?.textContent).toContain("codeBreaks C2, so Keel doesn't count it as an approval");
    });

    it("says why in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show(keptAsChanges(), ["calc.py"]);
      expect(verdicts()).toContain("codeRompe C2, así que Keel no lo cuenta como aprobación");
    });
  });

  // In a short window, "What proves it" can be far down the list, or out of it.
  describe("how much wasn't proven, where it can't scroll out of view", () => {
    const looked = (stepId: string, unchecked: string[]) => attempt({ id: stepId, stepId, role: "reviewer", output: { verdict: "approved", findings: [], summary: "Fine.", unread: [], unchecked } as unknown as Attempt["output"] });
    const met = said("C1", "met", "test_calc.py:1");
    const list = () => document.querySelector("dl") as HTMLElement;
    const lines = (text: string) => [...document.querySelectorAll("p")].filter((p) => p.textContent === text);
    const follows = (first: Node, then: Node) => (first.compareDocumentPosition(then) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

    it("counts what the reviews couldn't check and what wasn't verified, outside the list, by the note", () => {
      show([builder(["calc.py"]), check, verifier([met, said("C2", "unverified")]), looked("ux", ["How the Save bar looks in dark mode", "  "]), looked("code", ["The page in a wide window"]), human], ["calc.py"]);
      const found = lines("3 things weren't proven");
      expect(found).toHaveLength(1);
      expect(list().contains(found[0])).toBe(false);
      expect(follows(list(), found[0])).toBe(true);
      expect(follows(found[0], textarea())).toBe(true);
      // What each one is stays in what proves it.
      expect(list().textContent).toContain("How the Save bar looks in dark mode");
      expect(found[0].parentElement?.textContent).not.toContain("How the Save bar");
    });

    it("says one in the singular", () => {
      show([builder(["calc.py"]), check, verifier([met, said("C2", "unverified")]), human], ["calc.py"]);
      expect(lines("1 thing wasn't proven")).toHaveLength(1);
    });

    it("says why, counting each why apart, so a test that didn't run here doesn't read like no test", () => {
      const notRun = (id: string) => ({ ...said(id, "unverified", `tests/db.rs:${id}`), why: "notRun" });
      show([builder(["calc.py"]), check, verifier([met, notRun("C2"), notRun("C3"), notRun("C4"), { ...said("C5", "unverified"), why: "noTest" }, { ...said("C6", "unverified", "test_calc.py:9"), corrected: "noCheckPassed" }]), looked("ux", ["How the Save bar looks in dark mode"]), human], ["calc.py"]);
      const found = lines("6 things weren't proven");
      expect(found).toHaveLength(1);
      const why = [...(found[0].parentElement?.querySelectorAll("li") ?? [])].map((li) => li.textContent);
      expect(why).toEqual(["1: the reviews couldn't check it by reading the code", "3: their tests exist, but didn't run here", "1: no test proves it", "1: it was said to be met, but no check passed in this round"]);
      // It stays outside the list, by the note.
      expect(list().contains(found[0])).toBe(false);
    });

    it("says why in Spanish too", async () => {
      await i18next.changeLanguage("es");
      const notRun = (id: string) => ({ ...said(id, "unverified", `tests/db.rs:${id}`), why: "notRun" });
      show([builder(["calc.py"]), check, verifier([notRun("C1"), notRun("C2"), notRun("C3"), notRun("C4"), notRun("C5")]), human], ["calc.py"]);
      const found = lines("5 cosas quedaron sin probar");
      expect(found).toHaveLength(1);
      expect(found[0].parentElement?.querySelector("li")?.textContent).toBe("5: sus pruebas existen, pero no corrieron aquí");
      show([builder(["calc.py"]), check, verifier([{ ...said("C1", "unverified", "tests/db.rs:1"), why: "notRun" }, said("C2", "unverified")]), human], ["calc.py"]);
      const why = [...(lines("2 cosas quedaron sin probar")[0].parentElement?.querySelectorAll("li") ?? [])].map((li) => li.textContent);
      expect(why).toEqual(["1: su prueba existe, pero no corrió aquí", "1: ninguna prueba la demuestra"]);
    });

    // The prompt from before asked to say it in what it pointed at, so Keel doesn't say nobody did.
    it("points to what each one says when the verifier didn't give why apart", async () => {
      const old = (id: string, evidence: string) => said(id, "unverified", evidence);
      show([builder(["calc.py"]), check, verifier([old("C1", "tests/db.rs:12 needs Postgres, skipped"), old("C2", "tests/db.rs:30 exists, but didn't run"), { ...said("C3", "unverified", "tests/ui.spec.ts:4"), why: "" }]), human], ["calc.py"]);
      const found = lines("3 things weren't proven");
      expect(found).toHaveLength(1);
      expect([...(found[0].parentElement?.querySelectorAll("li") ?? [])].map((li) => li.textContent)).toEqual(["3: see what was said about each in “What proves it”"]);
      expect(page()).not.toMatch(/nobody said why/i);
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, verifier([old("C1", "requiere Postgres; se salta")]), human], ["calc.py"]);
      expect(lines("1 cosa quedó sin probar")[0].parentElement?.querySelector("li")?.textContent).toBe("1: mira qué se dijo de ella en «Qué lo demuestra»");
    });

    it("counts each one the verifier couldn't verify when it verified none", () => {
      show([builder(["calc.py"]), check, verifier([said("C1", "unverified"), said("C2", "unverified")]), human], ["calc.py"]);
      expect(lines("2 things weren't proven")).toHaveLength(1);
    });

    it("leaves the card as it was when everything was proven", () => {
      show([builder(["calc.py"]), check, verifier([met]), looked("ux", []), human], ["calc.py"]);
      expect(page()).not.toMatch(/proven/);
      expect(button("See which")).toBeUndefined();
      expect(list().nextElementSibling?.contains(textarea())).toBe(true);
    });

    it("doesn't count what was found not met: that isn't unproven, and says so in what proves it", () => {
      show([builder(["calc.py"]), check, verifier([met, said("C2", "unmet", "calc.py:3")]), human], ["calc.py"]);
      expect(page()).not.toMatch(/proven/);
    });

    it("takes you to what proves it, in the list", () => {
      const scrolled = vi.spyOn(Element.prototype, "scrollIntoView").mockImplementation(() => {});
      try {
        show([builder(["calc.py"]), check, looked("ux", ["How the Save bar looks in dark mode"]), human], ["calc.py"]);
        press("See which");
        expect(scrolled).toHaveBeenCalledTimes(1);
        expect(scrolled).toHaveBeenCalledWith({ block: "nearest" });
        const row = scrolled.mock.contexts[0] as Element;
        expect(row.querySelector("dt")?.textContent).toBe("What proves it");
        expect(list().contains(row)).toBe(true);
        expect(answers).toEqual([]);
      } finally {
        scrolled.mockRestore();
      }
    });

    it("is in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, looked("ux", ["Cómo se ve en tema oscuro", "Cómo se ve en una ventana ancha"]), human], ["calc.py"]);
      expect(lines("2 cosas quedaron sin probar")).toHaveLength(1);
      expect(button("Ver cuáles")).toBeDefined();
      show([builder(["calc.py"]), check, looked("ux", ["Cómo se ve en tema oscuro"]), human], ["calc.py"]);
      expect(lines("1 cosa quedó sin probar")).toHaveLength(1);
      expect(button("Ver cuál")).toBeDefined();
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

    it("says apart what was left out for the risks of what was asked", async () => {
      const nothing = attempt({ id: "ux", stepId: "ux", role: "reviewer", state: "skipped", reason: { code: "noMatchingPaths", paths: ["**/*.tsx"] } });
      const state = attempt({ id: "state", stepId: "state", role: "reviewer", state: "skipped", reason: { code: "noMatchingRisks", risks: ["concurrency"] } });
      show([builder(["calc.py"]), check, reviewer, nothing, state, human], ["calc.py"]);
      expect(row("Reviews")).toContain("ux: skipped, nothing they cover changed");
      expect(row("Reviews")).toContain("state: skipped, what was asked names none of the risks they look at");
      await i18next.changeLanguage("es");
      show([builder(["calc.py"]), check, reviewer, nothing, state, human], ["calc.py"]);
      expect(row("Revisiones")).toContain("state: omitidas, lo pedido no nombra ninguno de los riesgos que revisan");
    });

    it("never shows a review that couldn't read all of the change as no problems", () => {
      const partly = attempt({ id: "code", stepId: "code", role: "reviewer", output: { verdict: "changes", findings: [], summary: "Fine as far as I got.", unread: ["src/b.ts"] } as unknown as Attempt["output"] });
      show([builder(["calc.py"]), check, partly, human], ["calc.py"]);
      expect(row("Reviews")).toContain("Didn't read all of it");
      expect(row("Reviews")).not.toContain("No problems");
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
      // By the id a review names it by when it says the change breaks it.
      expect(dialog).toContain("L1 Its signature stays");
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

      it("shows what the request already decided apart, saying when an assistant or the project's GitHub account wrote it", async () => {
        const fromRequest = [
          { text: "If the author resolves their own critical thread, it counts for now.", taken: "request" as const },
          { text: "Old threads stay as they are.", taken: "request" as const, assistant: "Claude Code" },
          { text: "Invoices already sent keep their total.", taken: "request" as const, githubAccount: "alice" },
          ...decided,
        ];
        show([specifier({ decided: fromRequest }), builder(["calc.py"]), human], ["calc.py"], building);
        const row = (label: string) => [...document.querySelectorAll("dt")].find((dt) => dt.textContent?.startsWith(label))?.parentElement?.textContent ?? "";
        const inRequest = row("Decided in the request");
        expect(inRequest).toContain("If the author resolves their own critical thread, it counts for now.");
        expect(inRequest).toContain("Old threads stay as they are.ChangeClaude Code wrote it");
        expect(inRequest).toContain("Invoices already sent keep their total.ChangeWritten with the project's GitHub account");
        expect(inRequest).not.toContain("Keeps the rounding as it is");
        expect(inRequest).not.toContain("Written by the agent");
        const alone = row("What it decided alone");
        expect(alone).toContain("Keeps the rounding as it is");
        expect(alone).toContain("You chose this.");
        expect(alone).not.toContain("Old threads stay");
        expect(page().match(/wrote it/g)).toHaveLength(1);
        expect(page().match(/GitHub account/g)).toHaveLength(1);

        await i18next.changeLanguage("es");
        show([specifier({ decided: fromRequest }), builder(["calc.py"]), human], ["calc.py"], building);
        expect(page()).toContain("Lo escribió Claude Code");
        expect(page()).toContain("Lo escribió la cuenta del proyecto en GitHub");
      });

      it("has no row for the request when it decided nothing, nor for the agent when only the request did", () => {
        show([specifier({ decided: [{ text: "Old threads stay as they are.", taken: "request" }] }), builder(["calc.py"]), human], ["calc.py"], building);
        expect(page()).toContain("Decided in the request");
        expect(page()).not.toContain("What it decided alone");
      });

      it("says on its line what a review questions, for the person to decide, and quotes one that matches no line", () => {
        const questioned = [
          { decided: "“keeps the rounding  as it is.”", why: "Totals now drift by a cent." },
          { decided: "Sorts by name", why: "The list was by date." },
        ];
        const approver = attempt({ id: "approver", stepId: "approver", role: "approver", output: { decision: "approve", risk: "low", reasons: "The code review questions the rounding, which is the person's to decide.", instructions: "", questioned } as unknown as Attempt["output"] });
        show([specifier({ decided }), builder(["calc.py"]), approver, human], ["calc.py"], building);
        const line = (text: string) => [...document.querySelectorAll("li")].find((li) => li.textContent?.startsWith(text))?.textContent ?? "";
        expect(line("Keeps the rounding as it is")).toContain("A review questions this, and it's yours to decide: Totals now drift by a cent.");
        expect(change("Keeps the rounding as it is")).not.toBeNull();
        expect(line("If it's 0?")).not.toContain("A review questions");
        expect(page()).toContain("A review questions “Sorts by name”, and it's yours to decide: The list was by date.");
        expect(page().match(/A review questions/g)).toHaveLength(2);
      });

      describe("by the number of its line", () => {
        const questionedBy = (questioned: unknown[]) => {
          const approver = attempt({ id: "approver", stepId: "approver", role: "approver", output: { decision: "approve", risk: "low", reasons: "The code review questions what was decided alone.", instructions: "", questioned } as unknown as Attempt["output"] });
          show([specifier({ decided }), builder(["calc.py"]), approver, human], ["calc.py"], building);
        };
        const line = (text: string) => [...document.querySelectorAll("li")].find((li) => li.textContent?.startsWith(text))?.textContent ?? "";

        it("goes on the line its number names, though the quote is in other words", () => {
          questionedBy([{ decided: "El redondeo sigue igual", number: 1, why: "Totals now drift by a cent." }]);
          expect(line("Keeps the rounding as it is")).toContain("A review questions this, and it's yours to decide: Totals now drift by a cent.");
          expect(page()).not.toContain("A review questions “El redondeo sigue igual”");
          expect(page().match(/A review questions/g)).toHaveLength(1);
        });

        it("goes by the quote, as before, without a number or with one outside the list", () => {
          questionedBy([
            { decided: "round up? leave it", why: "No number." },
            { decided: "Sorts by name", number: 0, why: "Before the first." },
            { decided: "Sorts by date", number: 4, why: "Past the last." },
            { decided: "Keeps the rounding as it is", number: 1.5, why: "Not a whole number." },
          ]);
          expect(line("Round up? Leave it")).toContain("A review questions this, and it's yours to decide: No number.");
          expect(page()).toContain("A review questions “Sorts by name”, and it's yours to decide: Before the first.");
          expect(page()).toContain("A review questions “Sorts by date”, and it's yours to decide: Past the last.");
          expect(line("Keeps the rounding as it is")).toContain("A review questions this, and it's yours to decide: Not a whole number.");
          expect(page().match(/A review questions/g)).toHaveLength(4);
        });

        it("goes by the quote when it's word for word another line, which a wrong number can't move", () => {
          questionedBy([{ decided: "Keeps the rounding as it is", number: 3, why: "Totals now drift by a cent." }]);
          expect(line("Keeps the rounding as it is")).toContain("Totals now drift by a cent.");
          expect(line("Round up? Leave it")).not.toContain("A review questions");
        });
      });

      describe("what the change added that nothing asks for", () => {
        const added = { decided: "Fails the check when a thread's severity is unknown", number: null, round: 1, why: "A lost save leaves the PR unreviewed for good." };
        const questionedBy = (questioned: unknown[], fields: Partial<SpecOutput> = { decided }) => {
          const approver = attempt({ id: "approver", stepId: "approver", role: "approver", output: { decision: "approve", risk: "low", reasons: "The code review found a rule nothing asks for.", instructions: "", questioned } as unknown as Attempt["output"] });
          show([specifier(fields), builder(["calc.py"]), approver, human], ["calc.py"], building);
        };
        const line = (text: string) => [...document.querySelectorAll("li")].find((li) => li.textContent?.startsWith(text))?.textContent ?? "";

        it("shows with the other questioned ones, saying in which round it appeared, with a way to change it", () => {
          questionedBy([{ decided: "El redondeo sigue igual", number: 1, why: "Totals now drift by a cent." }, added]);
          expect(line("Keeps the rounding as it is")).toContain("A review questions this, and it's yours to decide: Totals now drift by a cent.");
          expect(line(added.decided)).toContain("The change added this in round 1, and nothing asked for it. It's yours to decide: A lost save leaves the PR unreviewed for good.");
          expect(change(added.decided)).not.toBeNull();
          expect(page()).not.toContain("A review questions “Fails the check");
        });

        it("sends the person's words about it, as with a line it decided alone", async () => {
          questionedBy([added]);
          act(() => change(added.decided)?.click());
          act(() => vi.advanceTimersByTime(800));
          write("Drop that rule");
          press("Send it back");
          await settle();
          expect(sentBack).toEqual([{ attemptId: "human", instructions: "About “Fails the check when a thread's severity is unknown”: Drop that rule" }]);
        });

        it("shows even when the specification decided nothing alone", () => {
          questionedBy([added], {});
          expect(page()).toContain("What it decided alone");
          expect(line(added.decided)).toContain("The change added this in round 1");
        });

        it("reads one without a round as before, quoted after the list", () => {
          questionedBy([
            { decided: "Sorts by name", number: null, why: "No round." },
            { decided: "Sorts by size", number: null, round: 0, why: "Before the first round." },
          ]);
          expect(page()).toContain("A review questions “Sorts by name”, and it's yours to decide: No round.");
          expect(page()).toContain("A review questions “Sorts by size”, and it's yours to decide: Before the first round.");
          expect(page()).not.toContain("The change added this");
        });
      });

      it("questions nothing when the approver's answer is from before it could", () => {
        const approver = attempt({ id: "approver", stepId: "approver", role: "approver", output: { decision: "approve", risk: "low", reasons: "Fine.", instructions: "" } as unknown as Attempt["output"] });
        show([specifier({ decided }), builder(["calc.py"]), approver, human], ["calc.py"], building);
        expect(page()).toContain("Keeps the rounding as it is");
        expect(page()).not.toContain("A review questions");
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

  describe("where a change across projects is agreed", () => {
    const agreement = [
      { type: "agent", id: "spec" },
      { type: "approval", id: "human" },
    ];
    const parts = [
      { project: "valora", text: "Sends the token" },
      { project: "app", text: "Reads the token" },
    ];
    const agreed = specifier({ parts, together: ["Log in on Valora and land in back-adm"] });

    it("shows each part in order and what's tried together, without having to open anything", () => {
      show([agreed, human], [], agreement);
      expect(page()).toContain("Approve what was agreed?");
      expect(page()).toContain("What changes in each project");
      const items = [...document.querySelectorAll("ol li")].map((item) => item.textContent);
      expect(items).toEqual(["valora: Sends the token", "app: Reads the token"]);
      expect(page()).toContain("Log in on Valora and land in back-adm");
      act(() => vi.advanceTimersByTime(800));
      press("Approve and open the parts");
      expect(answers).toEqual([{ attemptId: "human", approve: true, text: "" }]);
    });

    it("shows what the parts agree on exactly, so it's approved along with them", () => {
      show([specifier({ parts, contract: [{ id: "K1", when: "valora posts /session", then: "app answers 201", projects: ["valora", "app"] }] }), human], [], agreement);
      expect(page()).toContain("What the parts agree on exactly");
      expect(page()).toContain("K1 (valora, app): valora posts /session → app answers 201");
    });

    it("is an ordinary approval when nothing has parts", () => {
      show([specifier(), human], [], agreement);
      expect(page()).not.toContain("Approve what was agreed?");
      expect(button("Approve and open the parts")).toBeUndefined();
    });

    it("says it in Spanish too", async () => {
      await i18next.changeLanguage("es");
      show([agreed, human], [], agreement);
      expect(page()).toContain("¿Apruebas lo acordado?");
      expect(button("Aprobar y abrir las partes")).toBeDefined();
    });
  });

  it("warns, before approving a part, about what became of the parts before it", () => {
    const joint = { changeId: "plan", agrees: false, part: { changeId: "plan", position: 2, total: 2, waitsFor: null, warnings: [{ kind: "closed" as const, position: 1, project: "valora" }], contractChanged: ["K2"] } };
    act(() => root.render(<ApprovalCard attempt={human} run={runWith(deliverNext)} attempts={[builder(["calc.py"]), human]} files={["calc.py"]} commands={[]} rawCheck="intact" joint={joint} onShowChanges={() => {}} onError={() => {}} />));
    expect(page()).toContain("Part 1, in valora: its pull request was closed.");
    expect(document.querySelector("[data-contract-changed]")?.textContent).toBe("Specified with an earlier version of what the parts agree on (K2). Only a new run of this part takes the current one.");
    act(() => vi.advanceTimersByTime(800));
    expect(button("Approve and open a draft")?.disabled).toBe(false);
  });

  describe("back after the work was sent back", () => {
    const returned = (output: Record<string, unknown>) => attempt({ id: "human-0", stepId: "human", kind: "approval", tools: "none", state: "returned", output: output as unknown as Attempt["output"] });
    const again = { ...human, round: 1 };
    const showRound = (attempts: Attempt[], round = 1) => act(() => root.render(<ApprovalCard attempt={again} run={{ ...runWith(deliverNext), round }} attempts={attempts} files={["calc.py"]} commands={[]} rawCheck="intact" onShowChanges={() => {}} onError={() => {}} />));

    it("says which assistant sent it back, and what it asked for", () => {
      showRound([builder(["calc.py"]), returned({ instructions: "Return 0 for an empty list.", by: "Claude Code" }), { ...builder(["calc.py"]), id: "builder-1", round: 1 }, again]);
      expect(page()).toContain("Why this round");
      expect(page()).toContain("Claude Code sent it back:");
      expect(page()).toContain("Return 0 for an empty list.");
    });

    it("looks just as before when the person sent it back", () => {
      showRound([builder(["calc.py"]), { ...builder(["calc.py"]), id: "builder-1", round: 1 }, again]);
      const before = page();
      showRound([builder(["calc.py"]), returned({ instructions: "Return 0 for an empty list." }), { ...builder(["calc.py"]), id: "builder-1", round: 1 }, again]);
      expect(page()).toBe(before);
      expect(page()).not.toContain("Why this round");
    });

    it("says nothing once something else started a later round", () => {
      showRound([builder(["calc.py"]), returned({ instructions: "Return 0 for an empty list.", by: "Claude Code" }), again], 2);
      expect(page()).not.toContain("Claude Code sent it back");
    });

    it("says it in Spanish too", async () => {
      await i18next.changeLanguage("es");
      showRound([builder(["calc.py"]), returned({ instructions: "Devuelve 0.", by: "Claude Code" }), again]);
      expect(page()).toContain("Por qué esta ronda");
      expect(page()).toContain("Claude Code lo devolvió:");
    });

    it("says in which round the change added what nothing asks for, which may be an earlier one", async () => {
      const approver = (round: number, appeared: number) => attempt({ id: `approver-${round}`, stepId: "approver", role: "approver", round, output: { decision: "approve", risk: "low", reasons: "r", instructions: "", questioned: [{ decided: "Retries a lost save", number: null, round: appeared, why: "Nothing asks for it." }] } as unknown as Attempt["output"] });
      showRound([specifier(), builder(["calc.py"]), approver(0, 1), { ...builder(["calc.py"]), id: "builder-1", round: 1 }, approver(1, 1), again]);
      expect(page()).toContain("The change added this in round 1, and nothing asked for it. It's yours to decide: Nothing asks for it.");
      // Only this round's approver speaks.
      expect(page().match(/The change added this/g)).toHaveLength(1);
      await i18next.changeLanguage("es");
      showRound([specifier(), builder(["calc.py"]), approver(1, 2), again]);
      expect(page()).toContain("El cambio agregó esto en la ronda 2, y nada lo pedía. Te toca decidirlo: Nothing asks for it.");
    });
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

describe("the run summary", () => {
  let root: Root;
  const page = () => document.body.textContent ?? "";
  const show = (attempts: Attempt[], commands: CommandRun[], finishedAt: string | null = null) => {
    const run = { ...runWith(deliverNext), finishedAt } as Run;
    act(() => root.render(<Summary run={run} attempts={attempts} current={attempts[attempts.length - 1]} commands={commands} rawCheck="intact" closedBy={null} onShowChanges={() => {}} />));
  };
  const command = (status: string) => ({ status }) as unknown as CommandRun;

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC(() => null);
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("says it's the run's so far, and names the step that's working", () => {
    show([attempt({ id: "spec", stepId: "spec" }), attempt({ id: "builder", stepId: "builder", state: "running" })], []);
    expect(document.querySelector("h2")?.textContent).toBe("The run so far");
    expect(document.querySelector("h2")?.parentElement?.className).toContain("border-l-accent");
    expect(page()).toContain("builder is still working. Its explanation appears here when it finishes.");
    expect(page()).not.toContain("spec is still working");
  });

  it("names the last step that started when several are working", () => {
    show([attempt({ id: "a", stepId: "code", state: "running" }), attempt({ id: "b", stepId: "tests", state: "running" })], []);
    expect(page()).toContain("tests is still working.");
  });

  it("counts the commands as the run's", () => {
    show([spec("Done.")], [...Array(8).fill(command("ok")), command("failed")]);
    expect(page()).toContain("8 of 9 commands of the run succeeded.");
  });

  it("says when the run ran no commands", () => {
    show([spec("Done.")], []);
    expect(page()).toContain("The run didn't run any commands.");
  });

  it("says how many tests the checks skipped, though they passed", async () => {
    const lint = attempt({ id: "lint", stepId: "lint", kind: "check", tools: "check", prompt: "make lint", skippedTests: 2 });
    show([builder(["calc.py"]), { ...check, skippedTests: 385 }, lint], []);
    expect(page()).toContain("2 of 2 checks passed.");
    expect(page()).toContain("387 tests were skipped, so they prove nothing.");
    // Only the latest attempt at each check counts.
    show([builder(["calc.py"]), { ...check, skippedTests: 385 }, { ...check, id: "check-2", skippedTests: 1 }], []);
    expect(page()).toContain("1 test was skipped, so it proves nothing.");
    show([builder(["calc.py"]), { ...check, skippedTests: 0 }, { ...lint, skippedTests: null }], []);
    expect(page()).toContain("2 of 2 checks passed.");
    expect(page()).not.toContain("skipped");
    await i18next.changeLanguage("es");
    show([builder(["calc.py"]), { ...check, skippedTests: 385 }], []);
    expect(page()).toContain("Se saltaron 385 pruebas, así que no demuestran nada.");
  });

  it("is titled as the run's once it's over", () => {
    show([spec("Done.")], [], "2026-01-01T00:00:00Z");
    expect(document.querySelector("h2")?.textContent).toBe("The run");
  });

  it("is titled as the run's, and not set apart, while it waits for a person", () => {
    show([spec("Done."), attempt({ id: "human", stepId: "human", kind: "approval", tools: "none", state: "waitingApproval" })], []);
    expect(document.querySelector("h2")?.textContent).toBe("The run");
    expect(page()).not.toContain("The run so far");
    expect(document.querySelector("h2")?.parentElement?.className).not.toContain("border-l-accent");
  });

  it("is all in Spanish too", async () => {
    await i18next.changeLanguage("es");
    show([attempt({ id: "spec", stepId: "spec" }), attempt({ id: "builder", stepId: "builder", state: "running" })], [command("ok"), command("failed")]);
    expect(page()).toContain("La corrida hasta ahora");
    expect(page()).toContain("builder sigue trabajando.");
    expect(page()).toContain("1 de 2 comandos de la corrida salieron bien.");
  });

  describe("once a person approved it", () => {
    const approved = attempt({ id: "human", stepId: "human", kind: "approval", tools: "none", state: "succeeded" });
    const met = said("C1", "met", "test_calc.py:1");
    const looked = (unchecked: string[]) => attempt({ id: "ux", stepId: "ux", role: "reviewer", output: { verdict: "approved", findings: [], summary: "Fine.", unread: [], unchecked } as unknown as Attempt["output"] });
    const greenCheck = () => document.querySelector("details summary svg.text-success");

    it("still says what the reviews couldn't check, without the check that says it's all proven", () => {
      show([builder(["calc.py"]), check, verifier([met]), looked(["How the Save bar looks in dark mode"]), approved], [], "2026-01-01T00:00:00Z");
      expect(page()).toContain("Not proven: the reviews couldn't check this by reading the code.");
      expect(page()).toContain("How the Save bar looks in dark mode");
      expect(page().indexOf("How the Save bar")).toBeLessThan(page().indexOf("1 of 1 met"));
      expect(document.querySelector("details summary")?.textContent).toBe("1 of 1 met");
      expect(greenCheck()).toBeNull();
    });

    it("keeps the check when the reviews could check all of it", () => {
      show([builder(["calc.py"]), check, verifier([met]), looked([]), approved], [], "2026-01-01T00:00:00Z");
      expect(page()).not.toContain("Not proven");
      expect(greenCheck()).not.toBeNull();
    });
  });
});

describe("what a check printed", () => {
  let root: Root;
  const page = () => document.body.textContent ?? "";
  const ended = (exitCode: number): AuditEvent => ({ seq: 1, attemptId: "check", at: "", kind: "checkResult", data: { kind: "check", exitCode, stdout: "test result: ok.", stderr: "" } });
  const show = (attempt: Attempt, exitCode: number) => act(() => root.render(<OutputTab events={[ended(exitCode)]} attempt={attempt} />));

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("says how many tests it skipped when it passed", () => {
    show({ ...check, skippedTests: 385 }, 0);
    expect(page()).toContain("The check ended with code 0");
    expect(page()).toContain("385 tests were skipped, so they prove nothing.");
    for (const skippedTests of [0, null]) {
      show({ ...check, skippedTests }, 0);
      expect(page()).not.toContain("skipped");
    }
    show({ ...check, state: "failed", skippedTests: 3 }, 1);
    expect(page()).toContain("The check ended with code 1");
    expect(page()).not.toContain("skipped");
  });
});

describe("the commands a step ran", () => {
  let root: Root;
  const page = () => document.body.textContent ?? "";
  const event = (seq: number, kind: string, data: Record<string, unknown>): AuditEvent => ({ seq, attemptId: "builder", at: "2026-01-01T00:00:00Z", kind, data });
  // What the builder ran in run 40c0fe47: fifteen tests failed, and head still ended with 0.
  const pipedTests = 'cargo test --workspace 2>&1 | grep -E "^test result|FAILED|failed|panicked|error" | head -30';
  const ran = (command: string, result: Record<string, unknown>) => [event(1, "toolCall", { id: "1", name: "Bash", input: { command } }), event(2, "toolResult", { id: "1", isError: false, text: "", ...result })];
  const failedBehindPipe = ran(pipedTests, { text: "test prompts::it_works ... FAILED\ntest result: FAILED. 120 passed; 15 failed" });
  const failedPlain = ran("cargo test --workspace", { isError: true, exitCode: 101, text: "Exit code 101\ntest result: FAILED. 120 passed; 15 failed" });
  const passedPlain = ran("cargo test --workspace", { text: "test result: ok. 135 passed; 0 failed" });
  const listed = (events: AuditEvent[]) => act(() => root.render(<CommandsTab commands={commandsFrom(events)} />));
  const inActivity = (events: AuditEvent[]) => act(() => root.render(<ActivityTab events={events} role="worker" />));

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC(() => null);
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("doesn't say tests that failed behind a pipe went well, and says why it can't tell", () => {
    listed(failedBehindPipe);
    expect(page()).not.toContain("succeeded");
    expect(page()).toContain("finished");
    expect(page()).toContain("With pipes, the exit code may not reflect the result.");
    expect(page()).toContain("The output says “FAILED”.");
  });

  it("still says a plain run of the tests that failed failed, with its code", () => {
    listed(failedPlain);
    expect(page()).toContain("failed with code 101");
    expect(page()).not.toContain("The output says");
    expect(page()).not.toContain("With pipes");
  });

  it("says a plain run that went well went well, with nothing to warn about", () => {
    listed(passedPlain);
    expect(page()).toContain("succeeded");
    expect(page()).not.toContain("The output says");
    expect(page()).not.toContain("With pipes");
  });

  it("warns about a failure in the output even when the code is 0 and there's no pipe", () => {
    listed(ran("cargo build || true", { text: "error[E0308]: mismatched types" }));
    expect(page()).not.toContain("succeeded");
    expect(page()).toContain("The output says “error”.");
    expect(page()).not.toContain("With pipes");
  });

  it("tells the same in the activity, from the same answer", () => {
    inActivity(failedBehindPipe);
    expect(page()).toContain("With pipes, the exit code may not reflect the result.");
    expect(page()).toContain("The output says “FAILED”.");

    inActivity(failedPlain);
    expect(page()).toContain("exit code 101");
    expect(page()).not.toContain("The output says");
  });

  it("only reads a command's output for failures, not a file that was read", () => {
    inActivity([event(1, "toolCall", { id: "1", name: "Read", input: { file_path: "notes.md" } }), event(2, "toolResult", { id: "1", isError: false, text: "error: this is what it prints when it fails" })]);
    expect(page()).not.toContain("The output says");
  });

  it("is all in Spanish too", async () => {
    await i18next.changeLanguage("es");
    listed(failedBehindPipe);
    expect(page()).not.toContain("salió bien");
    expect(page()).toContain("terminó");
    expect(page()).toContain("Con tuberías, el código de salida puede no reflejar el resultado.");
    expect(page()).toContain("La salida dice «FAILED».");

    listed(failedPlain);
    expect(page()).toContain("falló con código 101");
  });
});

describe("an answer Keel asked to fix", () => {
  let root: Root;
  const page = () => document.body.textContent ?? "";
  const event = (seq: number, kind: string, data: Record<string, unknown>): AuditEvent => ({ seq, attemptId: "spec", at: "2026-01-01T00:00:00Z", kind, data });
  // What run #47 wrote first: a criterion and a decision past their limits.
  const problems = [
    { code: "criterionTooLong", id: "C2", chars: 209, limit: 200 },
    { code: "decidedTooLong", position: 1, chars: 141, limit: 140 },
  ];
  const events = [event(1, "result", {}), event(2, "finished", { state: "failed", reason: { code: "specNotUsable", problems } }), event(3, "specCorrected", { problems })];
  const inActivity = (corrected: boolean) => act(() => root.render(<ActivityTab events={events} role="specifier" corrected={corrected} />));

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC(() => null);
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("doesn't say it failed, and says what went past its limit, by how much", () => {
    inActivity(true);
    expect(page()).not.toContain("Failed");
    expect(page()).toContain("Final state: Keel asked for a fix");
    expect(page()).toContain("C2 has 209 characters, and the limit is 200");
    expect(page()).toContain("item 1 of what it decided alone has 141 characters, and the limit is 140");
  });

  it("says it failed while nothing went on to fix it", () => {
    inActivity(false);
    expect(page()).toContain("Final state: Failed");
  });

  it("is all in Spanish too", async () => {
    await i18next.changeLanguage("es");
    inActivity(true);
    expect(page()).not.toContain("Falló");
    expect(page()).toContain("Estado final: Keel pidió corregirlo");
    expect(page()).toContain("el punto 1 de lo que decidió por su cuenta tiene 141 caracteres y el límite es 140");
  });

  it("says which points of the contract a part's specifier wrote again, and that what was agreed stands", async () => {
    const replaced = [event(1, "contractReplaced", { replaced: ["K1", "K3"] }), event(2, "finished", { state: "succeeded" })];
    act(() => root.render(<ActivityTab events={replaced} role="specifier" corrected={false} />));
    expect(page()).toContain("It wrote K1, K3 in its own words. Keel kept them as they were agreed.");
    await i18next.changeLanguage("es");
    act(() => root.render(<ActivityTab events={[...replaced]} role="specifier" corrected={false} />));
    expect(page()).toContain("Escribió K1, K3 con sus propias palabras. Keel los dejó como se acordaron.");
  });

  it("says which points of the contract a part's specifier was given, and nothing when it was given none", async () => {
    const given = (points: { id: string }[]) => [event(1, "contractGiven", { points }), event(2, "finished", { state: "succeeded" })];
    act(() => root.render(<ActivityTab events={given([{ id: "K1" }, { id: "K4" }])} role="specifier" corrected={false} />));
    expect(page()).toContain("Keel added K1, K4 of what the parts agree on to its criteria, as they read then.");
    act(() => root.render(<ActivityTab events={given([])} role="specifier" corrected={false} />));
    expect(page()).not.toContain("what the parts agree on");
    await i18next.changeLanguage("es");
    act(() => root.render(<ActivityTab events={given([{ id: "K1" }])} role="specifier" corrected={false} />));
    expect(page()).toContain("Keel agregó K1 de lo que las partes acuerdan a sus criterios, como decían entonces.");
  });
});

describe("starting over a step whose command the project changed", () => {
  let root: Root;
  let changes: CommandChange[];
  let retried: unknown[];
  const page = () => document.body.textContent ?? "";
  const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent === text);
  const settle = () => act(() => Promise.resolve());
  const failedCheck = attempt({ id: "check-2", runId: "r1", stepId: "check", kind: "check", tools: "check", state: "failed", reason: { code: "checkFailed", exitCode: 1 }, prompt: "uv run pytest" });
  const failedSetup = attempt({ id: "setup-1", runId: "r1", stepId: "setup", kind: "setup", tools: "setup", state: "failed", reason: { code: "setupFailed", exitCode: 1 }, prompt: "uv sync" });
  const newSetup: CommandChange = { stepId: "setup", kind: "setup", before: "uv sync", now: "uv sync --group test" };
  const newCheck: CommandChange = { stepId: "check", kind: "check", before: "uv run pytest", now: "uv run --group test pytest" };
  const offered = () => button("Start over with the new commands") !== undefined;
  const show = async (shown: Attempt, agentsWrote = false) => {
    act(() => root.render(<Actions attempt={shown} projectId="p1" agentsWrote={agentsWrote} onError={() => {}} />));
    await settle();
  };
  const limited = "This setup runs with no more permissions than the agents, because an agent that can change the code has run before it: no internet, and not the project's shared cache. An install that needs to download something may fail. If it does, start a new run.";
  const projectHas = (setupCommand: string, checkCommand: string) => useApp.setState({ overview: { ...useApp.getState().overview, projects: [{ id: "p1", setupCommand, checkCommand }] as never } });

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    changes = [];
    retried = [];
    mockIPC((command, args) => {
      if (command === "run_command_changes") return changes;
      if (command === "attempt_retry") retried.push(args);
      return null;
    });
    projectHas("uv sync", "uv run pytest");
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("offers the new commands, says which they were and which they are, and runs them only when chosen", async () => {
    changes = [newSetup, newCheck];
    await show(failedCheck);
    expect(page()).toContain("The project's commands changed after this run started.");
    for (const command of ["uv sync", "uv sync --group test", "uv run pytest", "uv run --group test pytest"]) expect(page()).toContain(command);
    // The setup runs again first, so what it installs is there for the check.
    expect(page()).toContain("The new setup runs first, then this step again.");
    act(() => button("Start over")?.click());
    await settle();
    act(() => button("Start over with the new commands")?.click());
    await settle();
    expect(retried).toEqual([
      { attemptId: "check-2", resume: false, currentCommands: false },
      { attemptId: "check-2", resume: false, currentCommands: true },
    ]);
  });

  it("offers them to a check when its setup changed, though the check didn't", async () => {
    changes = [newSetup];
    await show(failedCheck);
    expect(offered()).toBe(true);
    expect(page()).toContain("The new setup runs first, then this step again.");
  });

  it("offers them to a setup only when the setup changed", async () => {
    changes = [newCheck];
    await show(failedSetup);
    expect(offered()).toBe(false);
    changes = [newSetup];
    await show({ ...failedSetup, id: "setup-2" });
    expect(offered()).toBe(true);
    // It is the setup, so nothing else runs before it.
    expect(page()).not.toContain("runs first");
  });

  it("offers nothing new when what the step needs is still the project's", async () => {
    await show(failedCheck);
    expect(button("Start over")).toBeDefined();
    expect(offered()).toBe(false);
    expect(page()).not.toContain("changed after this run started");
    // A check of the workflow's own runs the same either way.
    changes = [newCheck];
    await show({ ...failedCheck, id: "lint-1", stepId: "lint" });
    expect(offered()).toBe(false);
  });

  it("asks again once the project's commands change, so what it says now is what the project has", async () => {
    changes = [newCheck];
    await show(failedCheck);
    expect(page()).toContain("uv run --group test pytest");
    changes = [{ ...newCheck, now: "uv run --group test --group dev pytest" }];
    act(() => projectHas("uv sync", "uv run --group test --group dev pytest"));
    await settle();
    expect(page()).toContain("uv run --group test --group dev pytest");
  });

  it("says the new setup gets no more than the agents once they've changed the code, and that a new run is the way out", async () => {
    changes = [newSetup];
    await show(failedCheck, true);
    expect(page()).toContain(limited);
    // Before any agent, the setup runs what's published, as the first one did.
    await show({ ...failedCheck, id: "check-3" }, false);
    expect(page()).toContain("The new setup runs first, then this step again.");
    expect(page()).not.toContain("no more permissions than the agents");
  });

  it("says it of a setup started over once agents have run, and not of a check started over as it was", async () => {
    await show(failedSetup, true);
    expect(page()).toContain(limited);
    await show(failedCheck, true);
    expect(page()).not.toContain("no more permissions than the agents");
  });

  it("is all in Spanish too", async () => {
    await i18next.changeLanguage("es");
    changes = [newSetup, newCheck];
    await show(failedCheck, true);
    expect(page()).toContain("Los comandos del proyecto cambiaron después de que empezó esta ejecución.");
    expect(page()).toContain("La preparación nueva corre primero, y después este paso otra vez.");
    expect(page()).toContain("Esta preparación corre sin más permisos que los agentes, porque antes corrió un agente que puede cambiar el código");
    expect(button("Empezar de nuevo con los comandos nuevos")).toBeDefined();
  });

  it("says in the activity which commands the step started over with", async () => {
    const event: AuditEvent = { seq: 1, attemptId: "check-2", at: "2026-01-01T00:00:00Z", kind: "commandsUpdated", data: { changes: [newCheck] } };
    act(() => root.render(<ActivityTab events={[event]} role="worker" />));
    expect(page()).toContain("Started over with the project's new commands");
    expect(page()).toContain("check: uv run pytest → uv run --group test pytest");
  });

  it("says in a setup's activity when it ran with no more than the agents, and why", () => {
    const event: AuditEvent = { seq: 1, attemptId: "setup-2", at: "2026-01-01T00:00:00Z", kind: "setupLimited", data: {} };
    act(() => root.render(<ActivityTab events={[event]} role="worker" />));
    expect(page()).toContain("Runs with no more permissions than the agents, because an agent that can change the code ran before it: no internet, and not the project's shared cache. An install that needs to download something may fail for it. Only a setup before any such agent, as at the start of a new run, gets the internet.");
  });
});

describe("a review that approved what breaks a criterion", () => {
  let root: Root;
  const page = () => document.body.textContent ?? "";
  const events: AuditEvent[] = [{ seq: 1, attemptId: "code", at: "2026-01-01T00:00:00Z", kind: "reviewBreaks", data: { breaks: ["C5", "L2"] } }];
  const inActivity = () => act(() => root.render(<ActivityTab events={events} role="reviewer" />));

  beforeEach(async () => {
    await i18next.changeLanguage("en");
    mockIPC(() => null);
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("says why Keel doesn't count it as an approval", () => {
    inActivity();
    expect(page()).toContain("It says the change breaks C5, L2, so Keel doesn't count it as an approval.");
  });

  it("says it in Spanish too", async () => {
    await i18next.changeLanguage("es");
    inActivity();
    expect(page()).toContain("Dice que el cambio rompe C5, L2, así que Keel no lo cuenta como aprobación.");
  });
});

describe("a step skipped for the risks of what was asked", () => {
  let root: Root;
  const page = () => document.body.textContent ?? "";
  const skipped: AuditEvent = { seq: 1, attemptId: "state", at: "2026-01-01T00:00:00Z", kind: "skipped", data: { paths: [], reason: { code: "noMatchingRisks", risks: ["concurrency"] } } };

  beforeEach(() => {
    mockIPC(() => null);
    root = createRoot(document.body.appendChild(document.createElement("div")));
  });
  afterEach(() => {
    act(() => root.unmount());
    document.body.innerHTML = "";
  });

  it("says why in its activity, not that nothing changed", async () => {
    await i18next.changeLanguage("en");
    act(() => root.render(<ActivityTab events={[skipped]} role="reviewer" />));
    expect(page()).toContain("Skipped: what was asked names none of the risks it looks at");
    expect(page()).not.toContain("nothing it covers");
    await i18next.changeLanguage("es");
    act(() => root.render(<ActivityTab events={[{ ...skipped, seq: 2 }]} role="reviewer" />));
    expect(page()).toContain("Omitido: lo pedido no nombra ninguno de los riesgos que revisa");
  });
});
