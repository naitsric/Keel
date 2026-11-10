import { AlertTriangle, Check, Search } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { api, type EnvVar, type GithubStatus, type Project, type Service, type Workflow } from "../lib/api";
import { githubAccess, repoAccessUrl, repoInvitationUrl } from "../lib/github";
import { needsSetup } from "../lib/projects";
import { errorText, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass } from "../ui/primitives";
import { AddProjectButton, useAddProject } from "./AddProject";
import { RemoveProjectDialog } from "./RemoveProjectDialog";

/** From this many projects, the list gets a filter. With fewer it only gets in the way. */
const FILTER_FROM = 6;

const lines = (text: string) =>
  text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

const NONE = "__none__";
// A project with no default workflow runs this one, so it's what the setting shows and saves as nothing.
const FULL_WORKFLOW = "builtin:full";
const POSTGRES_PORT = 5432;
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
        <Field label={t("settings.serviceSocket")} hint={service.type === "bridge" && service.port === POSTGRES_PORT ? t("settings.serviceSocketHintPostgres") : t("settings.serviceSocketHint")}>
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

function ProjectForm({ project, saved, onSaved }: { project: Project; saved: boolean; onSaved: () => void }) {
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
  const [sensitive, setSensitive] = useState(project.sensitivePaths.join("\n"));
  const [workflows, setWorkflows] = useState<Workflow[] | null>(null);
  const [status, setStatus] = useState<GithubStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const latestRequest = useRef(0);
  const [removing, setRemoving] = useState(false);
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
        sensitivePaths: lines(sensitive),
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
  const removeProject = () => setRemoving(true);
  return (
    <div className="space-y-4">
      <Card className="space-y-5 p-5">
        <header>
          <h2 className="text-[16px] font-semibold">{project.name}</h2>
          <p className="selectable mt-0.5 font-mono text-[11.5px] text-subtle">{project.path}</p>
        </header>

        <Section title={t("projects.sectionGeneral")}>
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
        </Section>

        <Section title={t("projects.sectionGithub")}>
          {status?.repo ? <p className="text-[12px] text-muted">{t("settings.githubRepo", { repo: status.repo })}</p> : <p className="text-[12px] text-muted">{t("settings.githubNoRepo")}</p>}
          <div className="max-w-sm">
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
            <div className="space-y-1 text-[12px] text-muted">
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
            <p className="flex gap-1.5 text-[12px] text-warning">
              <AlertTriangle size={13} className="mt-0.5 shrink-0" />
              {t("settings.githubScopes", { scopes: status.excessScopes.join(", ") })}
            </p>
          )}
          {status?.error && !needsAccess && <p className="text-[12px] text-danger">{reasonText(t, status.error)}</p>}
          {checkError && <p className="text-[12px] text-danger">{checkError}</p>}
          {needsAccess && repo && (
            <div role="alert" className="rounded-md border border-border-strong bg-inset px-3 py-2.5">
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
            <div>
              <div className="flex flex-wrap gap-2">
                {repo && !needsAccess && <Button onClick={() => openOnGithub(repoAccessUrl(repo))}>{t("settings.githubManageAccess")}</Button>}
                <Button onClick={connectAccount}>{t("settings.githubConnect")}</Button>
              </div>
              <p className="mt-1.5 text-[11.5px] leading-snug text-subtle">{t(repo && !needsAccess ? "settings.githubLinksHint" : "settings.githubConnectHint")}</p>
            </div>
          )}
        </Section>

        <Section title={t("projects.sectionAccess")} hint={t("projects.sectionAccessHint")}>
          <div className="grid gap-3 md:grid-cols-2">
            <Field label={t("settings.domains")} hint={t("settings.domainsHint")}>
              <textarea rows={3} value={domains} onChange={(e) => setDomains(e.target.value)} className={`${inputClass} resize-y font-mono`} />
            </Field>
            <Field label={t("settings.readPaths")} hint={t("settings.readPathsHint")}>
              <textarea rows={3} value={paths} onChange={(e) => setPaths(e.target.value)} className={`${inputClass} resize-y font-mono`} />
            </Field>
          </div>
          <Field label={t("settings.sensitive")} hint={t("settings.sensitiveHint")}>
            <textarea rows={3} value={sensitive} onChange={(e) => setSensitive(e.target.value)} className={`${inputClass} resize-y font-mono`} />
          </Field>
        </Section>

        <Section title={t("projects.sectionServices")} hint={t("settings.servicesHint")}>
          {services.length === 0 ? (
            <p className="text-[12px] text-subtle">{t("settings.servicesEmpty")}</p>
          ) : (
            <ul className="space-y-2">
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
          <div className="flex gap-2">
            <Button onClick={() => setServices([...services, { type: "bridge", name: "db", port: 5432, socket: ".s.PGSQL.5432" }])}>{t("settings.addBridge")}</Button>
            <Button onClick={() => setServices([...services, { type: "command", name: "db", start: "", socket: null }])}>{t("settings.addCommand")}</Button>
          </div>
          <Field label={t("settings.env")} hint={t("settings.envHint")}>
            <textarea rows={3} value={env} onChange={(e) => setEnv(e.target.value)} placeholder={ENV_EXAMPLE} className={`${inputClass} resize-y font-mono`} />
          </Field>
        </Section>

        {/* Stays in view while the form scrolls, so saving is never a search. */}
        <div className="sticky -bottom-5 z-10 -mx-5 -mb-5 flex items-center gap-2 rounded-b-lg border-t border-border bg-elevated px-5 py-3">
          <Button variant="primary" onClick={() => void save()}>
            {t("settings.save")}
          </Button>
          {saved && <span className="text-[12px] text-success">{t("settings.saved")}</span>}
          {error && (
            <p role="alert" className="min-w-0 flex-1 text-[12px] text-danger">
              {error}
            </p>
          )}
        </div>
      </Card>

      <Card className="flex flex-wrap items-center gap-3 border-danger/40 p-4">
        <div className="min-w-0 flex-1">
          <h3 className="text-[13px] font-semibold text-danger">{t("projects.dangerTitle")}</h3>
          <p className="mt-0.5 text-[12px] text-muted">{t("projects.dangerHint")}</p>
        </div>
        <Button variant="danger" onClick={removeProject}>
          {t("remove.confirm")}
        </Button>
      </Card>
      <RemoveProjectDialog project={project} open={removing} onOpenChange={setRemoving} />
    </div>
  );
}

