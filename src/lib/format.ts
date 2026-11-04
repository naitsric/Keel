import i18next from "../i18n";

const locale = () => (i18next.language === "es" ? "es" : "en");

export const formatNumber = (value: number) => new Intl.NumberFormat(locale()).format(value);

export const formatCost = (value: number) =>
  new Intl.NumberFormat(locale(), { style: "currency", currency: "USD", maximumFractionDigits: value < 1 ? 4 : 2 }).format(value);

export const formatDateTime = (iso: string) =>
  new Intl.DateTimeFormat(locale(), { dateStyle: "medium", timeStyle: "short" }).format(new Date(iso));

export function formatDuration(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const unit = (value: number, name: string) =>
    new Intl.NumberFormat(locale(), { style: "unit", unit: name, unitDisplay: "narrow" }).format(value);
  if (hours > 0) return `${unit(hours, "hour")} ${unit(minutes, "minute")}`;
  if (minutes > 0) return `${unit(minutes, "minute")} ${unit(rest, "second")}`;
  return unit(rest, "second");
}

export function formatRelative(iso: string, now = Date.now()): string {
  const seconds = Math.round((new Date(iso).getTime() - now) / 1000);
  const rtf = new Intl.RelativeTimeFormat(locale(), { numeric: "auto" });
  const abs = Math.abs(seconds);
  if (abs < 60) return rtf.format(seconds, "second");
  if (abs < 3600) return rtf.format(Math.round(seconds / 60), "minute");
  if (abs < 86400) return rtf.format(Math.round(seconds / 3600), "hour");
  return rtf.format(Math.round(seconds / 86400), "day");
}

/** How long something took, or has been taking. */
export function elapsedMs(startedAt: string, finishedAt: string | null, now = Date.now()): number {
  return (finishedAt ? new Date(finishedAt).getTime() : now) - new Date(startedAt).getTime();
}
