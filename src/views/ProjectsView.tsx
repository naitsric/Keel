import { AlertTriangle, Check, Plus, Search, X } from "lucide-react";
import { type ReactNode, type RefObject, useCallback, useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type CiGap, type EnvVar, type GithubStatus, type IssueWatch, type Project, type Service, type Workflow } from "../lib/api";
import { formatRelative } from "../lib/format";
import { githubAccess, repoAccessUrl, repoInvitationUrl } from "../lib/github";
import { ciCommandsLeft, ciLeftOutLeft, type Configured, configuredChanges, configuredValue, foldersToOffer, needsNode, needsSetup, setupMissing, withCheckOption, withFolders } from "../lib/projects";
import { errorText, reasonText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, fieldHintClass, fieldLabelClass, inputClass, SettingRow } from "../ui/primitives";
import { AddProjectButton, useAddProject } from "./AddProject";
import { ProjectChangedNotice } from "./ProjectChangedNotice";
import { RemoveProjectDialog } from "./RemoveProjectDialog";
import { FolderOffers, useToolFolders } from "./ToolFolders";

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

/** The parts of a project's settings, in the order the page and its index show them. */
const SECTIONS = ["commands", "github", "workflow", "access", "services"] as const;
type SectionId = (typeof SECTIONS)[number];
/** Their names. The index says "Services", shorter than that card's own title. */
const SECTION_TITLES: Record<SectionId, string> = {
  commands: "projects.sectionCommands",
  github: "projects.sectionGithub",
  workflow: "projects.sectionWorkflow",
  access: "projects.sectionAccess",
  services: "settings.services",
};

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
    <li className="rounded-md border border-border px-3 pt-2 pb-3">
      <div className="mb-2 flex items-center gap-2">
        <p className="min-w-0 flex-1 text-[12px] leading-snug text-muted">{service.type === "bridge" ? t("settings.serviceTypeBridge") : t("settings.serviceTypeCommand")}</p>
        <Button variant="ghost" className="-mr-1.5 h-6 shrink-0 px-1.5" onClick={onRemove}>
          <X size={12} />
          {t("settings.serviceRemove")}
        </Button>
      </div>
      <div className="grid gap-2 @xl:grid-cols-[1fr_1fr_1.4fr]">
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
    </li>
  );
}

/**
 * The card reads most of its values only as it mounts, so it has to start over when the setup dialog
 * changes them, or its next Save would put the old ones back: the folders it adds and localhost. The
 * account, the commands and the domains aren't here. A client can change them at any moment, and
 * starting over would drop what the person hasn't saved, so the card follows them: see
 * `useSavedConfiguration`.
 */
export const projectCardKey = (project: Project) => [project.id, project.githubRepo, project.allowLocalhost, ...project.allowedRead].join("\0");

/** The domains as their box holds them. */
const domainsText = (project: Configured) => configuredValue(project, "allowedDomains") ?? "";

/**
 * The account, commands and domains as the form holds them. When the saved ones change under it, a
 * field the person hasn't edited takes the new value and an edited one stays. `byClient` has the
 * fields an MCP client changed since the form opened or was last saved.
 */
