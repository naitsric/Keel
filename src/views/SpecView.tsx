import { useTranslation } from "react-i18next";
import type { SpecDecided, SpecOutput } from "../lib/api";

/** Whether a line is what the request already decided, word for word, rather than what the agent decided. */
export const fromRequest = (decided: SpecDecided) => decided.taken === "request";

function SpecList({ title, items }: { title: string; items: React.ReactNode[] }) {
  if (items.length === 0) return null;
  return (
    <div>
      <h4 className="text-[12px] font-semibold text-muted">{title}</h4>
      <ul className="selectable mt-1 list-disc space-y-0.5 pl-4 break-words">
        {items.map((item, index) => (
          // Lines of a fixed text: their position is their identity.
          <li key={index}>{item}</li>
        ))}
      </ul>
    </div>
  );
}

/** What a specifier understood, as text to read. Where a change across projects is agreed, also each part and what's tried together. */
export function SpecView({ spec }: { spec: SpecOutput }) {
  const { t } = useTranslation();
  const kinds = { adds: "run.specAdds", changes: "run.specAlters", removes: "run.specRemoves" };
  return (
    <div className="space-y-3 text-[13px] leading-relaxed">
      <p className="selectable break-words">{spec.summary}</p>
      <SpecList
        title={t("run.specParts")}
        items={(spec.parts ?? []).map((part) => (
          <>
            <span className="font-medium">{part.project}</span>: {part.text}
          </>
        ))}
      />
      <SpecList
        title={t("run.specHolds")}
        items={spec.criteria.map((criterion) => (
          <>
            <span className="font-mono text-[12px] text-muted">{criterion.id}</span> {criterion.when} → {criterion.then}
          </>
        ))}
      />
      <SpecList title={t("run.specChanges")} items={spec.changes.map((change) => `${t(kinds[change.kind])}: ${change.text}`)} />
      <SpecList title={t("run.specLimits")} items={spec.limits} />
      <SpecList title={t("run.specTogether")} items={spec.together ?? []} />
      <SpecList
        title={t("run.decidedInRequest")}
        items={spec.decided.filter(fromRequest).map((decided) => (
          <>
            {decided.text}
            {decided.assistant && <span className="block text-[11.5px] text-subtle">{t("run.decidedWrittenBy", { client: decided.assistant })}</span>}
            {!decided.assistant && decided.githubAccount && <span className="block text-[11.5px] text-subtle">{t("run.decidedWrittenWithAccount")}</span>}
          </>
        ))}
      />
      <SpecList
        title={t("run.decidedTitle")}
        items={spec.decided
          .filter((decided) => !fromRequest(decided))
          .map((decided) => (
            <>
              {decided.text}
              {decided.taken === "person" && <span className="block text-[11.5px] text-subtle">{t("run.decidedYouChose")}</span>}
              {decided.taken === "roundsLimit" && <span className="block text-[11.5px] text-warning">{t("run.decidedKeelTook")}</span>}
            </>
          ))}
      />
      <SpecList title={t("run.specAsked")} items={spec.questions.map((question) => `${question.text} ${question.options.map((option) => option.label).join(" / ")}`)} />
      <SpecList title={t("run.specAnswered")} items={(spec.answered ?? []).map((answer) => `${answer.asked} ${answer.answer}`)} />
    </div>
  );
}
