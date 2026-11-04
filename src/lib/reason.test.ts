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