function useSavedConfiguration(project: Project) {
  const [account, setAccount] = useState(project.githubAccount ?? NONE);
  const [setup, setSetup] = useState(project.setupCommand ?? "");
  const [check, setCheck] = useState(project.checkCommand ?? "");
  const [domains, setDomains] = useState(() => domainsText(project));
  const heldDomains = useRef(domains);
  heldDomains.current = domains;
  // The saved domains the box was last filled from, which is what the person has seen in it.
  const filledFrom = useRef(project.allowedDomains);
  const [byClient, setByClient] = useState<(keyof Configured)[]>([]);
  const seen = useRef<Configured>(project);
  const [openedAt] = useState(() => Date.now());
  const announced = useRef(0);
  // The last of them the person had been told of when they last saved.
  const settled = useRef(0);
  const { id, githubAccount, setupCommand, checkCommand } = project;
  // A new list comes with every look at the projects, so it's told apart by what it says.
  const allowed = project.allowedDomains.join("\n");

  useEffect(() => {
    const before = seen.current;
    const now: Configured = { githubAccount, setupCommand, checkCommand, allowedDomains: lines(allowed) };
    const changed = configuredChanges(before, now);
    if (changed.length === 0) return;
    seen.current = now;
    setAccount((held) => (held === (before.githubAccount ?? NONE) ? (now.githubAccount ?? NONE) : held));
    setSetup((held) => (held === (before.setupCommand ?? "") ? (now.setupCommand ?? "") : held));
    setCheck((held) => (held === (before.checkCommand ?? "") ? (now.checkCommand ?? "") : held));
    if (heldDomains.current === domainsText(before)) filledFrom.current = now.allowedDomains ?? [];
    setDomains((held) => (held === domainsText(before) ? domainsText(now) : held));
    // The change may be the person's, from the setup dialog. The record of what clients did says.
    api
      .mcpOverview()
      .then(({ projectChanges }) => {
        const news = projectChanges.filter((c) => c.projectId === id && c.seq > announced.current && Date.parse(c.at) >= openedAt);
        if (news.length === 0) return;
        // Told only once the form holds what the latest of them left: one saved after the values
        // this effect has may be on record already, and saving must still stop for it.
        const latest = news.reduce((a, b) => (b.seq > a.seq ? b : a));
        if (latest.changes.every(({ field, after }) => configuredValue(now, field) === after)) announced.current = latest.seq;
        setByClient((fields) => [...new Set([...fields, ...changed])]);
      })
      .catch(() => undefined);
  }, [id, githubAccount, setupCommand, checkCommand, allowed, openedAt]);

  /** After a save of `sent`, shows it as Keel wrote it, unless the person has typed since. */
  const takeSaved = (sent: { account: string; setup: string; check: string; domains: string }, now: Project) => {
    seen.current = now;
    settled.current = announced.current;
    filledFrom.current = heldDomains.current === sent.domains ? now.allowedDomains : lines(sent.domains);
    setAccount((held) => (held === sent.account ? (now.githubAccount ?? NONE) : held));
    setSetup((held) => (held === sent.setup ? (now.setupCommand ?? "") : held));
    setCheck((held) => (held === sent.check ? (now.checkCommand ?? "") : held));
    setDomains((held) => (held === sent.domains ? domainsText(now) : held));
    setByClient([]);
  };

  /**
   * The saved domains the person has seen, to send with a save: Keel keeps any other the project
   * has by then, such as one they allowed from a step that waited for it. They're those the box was
   * filled from, and those a client added since the last save, that the person has been told of and
   * no client took away again: for those the box wins.
   */
  const seenDomains = async (from: string[]) => {
    // Without the record, only what filled the box counts, so nothing else is taken away.
    const recorded = await api
      .mcpOverview()
      .then(({ projectChanges }) => projectChanges)
      .catch(() => []);
    const theirs = new Set<string>();
    recorded
      .filter((c) => c.projectId === id && c.seq > settled.current && c.seq <= announced.current && Date.parse(c.at) >= openedAt)
      .sort((a, b) => a.seq - b.seq)
      .flatMap((c) => c.changes.filter((change) => change.field === "allowedDomains"))
      .forEach(({ before, after }) => {
        const [had, has] = [lines(before ?? ""), lines(after ?? "")];
        had.filter((domain) => !has.includes(domain)).forEach((domain) => theirs.delete(domain));
        has.filter((domain) => !had.includes(domain)).forEach((domain) => theirs.add(domain));
      });
    return [...new Set([...from, ...theirs])];
  };

  /** What the box was filled from, to read before saving waits for anything. */
  const filled = () => filledFrom.current;

  /**
   * The last change a client made to the project since the form opened that the form hasn't taken
   * in yet, or null: the app hears of a change a moment after it's saved, and saving then would
   * write over it.
   */
  const clientChangeUnheard = () =>
    api
      .mcpOverview()
      .then(({ projectChanges }) => {
        const seqs = projectChanges.filter((c) => c.projectId === id && c.seq > announced.current && Date.parse(c.at) >= openedAt).map((c) => c.seq);
        return seqs.length > 0 ? Math.max(...seqs) : null;
      })
      .catch(() => null);

  /**
   * Counts the changes up to `seq` as told, once the saved values have been read again, and marks
   * what differs from `before`. A client may have put a value back, so there can be nothing to mark.
   */
  const takeUnheard = (seq: number, before: Configured, now: Configured) => {
    announced.current = Math.max(announced.current, seq);
    setByClient((fields) => [...new Set([...fields, ...configuredChanges(before, now)])]);
  };

  /** The saved values the form has taken in. */
  const taken = () => seen.current;

  return { account, setAccount, setup, setSetup, check, setCheck, domains, setDomains, byClient, takeSaved, taken, filled, seenDomains, clientChangeUnheard, takeUnheard };
}

