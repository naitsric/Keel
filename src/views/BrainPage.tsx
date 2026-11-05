import { RefreshCw } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { openRun } from "../commands/runs";
import { api, type BrainNote, type BrainView, type DecisionView, type NoteView } from "../lib/api";
import { formatDateTime, formatNumber } from "../lib/format";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass } from "../ui/primitives";
import { AddProject } from "./AddProject";
import { ModelField } from "./ModelField";

export function Paths({ paths, commit }: { paths: string[]; commit?: string | null }) {
  if (paths.length === 0) return null;
  return (
    <p className="selectable mt-0.5 font-mono text-[11px] break-words text-subtle">
      {paths.join(", ")}
      {commit && ` @ ${commit.slice(0, 7)}`}
    </p>
  );
}

/** A proposal waiting for a person: here, and in the summary of the run that made it. */
export function ProposalRow({ note, onDone, onError }: { note: BrainNote; onDone: () => void; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const act = async (action: () => Promise<void>) => {
    try {
      await action();
      onDone();
    } catch (e) {
      onError(errorText(t, e));
    }
  };
  return (
    <li className="rounded-md border border-border p-3">
      <p className="selectable text-[12.5px]">{note.text}</p>
      {note.evidence && (
        <p className="selectable mt-1 text-[12px] text-muted">
          <span className="font-medium">{t("brain.evidence")}:</span> {note.evidence}
        </p>
      )}
      <Paths paths={note.paths} />
      <div className="mt-2 flex gap-2">
        <Button onClick={() => void act(() => api.brainAccept(note.seq))}>{t("brain.accept")}</Button>
        <Button variant="ghost" onClick={() => void act(() => api.brainDismiss(note.seq))}>
          {t("brain.dismiss")}
        </Button>
      </div>
    </li>
  );
}

function NoteRow({ note, onRetire }: { note: NoteView; onRetire: () => void }) {
  const { t } = useTranslation();
  return (
    <li className="flex items-start gap-3 rounded-md border border-border p-2.5">
      <div className="min-w-0 flex-1">
        <p className="selectable text-[12.5px]">{note.text}</p>
        <Paths paths={note.paths} commit={note.commitSha} />
        {note.stale && <p className="mt-0.5 text-[11.5px] text-warning">{t("brain.stale")}</p>}
        {!note.included && <p className="mt-0.5 text-[11.5px] text-warning">{t("brain.leftOut")}</p>}
      </div>
      <Button variant="ghost" onClick={onRetire}>
        {t("brain.retire")}
      </Button>
    </li>
  );
}

function DecisionRow({ decision, onRetire }: { decision: DecisionView; onRetire: () => void }) {
  const { t } = useTranslation();
  return (
    <li className="flex items-start gap-3 rounded-md border border-border p-2.5">
      <div className="min-w-0 flex-1">
        <p className="selectable text-[12.5px]">{decision.text}</p>
        <p className="mt-0.5 text-[11px] text-subtle">{formatDateTime(decision.at)}</p>
        {!decision.included && <p className="mt-0.5 text-[11.5px] text-warning">{t("brain.leftOut")}</p>}
      </div>
      <Button variant="ghost" onClick={onRetire}>
        {t("brain.retire")}
      </Button>
    </li>
  );
}

function Section({ title, hint, children }: { title: string; hint: string; children: React.ReactNode }) {
  return (
    <Card className="p-4">
      <h2 className="text-[13px] font-semibold">{title}</h2>
      <p className="mt-0.5 mb-2.5 text-[12px] text-muted">{hint}</p>
      {children}
    </Card>
  );
}

const kilobytes = (bytes: number) => formatNumber(Math.round((bytes / 1024) * 10) / 10);

export function BrainPage() {
  const { t, i18n } = useTranslation();
  const projects = useApp((s) => s.overview.projects);
  const revision = useApp((s) => s.revision);
  const refreshOverview = useApp((s) => s.refreshOverview);
  const wanted = useApp((s) => s.brainProjectId);
  const [projectId, setProjectId] = useState(wanted ?? "");
  const [brain, setBrain] = useState<BrainView | null>(null);
  const [model, setModel] = useState("sonnet");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (projects.length > 0 && !projects.some((p) => p.id === projectId)) setProjectId(projects[0].id);
  }, [projects, projectId]);

  const load = useCallback(async () => {
    if (!projectId) return;
    setBrain(await api.brainGet(projectId));
  }, [projectId]);

  useEffect(() => {
    load().catch((e) => setError(errorText(t, e)));
  }, [load, revision, t]);

  const changed = () => {
    setError(null);
    void load();
    void refreshOverview();
  };
  const act = (action: () => Promise<unknown>) => {
    action().then(changed, (e) => setError(errorText(t, e)));
  };
  const generate = () => act(() => api.brainGenerateMap(projectId, model, i18n.language));

  const staleCount = brain?.map.filter((n) => n.stale).length ?? 0;
  const over = brain ? brain.used > brain.limit : false;

  return (
    <>
      <main className="flex-1 overflow-y-auto p-5">
        {/* The top bar already says which page this is. */}
        <h1 className="sr-only">{t("brain.title")}</h1>
        <p className="text-[12.5px] text-muted">{t("brain.subtitle")}</p>
        {projects.length === 0 ? (
          <div className="mt-4">
            <p className="text-[12.5px] text-subtle">{t("brain.noProjects")}</p>
            <AddProject className="mt-2" variant="primary" />
          </div>
        ) : (
          <div className="mt-4 max-w-3xl space-y-4">
            <div className="flex flex-wrap items-end gap-4">
              <div className="w-64">
                <Field label={t("brain.project")}>
                  <select value={projectId} onChange={(e) => setProjectId(e.target.value)} className={inputClass}>
                    {projects.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                  </select>
                </Field>
              </div>
              {brain && (
                <p className={cx("pb-1.5 text-[12px]", over ? "text-warning" : "text-muted")}>
                  {t("brain.usage", { used: kilobytes(brain.used), limit: kilobytes(brain.limit) })}
                  {over && `. ${t("brain.overLimit")}`}
                </p>
              )}
            </div>
            {error && (
              <p role="alert" className="text-[12.5px] text-danger">
                {error}
              </p>
            )}
            {brain && brain.proposals.length > 0 && (
              <Section title={t("brain.proposals")} hint={t("brain.proposalsHint")}>
                <ul className="space-y-2">
                  {brain.proposals.map((note) => (
                    <ProposalRow key={note.seq} note={note} onDone={changed} onError={setError} />
                  ))}
                </ul>
              </Section>
            )}
            {brain && (
              <Section title={t("brain.map")} hint={t("brain.mapHint")}>
                {brain.mapRun ? (
                  <p className="mb-2.5 text-[12.5px] text-muted">
                    {t("brain.mapRunning")}{" "}
                    <button type="button" className="text-accent hover:underline" onClick={() => brain.mapRun && openRun(brain.mapRun)}>
                      {t("brain.mapRunningLink")}
                    </button>
                  </p>
                ) : (
                  brain.map.length === 0 && <p className="mb-2.5 text-[12.5px] text-subtle">{t("brain.mapEmpty")}</p>
                )}
                {staleCount > 0 && <p className="mb-2.5 text-[12.5px] text-warning">{t("brain.staleCount", { count: staleCount })}</p>}
                {brain.map.length > 0 && (
                  <ul className="mb-3 space-y-1.5">
                    {brain.map.map((note) => (
                      <NoteRow key={note.seq} note={note} onRetire={() => act(() => api.brainRetire(note.seq))} />
                    ))}
                  </ul>
                )}
                <div className="flex flex-wrap items-end gap-2">
                  <div className="w-44">
                    <ModelField label={t("newRun.model")} value={model} onChange={setModel} />
                  </div>
                  <Button disabled={!model || brain.mapRun !== null} onClick={generate}>
                    <RefreshCw size={13} />
                    {brain.map.length === 0 ? t("brain.mapGenerate") : t("brain.mapRegenerate")}
                  </Button>
                </div>
              </Section>
            )}
            {brain && (
              <Section title={t("brain.learnings")} hint={t("brain.learningsHint")}>
                {brain.learnings.length === 0 ? (
                  <p className="text-[12.5px] text-subtle">{t("brain.learningsEmpty")}</p>
                ) : (
                  <ul className="space-y-1.5">
                    {brain.learnings.map((note) => (
                      <NoteRow key={note.seq} note={note} onRetire={() => act(() => api.brainRetire(note.seq))} />
                    ))}
                  </ul>
                )}
              </Section>
            )}
            {brain && (
              <Section title={t("brain.decisions")} hint={t("brain.decisionsHint")}>
                {brain.decisions.length === 0 ? (
                  <p className="text-[12.5px] text-subtle">{t("brain.decisionsEmpty")}</p>
                ) : (
                  <ul className="space-y-1.5">
                    {brain.decisions.map((decision) => (
                      <DecisionRow key={decision.seq} decision={decision} onRetire={() => act(() => api.decisionRetire(decision.seq))} />
                    ))}
                  </ul>
                )}
              </Section>
            )}
            {brain?.prompt && (
              <details className="rounded-lg border border-border bg-surface p-4">
                <summary className="cursor-pointer text-[13px] font-semibold">{t("brain.prompt")}</summary>
                <p className="mt-1 text-[12px] text-muted">{t("brain.promptHint")}</p>
                <pre className="selectable mt-2 max-h-96 overflow-auto rounded bg-inset p-2 font-mono text-[11.5px] whitespace-pre-wrap">{brain.prompt.trim()}</pre>
              </details>
            )}
          </div>
        )}
      </main>
    </>
  );
}
