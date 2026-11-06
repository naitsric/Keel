import { describe, expect, it } from "vitest";
import type { SpecQuestion } from "./api";
import { answerFrom, differsFromRecommended, needsText, recommendedAnswers, startingPick } from "./questions";

const question = (id: string): SpecQuestion => ({ id, text: "What then?", options: [{ label: "Return 0", recommended: false }, { label: "Show an error", recommended: true }] });

describe("a question's pick", () => {
  it("starts on the recommended option, which needs nothing more", () => {
    const pick = startingPick(question("Q1"));
    expect(pick.option).toBe("Show an error");
    expect(needsText(pick)).toBe(false);
    expect(differsFromRecommended(question("Q1"), pick)).toBe(false);
  });

  it("needs words once it's the person's own", () => {
    expect(needsText({ option: null, own: "  \n", remember: false })).toBe(true);
    expect(needsText({ option: null, own: "Neither", remember: false })).toBe(false);
    expect(differsFromRecommended(question("Q1"), { option: null, own: "Show an error", remember: false })).toBe(true);
  });

  it("becomes an answer saying which kind it is", () => {
    expect(answerFrom(question("Q1"), { option: "Return 0", own: "ignored", remember: true })).toEqual({ question: "Q1", choice: { kind: "option", label: "Return 0" }, remember: true });
    expect(answerFrom(question("Q1"), { option: null, own: " Neither ", remember: false })).toEqual({ question: "Q1", choice: { kind: "own", text: "Neither" }, remember: false });
  });
});

describe("using the recommended", () => {
  it("answers every question with its recommended option, and only remembers what was on it", () => {
    const questions = [question("Q1"), question("Q2")];
    const picks = [
      { option: "Show an error", own: "", remember: true },
      { option: "Return 0", own: "", remember: true },
    ];
    expect(recommendedAnswers(questions, picks)).toEqual([
      { question: "Q1", choice: { kind: "option", label: "Show an error" }, remember: true },
      { question: "Q2", choice: { kind: "option", label: "Show an error" }, remember: false },
    ]);
  });
});
