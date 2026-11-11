import { describe, expect, it } from "vitest";
import i18next from "../i18n";
import { errorText, needsSignIn, reasonText } from "./reason";

const t = (language: string) => i18next.getFixedT(language);

describe("reasonText", () => {
  it("names the stuck command, in both languages", () => {
    const reason = { code: "stalled", command: "uv run pytest", minutes: 11, cpuPercent: 0.2 };
    expect(reasonText(t("es"), reason)).toBe("Sin avances desde hace 11 min. Está corriendo «uv run pytest» con 0% de CPU.");
    expect(reasonText(t("en"), reason)).toContain("uv run pytest");
  });

  it("says when no command is running", () => {
    expect(reasonText(t("en"), { code: "stalled", command: null, minutes: 4 })).toContain("no command is running");
  });

  it("gives the reset time of a usage limit when known", () => {
    expect(reasonText(t("en"), { code: "rateLimited", resetsAt: 1800000000 })).toContain("resets");
    expect(reasonText(t("en"), { code: "rateLimited", resetsAt: null })).not.toContain("resets");
  });

  it("does not escape commands or paths", () => {
    const text = reasonText(t("en"), { code: "stalled", command: "echo 'a' && b", minutes: 1, cpuPercent: 0 });
    expect(text).toContain("echo 'a' && b");
  });
});

describe("a step waiting for its project's turn", () => {
  it("names the step and the run that have the turn, and that it's about running commands", () => {
    const reason = { code: "waitingForProject", step: "builder", runId: "r44", title: "The runs' folders" };
    expect(reasonText(t("es"), reason)).toBe("Espera a que termine el paso «builder» de «The runs' folders». Cada corrida trabaja en su propia copia, pero un proyecto corre un solo paso con comandos a la vez.");
    expect(reasonText(t("en"), reason)).toBe("Waiting for the “builder” step of “The runs' folders” to finish. Each run works in its own copy, but a project runs one step with commands at a time.");
  });

  it("still says why when it doesn't know who has the turn", () => {
    expect(reasonText(t("es"), { code: "waitingForProject" })).toBe("Espera su turno. Cada corrida trabaja en su propia copia, pero un proyecto corre un solo paso con comandos a la vez.");
    expect(reasonText(t("en"), { code: "waitingForProject" })).toBe("Waiting for its turn. Each run works in its own copy, but a project runs one step with commands at a time.");
    // What Rust reads back from a reason saved before it said who has the turn.
    expect(reasonText(t("en"), { code: "waitingForProject", step: "", runId: "", title: "" })).toBe("Waiting for its turn. Each run works in its own copy, but a project runs one step with commands at a time.");
  });

  it("still names the step when it can't name the run", () => {
    const reason = { code: "waitingForProject", step: "builder", runId: "r44", title: "" };
    expect(reasonText(t("es"), reason)).toBe("Espera a que termine el paso «builder» de otra corrida. Cada corrida trabaja en su propia copia, pero un proyecto corre un solo paso con comandos a la vez.");
    expect(reasonText(t("en"), reason)).toBe("Waiting for the “builder” step of another run to finish. Each run works in its own copy, but a project runs one step with commands at a time.");
  });
});

describe("what's said about a verifier", () => {
  const JARGON = /\bspec\b|\[TBC\]|acceptance|\bscope\b|edge case|assumption|\bdelta\b|criteria/i;

  it("names the step that needs a specifier, in both languages", () => {
    const reason = { code: "invalidWorkflow", problem: "verifierNeedsSpecifier", step: "verify" };
    expect(reasonText(t("en"), reason)).toBe("“verify” goes through what has to be true, so a step that works out the change has to come before it.");
    expect(reasonText(t("es"), reason)).toBe("«verify» repasa lo que tiene que cumplirse, así que antes tiene que haber un paso que defina el cambio.");
  });

  it("is in plain words wherever a person reads it", () => {
    for (const language of ["en", "es"]) {
      const say = t(language);
      const texts = [
        say("run.verifyMet", { count: 3, total: 4 }),
        say("run.verifyUnmet", { count: 1 }),
        say("run.verifyUnverified", { count: 1 }),
        say("run.verifyNone", { count: 4 }),
        ...["met", "unmet", "unverified"].map((result) => say(`run.verifyResult.${result}`)),
        ...["noEvidence", "noCheckPassed", "leftOut", "noProof"].map((why) => say(`run.verifyWhy.${why}`)),
        say("activity.verifyAnswer"),
        say("activity.verificationCorrected", { count: 2 }),
        say("activity.verificationDropped", { count: 2 }),
        say("invalidWorkflow.verifierNeedsSpecifier", { step: "x" }),
        say("invalidWorkflow.whenBeforeBuild", { step: "x" }),
        say("invalidSettings.sensitivePaths", { value: "x" }),
        say("reasons.treatedAsSmall"),
        say("reasons.nothingSkippedToRun"),
        say("reasons.keptAsking", { rounds: 3 }),
        say("run.questionNoneRecommended"),
        say("run.questionsUseRecommendedNone"),
        say("run.questionsExtraRound"),
        say("run.questionsAnswerEach"),
        say("workflows.results.criteriaMet"),
        say("workflows.results.criteriaUnmet"),
        say("workflows.results.criteriaUnverified"),
        say("workflows.results.criteriaUnverifiedNote"),
        say("workflows.results.nothingChecked"),
        say("workflows.roleVerifier"),
      ];
      for (const text of texts) expect(text).not.toMatch(JARGON);
    }
  });
});