function Section({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <section className="space-y-3 border-t border-border pt-5 first-of-type:border-t-0 first-of-type:pt-0">
      <div>
        <h3 className="text-[13px] font-semibold">{title}</h3>
        {hint && <p className="mt-0.5 text-[12px] text-muted">{hint}</p>}
      </div>
      {children}
    </section>
  );
}

function ProjectItem({ project, selected, onSelect }: { project: Project; selected: boolean; onSelect: () => void }) {
  const { t } = useTranslation();
  const pending = needsSetup(project);
  return (
    <li>
      <button type="button" aria-current={selected ? "true" : undefined} onClick={onSelect} className={cx("flex w-full flex-col gap-0.5 rounded-md px-2.5 py-2 text-left transition-colors hover:bg-hover", selected && "bg-active")}>
        <span className="truncate text-[13px] font-medium">{project.name}</span>
        <span className="truncate font-mono text-[11.5px] text-subtle">{project.githubRepo ?? t("projects.noRepo")}</span>
        <span className={cx("flex items-center gap-1 text-[11.5px]", pending ? "text-warning" : "text-success")}>
          {pending ? <AlertTriangle size={11} className="shrink-0" /> : <Check size={11} className="shrink-0" />}
          {t(pending ? "projects.needsSetup" : "projects.ready")}
        </span>
      </button>
    </li>
  );
}

/** Every project in a list, and the one chosen on the right, so the page doesn't grow with them. */
export function ProjectsView({ projectId }: { projectId?: string }) {
  const { t } = useTranslation();
  const projects = useApp((s) => s.overview.projects);
  const { add, error, busy } = useAddProject();
  const [chosen, setChosen] = useState<string | undefined>(projectId);
  const [filter, setFilter] = useState("");
  // Out here because saving can start the form over.
  const [savedId, setSavedId] = useState<string | null>(null);
  const showSaved = (id: string) => {
    setSavedId(id);
    setTimeout(() => setSavedId((current) => (current === id ? null : current)), 2000);
  };
  useEffect(() => setChosen(projectId), [projectId]);

  // The one asked for, or the first when none was or it's gone.
  const selected = projects.find((p) => p.id === chosen) ?? projects[0];
  const text = filter.trim().toLowerCase();
  const shown = projects.length >= FILTER_FROM && text ? projects.filter((p) => p.name.toLowerCase().includes(text) || (p.githubRepo ?? "").toLowerCase().includes(text)) : projects;
  return (
    <main className="flex-1 overflow-y-auto p-5">
      <div className="mx-auto w-full max-w-6xl">
        <div className="flex items-center justify-between gap-2">
          <h1 className="text-[18px] font-semibold">{t("projects.title")}</h1>
          <AddProjectButton add={add} busy={busy} />
        </div>
        {error && (
          <p role="alert" className="mt-2 text-[12px] text-danger">
            {error}
          </p>
        )}
        {projects.length === 0 ? (
          <p className="mt-4 text-[12.5px] text-subtle">{t("projects.none")}</p>
        ) : (
          <div className="mt-4 grid items-start gap-4 lg:grid-cols-[240px_1fr]">
            <div className="space-y-2 lg:sticky lg:top-0">
              {projects.length >= FILTER_FROM && (
                <label className="relative block">
                  <Search size={13} className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-subtle" />
                  <input type="search" aria-label={t("projects.filter")} placeholder={t("projects.filterPlaceholder")} value={filter} onChange={(e) => setFilter(e.target.value)} className={`${inputClass} pl-8`} />
                </label>
              )}
              <div aria-label={t("projects.title")} role="group">
                <ul className="space-y-0.5">
                  {shown.map((project) => (
                    <ProjectItem key={project.id} project={project} selected={project.id === selected?.id} onSelect={() => setChosen(project.id)} />
                  ))}
                </ul>
                {shown.length === 0 && <p className="px-2.5 py-2 text-[12px] text-subtle">{t("projects.noMatch")}</p>}
              </div>
            </div>
            {selected && <ProjectForm key={projectCardKey(selected)} project={selected} saved={savedId === selected.id} onSaved={() => showSaved(selected.id)} />}
          </div>
        )}
      </div>
    </main>
  );
}
