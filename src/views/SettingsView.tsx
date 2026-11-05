import { AlertTriangle } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Language } from "../i18n";
import { api, type EnvVar, type GithubStatus, type Project, type Service, type Workflow } from "../lib/api";
import { githubAccess, repoAccessUrl, repoInvitationUrl } from "../lib/github";
import { errorText, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, Field, inputClass, Segmented } from "../ui/primitives";
import { AddProjectButton, useAddProject } from "./AddProject";
import { McpCard } from "./McpCard";
import { TopBar } from "./TopBar";

const lines = (text: string) =>
  text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

const NONE = "__none__";
// A project with no default workflow runs this one, so it's what the setting shows and saves as nothing.
const FULL_WORKFLOW = "builtin:full";
const AGENT_LIMITS = Array.from({ length: 10 }, (_, i) => String(i + 1));
const ENV_EXAMPLE = "DB_HOST={{socketDir:db}}";

/** Turns `NAME=value` lines into variables, or says which line isn't one. */
export function parseEnv(text: string): { env: EnvVar[]; invalid: string | null } {
  const env: EnvVar[] = [];
  for (const line of lines(text)) {
    const at = line.indexOf("=");
    if (at <= 0) return { env, invalid: line };
    env.push({ name: line.slice(0, at).trim(), value: line.slice(at + 1).trim() });
  }
  return { env, invalid: null };
}

function ServiceRow({ service, onChange, onRemove }: { service: Service; onChange: (service: Service) => void; onRemove: () => void }) {
  const { t } = useTranslation();
  return (
    <li className="rounded-md border border-border p-3">
      <p className="mb-2 text-[11.5px] text-subtle">{service.type === "bridge" ? t("settings.serviceTypeBridge") : t("settings.serviceTypeCommand")}</p>
      <div className="grid gap-2 md:grid-cols-[1fr_1fr_1.4fr]">
        <Field label={t("settings.serviceName")}>
          <input value={service.name} onChange={(e) => onChange({ ...service, name: e.target.value })} className={`${inputClass} font-mono`} />
        </Field>
        {service.type === "bridge" ? (
          <Field label={t("settings.servicePort")}>
            <input inputMode="numeric" value={service.port || ""} onChange={(e) => onChange({ ...service, port: Number(e.target.value.replace(/\D/g, "")) || 0 })} className={`${inputClass} font-mono`} />
          </Field>
        ) : (
          <Field label={t("settings.serviceStart")}>
            <input value={service.start} onChange={(e) => onChange({ ...service, start: e.target.value })} className={`${inputClass} font-mono`} />
          </Field>
        )}
        <Field label={t("settings.serviceSocket")} hint={t("settings.serviceSocketHint")}>
          <input value={service.socket ?? ""} onChange={(e) => onChange({ ...service, socket: e.target.value.trim() || null })} className={`${inputClass} font-mono`} />
        </Field>
      </div>
      <Button variant="ghost" className="mt-1 -ml-2" onClick={onRemove}>
        {t("settings.serviceRemove")}
      </Button>
    </li>
  );
}

/**
 * The card reads its values only as it mounts, so it has to start over when the setup dialog changes
 * them, or its next Save would put the old ones back.
 */
export const projectCardKey = (project: Project) => [project.id, project.githubAccount, project.githubRepo, project.setupCommand, project.checkCommand].join("\0");

