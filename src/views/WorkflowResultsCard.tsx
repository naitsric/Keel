import { useTranslation } from "react-i18next";
import type { Workflow, WorkflowResults } from "../lib/api";
import { formatCost, formatNumber } from "../lib/format";
import { averageRounds, comparedResults, costPerMerged, ENOUGH_FOR_AN_AVERAGE, matchedShare } from "../lib/results";
import { Card } from "../ui/primitives";

/** Workflows side by side, one row per measure, so a new measure is one more line here. */
export function WorkflowResultsCard({ results, workflows }: { results: WorkflowResults[]; workflows: Pick<Workflow, "id" | "name">[] }) {
  const { t } = useTranslation();
  const notEnough = t("workflows.results.notEnough");
  const nothingChecked = t("workflows.results.nothingChecked");
  // A workflow with no verifier never checks anything, so "not enough yet" would be wrong for it.
  const checked = (r: WorkflowResults, count: number) => (r.criteriaMet + r.criteriaUnmet + r.criteriaUnverified === 0 ? nothingChecked : formatNumber(count));
  const rows: { label: string; note?: string; value: (r: WorkflowResults) => string }[] = [
    { label: t("workflows.results.runs"), value: (r) => formatNumber(r.runs) },
    { label: t("workflows.results.merged"), value: (r) => formatNumber(r.merged) },
    { label: t("workflows.results.closed"), value: (r) => formatNumber(r.closed) },
    {
      label: t("workflows.results.cost"),
      value: (r) => {
        const cost = costPerMerged(r);
        return cost === null ? notEnough : formatCost(cost);
      },
    },
    {
      label: t("workflows.results.rounds"),
      value: (r) => {
        const rounds = averageRounds(r);
        return rounds === null ? notEnough : formatNumber(Math.round(rounds * 10) / 10);
      },
    },
    { label: t("workflows.results.stoppedToAsk"), value: (r) => formatNumber(r.stoppedToAsk) },
    {
      label: t("workflows.results.matched"),
      note: t("workflows.results.matchedNote"),
      value: (r) => {
        const share = matchedShare(r);
        return share === null ? notEnough : t("workflows.results.matchedValue", { percent: Math.round(share * 100), matched: formatNumber(r.answersRecommended), total: formatNumber(r.answers) });
      },
    },
    { label: t("workflows.results.sentBack"), note: t("workflows.results.sentBackNote"), value: (r) => formatNumber(r.sentBack) },
    { label: t("workflows.results.criteriaMet"), value: (r) => checked(r, r.criteriaMet) },
    { label: t("workflows.results.criteriaUnmet"), value: (r) => checked(r, r.criteriaUnmet) },
    { label: t("workflows.results.criteriaUnverified"), note: t("workflows.results.criteriaUnverifiedNote"), value: (r) => checked(r, r.criteriaUnverified) },
  ];
  const compared = comparedResults(results, workflows);
  return (
    <Card aria-labelledby="workflow-results" className="mt-4 p-3">
      <h2 id="workflow-results" className="px-1 text-[12px] font-semibold text-muted">
        {t("workflows.results.title")}
      </h2>
      <p className="px-1 pt-0.5 text-[12px] text-subtle">{t("workflows.results.hint")}</p>
      {compared.length === 0 ? (
        <p className="px-1 pt-2 text-[12.5px]">{t("workflows.results.none")}</p>
      ) : (
        <>
          <div className="mt-2 overflow-x-auto">
            <table className="text-left text-[12.5px]">
              <thead>
                <tr>
                  {/* The first column names the rows. */}
                  <td />
                  {compared.map((r) => (
                    <th key={r.workflowId} scope="col" className="min-w-32 px-2 py-1 align-bottom font-medium break-words">
                      {r.workflowName}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr key={row.label} className="border-t border-border">
                    <th scope="row" className="px-1 py-1 pr-3 align-top font-normal text-muted">
                      {row.label}
                      {row.note && <span className="block text-[11px] text-subtle">{row.note}</span>}
                    </th>
                    {compared.map((r) => {
                      const value = row.value(r);
                      return (
                        <td key={r.workflowId} className={value === notEnough || value === nothingChecked ? "px-2 py-1 align-top text-subtle" : "px-2 py-1 align-top"}>
                          {value}
                        </td>
                      );
                    })}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="px-1 pt-2 text-[11.5px] text-subtle">{t("workflows.results.enoughNote", { min: ENOUGH_FOR_AN_AVERAGE })}</p>
        </>
      )}
    </Card>
  );
}
