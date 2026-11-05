import { Check, Circle, Minus } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Language } from "../i18n";
import { api, type GithubAccount } from "../lib/api";
import { onboardingState, type Readiness } from "../lib/onboarding";
import { reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Segmented } from "../ui/primitives";
import { AddProjectButton, useAddProject } from "./AddProject";

function Item({ state, optional, title, detail, children }: { state: Readiness; optional?: boolean; title: string; detail: string; children?: ReactNode }) {
  const { t } = useTranslation();
  const ready = state === "ready";
  const waiting = state === "checking";
  return (
    <li className="flex items-start gap-3 border-t border-border py-3 first:border-t-0">
      <span className={cx("mt-0.5 shrink-0", ready ? "text-success" : waiting || optional ? "text-subtle" : "text-warning")}>
        {ready ? <Check size={15} /> : optional ? <Minus size={15} /> : <Circle size={15} />}
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium">
          {title}
          {optional && <span className="ml-2 text-[11.5px] font-normal text-subtle">{t("onboarding.neededForBuiltIn")}</span>}
        </p>
        <p className="selectable mt-0.5 text-[12px] text-muted">{detail}</p>
        {children && <div className="mt-2 flex flex-wrap gap-2">{children}</div>}
      </div>
    </li>
  );
}

/** What a new Mac needs before Keel can do anything, with the way to get each one. */
export function Onboarding() {
  const { t, i18n } = useTranslation();
  const preflight = useApp((s) => s.preflight);
  const refreshSession = useApp((s) => s.refreshSession);
  const setLanguage = useApp((s) => s.setLanguage);
  const [accounts, setAccounts] = useState<GithubAccount[] | null>(null);
  const { add, error, busy } = useAddProject();

  const ghFound = preflight?.ghFound ?? false;
  const check = () => {
    void refreshSession();
    if (ghFound) void api.githubAccounts().then(setAccounts, () => setAccounts([]));
  };

  useEffect(() => {
    if (ghFound) void api.githubAccounts().then(setAccounts, () => setAccounts([]));
  }, [ghFound]);

  // Signing in happens in Terminal, so look again when the person comes back.
  useEffect(() => {
    const onFocus = () => {
      void useApp.getState().refreshSession();
      if (useApp.getState().preflight?.ghFound) void api.githubAccounts().then(setAccounts, () => setAccounts([]));
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, []);

  const state = onboardingState(preflight, accounts);
  const auth = preflight?.auth;
  const claudeAccount = auth?.email ? `${auth.email}${auth.subscription ? ` (${auth.subscription})` : ""}` : null;

  const claudeDetail = {
    checking: t("session.checking"),
    ready: claudeAccount ? t("session.account", { account: claudeAccount }) : t("session.accountNoEmail"),
    missing: t("reasons.claudeMissing"),
    signedOut: t("reasons.claudeLoggedOut"),
    problem: state.claudeProblem ? reasonText(t, state.claudeProblem) : "",
  }[state.claude];
  const githubDetail = {
    checking: t("session.checking"),
    ready: t("onboarding.githubReady", { accounts: (accounts ?? []).map((a) => a.login).join(", ") }),
    missing: t("onboarding.githubMissing"),
    signedOut: t("onboarding.githubSignedOut"),
    problem: "",
  }[state.github];

  return (
    <Card className="mx-auto max-w-2xl p-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <h1 className="text-[18px] font-semibold">{t("onboarding.title")}</h1>
          <p className="mt-1 text-[12.5px] text-muted">{t("onboarding.intro")}</p>
        </div>
        <Segmented<Language>
          label={t("settings.language")}
          value={i18n.language === "es" ? "es" : "en"}
          onChange={(language) => void setLanguage(language)}
          options={[
            { value: "en", label: t("settings.languages.en") },
            { value: "es", label: t("settings.languages.es") },
          ]}
        />
      </div>
      <ul className="mt-3">
        <Item state={state.claude} title={t("onboarding.claude")} detail={claudeDetail}>
          {state.claude === "missing" && <Button onClick={() => void api.openHelp("claude")}>{t("onboarding.install")}</Button>}
          {state.claude === "signedOut" && <Button onClick={() => void api.openClaudeLogin()}>{t("session.signIn")}</Button>}
        </Item>
        <Item state={state.git} title={t("onboarding.git")} detail={state.git === "ready" ? t("onboarding.gitReady") : t("reasons.gitMissing")}>
          {state.git === "missing" && <Button onClick={() => void api.openHelp("git")}>{t("onboarding.install")}</Button>}
        </Item>
        <Item state={state.github} optional title={t("onboarding.github")} detail={githubDetail}>
          {state.github === "missing" && <Button onClick={() => void api.openHelp("gh")}>{t("onboarding.install")}</Button>}
          {state.github === "signedOut" && <Button onClick={() => void api.openGithubLogin()}>{t("session.signIn")}</Button>}
        </Item>
      </ul>
      <div className="mt-2 border-t border-border pt-4">
        <p className="text-[13px] font-medium">{t("onboarding.project")}</p>
        <p className="mt-0.5 text-[12px] text-muted">{t("onboarding.projectHint")}</p>
        {error && (
          <p role="alert" className="mt-2 text-[12px] text-danger">
            {error}
          </p>
        )}
        <div className="mt-3 flex flex-wrap gap-2">
          <AddProjectButton add={add} busy={busy} variant="primary" disabled={!state.canAddProject} />
          <Button variant="ghost" onClick={check}>
            {t("onboarding.checkAgain")}
          </Button>
        </div>
      </div>
    </Card>
  );
}
