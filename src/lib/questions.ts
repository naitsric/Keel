import type { QuestionAnswer, SpecQuestion } from "./api";

/** What's chosen for one question on the card, before it's sent. */
export interface QuestionPick {
  /** The option's label. Null means "Other", in the person's own words. */
  option: string | null;
  own: string;
  remember: boolean;
}

export const recommendedOption = (question: SpecQuestion): string | null => question.options.find((o) => o.recommended)?.label ?? null;

/** Where a question starts: the recommended option, already selected. */
export const startingPick = (question: SpecQuestion): QuestionPick => ({ option: recommendedOption(question), own: "", remember: false });

/** "Other" with nothing written isn't an answer yet. */
export const needsText = (pick: QuestionPick) => pick.option === null && pick.own.trim() === "";

export const differsFromRecommended = (question: SpecQuestion, pick: QuestionPick) => pick.option !== recommendedOption(question);

export function answerFrom(question: SpecQuestion, pick: QuestionPick): QuestionAnswer {
  return { question: question.id, choice: pick.option === null ? { kind: "own", text: pick.own.trim() } : { kind: "option", label: pick.option }, remember: pick.remember };
}

/**
 * Everything goes with the recommended option. "Remember" stays where the person was already on
 * it: ticked over an answer they've now dropped, it wasn't about this one.
 */
export function recommendedAnswers(questions: SpecQuestion[], picks: QuestionPick[]): QuestionAnswer[] {
  return questions.map((question, index) => ({
    question: question.id,
    choice: { kind: "option", label: recommendedOption(question) ?? "" },
    remember: picks[index].remember && !differsFromRecommended(question, picks[index]),
  }));
}