function ProjectSettingsCard({ project, saved, onSaved }: { project: Project; saved: boolean; onSaved: () => void }) {
  const { t, i18n } = useTranslation();
  const refreshOverview = useApp((s) => s.refreshOverview);
  const [account, setAccount] = useState(project.githubAccount ?? NONE);
  const [setup, setSetup] = useState(project.setupCommand ?? "");
  const [check, setCheck] = useState(project.checkCommand ?? "");
  const [domains, setDomains] = useState(project.allowedDomains.join("\n"));
  const [paths, setPaths] = useState(project.allowedRead.join("\n"));
  const [services, setServices] = useState<Service[]>(project.services);
  const [env, setEnv] = useState(project.env.map((v) => `${v.name}=${v.value}`).join("\n"));
  const [workflow, setWorkflow] = useState(project.defaultWorkflow ?? FULL_WORKFLOW);
  const [workflows, setWorkflows] = useState<Workflow[] | null>(null);
  const [status, setStatus] = useState<GithubStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const latestRequest = useRef(0);
  const selected = account === NONE ? null : account;

  // Asks about the account that's selected, saved or not. An answer that comes late never wins.
  const load = useCallback(() => {
    const request = ++latestRequest.current;
    setChecking(true);
    api
      .githubStatus(project.id, selected)
      .then((next) => {
        if (request !== latestRequest.current) return;
        setStatus(next);
        setCheckError(null);
      })
      .catch((e) => {
        if (request === latestRequest.current) setCheckError(errorText(t, e));
      })
      .finally(() => {
        if (request === latestRequest.current) setChecking(false);
      });
  }, [project.id, selected, t]);

  useEffect(load, [load]);

  // Access is given in the browser and accounts connect in Terminal, so look again on the way back.
  useEffect(() => {
    window.addEventListener("focus", load);
    return () => window.removeEventListener("focus", load);
  }, [load]);

  useEffect(() => {
    api
      .workflowsList(project.id, i18n.language)
      .then(setWorkflows)
      .catch((e) => setError(errorText(t, e)));
  }, [project.id, i18n.language, t]);

  const openOnGithub = (url: string) => {
    setError(null);
    api.openGithubUrl(url).catch((e) => setError(errorText(t, e)));
  };

  const connectAccount = () => {
    setError(null);
    api.openGithubLogin().catch((e) => setError(errorText(t, e)));
  };

  const save = async () => {
    setError(null);
    const parsed = parseEnv(env);
    if (parsed.invalid) {
      setError(t("settings.envInvalid", { line: parsed.invalid }));
      return;
    }
    try {
      await api.projectUpdate(project.id, {
        githubAccount: account === NONE ? null : account,
        setupCommand: setup,
        checkCommand: check,
        allowedDomains: lines(domains),
        allowedRead: lines(paths),
        services,
        env: parsed.env,
        defaultWorkflow: workflow === FULL_WORKFLOW ? null : workflow,
      });
      await refreshOverview();
      onSaved();
      load();
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  const info = status?.info;
  const repo = status?.repo ?? null;
  const access = githubAccess(status);
  const needsAccess = selected !== null && (access === "none" || access === "readOnly");
  const workflowGone = workflows !== null && !workflows.some((w) => w.id === workflow);
  return (
    <Card className="p-4">
      <h3 className="text-[14px] font-semibold">{project.name}</h3>
      <p className="selectable mt-0.5 font-mono text-[11.5px] text-subtle">{project.path}</p>

      <h4 className="mt-4 text-[12.5px] font-semibold">{t("settings.github")}</h4>
      {status?.repo ? <p className="mt-1 text-[12px] text-muted">{t("settings.githubRepo", { repo: status.repo })}</p> : <p className="mt-1 text-[12px] text-muted">{t("settings.githubNoRepo")}</p>}
      <div className="mt-2 max-w-sm">
        <Field label={t("settings.githubAccount")} hint={t("settings.githubHint")}>
          <select value={account} onChange={(e) => setAccount(e.target.value)} className={inputClass}>
            <option value={NONE}>{t("settings.githubNone")}</option>
            {status?.accounts.map((a) => (
              <option key={a.login} value={a.login}>
                {a.login}
                {a.active ? ` (${t("settings.githubActive")})` : ""}
              </option>
            ))}
          </select>
        </Field>
      </div>
      {info && (
        <div className="mt-2 space-y-1 text-[12px] text-muted">
          <p>
            {t("settings.githubPermission", { permission: info.permission })} · {t("settings.githubDefault", { branch: info.defaultBranch })}
          </p>
          {info.defaultBranchProtected === false && (
            <p className="flex gap-1.5 text-warning">
              <AlertTriangle size={13} className="mt-0.5 shrink-0" />
              {t("settings.githubUnprotected", { branch: info.defaultBranch })}
            </p>
          )}
        </div>
      )}
      {status && status.excessScopes.length > 0 && (
        <p className="mt-1 flex gap-1.5 text-[12px] text-warning">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          {t("settings.githubScopes", { scopes: status.excessScopes.join(", ") })}
        </p>
      )}
      {status?.error && !needsAccess && <p className="mt-1 text-[12px] text-danger">{reasonText(t, status.error)}</p>}
      {checkError && <p className="mt-1 text-[12px] text-danger">{checkError}</p>}
      {needsAccess && repo && (
        <div role="alert" className="mt-2 rounded-md border border-border-strong bg-inset px-3 py-2.5">
          <p className="flex gap-1.5 text-[12.5px] text-warning">
            <AlertTriangle size={13} className="mt-0.5 shrink-0" />
            {t(access === "none" ? "settings.githubNoAccess" : "settings.githubReadOnly", { account: selected })}
          </p>
          <div className="mt-2 flex flex-wrap gap-2">
            <Button onClick={() => openOnGithub(repoAccessUrl(repo))}>{t("settings.githubGiveAccess")}</Button>
            <Button onClick={() => openOnGithub(repoInvitationUrl(repo))}>{t("settings.githubAcceptInvite")}</Button>
            <Button variant="ghost" disabled={checking} onClick={load}>
              {t(checking ? "settings.githubChecking" : "settings.githubCheckAgain")}
            </Button>
          </div>
          <p className="mt-2 text-[11.5px] leading-snug text-subtle">{t("settings.githubAccessHint", { account: selected })}</p>
        </div>
      )}
      {status && status.error?.code !== "ghMissing" && (
        <div className="mt-2">
          <div className="flex flex-wrap gap-2">
            {repo && !needsAccess && <Button onClick={() => openOnGithub(repoAccessUrl(repo))}>{t("settings.githubManageAccess")}</Button>}
            <Button onClick={connectAccount}>{t("settings.githubConnect")}</Button>
          </div>
          <p className="mt-1.5 text-[11.5px] leading-snug text-subtle">{t(repo && !needsAccess ? "settings.githubLinksHint" : "settings.githubConnectHint")}</p>
        </div>
      )}

      <div className="mt-4 space-y-3">
        <div className="max-w-sm">
          <Field label={t("settings.defaultWorkflow")} hint={t("settings.defaultWorkflowHint")}>
            <select value={workflow} disabled={workflows === null} onChange={(e) => setWorkflow(e.target.value)} className={inputClass}>
              {workflows === null && <option value={workflow}>{t("settings.githubChecking")}</option>}
              {workflowGone && <option value={workflow}>{t("settings.defaultWorkflowGone")}</option>}
              {workflows?.map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          </Field>
          {workflowGone && (
            <p className="mt-1 flex gap-1.5 text-[12px] text-warning">
              <AlertTriangle size={13} className="mt-0.5 shrink-0" />
              {t("reasons.defaultWorkflowMissing")}
            </p>
          )}
        </div>
        <Field label={t("settings.setupCommand")} hint={t("settings.setupCommandHint")}>
          <input value={setup} onChange={(e) => setSetup(e.target.value)} placeholder={t("settings.setupCommandPlaceholder")} className={`${inputClass} font-mono`} />
        </Field>
        <Field label={t("settings.checkCommand")} hint={t("settings.checkCommandHint")}>
          <input value={check} onChange={(e) => setCheck(e.target.value)} placeholder={t("settings.checkCommandPlaceholder")} className={`${inputClass} font-mono`} />
        </Field>
        <div className="grid gap-3 md:grid-cols-2">
          <Field label={t("settings.domains")} hint={t("settings.domainsHint")}>
            <textarea rows={3} value={domains} onChange={(e) => setDomains(e.target.value)} className={`${inputClass} resize-y font-mono`} />
          </Field>
          <Field label={t("settings.readPaths")} hint={t("settings.readPathsHint")}>
            <textarea rows={3} value={paths} onChange={(e) => setPaths(e.target.value)} className={`${inputClass} resize-y font-mono`} />
          </Field>
        </div>
      </div>
      <h4 className="mt-5 text-[12.5px] font-semibold">{t("settings.services")}</h4>
      <p className="mt-0.5 text-[12px] text-muted">{t("settings.servicesHint")}</p>
      {services.length === 0 ? (
        <p className="mt-2 text-[12px] text-subtle">{t("settings.servicesEmpty")}</p>
      ) : (
        <ul className="mt-2 space-y-2">
          {services.map((service, index) => (
            <ServiceRow
              // Rows have no identity until saved, and names change as they're typed.
              key={index}
              service={service}
              onChange={(next) => setServices(services.map((s, i) => (i === index ? next : s)))}
              onRemove={() => setServices(services.filter((_, i) => i !== index))}
            />
          ))}
        </ul>
      )}
      <div className="mt-2 flex gap-2">
        <Button onClick={() => setServices([...services, { type: "bridge", name: "db", port: 5432, socket: ".s.PGSQL.5432" }])}>{t("settings.addBridge")}</Button>
        <Button onClick={() => setServices([...services, { type: "command", name: "db", start: "", socket: null }])}>{t("settings.addCommand")}</Button>
      </div>
      <div className="mt-3">
        <Field label={t("settings.env")} hint={t("settings.envHint")}>
          <textarea rows={3} value={env} onChange={(e) => setEnv(e.target.value)} placeholder={ENV_EXAMPLE} className={`${inputClass} resize-y font-mono`} />
        </Field>
      </div>
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
      <div className="mt-3 flex items-center gap-2">
        <Button variant="primary" onClick={() => void save()}>
          {t("settings.save")}
        </Button>
        {saved && <span className="text-[12px] text-success">{t("settings.saved")}</span>}
      </div>
    </Card>
  );
}

function MaxAgentsCard() {
  const { t } = useTranslation();
  const [value, setValue] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .maxAgentsGet()
      .then((limit) => setValue(String(limit)))
      .catch((e) => setError(errorText(t, e)));
  }, [t]);

  const change = (next: string) => {
    const previous = value;
    setValue(next);
    setError(null);
    api.maxAgentsSet(Number(next)).catch((e) => {
      setValue(previous);
      setError(errorText(t, e));
    });
  };

  return (
    <Card className="p-4">
      <p className="text-[13px] font-medium">{t("settings.maxAgents")}</p>
      <p className="mt-0.5 mb-3 text-[12px] text-muted">{t("settings.maxAgentsHint")}</p>
      {value !== null && <Segmented<string> label={t("settings.maxAgents")} value={value} onChange={change} options={AGENT_LIMITS.map((limit) => ({ value: limit, label: limit }))} />}
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
    </Card>
  );
}

export function SettingsView() {
  const { t, i18n } = useTranslation();
  const setLanguage = useApp((s) => s.setLanguage);
  const projects = useApp((s) => s.overview.projects);
  const { add, error, busy } = useAddProject();
  // Out here because saving can start a card over.
  const [savedId, setSavedId] = useState<string | null>(null);
  const showSaved = (id: string) => {
    setSavedId(id);
    setTimeout(() => setSavedId((current) => (current === id ? null : current)), 2000);
  };
  return (
    <>
      <TopBar back />
      <main className="flex-1 overflow-y-auto p-5">
        <h1 className="text-[18px] font-semibold">{t("settings.title")}</h1>
        <div className="mt-4 max-w-3xl space-y-4">
          <Card className="p-4">
            <p className="text-[13px] font-medium">{t("settings.language")}</p>
            <p className="mt-0.5 mb-3 text-[12px] text-muted">{t("settings.languageHint")}</p>
            <Segmented<Language>
              label={t("settings.language")}
              value={i18n.language === "es" ? "es" : "en"}
              onChange={(language) => void setLanguage(language)}
              options={[
                { value: "en", label: t("settings.languages.en") },
                { value: "es", label: t("settings.languages.es") },
              ]}
            />
          </Card>
          <MaxAgentsCard />
          <McpCard />
          <div className="flex items-center justify-between gap-2 pt-1">
            <h2 className="text-[14px] font-semibold">{t("settings.projects")}</h2>
            <AddProjectButton add={add} busy={busy} />
          </div>
          {error && (
            <p role="alert" className="text-[12px] text-danger">
              {error}
            </p>
          )}
          {projects.length === 0 && <p className="text-[12.5px] text-subtle">{t("settings.noProjects")}</p>}
          {projects.map((project) => (
            <ProjectSettingsCard key={projectCardKey(project)} project={project} saved={savedId === project.id} onSaved={() => showSaved(project.id)} />
          ))}
        </div>
      </main>
    </>
  );
}
