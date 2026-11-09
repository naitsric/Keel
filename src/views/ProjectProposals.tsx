import { FolderPlus, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type ProjectProposal } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Modal } from "../ui/primitives";

/**
 * Projects an assistant proposed over MCP. The bar shows on every page, since one can arrive on any
 * of them, and only opens the review. Adding takes a click in there, and the focus starts on "Not
 * now", so a stray Enter can't add anything. From there on it's the same as adding one by hand.
 */
export function ProjectProposals() {
  const { t, i18n } = useTranslation();
  const waiting = useApp((s) => s.overview.projectProposals);
  const revision = useApp((s) => s.revision);
  const [proposals, setProposals] = useState<ProjectProposal[]>([]);
  const [reviewing, setReviewing] = useState<ProjectProposal | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!waiting) {
      setProposals([]);
      setReviewing(null);
      setError(null);
      return;
    }
    let current = true;
    api
      .projectProposalsList()
      .then((list) => {
        if (!current) return;
        setProposals(list);
        // Added by hand while it was open, say: there's nothing left to decide on.
        setReviewing((open) => (open && list.some((proposal) => proposal.id === open.id) ? open : null));
      })
      .catch((e) => {
        if (current) setError(errorText(t, e));
      });
    return () => {
      current = false;
    };
  }, [waiting, revision, t]);

  const review = (proposal: ProjectProposal) => {
    setError(null);
    setReviewing(proposal);
  };

  const add = async (proposal: ProjectProposal) => {
    setError(null);
    setBusy(true);
    try {
      const added = await api.projectProposalAccept(proposal.id, i18n.language);
      await useApp.getState().refreshOverview();
      setReviewing(null);
      useApp.getState().openProjectSetup({ projectId: added.project.id, added, returnToNewRun: false });
    } catch (e) {
      setError(errorText(t, e));
    } finally {
      setBusy(false);
    }
  };

  const discard = async (proposal: ProjectProposal) => {
    setError(null);
    setBusy(true);
    try {
      await api.projectProposalDiscard(proposal.id);
      setReviewing(null);
      await useApp.getState().refreshOverview();
    } catch (e) {
      setError(errorText(t, e));
    } finally {
      setBusy(false);
    }
  };

  // The oldest, so one arriving later doesn't change what the bar says under the pointer.
  const next = proposals[proposals.length - 1];
  const label = "text-[12px] font-medium text-muted";
  return (
    <>
      {(next || error) && (
        <div role="status" className="flex shrink-0 items-center gap-2 border-b border-accent bg-inset px-3 py-1.5 text-[12.5px]">
          <FolderPlus size={14} className="shrink-0 text-muted" />
          {next && <span className="min-w-0 flex-1 truncate">{t("projects.proposalBanner", { count: proposals.length, more: proposals.length - 1, client: next.proposedBy, name: next.name })}</span>}
          {!reviewing && error && <span className="min-w-0 flex-1 truncate text-danger">{error}</span>}
          {next && <Button onClick={() => review(next)}>{t("projects.proposalReview")}</Button>}
        </div>
      )}
      <Modal
        open={reviewing !== null}
        onOpenChange={(open) => {
          if (!open) setReviewing(null);
        }}
        title={t("projects.proposalTitle", { client: reviewing?.proposedBy ?? "" })}
      >
        {reviewing && (
          <div className="space-y-3">
            <dl className="space-y-2 text-[12.5px]">
              <div>
                <dt className={label}>{t("projects.proposalName")}</dt>
                <dd className="selectable break-words">{reviewing.name}</dd>
              </div>
              <div>
                <dt className={label}>{t("projects.proposalFolder")}</dt>
                <dd className="selectable font-mono text-[11.5px] break-all">{reviewing.path}</dd>
              </div>
              <div>
                <dt className={label}>{t("projects.proposalGithub")}</dt>
                <dd className="selectable break-words">{reviewing.githubRepo ?? t("projects.proposalNoGithub")}</dd>
              </div>
            </dl>
            {reviewing.gone ? (
              <p role="alert" className="flex gap-1.5 text-[12.5px] font-medium text-warning">
                <TriangleAlert size={14} className="mt-0.5 shrink-0" />
                <span className="min-w-0">{t("projects.proposalGone")}</span>
              </p>
            ) : (
              <p className="text-[12.5px] text-muted">{t(reviewing.returning ? "projects.proposalReturning" : "projects.proposalMap")}</p>
            )}
            {error && (
              <p role="alert" className="text-[12px] text-danger">
                {error}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <Button autoFocus onClick={() => setReviewing(null)}>
                {t("projects.proposalNotNow")}
              </Button>
              <Button disabled={busy} onClick={() => void discard(reviewing)}>
                {t("projects.proposalDiscard")}
              </Button>
              <Button variant="primary" disabled={busy || reviewing.gone} onClick={() => void add(reviewing)}>
                {t("projects.proposalAdd")}
              </Button>
            </div>
          </div>
        )}
      </Modal>
    </>
  );
}
