import { AlertTriangle } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Language } from "../i18n";
import { api, type Decision, type EnvVar, type GithubStatus, type Project, type Service } from "../lib/api";
import { formatDateTime } from "../lib/format";
import { errorText, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, Field, inputClass, Segmented } from "../ui/primitives";
import { TopBar } from "./TopBar";

const lines = (text: string) =>
  text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

const NONE = "__none__";
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

function ProjectSettingsCard({ project }: { project: Project }) {
  const { t } = useTranslation();
  const refreshOverview = useApp((s) => s.refreshOverview);
  const [account, setAccount] = useState(project.githubAccount ?? NONE);
  const [setup, setSetup] = useState(project.setupCommand ?? "");
  const [check, setCheck] = useState(project.checkCommand ?? "");
  const [domains, setDomains] = useState(project.allowedDomains.join("\n"));
  const [paths, setPaths] = useState(project.allowedRead.join("\n"));
  const [services, setServices] = useState<Service[]>(project.services);
  const [env, setEnv] = useState(project.env.map((v) => `${v.name}=${v.value}`).join("\n"));
  const [status, setStatus] = useState<GithubStatus | null>(null);
  const [decisions, setDecisions] = useState<Decision[]>([]);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    api.githubStatus(project.id).then(setStatus).catch((e) => setError(errorText(t, e)));
    api.decisionsList(project.id).then(setDecisions).catch((e) => setError(errorText(t, e)));
  }, [project.id, t]);

  useEffect(load, [load]);

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
      });
      await refreshOverview();
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
      load();
    } catch (e) {
      setError(errorText(t, e));
    }
  };

  const retire = async (seq: number) => {
    await api.decisionRetire(seq);
    load();
  };

  const info = status?.info;
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
      {status?.error && <p className="mt-1 text-[12px] text-danger">{reasonText(t, status.error)}</p>}

      <div className="mt-4 space-y-3">
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

      <h4 className="mt-5 text-[12.5px] font-semibold">{t("settings.decisions")}</h4>
      <p className="mt-0.5 text-[12px] text-muted">{t("settings.decisionsHint")}</p>
      {decisions.filter((d) => d.text).length === 0 ? (
        <p className="mt-2 text-[12px] text-subtle">{t("settings.decisionsEmpty")}</p>
      ) : (
        <ul className="mt-2 space-y-1.5">
          {decisions
            .filter((d) => d.text)
            .map((decision) => (
              <li key={decision.seq} className="flex items-start gap-3 rounded-md border border-border p-2.5">
                <div className="min-w-0 flex-1">
                  <p className="selectable text-[12.5px]">{decision.text}</p>
                  <p className="mt-0.5 text-[11px] text-subtle">{formatDateTime(decision.at)}</p>
                </div>
                <Button variant="ghost" onClick={() => void retire(decision.seq)}>
                  {t("settings.retire")}
                </Button>
              </li>
            ))}
        </ul>
      )}
    </Card>
  );
}

export function SettingsView() {
  const { t, i18n } = useTranslation();
  const setLanguage = useApp((s) => s.setLanguage);
  const projects = useApp((s) => s.overview.projects);
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
          <h2 className="pt-1 text-[14px] font-semibold">{t("settings.projects")}</h2>
          {projects.length === 0 && <p className="text-[12.5px] text-subtle">{t("settings.noProjects")}</p>}
          {projects.map((project) => (
            <ProjectSettingsCard key={project.id} project={project} />
          ))}
        </div>
      </main>
    </>
  );
}