/** What the last look at the project's issues found, or why it couldn't look. */
function IssueWatchLine({ watch, looking }: { watch: IssueWatch | null; looking: boolean }) {
  const { t } = useTranslation();
  if (looking) return <p className="text-[12px] text-muted">{t("projects.issueWatch.looking")}</p>;
  if (!watch) return null;
  const problem = watch.problem;
  const failed = watch.commentFailed;
  return (
    <div className="space-y-0.5 text-[12px] leading-snug">
      {problem?.kind === "labelMissing" && (
        <p className="flex gap-1.5 text-warning">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          {t("projects.issueWatch.labelMissing", { label: watch.label })}
        </p>
      )}
      {problem?.kind === "github" && <p className="text-danger">{t("projects.issueWatch.github", { detail: problem.detail })}</p>}
      {problem?.kind === "ghMissing" && <p className="text-danger">{t("reasons.ghMissing")}</p>}
      {(problem?.kind === "noAccount" || problem?.kind === "noRepo") && <p className="text-warning">{t(`projects.issueWatch.${problem.kind}`)}</p>}
      {(!problem || problem.kind === "labelMissing") && (
        <p className="text-muted">
          {t("projects.issueWatch.done", { when: formatRelative(watch.at), open: watch.open })}
          {watch.brought > 0 && ` ${t("projects.issueWatch.brought", { count: watch.brought })}`}
          {watch.removed > 0 && ` ${t("projects.issueWatch.removed", { count: watch.removed })}`}
        </p>
      )}
      {failed && <p className="text-warning">{t("projects.issueWatch.commentFailed", { number: failed.number, detail: failed.detail })}</p>}
    </div>
  );
}

/**
 * Which section the page is on, for the index to mark, and the way to jump to one. A section is
 * the current one once its top passes under the header, which stays at the top of the page.
 */
function useSections(form: RefObject<HTMLDivElement | null>, header: RefObject<HTMLDivElement | null>) {
  const [active, setActive] = useState<SectionId>(SECTIONS[0]);
  const section = (id: SectionId) => form.current?.querySelector<HTMLElement>(`[data-section="${id}"]`) ?? null;
  // The section a click went to and where the page stopped for it. In a tall window the page ends
  // before a late section gets under the header, and the index still has to mark the one chosen.
  const landed = useRef<{ id: SectionId; top: number } | null>(null);

  useEffect(() => {
    const scroller = form.current?.closest("main");
    if (!scroller) return;
    const look = () => {
      if (landed.current && Math.abs(scroller.scrollTop - landed.current.top) < 2) return setActive(landed.current.id);
      landed.current = null;
      const line = (header.current?.getBoundingClientRect().bottom ?? 0) + 8;
      // At the end of the page the last sections never get as far up as the header.
      const atEnd = scroller.scrollTop > 0 && scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 2;
      const passed = SECTIONS.filter((id) => (section(id)?.getBoundingClientRect().top ?? Infinity) <= line);
      setActive(atEnd ? SECTIONS[SECTIONS.length - 1] : (passed.at(-1) ?? SECTIONS[0]));
    };
    // The page may already be scrolled: choosing another project starts the form over where it was.
    if (scroller.scrollTop > 0) look();
    scroller.addEventListener("scroll", look, { passive: true });
    return () => scroller.removeEventListener("scroll", look);
  }, []);

  const goTo = (id: SectionId) => {
    const target = section(id);
    const scroller = form.current?.closest("main");
    if (!target || !scroller) return;
    // Under the header where it rests once the page has scrolled, which is higher than where it starts.
    const under = scroller.getBoundingClientRect().top + (header.current?.offsetHeight ?? 0);
    if (id === SECTIONS[0]) scroller.scrollTo({ top: 0 });
    else scroller.scrollBy({ top: target.getBoundingClientRect().top - under });
    landed.current = { id, top: scroller.scrollTop };
    setActive(id);
    target.focus({ preventScroll: true });
  };

  return { active, goTo };
}

