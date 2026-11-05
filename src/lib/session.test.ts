import { describe, expect, it } from "vitest";
import type { AuthStatus } from "./api";
import { accountLabel, sessionExpiry } from "./session";

const DAY = 24 * 60 * 60 * 1000;
const now = Date.parse("2026-11-05T12:00:00.000Z");
const inDays = (days: number) => new Date(now + days * DAY).toISOString();

describe("sessionExpiry", () => {
  it("reads the format `claude auth status` really reports", () => {
    expect(sessionExpiry("2026-11-08T14:48:03.940Z", now)).toBe("soon");
    expect(sessionExpiry("2026-11-30T14:48:03.940Z", now)).toBeNull();
  });

  it("warns within 7 days and not before", () => {
    expect(sessionExpiry(inDays(1), now)).toBe("soon");
    expect(sessionExpiry(inDays(7), now)).toBe("soon");
    expect(sessionExpiry(new Date(now + 7 * DAY + 1000).toISOString(), now)).toBeNull();
    expect(sessionExpiry(inDays(30), now)).toBeNull();
  });

  it("says when it has already ended", () => {
    expect(sessionExpiry(new Date(now).toISOString(), now)).toBe("expired");
    expect(sessionExpiry(inDays(-1), now)).toBe("expired");
  });

  it("shows nothing when the value is missing or can't be read", () => {
    for (const value of [null, undefined, "", "garbage", "garbage 1", "tomorrow", "1801234567", "1801234567000", "2026-13-45T99:99:00Z"]) {
      expect(sessionExpiry(value, now), String(value)).toBeNull();
    }
  });
});

describe("accountLabel", () => {
  const auth = (fields: Partial<AuthStatus>) => ({ email: null, subscription: null, ...fields }) as AuthStatus;

  it("names the account and its plan as attempts record them", () => {
    expect(accountLabel(auth({ email: "you@example.com", subscription: "max" }))).toBe("you@example.com (max)");
    expect(accountLabel(auth({ email: "you@example.com" }))).toBe("you@example.com");
  });

  it("has nothing to say without an email", () => {
    expect(accountLabel(auth({ subscription: "max" }))).toBeNull();
    expect(accountLabel(null)).toBeNull();
    expect(accountLabel(undefined)).toBeNull();
  });
});
