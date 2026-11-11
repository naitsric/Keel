import { AlertTriangle, ExternalLink, GitPullRequest } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type AttemptState, type CheckedCriterion, type ContractPoint, type JointCard, type JointChange } from "../lib/api";
import { isLane, partText, partWarningText, waitsForText } from "../lib/board";
import { errorText } from "../lib/reason";
import { useApp, type View } from "../lib/store";
import { Button, Modal, StateBadge } from "../ui/primitives";
import { SpecView } from "./SpecView";

/** Its badge: what became of its pull request, or where its run stands, or which column it's in. */
function badgeOf(t: (key: string) => string, card: JointCard): { state: AttemptState; label: string } {
  if (card.prState) return { state: card.prState === "merged" ? "succeeded" : card.prState === "closed" ? "rejected" : "queued", label: t(`board.pr.${card.prState}`) };
  if (card.runState) return { state: card.runState, label: t(`states.${card.runState}`) };
  return { state: "queued", label: t(card.column === "todo" || card.column === "backlog" ? "joint.notStarted" : `board.columns.${card.column}`) };
}

function PartRow({ card, busy, onOpenRun, onOpenPr, onStopWaiting }: { card: JointCard; busy: boolean; onOpenRun: (runId: string) => void; onOpenPr: (url: string) => void; onStopWaiting: (cardId: string) => void }) {
  const { t } = useTranslation();
  const part = card.part;
  const badge = badgeOf(t, card);
  const waiting = part ? waitsForText(t, part) : null;
  return (
    <li className="border-t border-border py-2 first:border-t-0">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        {part && <span className="text-[12px] font-semibold">{partText(t, part)}</span>}
        <span className="min-w-0 truncate text-[12.5px]">{card.project}</span>
        <StateBadge state={badge.state} label={badge.label} />
        <span className="flex-1" />
        {card.prUrl && (
          <Button variant="ghost" className="h-6 px-1 text-[11.5px] text-accent" onClick={() => onOpenPr(card.prUrl as string)}>
            <GitPullRequest size={12} />
            {t("joint.openPr")}
          </Button>
        )}
        {card.runId && (
          <Button variant="ghost" className="h-6 px-1 text-[11.5px]" onClick={() => onOpenRun(card.runId as string)}>
            <ExternalLink size={12} />
            {t("joint.openRun")}
          </Button>
        )}
      </div>
      {waiting && (
        <div className="mt-1 flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
          <span className="min-w-0 flex-1">{waiting}</span>
          {isLane(card.column) && (
            <Button className="h-6 px-1.5 text-[11.5px]" title={t("joint.stopWaitingHint")} disabled={busy} onClick={() => onStopWaiting(card.cardId)}>
              {t("joint.stopWaiting")}
            </Button>
          )}
        </div>
      )}
      {part?.warnings.map((warning) => (
        <p key={`${warning.kind}-${warning.position}`} className="mt-1 flex gap-1.5 text-[11.5px] text-warning">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          {partWarningText(t, warning)}
        </p>
      ))}
    </li>
  );
}

const RESULT_TONES: Record<CheckedCriterion["result"], string> = {
  met: "text-success",
  unmet: "font-medium text-warning",
  unverified: "text-muted",
};

/** A point the parts agree on exactly, and what each part that keeps it found. */
function ContractRow({ point }: { point: ContractPoint }) {
  const { t } = useTranslation();
  return (
    <li data-contract={point.id} className="border-t border-border py-1.5 first:border-t-0">
      <p className="selectable break-words">
        <span className="font-mono text-[12px] text-muted">{point.id}</span> {point.when} → {point.then}
      </p>
      <ul className="mt-0.5 flex flex-wrap gap-x-3 gap-y-0.5 text-[11.5px]">
        {point.parts.map((part) => (
          <li key={part.cardId}>
            {t("joint.contractPart", { position: part.position, project: part.project })}: <span className={part.result ? RESULT_TONES[part.result] : "text-subtle"}>{part.result ? t(`run.verifyResult.${part.result}`) : t(part.verifies ? "joint.notChecked" : "joint.noVerifier")}</span>
          </li>
        ))}
      </ul>
    </li>
  );
}

/**
 * A change across several projects in one place: its parts in order and where each stands, what
 * the parts agree on exactly and what each found of it, what was agreed, and what only works with
 * every part in place. Only here does a person mark that.
 */