describe("what a specifier got wrong", () => {
  const JARGON = /\bspec\b|\[TBC\]|acceptance criteria|\bscope\b|edge case|assumption|\bdelta\b|criteria/i;

  it("says what's wrong in plain words, in both languages", () => {
    const reason = { code: "specNotUsable", problems: [{ code: "summaryTooLong", chars: 240, limit: 200 }, { code: "tooManyRecommended", id: "Q2", count: 2 }, { code: "leftUnresolved" }] };
    const english = reasonText(i18next.getFixedT("en"), reason);
    expect(english).toContain("has 240 characters, and the limit is 200");
    expect(english).toContain("question Q2 has 2 recommended options");
    expect(english).not.toMatch(JARGON);
    const spanish = reasonText(i18next.getFixedT("es"), reason);
    expect(spanish).toContain("tiene 240 caracteres y el límite es 200");
    expect(spanish).not.toMatch(JARGON);
  });

  it("has a sentence for everything that can go wrong, with no jargon in it", () => {
    for (const language of ["en", "es"]) {
      const problems = i18next.getResource(language, "translation", "specProblems") as Record<string, string>;
      expect(Object.keys(problems)).toEqual(["criterionNeedsText", "questionNeedsText", "tooManyChanges", "tooManyLimits", "unreadable", "noSummary", "summaryTooLong", "noCriteria", "tooManyCriteria", "criterionTooLong", "criterionNeedsId", "repeatedCriterion", "tooManyDecided", "decidedTooLong", "tooManyQuestions", "questionTooLong", "questionNeedsId", "repeatedQuestion", "wrongNumberOfOptions", "optionTooLong", "repeatedOption", "tooManyRecommended", "leftUnresolved", "noParts", "tooManyParts", "unknownProject", "repeatedPart", "tooManyTogether", "tooManyFromRequest"]);
      for (const text of Object.values(problems)) expect(text).not.toMatch(JARGON);
    }
  });

  it("says when an answer doesn't belong to the questions", () => {
    const t = i18next.getFixedT("en");
    expect(errorText(t, { code: "blocked", reason: { code: "invalidAnswer", problem: "unanswered", question: "Q2" }, detail: null })).toBe("Question Q2 still needs an answer.");
    expect(reasonText(t, { code: "invalidAnswer", problem: "somethingNew", question: "Q2" })).toBe("That answer doesn't belong to these questions.");
  });
});

describe("errorText", () => {
  it("turns a backend error into a sentence", () => {
    expect(errorText(t("es"), { code: "blocked", reason: { code: "claudeLoggedOut" }, detail: null })).toContain("No has iniciado sesión");
    expect(errorText(t("en"), { code: "notARepo", reason: null, detail: "/tmp/x" })).toContain("/tmp/x");
    expect(errorText(t("en"), { code: "internal", reason: null, detail: "boom" })).toContain("boom");
  });
});

describe("needsSignIn", () => {
  it("is true only when signing in fixes it", () => {
    expect(needsSignIn({ code: "claudeLoggedOut" })).toBe(true);
    expect(needsSignIn({ code: "sessionRejected" })).toBe(true);
    expect(needsSignIn({ code: "rateLimited" })).toBe(false);
    expect(needsSignIn(null)).toBe(false);
  });
});

describe("settings errors", () => {
  it("names what's wrong with a setting", () => {
    expect(reasonText(t("es"), { code: "invalidSetting", field: "envReserved", value: "GH_TOKEN" })).toContain("GH_TOKEN");
    expect(reasonText(t("en"), { code: "invalidSetting", field: "serviceDuplicate", value: "db" })).toContain("two services");
    expect(reasonText(t("en"), { code: "serviceFailed", name: "db", detail: "nothing is listening on port 5432" })).toContain("nothing is listening");
  });
});
