import type { TFunction } from "i18next";
import type { CommandError, Reason } from "./api";
import { formatDateTime } from "./format";

/** The sentence for a reason. Rust sends a code and data; only the UI has words. */
export function reasonText(t: TFunction, reason: Reason): string {
  const data = reason as Record<string, any>;
  switch (reason.code) {
    case "rateLimited":
      return data.resetsAt
        ? t("reasons.rateLimitedUntil", { date: formatDateTime(new Date(data.resetsAt * 1000).toISOString()) })
        : t("reasons.rateLimited");
    case "stalled":
      return data.command
        ? t("reasons.stalled", { minutes: data.minutes, command: data.command, cpu: Math.round(data.cpuPercent ?? 0) })
        : t("reasons.stalledModel", { minutes: data.minutes });
    case "unexpectedTools":
      return t("reasons.unexpectedTools", { expected: (data.expected ?? []).join(", "), found: (data.found ?? []).join(", ") });
    case "deliveryFailed": {
      const known = ["notSignedIn", "noPermission", "sso", "repoRules", "signedCommits", "workflowScope"].includes(data.kind);
      return known ? t(`deliveryKinds.${data.kind}`) : t("reasons.deliveryFailed", { detail: data.detail ?? "" });
    }
    case "invalidSetting":
      return t(`invalidSettings.${data.field}`, { value: data.value, defaultValue: t("reasons.invalidSetting", { value: data.value }) });
    case "invalidWorkflow":
      return t(`invalidWorkflow.${data.problem}`, { step: data.step, defaultValue: data.problem });
    case "specNotUsable":
      return t("reasons.specNotUsable", { problems: (data.problems ?? []).map((problem: Reason) => t(`specProblems.${problem.code}`, { ...problem, defaultValue: problem.code })).join("; ") });
    case "invalidAnswer":
      return t(`invalidAnswers.${data.problem}`, { question: data.question, defaultValue: t("reasons.invalidAnswer") });
    case "noMatchingPaths":
      return t("reasons.noMatchingPaths", { paths: (data.paths ?? []).join(", ") });
    case "setupFailed":
      return t("reasons.setupFailed", { exitCode: data.exitCode ?? "?" });
    case "checkFailed":
      return t("reasons.checkFailed", { exitCode: data.exitCode ?? "?" });
    case "exitedWithoutResult":
      return t("reasons.exitedWithoutResult", { exitCode: data.exitCode ?? "?" });
    case "projectHasOpenRuns":
      return data.count > 1 ? t("reasons.projectHasOpenRunsMany", { title: data.title, others: data.count - 1 }) : t("reasons.projectHasOpenRuns", { title: data.title });
    default:
      return t(`reasons.${reason.code}`, { ...data, defaultValue: reason.code });
  }
}

export function errorText(t: TFunction, error: unknown): string {
  const e = error as Partial<CommandError> | string;
  if (typeof e === "string") return t("errors.internal", { detail: e });
  if (e?.reason) return reasonText(t, e.reason);
  if (e?.code === "notARepo" || e?.code === "invalidState" || e?.code === "mcpNotRunning" || e?.code === "mcpConnectFailed") return t(`errors.${e.code}`, { detail: e.detail ?? "" });
  return t("errors.internal", { detail: e?.detail ?? String(error) });
}

const FIXED_IN_SETTINGS = ["githubAccountMissing", "noGithubRemote", "defaultWorkflowMissing"];
const DELIVERY_FIXED_IN_SETTINGS = ["notSignedIn", "noPermission"];

/** A reason the user fixes in the project's settings. */
export const needsProjectSettings = (reason: Reason | null | undefined) =>
  reason != null && (FIXED_IN_SETTINGS.includes(reason.code) || (reason.code === "deliveryFailed" && DELIVERY_FIXED_IN_SETTINGS.includes(reason.kind as string)));

/** A reason the user fixes by signing in. */
export const needsSignIn = (reason: Reason | null | undefined) =>
  reason?.code === "claudeLoggedOut" || reason?.code === "sessionRejected";
