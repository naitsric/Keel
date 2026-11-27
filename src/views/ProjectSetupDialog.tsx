import { AlertTriangle, Check, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type Attempt, type AuditEvent, type GithubStatus, type MapStart, type Project, type ProjectSetup } from "../lib/api";
import { githubAccess, repoAccessUrl, repoInvitationUrl } from "../lib/github";
import { accountToSelect, checkOptionsLeft, commandNotFound, type Configured, configuredChanges, foldersToOffer, needsNode, setupOptionsLeft, shortPath, trialOutcome, withCheckOption, withFolders, withSetup, withSetupOption } from "../lib/projects";
import { errorText, needsSignIn, reasonText } from "../lib/reason";
import { type ProjectSetupRequest, useApp } from "../lib/store";
import { Button, cx, Field, inputClass, Modal } from "../ui/primitives";
import { ProjectChangedNotice } from "./ProjectChangedNotice";
import { FolderChoices, useToolFolders } from "./ToolFolders";

const NONE = "__none__";
const POLL_MS = 1000;

/** What each signed-in account can do in this repo, and what `gh` said. */
interface GithubView {
  base: GithubStatus;
  byAccount: Record<string, GithubStatus>;
}

function AccessLabel({ status }: { status: GithubStatus | undefined }) {
  const { t } = useTranslation();
  const access = githubAccess(status ?? null);
  if (access === "canPush") {
    return (
      <span className="flex items-center gap-1 text-[12px] whitespace-nowrap text-success">
        <Check size={13} />
        {t("setup.canPush")}
      </span>
    );
  }
  if (access === "unknown") return null;
  return <span className="text-[12px] whitespace-nowrap text-warning">{t(access === "readOnly" ? "setup.readOnly" : "setup.noAccess")}</span>;
}