function ProjectForm({ project, saved, onSaved }: { project: Project; saved: boolean; onSaved: () => void }) {
  const { t, i18n } = useTranslation();
  const refreshOverview = useApp((s) => s.refreshOverview);
  const { account, setAccount, setup, setSetup, check, setCheck, domains, setDomains, byClient, takeSaved, taken, filled, seenDomains, clientChangeUnheard, takeUnheard } = useSavedConfiguration(project);
  const [paths, setPaths] = useState(project.allowedRead.join("\n"));
  const [services, setServices] = useState<Service[]>(project.services);
  const [env, setEnv] = useState(project.env.map((v) => `${v.name}=${v.value}`).join("\n"));
  const [workflow, setWorkflow] = useState(project.defaultWorkflow ?? FULL_WORKFLOW);
  const [sensitive, setSensitive] = useState(project.sensitivePaths.join("\n"));
  const [localhost, setLocalhost] = useState(project.allowLocalhost);
  const [label, setLabel] = useState(project.issueLabel ?? "");
  const [watch, setWatch] = useState<IssueWatch | null>(null);
  const [looking, setLooking] = useState(false);
  const [workflows, setWorkflows] = useState<Workflow[] | null>(null);
  const [ciGap, setCiGap] = useState<CiGap | null>(null);
  const [status, setStatus] = useState<GithubStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const latestRequest = useRef(0);
  const [removing, setRemoving] = useState(false);
  const openSetup = useApp((s) => s.openProjectSetup);
  const form = useRef<HTMLDivElement>(null);
  const header = useRef<HTMLDivElement>(null);
  const { active, goTo } = useSections(form, header);
  const selected = account === NONE ? null : account;
  // Where the commands' programs are, for the person to add what isn't listed yet.
  const found = useToolFolders(project.id, setup, check);
  const offered = foldersToOffer(found ?? [], lines(paths));

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

  // Against the saved check. A hint, so one that can't be had shows nothing.
  useEffect(() => {
    let current = true;
    api
      .projectCiGap(project.id)
      .then((gap) => current && setCiGap(gap))
      .catch(() => current && setCiGap(null));
    return () => {
      current = false;
    };
  }, [project.id, project.setupCommand, project.checkCommand]);

  // What the last look at the issues found, or, when none has for this label, what a look now
  // finds. Keyed by the saved label, so saving one looks even if this form starts over meanwhile.
  useEffect(() => {
    let current = true;
    api
      .issuesWatch(project.id)
      .then((found) => {
        if (!current) return;
        setWatch(found);
        if (found || !project.issueLabel) return;
        setLooking(true);
        api
          .issuesLook(project.id)
          .then((looked) => current && setWatch(looked))
          .catch((e) => current && setError(errorText(t, e)))
          .finally(() => current && setLooking(false));
      })
      .catch(() => current && setWatch(null));
    return () => {
      current = false;
      setLooking(false);
    };
  }, [project.id, project.issueLabel, project.githubAccount, t]);

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
      // A client may have changed the account or commands since the form took them in. Those are
      // shown first rather than written over unseen, and saving again is the person's call.
      const before = taken();
      const from = filled();
      const unheard = await clientChangeUnheard();
      if (unheard !== null) {
        await refreshOverview();
        const fresh = useApp.getState().overview.projects.find((p) => p.id === project.id);
        if (fresh) takeUnheard(unheard, before, fresh);
        setError(t("projects.notSaved"));
        return;
      }
      await api.projectUpdate(project.id, {
        // So that the box doesn't write over a domain the person allowed from a run since it was
        // filled, which the app may not have heard of yet.
        domainsSeen: await seenDomains(from),
        githubAccount: account === NONE ? null : account,
        setupCommand: setup,
        checkCommand: check,
        allowedDomains: lines(domains),
        allowedRead: lines(paths),
        services,
        env: parsed.env,
        defaultWorkflow: workflow === FULL_WORKFLOW ? null : workflow,
        sensitivePaths: lines(sensitive),
        allowLocalhost: localhost,
        issueLabel: label.trim() || null,
      });
      await refreshOverview();
      const now = useApp.getState().overview.projects.find((p) => p.id === project.id);
      if (now) takeSaved({ account, setup, check, domains }, now);
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
  const ciMissing = ciGap ? ciCommandsLeft(check, ciGap.commands) : [];
  const ciLeftOut = ciGap ? ciLeftOutLeft(check, ciGap.leftOut) : [];
  const removeProject = () => setRemoving(true);
  // Of what's saved, like the list beside it: the form may hold a command nobody saved yet.
  const missing = setupMissing(project);
  const githubNotices = [info?.defaultBranchProtected === false && t("settings.githubUnprotected", { branch: info.defaultBranch }), status && status.excessScopes.length > 0 && t("settings.githubScopes", { scopes: status.excessScopes.join(", ") })].filter((text): text is string => Boolean(text));
  const addService = (
    <>
      <Button onClick={() => setServices([...services, { type: "bridge", name: "db", port: 5432, socket: ".s.PGSQL.5432" }])}>
        <Plus size={13} />
        {t("settings.addBridge")}
      </Button>
      <Button onClick={() => setServices([...services, { type: "command", name: "db", start: "", socket: null }])}>
        <Plus size={13} />
        {t("settings.addCommand")}
      </Button>
    </>
  );
  return (
    <div ref={form}>
      {/* Stays at the top while the form scrolls, so the project's name and the way to each section are never out of reach. The padding hides what scrolls past above it. */}
      <div ref={header} className="sticky -top-5 z-10 -mt-5 bg-bg pt-5 pb-3">
        <Card className="px-5 pt-4">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <h2 className="text-[16px] font-semibold">{project.name}</h2>
            <span className={cx("inline-flex items-center gap-1 text-[12px] font-medium", missing.length > 0 ? "text-warning" : "text-success")}>
              {missing.length > 0 ? <AlertTriangle size={13} className="shrink-0" /> : <Check size={13} className="shrink-0" />}
              {t(missing.length > 0 ? "projects.needsSetup" : "projects.ready")}
            </span>
          </div>
          <p className="selectable mt-0.5 font-mono text-[11.5px] break-all text-muted">
            {project.path}
            {project.githubRepo && ` · ${project.githubRepo}`}
          </p>
          {missing.length > 0 && (
            <div className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-2 rounded-md border border-border-strong bg-inset px-3 py-2">
              <p className="min-w-0 flex-1 basis-64 text-[12px] leading-snug">{missing.map((what) => t(`projects.missing.${what}`)).join(" ")}</p>
              <Button onClick={() => openSetup({ projectId: project.id, added: null, returnToNewRun: false })}>{t("projects.finishSetup")}</Button>
            </div>
          )}
          <nav aria-label={t("projects.sections")} className="mt-3 flex flex-wrap gap-x-5 border-t border-border">
            {SECTIONS.map((id) => (
              <button
                key={id}
                type="button"
                aria-current={active === id ? "location" : undefined}
                onClick={() => goTo(id)}
                className={cx("border-b-2 pt-2 pb-1.5 text-[12.5px] font-medium transition-colors", active === id ? "border-accent text-fg" : "border-transparent text-muted hover:text-fg")}
              >
                {t(SECTION_TITLES[id])}
              </button>
            ))}
          </nav>
        </Card>
      </div>

      <div className="space-y-3">
        <Section id="commands" title={t(SECTION_TITLES.commands)}>
          <Field label={t("settings.setupCommand")} hint={t("settings.setupCommandHint")}>
            <input value={setup} onChange={(e) => setSetup(e.target.value)} placeholder={t("settings.setupCommandPlaceholder")} className={`${inputClass} font-mono`} />
          </Field>
          <div>
            <Field label={t("settings.checkCommand")} hint={t("settings.checkCommandHint")}>
              <input value={check} onChange={(e) => setCheck(e.target.value)} placeholder={t("settings.checkCommandPlaceholder")} className={`${inputClass} font-mono`} />
            </Field>
            {(ciMissing.length > 0 || ciLeftOut.length > 0) && (
              <div className="mt-2.5 divide-y divide-border rounded-md border border-border-strong bg-inset px-3">
                {ciMissing.length > 0 && (
                  <div className="flex flex-wrap items-start gap-x-4 gap-y-2 py-2.5">
                    <div className="min-w-0 flex-1 basis-72">
                      <p className="flex gap-1.5 text-[12.5px] text-warning">
                        <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                        {t("projects.ciMissing")}
                      </p>
                      <CommandList commands={ciMissing} />
                      {needsNode(ciMissing) && <p className="mt-1 pl-5 text-[12px] text-muted">{t("projects.pyrightNode")}</p>}
                    </div>
                    <Button onClick={() => setCheck(ciMissing.reduce(withCheckOption, check))}>
                      <Plus size={13} />
                      {t("projects.ciMissingAdd")}
                    </Button>
                  </div>
                )}
                {ciLeftOut.length > 0 && (
                  <div className="py-2.5">
                    <p className="text-[12px] text-muted">{t("projects.ciLeftOut")}</p>
                    <ul className="mt-1 space-y-0.5 text-[11.5px] text-muted">
                      {ciLeftOut.map((left) => (
                        <li key={left.command} className="break-all">
                          <span className="selectable font-mono text-fg">{left.command}</span>
                          <span> · {t(`projects.ciWhy.${left.why}`)}</span>
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
              </div>
            )}
          </div>
        </Section>

        <Section
          id="github"
          title={t(SECTION_TITLES.github)}
          aside={
            status?.repo ? (
              <>
                <span className="selectable font-mono text-[12px]">{status.repo}</span>
                {info && (
                  <span className="text-[12px] text-muted">
                    {t("settings.githubPermission", { permission: info.permission })} · {t("settings.githubDefault", { branch: info.defaultBranch })}
                  </span>
                )}
              </>
            ) : (
              <span className="text-[12px] text-muted">{t("settings.githubNoRepo")}</span>
            )
          }
        >
          <SettingRow label={t("settings.githubAccount")} hint={t("settings.githubHint")}>
            <select value={account} onChange={(e) => setAccount(e.target.value)} className={inputClass}>
              <option value={NONE}>{t("settings.githubNone")}</option>
              {status?.accounts.map((a) => (
                <option key={a.login} value={a.login}>
                  {a.login}
                  {a.active ? ` (${t("settings.githubActive")})` : ""}
                </option>
              ))}
            </select>
          </SettingRow>
          {githubNotices.length > 0 && (
            <ul className="space-y-2 rounded-md border border-border-strong bg-inset px-3 py-2.5">
              {githubNotices.map((text) => (
                <li key={text} className="flex gap-1.5 text-[12px] leading-snug text-warning">
                  <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                  {text}
                </li>
              ))}
            </ul>
          )}
          {status?.error && !needsAccess && <p className="text-[12px] text-danger">{reasonText(t, status.error)}</p>}
          {checkError && <p className="text-[12px] text-danger">{checkError}</p>}
          {needsAccess && repo && (
            <div role="alert" className="rounded-md border border-border-strong bg-inset px-3 py-2.5">
              <p className="flex gap-1.5 text-[12.5px] leading-snug text-warning">
                <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                {t(access === "none" ? "settings.githubNoAccess" : "settings.githubReadOnly", { account: selected })}
              </p>
              <div className="mt-2 flex flex-wrap gap-2 pl-5">
                <Button onClick={() => openOnGithub(repoAccessUrl(repo))}>{t("settings.githubGiveAccess")}</Button>
                <Button onClick={() => openOnGithub(repoInvitationUrl(repo))}>{t("settings.githubAcceptInvite")}</Button>
                <Button variant="ghost" disabled={checking} onClick={load}>
                  {t(checking ? "settings.githubChecking" : "settings.githubCheckAgain")}
                </Button>
              </div>
              <p className={cx("mt-2 pl-5", fieldHintClass)}>{t("settings.githubAccessHint", { account: selected })}</p>
            </div>
          )}
          {project.githubRepo && (
            <div className="border-t border-border pt-4">
              <SettingRow label={t("projects.issueLabel")} hint={t("projects.issueLabelHint")} below={<IssueWatchLine watch={watch?.label === label.trim() ? watch : null} looking={looking} />}>
                <input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={50} placeholder={t("projects.issueLabelPlaceholder")} className={inputClass} />
              </SettingRow>
            </div>
          )}
          {status && status.error?.code !== "ghMissing" && (
            <div className="border-t border-border pt-4">
              <div className="flex flex-wrap gap-2">
                {repo && !needsAccess && <Button onClick={() => openOnGithub(repoAccessUrl(repo))}>{t("settings.githubManageAccess")}</Button>}
                <Button onClick={connectAccount}>{t("settings.githubConnect")}</Button>
              </div>
              <p className={cx("mt-1.5", fieldHintClass)}>{t(repo && !needsAccess ? "settings.githubLinksHint" : "settings.githubConnectHint")}</p>
            </div>
          )}
        </Section>

        <Section id="workflow" title={t(SECTION_TITLES.workflow)}>
          <SettingRow
            label={t("settings.defaultWorkflow")}
            hint={t("settings.defaultWorkflowHint")}
            below={
              workflowGone && (
                <p className="flex gap-1.5 text-[12px] leading-snug text-warning">
                  <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                  {t("reasons.defaultWorkflowMissing")}
                </p>
              )
            }
          >
            <select value={workflow} disabled={workflows === null} onChange={(e) => setWorkflow(e.target.value)} className={inputClass}>
              {workflows === null && <option value={workflow}>{t("settings.githubChecking")}</option>}
              {workflowGone && <option value={workflow}>{t("settings.defaultWorkflowGone")}</option>}
              {workflows?.map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          </SettingRow>
          {/* Here and not under Access: these decide how much review a change gets, not what the sandbox can reach. */}
          <Field label={t("settings.sensitive")} hint={t("settings.sensitiveHint")}>
            <textarea rows={3} value={sensitive} onChange={(e) => setSensitive(e.target.value)} className={`${inputClass} resize-y font-mono`} />
          </Field>
        </Section>

        <Section id="access" title={t(SECTION_TITLES.access)} hint={t("projects.sectionAccessHint")}>
          <div className="grid gap-4 @xl:grid-cols-2">
            <Field label={t("settings.domains")} hint={t("settings.domainsHint")}>
              <textarea rows={3} value={domains} onChange={(e) => setDomains(e.target.value)} className={`${inputClass} resize-y font-mono`} />
            </Field>
            <div>
              <Field label={t("settings.readPaths")} hint={t("settings.readPathsHint")}>
                <textarea rows={3} value={paths} onChange={(e) => setPaths(e.target.value)} className={`${inputClass} resize-y font-mono`} />
              </Field>
              <FolderOffers folders={offered} onAdd={(folder) => setPaths(withFolders(lines(paths), [folder]).join("\n"))} />
            </div>
          </div>
          <label className="flex items-start gap-2.5 border-t border-border pt-4">
            <input type="checkbox" className="mt-0.5 size-3.5 shrink-0 accent-accent" checked={localhost} onChange={(e) => setLocalhost(e.target.checked)} />
            <span className="min-w-0">
              <span className={fieldLabelClass}>{t("settings.localhost")}</span>
              <span className={cx("mt-0.5", fieldHintClass)}>{t("settings.localhostHint")}</span>
            </span>
          </label>
        </Section>

        <Section id="services" title={t("projects.sectionServices")} hint={t("settings.servicesHint")}>
          {services.length === 0 ? (
            <div className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-md border border-dashed border-border-strong p-3">
              <p className="min-w-0 flex-1 basis-40 text-[12px] text-muted">{t("settings.servicesEmpty")}</p>
              {addService}
            </div>
          ) : (
            <>
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
              <div className="flex flex-wrap gap-2">{addService}</div>
            </>
          )}
          <div className="border-t border-border pt-4">
            <Field label={t("settings.env")} hint={t("settings.envHint")}>
              <textarea rows={3} value={env} onChange={(e) => setEnv(e.target.value)} placeholder={ENV_EXAMPLE} className={`${inputClass} resize-y font-mono`} />
            </Field>
          </div>
        </Section>
      </div>

      {/* Stays in view while the form scrolls, so saving is never a search. One Save for every section. */}
      <div className="sticky -bottom-5 z-10 bg-bg pt-3 pb-5">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg border border-border-strong bg-elevated px-5 py-2.5 shadow-[0_-6px_16px_-12px_rgb(0_0_0/0.35)]">
          {byClient.length > 0 && <ProjectChangedNotice title={t("projects.clientChanged")} saved={project} fields={byClient} kept={configuredChanges(project, { githubAccount: account === NONE ? null : account, setupCommand: setup || null, checkCommand: check || null, allowedDomains: lines(domains) })} />}
          <Button variant="primary" onClick={() => void save()}>
            {t("settings.save")}
          </Button>
          {saved && (
            <span className="inline-flex items-center gap-1 text-[12px] text-success">
              <Check size={13} className="shrink-0" />
              {t("settings.saved")}
            </span>
          )}
          {error ? (
            <p role="alert" className="min-w-0 flex-1 text-[12px] leading-snug text-danger">
              {error}
            </p>
          ) : (
            <p className="ml-auto text-[12px] text-muted">{t("projects.savesAll")}</p>
          )}
        </div>
      </div>

      <Card className="flex flex-wrap items-center gap-3 border-danger/40 px-5 py-4">
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

function CommandList({ commands }: { commands: string[] }) {
  return (
    <ul className="mt-1 space-y-0.5 pl-5 font-mono text-[11.5px]">
      {commands.map((command) => (
        <li key={command} className="selectable break-all">
          {command}
        </li>
      ))}
    </ul>
  );
}

/** One part of a project's settings, in its own card. `aside` is what the title needs beside it, like the repo. */
function Section({ id, title, hint, aside, children }: { id: SectionId; title: string; hint?: string; aside?: ReactNode; children: ReactNode }) {
  const titleId = useId();
  return (
    // Takes focus when the index above jumps here, so Tab goes on from this section.
    <Card data-section={id} aria-labelledby={titleId} tabIndex={-1} className="@container space-y-4 p-5 outline-none">
      <div>
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <h3 id={titleId} className="text-[13px] font-semibold">
            {title}
          </h3>
          {aside}
        </div>
        {hint && <p className="mt-0.5 text-[12px] leading-snug text-muted">{hint}</p>}
      </div>
      {children}
    </Card>
  );
}

function ProjectItem({ project, selected, onSelect }: { project: Project; selected: boolean; onSelect: () => void }) {
  const { t } = useTranslation();
  const pending = needsSetup(project);
  return (
    <li>
      <button type="button" aria-current={selected ? "true" : undefined} onClick={onSelect} className={cx("flex w-full items-start gap-2 rounded-md px-2.5 py-1.5 text-left transition-colors hover:bg-hover", selected && "bg-active")}>
        <span className="flex min-w-0 flex-1 flex-col gap-0.5">
          <span className="truncate text-[13px] font-medium">{project.name}</span>
          <span className="truncate font-mono text-[11.5px] text-muted">{project.githubRepo ?? t("projects.noRepo")}</span>
          {/* Only what's missing gets a line of its own: a list where every project says "Set up" says nothing. */}
          {pending && (
            <span className="flex items-center gap-1 text-[11.5px] text-warning">
              <AlertTriangle size={11} className="shrink-0" />
              {t("projects.needsSetup")}
            </span>
          )}
        </span>
        {!pending && (
          <>
            <Check size={13} aria-hidden className="mt-1 shrink-0 text-success" />
            <span className="sr-only">{t("projects.ready")}</span>
          </>
        )}
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
  // Held from then on, so that a project a client adds ahead of it doesn't take its place and
  // drop what the person hasn't saved.
  useEffect(() => {
    if (selected && selected.id !== chosen) setChosen(selected.id);
  }, [selected?.id, chosen]);
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
          <p className="mt-4 text-[12.5px] text-muted">{t("projects.none")}</p>
        ) : (
          <div className="mt-5 grid items-start gap-x-4 gap-y-5 lg:grid-cols-[240px_minmax(0,1fr)]">
            <div className="space-y-2 lg:sticky lg:top-0">
              {projects.length >= FILTER_FROM && (
                <label className="relative block">
                  <Search size={13} className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-muted" />
                  <input type="search" aria-label={t("projects.filter")} placeholder={t("projects.filterPlaceholder")} value={filter} onChange={(e) => setFilter(e.target.value)} className={`${inputClass} bg-panel pl-8`} />
                </label>
              )}
              <div aria-label={t("projects.title")} role="group">
                <ul className="space-y-0.5">
                  {shown.map((project) => (
                    <ProjectItem key={project.id} project={project} selected={project.id === selected?.id} onSelect={() => setChosen(project.id)} />
                  ))}
                </ul>
                {shown.length === 0 && <p className="px-2.5 py-2 text-[12px] text-muted">{t("projects.noMatch")}</p>}
              </div>
            </div>
            {selected && <ProjectForm key={projectCardKey(selected)} project={selected} saved={savedId === selected.id} onSaved={() => showSaved(selected.id)} />}
          </div>
        )}
      </div>
    </main>
  );
}
