import type { QuestionAnswer, SpecQuestion } from "./api";

/** What's chosen for one question on the card, before it's sent. */
export interface QuestionPick {
  /**
   * The option's label. Null means "Other", in the person's own words. Left out while nothing is
   * chosen, which is where a question with no recommended option starts.
   */
  option?: string | null;
  own: string;
  remember: boolean;
}

export const recommendedOption = (question: SpecQuestion): string | undefined => question.options.find((o) => o.recommended)?.label;

/** Whether Keel could answer them all with what it recommends. One with none is the person's to answer. */
export const allRecommend = (questions: SpecQuestion[]) => questions.every((question) => recommendedOption(question) !== undefined);

/** Where a question starts: the recommended option, already selected, or nothing when there's none. */
export const startingPick = (question: SpecQuestion): QuestionPick => ({ option: recommendedOption(question), own: "", remember: false });

/** "Other" with nothing written isn't an answer yet. */
export const needsText = (pick: QuestionPick) => pick.option === null && pick.own.trim() === "";

/** Whether this pick can be sent as an answer. */
export const isAnswered = (pick: QuestionPick) => pick.option !== undefined && !needsText(pick);

export const differsFromRecommended = (question: SpecQuestion, pick: QuestionPick) => pick.option !== recommendedOption(question);

export function answerFrom(question: SpecQuestion, pick: QuestionPick): QuestionAnswer {
  return { question: question.id, choice: pick.option === null || pick.option === undefined ? { kind: "own", text: pick.own.trim() } : { kind: "option", label: pick.option }, remember: pick.remember };
}

/**
 * Everything goes with the recommended option. "Remember" stays where the person was already on
 * it: ticked over an answer they've now dropped, it wasn't about this one. Only for questions that
 * all recommend one: see `allRecommend`.
 */
export function recommendedAnswers(questions: SpecQuestion[], picks: QuestionPick[]): QuestionAnswer[] {
  return questions.map((question, index) => ({
    question: question.id,
    choice: { kind: "option", label: recommendedOption(question) ?? "" },
    remember: picks[index].remember && !differsFromRecommended(question, picks[index]),
  }));
}
