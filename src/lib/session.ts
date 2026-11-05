import type { AuthStatus } from "./api";

const WARN_WITHIN_MS = 7 * 24 * 60 * 60 * 1000;

/** What `claude auth status` reports, like 2026-11-30T14:48:03.940Z. The browser's own parsing alone would also take text like "garbage 1" for a date. */
const TIMESTAMP = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}/;

/** The Claude Code account as attempts record it, or null when there's no email to go by. */
export const accountLabel = (auth: AuthStatus | null | undefined) => (auth?.email ? `${auth.email}${auth.subscription ? ` (${auth.subscription})` : ""}` : null);

/** Whether the session needs a warning: it ends within a week, or it has already ended. Null for no warning, also when the date can't be read. */
export function sessionExpiry(expiresAt: string | null | undefined, now: number): "soon" | "expired" | null {
  if (!expiresAt || !TIMESTAMP.test(expiresAt)) return null;
  const at = Date.parse(expiresAt);
  if (Number.isNaN(at)) return null;
  if (at <= now) return "expired";
  return at - now <= WARN_WITHIN_MS ? "soon" : null;
}
