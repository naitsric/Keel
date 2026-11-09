import { describe, expect, it } from "vitest";
import type { WorkflowResults } from "./api";
import i18next from "../i18n";
import { averageRounds, comparedResults, costPerMerged, ENOUGH_FOR_AN_AVERAGE, matchedShare, movedUpReasons } from "./results";

const results = (fields: Partial<WorkflowResults>): WorkflowResults => ({ workflowId: "w", workflowName: "W", runs: 0, finished: 0, merged: 0, closed: 0, costPerMergedUsd: null, averageRounds: null, stoppedToAsk: 0, answers: 0, answersRecommended: 0, sentBack: 0, criteriaMet: 0, criteriaUnmet: 0, criteriaUnverified: 0, shortRoute: 0, movedUp: 0, movedUpBecause: [], ...fields });

describe("averages", () => {
  it("wait for enough merged pull requests or finished runs", () => {
    const few = results({ merged: ENOUGH_FOR_AN_AVERAGE - 1, finished: ENOUGH_FOR_AN_AVERAGE - 1, costPerMergedUsd: 2, averageRounds: 1 });
    expect(costPerMerged(few)).toBeNull();
    expect(averageRounds(few)).toBeNull();
    const enough = results({ merged: ENOUGH_FOR_AN_AVERAGE, finished: ENOUGH_FOR_AN_AVERAGE, costPerMergedUsd: 2, averageRounds: 0 });
    expect(costPerMerged(enough)).toBe(2);
    expect(averageRounds(enough)).toBe(0);
  });

  it("go by what each one counts: runs can finish without anything merged", () => {
    const unmerged = results({ merged: 0, finished: 10, averageRounds: 1 });
    expect(costPerMerged(unmerged)).toBeNull();
    expect(averageRounds(unmerged)).toBe(1);
  });
});

describe("how often answers took the recommended option", () => {
  it("says nothing until there are enough answers to go on", () => {
    expect(matchedShare(results({ answers: ENOUGH_FOR_AN_AVERAGE - 1, answersRecommended: 1 }))).toBeNull();
    expect(matchedShare(results({ answers: 4, answersRecommended: 3 }))).toBe(0.75);
  });
});

describe("why runs went to full review", () => {
  it("reads in plain words, most common first as the engine sends them, in either language", async () => {
    const r = results({ movedUp: 3, movedUpBecause: [{ code: "sensitiveFiles", runs: 2 }, { code: "personAsked", runs: 1 }, { code: "somethingNew", runs: 1 }] });
    await i18next.changeLanguage("en");
    expect(movedUpReasons(r, i18next.t)).toBe("sensitive files: 2, you asked: 1, another reason: 1");
    await i18next.changeLanguage("es");
    expect(movedUpReasons(r, i18next.t)).toBe("archivos sensibles: 2, lo pediste: 1, otra razón: 1");
    await i18next.changeLanguage("en");
  });
});

describe("the workflows compared", () => {
  it("follow the list beside them and use the names they have now", () => {
    const compared = comparedResults([results({ workflowId: "b", workflowName: "Old b" }), results({ workflowId: "gone", workflowName: "Deleted" }), results({ workflowId: "a", workflowName: "Old a" })], [{ id: "a", name: "A" }, { id: "b", name: "B" }]);
    expect(compared.map((r) => r.workflowName)).toEqual(["A", "B", "Deleted"]);
  });
});