export function JointChangeDialog({ changeId, backTo, onClose }: { changeId: string; backTo: View; onClose: () => void }) {
  const { t } = useTranslation();
  const revision = useApp((s) => s.revision);
  const navigate = useApp((s) => s.navigate);
  const [change, setChange] = useState<JointChange | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => api.jointChangeGet(changeId).then(setChange), [changeId]);
  useEffect(() => {
    load().catch((e) => setError(errorText(t, e)));
  }, [load, revision, t]);

  const call = (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    action()
      .then(load)
      .catch((e) => setError(errorText(t, e)))
      .finally(() => setBusy(false));
  };
  const openRun = (runId: string) => {
    onClose();
    navigate({ name: "run", runId, backTo });
  };
  const openPr = (url: string) => {
    api.openGithubUrl(url).catch((e) => setError(errorText(t, e)));
  };

  const agreeing = change?.agreedIn ?? null;
  const where = agreeing ? t(change?.agreed ? "joint.agreedIn" : "joint.notAgreed", { title: agreeing.title, project: agreeing.project }) : t("joint.agreedInGone");

  return (
    <Modal open onOpenChange={(open) => !open && onClose()} title={t("joint.title")} className="w-[640px]">
      {!change ? (
        <p className="text-[12.5px] text-subtle">{error ?? t("joint.loading")}</p>
      ) : (
        <div className="space-y-4 text-[12.5px]">
          <div className="flex flex-wrap items-center gap-2 text-muted">
            <span className="min-w-0 flex-1">{where}</span>
            {agreeing?.runId && (
              <Button variant="ghost" className="h-6 px-1 text-[11.5px]" onClick={() => openRun(agreeing.runId as string)}>
                <ExternalLink size={12} />
                {t("joint.openRun")}
              </Button>
            )}
          </div>
          {change.parts.length === 0 ? (
            <section>
              <h3 className="text-[12px] font-semibold text-muted">{t("joint.projects")}</h3>
              <p className="mt-1">{change.projects.map((project) => project.name).join(", ")}</p>
            </section>
          ) : (
            <section>
              <h3 className="text-[12px] font-semibold text-muted">{t("joint.parts")}</h3>
              <ul className="mt-1">
                {change.parts.map((part) => (
                  <PartRow key={part.cardId} card={part} busy={busy} onOpenRun={openRun} onOpenPr={openPr} onStopWaiting={(cardId) => call(() => api.cardStopWaiting(cardId))} />
                ))}
              </ul>
              {change.readOnly.length > 0 && <p className="mt-1 text-muted">{t("joint.readOnly", { projects: change.readOnly.map((project) => project.name).join(", ") })}</p>}
            </section>
          )}
          {change.contract.length > 0 && (
            <section>
              <h3 className="text-[12px] font-semibold text-muted">{t("joint.contract")}</h3>
              <p className="mt-0.5 text-[11.5px] text-muted">{t("joint.contractHint")}</p>
              <ul className="mt-1">
                {change.contract.map((point) => (
                  <ContractRow key={point.id} point={point} />
                ))}
              </ul>
            </section>
          )}
          {change.checks.length > 0 && (
            <section>
              <h3 className="text-[12px] font-semibold text-muted">{t("joint.together")}</h3>
              <p className="mt-0.5 text-[11.5px] text-muted">{t("joint.togetherHint")}</p>
              <ul className="mt-1.5 space-y-1">
                {change.checks.map((check) => (
                  <li key={check.seq}>
                    <label className="flex items-start gap-2">
                      <input type="checkbox" className="mt-0.5" checked={check.confirmedAt !== null} disabled={busy} onChange={(e) => call(() => api.jointCheckSet(change.id, check.seq, e.target.checked))} />
                      <span className="selectable break-words">{check.text}</span>
                    </label>
                  </li>
                ))}
              </ul>
            </section>
          )}
          {change.agreed && (
            <section>
              <h3 className="mb-1 text-[12px] font-semibold text-muted">{t("joint.agreed")}</h3>
              <div className="rounded-md border border-border bg-inset p-3">
                {/* What's tried together is right above, where it's marked, and so is what the parts agree on exactly, with what each found. */}
                <SpecView spec={{ ...change.agreed, together: [], contract: [] }} />
              </div>
            </section>
          )}
          {error && (
            <p role="alert" className="text-danger">
              {error}
            </p>
          )}
          <div className="flex justify-end">
            <Button onClick={onClose}>{t("joint.close")}</Button>
          </div>
        </div>
      )}
    </Modal>
  );
}
