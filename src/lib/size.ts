import type { TFunction } from "i18next";
import { type AuditEvent, type Reason } from "./api";
import { latestOfEachStep } from "./audit";

/** Whether the engine found a change small, on the record. It writes this, and only ever from facts. */
export interface SizeDecision {
  small: boolean;
  /** Why it isn't, as codes with their data. `whyNotSmallText` writes them. */
  because: Reason[];
  round: number;
}

/** The engine's closed list. Anything else an agent wrote still counts as a risk, and is called one. */
const KNOWN_RISKS = ["security", "money", "personalData", "irreversibleData", "sharedContracts", "concurrency", "environment", "noRegressionTest"];

export const skippedAsSmall = (attempt: { state: string; reason?: { code: string } | null }) => attempt.state === "skipped" && attempt.reason?.code === "treatedAsSmall";

/** The steps that stand skipped for being a small change: what a person can ask for. Once they've asked, all of it is on its way. */
export const skippedSteps = <T extends { stepId: string; state: string; reason?: { code: string } | null }>(attempts: T[]): T[] => (attempts.some((attempt) => attempt.state === "postponed") ? [] : latestOfEachStep(attempts).filter(skippedAsSmall));

/** What was last decided, if anything: a run whose workflow doesn't depend on this has nothing here. */
export function latestSizeDecision(events: AuditEvent[]): SizeDecision | null {
  const decided = events.filter((event) => event.kind === "sizeDecided");
  if (decided.length === 0) return null;
  const { data } = decided.reduce((a, b) => (b.seq > a.seq ? b : a));
  return { small: data.small === true, because: data.because ?? [], round: data.round ?? 0 };
}

const fileList = (t: TFunction, key: string, files: string[], total: number) => {
  const shown = files.join(", ");
  return t(`fits.why.${key}`, { files: total > files.length ? t("fits.andMore", { files: shown, more: total - files.length }) : shown });
};

/** One reason, in the words of someone who asked for the change. */
export function whyNotSmallText(t: TFunction, why: Reason): string {
  const data = why as Record<string, any>;
  switch (why.code) {
    case "specNamesRisks": {
      const named = (data.risks ?? []).map((risk: string) => t(KNOWN_RISKS.includes(risk) ? `fits.risks.${risk}` : "fits.risks.other"));
      return t("fits.why.specNamesRisks", { risks: [...new Set(named)].join(", ") });
    }
    case "sensitiveFiles":
      return fileList(t, "sensitiveFiles", data.files ?? [], data.total ?? 0);
    case "filesWithoutLines":
      return fileList(t, "filesWithoutLines", data.files ?? [], data.total ?? 0);
    default:
      return t(`fits.why.${why.code}`, { ...data, defaultValue: t("fits.why.unknown") });
  }
}