/** Trying a command is a real run. This follows it and shows how it ended, and its output. */
function TrialResult({ runId, onRunning, onOpenRun }: { runId: string; onRunning: (running: boolean) => void; onOpenRun: () => void }) {
  const { t } = useTranslation();
  const revision = useApp((s) => s.revision);
  const [attempts, setAttempts] = useState<Attempt[]>([]);
  const [output, setOutput] = useState<{ stdout?: string; stderr?: string; truncated?: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const trial = trialOutcome(attempts);
  const running = trial.state === "running" && error === null;

  useEffect(() => onRunning(running), [running, onRunning]);

  useEffect(() => {
    let current = true;
    const look = () =>
      api
        .runAttempts(runId)
        .then((list) => current && setAttempts(list))
        .catch((e) => current && setError(errorText(t, e)));
    void look();
    const timer = running ? setInterval(() => void look(), POLL_MS) : undefined;
    return () => {
      current = false;
      clearInterval(timer);
    };
  }, [runId, revision, running, t]);

  const showOutput = async () => {
    if (output) {
      setOutput(null);
      return;
    }
    if (!trial.attempt) return;
    try {
      // Masked by the command, like every other place that shows a run.
      const events: AuditEvent[] = await api.attemptEvents(trial.attempt.id, 0);
      setOutput(events.find((event) => event.kind === "checkResult")?.data ?? {});
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  if (error) {
    return (
      <p role="alert" className="mt-1.5 text-[12px] text-danger">
        {error}
      </p>
    );
  }
  if (trial.state === "running") {
    const waiting = trial.attempt?.reason;
    return <p className="mt-1.5 text-[12px] text-muted">{waiting ? reasonText(t, waiting) : t(trial.attempt?.kind === "check" ? "setup.runningCheck" : "setup.runningSetup")}</p>;
  }
  const failed = trial.state === "failed";
  return (
    <div className="mt-1.5">
      <p className={cx("flex items-start gap-1.5 text-[12px]", failed ? "text-danger" : "text-success")}>
        {failed ? <X size={13} className="mt-0.5 shrink-0" /> : <Check size={13} className="mt-0.5 shrink-0" />}
        <span className="selectable min-w-0">{failed ? (trial.attempt.reason ? reasonText(t, trial.attempt.reason) : t(`states.${trial.attempt.state}`)) : t("setup.passed")}</span>
      </p>
      {failed && commandNotFound(trial.attempt) && <p className="mt-1 text-[12px] text-muted">{t("setup.notFound")}</p>}
      <p className="mt-1 flex gap-3 text-[12px]">
        <button type="button" className="text-accent hover:underline" onClick={() => void showOutput()}>
          {t(output ? "setup.hideOutput" : "setup.seeOutput")}
        </button>
        <button type="button" className="text-accent hover:underline" onClick={onOpenRun}>
          {t("setup.openRun")}
        </button>
      </p>
      {output && <pre className="selectable mt-1.5 max-h-40 overflow-auto rounded border border-border bg-inset p-2 font-mono text-[11.5px] whitespace-pre-wrap">{[output.stdout, output.stderr].filter(Boolean).join("\n").trim() || t("setup.noOutput")}</pre>}
    </div>
  );
}

/** What the repo's own files say could go in a command. Nothing changes until one is chosen. */
function OptionButtons({ options, onAdd }: { options: string[]; onAdd: (option: string) => void }) {
  const { t } = useTranslation();
  if (options.length === 0) return null;
  return (
    <div className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[12px] text-muted">
      <span>{t("setup.canAdd")}</span>
      {options.map((option) => (
        <button
          key={option}
          type="button"
          aria-label={t("setup.addOption", { option })}
          className="rounded border border-border px-1.5 py-0.5 font-mono text-[11.5px] text-accent hover:bg-inset"
          onClick={() => onAdd(option)}
        >
          {option}
        </button>
      ))}
    </div>
  );
}

/** Localhost, ticked when the check can't run with it closed, unless the person unticks it. */
function LocalhostChoice({ chosen, onToggle }: { chosen: boolean; onToggle: (chosen: boolean) => void }) {
  const { t } = useTranslation();
  return (
    <label className="flex items-start gap-2 text-[12px]">
      <input type="checkbox" className="mt-0.5" checked={chosen} onChange={(e) => onToggle(e.target.checked)} />
      <span className="min-w-0">
        <span>{t("setup.localhost")}</span>
        <span className="mt-0.5 block text-subtle">{t("setup.localhostWhy")}</span>
      </span>
    </label>
  );
}

function MapLine({ map, onOpenBrain }: { map: MapStart; onOpenBrain: () => void }) {
  const { t } = useTranslation();
  if (map.status === "started") return <p className="text-[12px] text-muted">{t("setup.mapStarted")}</p>;
  return (
    <p className="text-[12px] text-warning">
      {needsSignIn(map.reason) ? t("setup.mapSignedOut") : t("setup.mapFailed", { reason: reasonText(t, map.reason) })}{" "}
      <button type="button" className="text-accent hover:underline" onClick={onOpenBrain}>
        {t("setup.openBrain")}
      </button>
    </p>
  );
}

function SetupForm({ request }: { request: ProjectSetupRequest }) {
  const { t, i18n } = useTranslation();
  const { projectId } = request;
  const refreshOverview = useApp((s) => s.refreshOverview);
  const closeSetup = useApp((s) => s.closeProjectSetup);
  const navigate = useApp((s) => s.navigate);

  const [info, setInfo] = useState<ProjectSetup | null>(null);
  const [check, setCheck] = useState("");
  const [setup, setSetup] = useState("");
  const [github, setGithub] = useState<GithubView | null>(null);
  const [checking, setChecking] = useState(true);
  const [account, setAccount] = useState(NONE);
  const [runId, setRunId] = useState<string | null>(null);
  const [trying, setTrying] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const latestRequest = useRef(0);
  const chose = useRef(false);
  const typed = useRef({ setup: false, check: false });
  // The saved account and commands that changed while this was open.
  const [changed, setChanged] = useState<(keyof Configured)[]>([]);

  // Reads the folder first: `origin` may have changed, and GitHub is asked about what it says now.
  useEffect(() => {
    let current = true;
    api
      .projectSetup(projectId)
      .then(async (next) => {
        // Before showing it, so that nothing older than this is ever taken for a change. A refresh
        // that fails doesn't keep the dialog from opening: saving asks again.
        await refreshOverview().catch(() => undefined);
        if (!current) return;
        setInfo(next);
        setCheck(next.project.checkCommand ?? next.guess.check ?? "");
        setSetup(next.project.setupCommand ?? next.guess.setup ?? "");
      })
      .catch((e) => current && setError(errorText(t, e)));
    return () => {
      current = false;
    };
  }, [projectId, refreshOverview, t]);

  /**
   * Takes in the account and commands saved since they were last shown here, by an MCP client for
   * one. What the person typed or chose stays. Says whether there was anything.
   */
  const catchUp = (now: Project) => {
    if (!info) return false;
    const fields = configuredChanges(info.project, now);
    if (fields.length === 0) return false;
    const { githubAccount, setupCommand, checkCommand } = now;
    setInfo({ ...info, project: { ...info.project, githubAccount, setupCommand, checkCommand } });
    if (fields.includes("githubAccount") && !chose.current) setAccount(githubAccount ?? NONE);
    const newSetup = fields.includes("setupCommand") && !typed.current.setup;
    const newCheck = fields.includes("checkCommand") && !typed.current.check;
    if (newSetup) setSetup(setupCommand ?? "");
    if (newCheck) setCheck(checkCommand ?? "");
    if (newSetup || newCheck) forgetTrial();
    setChanged((before) => [...new Set([...before, ...fields])]);
    return true;
  };

  const listed = useApp((s) => s.overview.projects.find((p) => p.id === projectId));
  useEffect(() => {
    if (listed) catchUp(listed);
    // Only a change to what's saved, or the first sight of it, is news.
  }, [info === null, listed?.githubAccount, listed?.setupCommand, listed?.checkCommand]);

  const saved = info?.project.githubAccount ?? null;
  const known = info !== null;

  // What the commands need from the home folder, ticked unless the person unticks it. Saving lets
  // them read what's ticked, and trying it does for that run only.
  const found = useToolFolders(projectId, setup, check, known);
  const [skipped, setSkipped] = useState<string[]>([]);
  const offered = foldersToOffer(found ?? [], info?.project.allowedRead ?? []);
  const chosen = offered.map((f) => f.folder).filter((folder) => !skipped.includes(folder));
  // Localhost the same way, when the check can't run without it and the project keeps it closed.
  const [localhostSkipped, setLocalhostSkipped] = useState(false);
  const offerLocalhost = info !== null && info.guess.localhost && !info.project.allowLocalhost;
  const openLocalhost = offerLocalhost && !localhostSkipped;

  const loadGithub = useCallback(async () => {
    const latest = ++latestRequest.current;
    setChecking(true);
    try {
      const base = await api.githubStatus(projectId, null);
      const logins = base.repo ? base.accounts.map((a) => a.login) : [];
      const statuses = await Promise.all(logins.map((login) => api.githubStatus(projectId, login)));
      if (latest !== latestRequest.current) return;
      const byAccount = Object.fromEntries(logins.map((login, index) => [login, statuses[index]]));
      setGithub({ base, byAccount });
      if (!chose.current) {
        const canPush = logins.filter((login) => githubAccess(byAccount[login]) === "canPush");
        setAccount(accountToSelect(saved, canPush) ?? NONE);
      }
    } catch (e) {
      if (latest === latestRequest.current) setError(errorText(t, e));
    } finally {
      if (latest === latestRequest.current) setChecking(false);
    }
  }, [projectId, saved, t]);

  useEffect(() => {
    if (known) void loadGithub();
  }, [known, loadGithub]);

  // Access is given in the browser and accounts connect in Terminal, so look again on the way back.
  useEffect(() => {
    if (!known) return;
    const onFocus = () => void loadGithub();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [known, loadGithub]);

  const repo = github?.base.repo ?? null;
  const accounts = github?.base.accounts ?? [];
  const logins = repo ? accounts.map((a) => a.login) : [];
  const canPush = logins.filter((login) => githubAccess(github?.byAccount[login] ?? null) === "canPush");
  const selected = account === NONE ? null : account;
  const status = selected ? github?.byAccount[selected] : undefined;
  const access = githubAccess(status ?? null);
  const needsAccess = selected !== null && (access === "none" || access === "readOnly");
  const showAccounts = repo !== null && accounts.length > 0;
  const mustChoose = showAccounts && selected === null && canPush.length > 0;

  const guess = info?.guess;
  const fromGuess = (guess?.check != null && check === guess.check && !info?.project.checkCommand) || (guess?.setup != null && setup === guess.setup && !info?.project.setupCommand);
  const nothingToGo = known && !guess?.check && !guess?.setup && !info?.project.checkCommand && !info?.project.setupCommand;
  const fromCiChecks = guess && guess.ciChecks.length > 0 && check === guess.check ? guess.ciChecks : [];

  const openOnGithub = (url: string) => {
    setError(null);
    api.openGithubUrl(url).catch((e) => setError(errorText(t, e)));
  };
  const connectAccount = () => {
    setError(null);
    api.openGithubLogin().catch((e) => setError(errorText(t, e)));
  };

  // What's on screen is what gets tried, so a result only speaks for the commands it ran.
  const forgetTrial = () => {
    setRunId(null);
    setTrying(false);
  };
  const changeCheck = (value: string) => {
    typed.current.check = true;
    setCheck(value);
    forgetTrial();
  };
  const changeSetup = (value: string) => {
    typed.current.setup = true;
    setSetup(value);
    forgetTrial();
  };
  const toggleFolder = (folder: string, tick: boolean) => {
    setSkipped((list) => (tick ? list.filter((f) => f !== folder) : [...list, folder]));
    forgetTrial();
  };
  const toggleLocalhost = (tick: boolean) => {
    setLocalhostSkipped(!tick);
    forgetTrial();
  };

  const tryIt = async () => {
    setError(null);
    forgetTrial();
    setTrying(true);
    try {
      setRunId(await api.projectTryCheck(projectId, setup, check, chosen, openLocalhost, i18n.language));
      void refreshOverview();
    } catch (e) {
      setTrying(false);
      setError(errorText(t, e));
    }
  };

  const save = async () => {
    setError(null);
    if (mustChoose) {
      setError(t("reasons.githubAccountMissing"));
      return;
    }
    if (!info) return;
    setBusy(true);
    try {
      // Fresh, because Settings or an assistant may have changed something since this opened.
      await refreshOverview();
      const project = useApp.getState().overview.projects.find((p) => p.id === projectId) ?? info.project;
      // An account or command saved since this showed it isn't written over unseen: it's shown
      // first, and saving again is the person's call.
      if (catchUp(project)) return;
      await api.projectUpdate(projectId, withSetup(project, { githubAccount: showAccounts ? selected : project.githubAccount, setupCommand: setup, checkCommand: check, allowedRead: withFolders(project.allowedRead, chosen), allowLocalhost: project.allowLocalhost || openLocalhost }));
      await refreshOverview();
      closeSetup();
    } catch (e) {
      setError(errorText(t, e));
    } finally {
      setBusy(false);
    }
  };

  const leaveFor = (go: () => void) => {
    useApp.setState({ projectSetup: null });
    go();
  };

  const path = info?.project.path ?? request.added?.project.path ?? "";
  return (
    <div className="space-y-3">
      <p className="selectable -mt-2 font-mono text-[11.5px] break-all text-subtle">
        {shortPath(path)}
        {repo && ` · ${repo}`}
      </p>
      {request.added?.outcome === "restored" && <p className="text-[12px] text-muted">{t("setup.restored")}</p>}

      <div>
        {github === null && checking && <p className="text-[12px] text-subtle">{t("settings.githubChecking")}</p>}
        {github && !repo && <p className="text-[12px] text-muted">{t("setup.noRemote")}</p>}
        {repo && github?.base.error?.code === "ghMissing" && (
          <div className="flex flex-wrap items-center gap-2">
            <p className="min-w-0 flex-1 text-[12px] text-warning">{t("setup.noGh")}</p>
            <Button onClick={() => void api.openHelp("gh")}>{t("onboarding.install")}</Button>
          </div>
        )}
        {repo && github && github.base.error === null && accounts.length === 0 && (
          <div className="flex flex-wrap items-center gap-2">
            <p className="min-w-0 flex-1 text-[12px] text-warning">{t("setup.noAccount")}</p>
            <Button onClick={connectAccount}>{t("settings.githubConnect")}</Button>
          </div>
        )}
        {showAccounts && (
          <>
            <div className="flex items-end gap-3">
              <div className="w-64">
                <Field label={t("setup.account")}>
                  <select
                    value={account}
                    onChange={(e) => {
                      chose.current = true;
                      setAccount(e.target.value);
                    }}
                    className={inputClass}
                  >
                    <option value={NONE}>{t("setup.accountChoose")}</option>
                    {accounts.map((a) => (
                      <option key={a.login} value={a.login}>
                        {a.login}
                      </option>
                    ))}
                    {saved && !logins.includes(saved) && <option value={saved}>{saved}</option>}
                  </select>
                </Field>
              </div>
              <div className="pb-1.5">{checking ? <span className="text-[12px] text-subtle">{t("settings.githubChecking")}</span> : <AccessLabel status={status} />}</div>
            </div>
            {!checking && selected === null && canPush.length > 1 && <p className="mt-1 text-[12px] text-muted">{t("setup.several", { repo })}</p>}
            {!checking && canPush.length === 0 && selected === null && <p className="mt-1 text-[12px] text-warning">{t("setup.noPusher", { repo })}</p>}
            {status?.error && !needsAccess && <p className="mt-1 text-[12px] text-danger">{reasonText(t, status.error)}</p>}
            {needsAccess && (
              <p className="mt-1 flex gap-1.5 text-[12px] text-warning">
                <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                {t(access === "none" ? "settings.githubNoAccess" : "settings.githubReadOnly", { account: selected })}
              </p>
            )}
            <div className="mt-1.5 flex flex-wrap gap-2">
              {repo && (needsAccess || canPush.length === 0) && <Button onClick={() => openOnGithub(repoAccessUrl(repo))}>{t("settings.githubGiveAccess")}</Button>}
              {repo && needsAccess && <Button onClick={() => openOnGithub(repoInvitationUrl(repo))}>{t("settings.githubAcceptInvite")}</Button>}
              <Button variant="ghost" onClick={connectAccount}>
                {t("settings.githubConnect")}
              </Button>
            </div>
          </>
        )}
      </div>

      <div>
        <Field label={t("settings.checkCommand")}>
          <div className="flex gap-2">
            <input value={check} onChange={(e) => changeCheck(e.target.value)} placeholder={t("settings.checkCommandPlaceholder")} className={`${inputClass} font-mono`} />
            <Button className="h-auto shrink-0" disabled={check.trim() === "" || trying} onClick={() => void tryIt()}>
              {t("setup.tryIt")}
            </Button>
          </div>
        </Field>
        {guess && <OptionButtons options={checkOptionsLeft(check, guess)} onAdd={(option) => changeCheck(withCheckOption(check, option))} />}
        {runId && <TrialResult key={runId} runId={runId} onRunning={setTrying} onOpenRun={() => leaveFor(() => navigate({ name: "run", runId }))} />}
      </div>
      <div>
        <Field label={t("settings.setupCommand")}>
          <input value={setup} onChange={(e) => changeSetup(e.target.value)} placeholder={t("settings.setupCommandPlaceholder")} className={`${inputClass} font-mono`} />
        </Field>
        {guess && <OptionButtons options={setupOptionsLeft(setup, guess)} onAdd={(option) => changeSetup(withSetupOption(setup, guess, option))} />}
      </div>
      <FolderChoices folders={offered} skipped={skipped} onToggle={toggleFolder} />
      {offerLocalhost && <LocalhostChoice chosen={openLocalhost} onToggle={toggleLocalhost} />}

      <div className="space-y-0.5 text-[11.5px] leading-snug text-subtle">
        <p>
          {fromGuess && `${t(guess?.fromCi ? "setup.fromCi" : "setup.guessed", { files: guess?.from.join(", ") })} `}
          {fromCiChecks.length > 0 && `${t("setup.fromCiChecks", { commands: fromCiChecks.join(", ") })} `}
          {needsNode(fromCiChecks) && `${t("projects.pyrightNode")} `}
          {guess && guess.ciLeftOut.length > 0 && `${t("setup.ciLeftOut", { commands: guess.ciLeftOut.map((left) => `${left.command} (${t(`projects.ciWhy.${left.why}`)})`).join(", ") })} `}
          {nothingToGo && `${t("setup.noGuess")} `}
          {t("setup.tryNote")}
        </p>
        <p>{t("setup.noCheckNote")}</p>
      </div>
      {request.added?.map && (
        <MapLine
          map={request.added.map}
          onOpenBrain={() =>
            leaveFor(() => {
              useApp.setState({ brainProjectId: projectId });
              navigate({ name: "brain" });
            })
          }
        />
      )}
      {info && changed.length > 0 && <ProjectChangedNotice title={t("setup.changed")} saved={info.project} fields={changed} />}
      {error && (
        <p role="alert" className="text-[12px] text-danger">
          {error}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2 pt-1">
        <p className="min-w-0 flex-1 text-[11.5px] text-subtle">{t("setup.moreOptions")}</p>
        <Button onClick={closeSetup}>{t("setup.later")}</Button>
        <Button variant="primary" disabled={!known || checking || busy} onClick={() => void save()}>
          {t("setup.save")}
        </Button>
      </div>
    </div>
  );
}

/**
 * What every way of adding a project leads to, and what "Finish setup" reopens. Lives in App, since
 * Onboarding goes away the moment the first project exists and New Run closes for this.
 */
export function ProjectSetupDialog() {
  const { t } = useTranslation();
  const request = useApp((s) => s.projectSetup);
  const closeSetup = useApp((s) => s.closeProjectSetup);
  const listed = useApp((s) => s.overview.projects.find((p) => p.id === request?.projectId));
  if (!request) return null;
  const name = listed?.name ?? request.added?.project.name ?? "";
  const title = {
    new: "setup.titleNew",
    existing: "setup.titleExisting",
    restored: "setup.titleRestored",
    none: "setup.titleFinish",
  }[request.added?.outcome ?? "none"];
  return (
    <Modal open onOpenChange={(open) => !open && closeSetup()} title={t(title, { name })} className="w-[560px]">
      <SetupForm key={request.projectId} request={request} />
    </Modal>
  );
}
