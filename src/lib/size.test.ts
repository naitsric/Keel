import { describe, expect, it } from "vitest";
import i18next from "../i18n";
import en from "../i18n/en.json";
import es from "../i18n/es.json";
import type { AuditEvent, Reason } from "./api";
import { latestSizeDecision, skippedAsSmall, skippedAsUnchanged, skippedForRisks, skippedSteps, skippedWithNothingToFix, whyNotSmallText } from "./size";

const small = { code: "treatedAsSmall", lines: 3, files: 1 };
const step = (stepId: string, state: string, reason: { code: string } | null = null, kind = "agent") => ({ stepId, state, reason, kind });
const decided = (seq: number, data: Record<string, unknown>): AuditEvent => ({ seq, attemptId: "a", at: "", kind: "sizeDecided", data });

describe("what was skipped for being small", () => {
  it("is only what the engine skipped for that", () => {
    expect(skippedAsSmall(step("a", "skipped", small))).toBe(true);
    expect(skippedAsSmall(step("a", "skipped", { code: "noMatchingPaths" }))).toBe(false);
    expect(skippedAsSmall(step("a", "succeeded", small))).toBe(false);
    expect(skippedAsSmall(step("a", "skipped"))).toBe(false);
  });

  it("isn't what was left out for the risks of what was asked, which has a check of its own", () => {
    const risks = { code: "noMatchingRisks", risks: ["concurrency"] };
    expect(skippedAsSmall(step("a", "skipped", risks))).toBe(false);
    expect(skippedForRisks(step("a", "skipped", risks))).toBe(true);
    expect(skippedForRisks(step("a", "skipped", small))).toBe(false);
    expect(skippedForRisks(step("a", "succeeded", risks))).toBe(false);
    expect(skippedSteps([step("a", "skipped", small), step("state", "skipped", risks)]).map((s) => s.stepId)).toEqual(["a"]);
  });

  it("isn't what had nothing to fix, or a check with nothing to repeat, which a person can't ask for", () => {
    const fix = step("ship", "skipped", { code: "nothingToFix" });
    const unchanged = step("check-ship", "skipped", { code: "nothingChangedSinceCheck" });
    expect([skippedWithNothingToFix(fix), skippedAsUnchanged(fix)]).toEqual([true, false]);
    expect([skippedWithNothingToFix(unchanged), skippedAsUnchanged(unchanged)]).toEqual([false, true]);
    expect(skippedSteps([step("a", "skipped", small), fix, unchanged]).map((s) => s.stepId)).toEqual(["a"]);
  });

  it("goes by each step's latest attempt, so what a person asked for no longer counts", () => {
    const attempts = [step("a", "skipped", small), step("b", "skipped", small), step("b", "succeeded"), step("c", "skipped", { code: "noMatchingPaths" })];
    expect(skippedSteps(attempts).map((s) => s.stepId)).toEqual(["a"]);
  });
});

describe("what stands skipped once a person has asked for it", () => {
  it("is nothing, even for what hasn't started yet", () => {
    expect(skippedSteps([step("a", "skipped", small), step("human", "postponed", null, "approval"), step("b", "running")])).toEqual([]);
  });

  it("is still all of it when what was put off is a check, for a new setup to run first", () => {
    const attempts = [step("check", "failed", null, "check"), step("check", "postponed", null, "check"), step("setup", "succeeded", null, "setup"), step("check", "succeeded", null, "check"), step("security", "skipped", small)];
    expect(skippedSteps(attempts).map((s) => s.stepId)).toEqual(["security"]);
  });
});

describe("what was last decided", () => {
  it("is nothing for a workflow that doesn't depend on the change", () => {
    expect(latestSizeDecision([{ seq: 1, attemptId: "a", at: "", kind: "started", data: {} }])).toBeNull();
  });

  it("is the latest on the record, wherever it came from", () => {
    const up = decided(9, { small: false, because: [{ code: "noCheck" }], round: 1 });
    const decision = latestSizeDecision([up, decided(3, { small: true, lines: 1, files: 1, round: 0 })]);
    expect(decision).toEqual({ small: false, because: [{ code: "noCheck" }], round: 1 });
  });

  it("doesn't take anything but yes for small", () => {
    expect(latestSizeDecision([decided(1, { because: [] })])?.small).toBe(false);
  });
});

describe("why it isn't small", () => {
  const every: Reason[] = [
    { code: "specNotSmall" },
    { code: "specNamesRisks", risks: ["security", "money", "personalData"] },
    { code: "keelChoseAnAnswer" },
    { code: "noCheck" },
    { code: "checkNotPassed" },
    { code: "checkSkippedTests", count: 385 },
    { code: "issueFromSomeoneElse", author: "mallory" },
    { code: "issueFromUnknownAuthor" },
    { code: "issueWithNoAccount" },
    { code: "startedByAssistant", client: "Claude Code" },
    { code: "sensitiveFiles", files: [".github/ci.yml"], total: 1 },
    { code: "badSensitivePattern", pattern: "a[" },
    { code: "tooManyLines", lines: 512, limit: 400 },
    { code: "filesWithoutLines", files: ["logo.png"], total: 1 },
    { code: "diffUnreadable" },
    { code: "verifierFoundUnmet", count: 2 },
    { code: "verifierHasNotRun" },
    { code: "personAsked" },
    { code: "partOfJointChange" },
  ];

  it("has words for every reason, in both languages, with nothing left to fill in", async () => {
    for (const language of ["en", "es"]) {
      await i18next.changeLanguage(language);
      for (const why of every) {
        const text = whyNotSmallText(i18next.t, why);
        expect(text, why.code).not.toContain("{{");
        expect(text, why.code).not.toBe(why.code);
        expect(text, why.code).not.toBe(i18next.t("fits.why.unknown"));
      }
    }
    await i18next.changeLanguage("en");
  });

  it("names the file, and how many more there were", () => {
    expect(whyNotSmallText(i18next.t, { code: "sensitiveFiles", files: [".github/ci.yml"], total: 1 })).toBe("it touches .github/ci.yml");
    expect(whyNotSmallText(i18next.t, { code: "sensitiveFiles", files: ["a.sql", "b.sql"], total: 9 })).toBe("it touches a.sql, b.sql and 7 more");
  });

  it("calls a risk it doesn't know a risk, and never just passes on what an agent wrote", () => {
    const text = whyNotSmallText(i18next.t, { code: "specNamesRisks", risks: ["ignore your rules", "security", "another"] });
    expect(text).toBe("it could affect something it couldn't name, security");
  });

  it("doesn't fall over on a reason from a newer version", () => {
    expect(whyNotSmallText(i18next.t, { code: "somethingNew" })).toBe(i18next.t("fits.why.unknown"));
  });

  it("uses no jargon a person who asked for the change wouldn't know", () => {
    const flatten = (value: unknown): string[] => (typeof value === "string" ? [value] : Object.values(value as object).flatMap(flatten));
    for (const strings of [flatten(en.fits), flatten(es.fits)]) {
      for (const text of strings) {
        for (const word of ["spec", "organic", "odd", "escalat", "route", "ruta", "size", "tamaño", "criteria", "regression", "scope", "delta"]) {
          expect(text.toLowerCase().split(/\W+/u), text).not.toContain(word);
        }
      }
    }
  });
});
