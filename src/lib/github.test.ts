import { describe, expect, it } from "vitest";
import type { GithubStatus } from "./api";
import { githubAccess, repoAccessUrl, repoInvitationUrl } from "./github";

const status = (fields: Partial<GithubStatus>): GithubStatus => ({ repo: "a/b", accounts: [], info: null, excessScopes: [], error: null, ...fields });
const info = (permission: string) => ({ defaultBranch: "main", permission, private: true, defaultBranchProtected: true });

describe("githubAccess", () => {
  it("says an account can push only with a permission that allows it", () => {
    for (const permission of ["ADMIN", "MAINTAIN", "WRITE"]) expect(githubAccess(status({ info: info(permission) }))).toBe("canPush");
    for (const permission of ["TRIAGE", "READ"]) expect(githubAccess(status({ info: info(permission) }))).toBe("readOnly");
  });

  it("tells an account that can't see the repo from other failures", () => {
    expect(githubAccess(status({ error: { code: "deliveryFailed", kind: "noPermission", detail: "x" } }))).toBe("none");
    expect(githubAccess(status({ error: { code: "deliveryFailed", kind: "other", detail: "offline" } }))).toBe("unknown");
    expect(githubAccess(status({ error: { code: "ghMissing" } }))).toBe("unknown");
  });

  it("doesn't guess before there's an answer", () => {
    expect(githubAccess(null)).toBe("unknown");
    expect(githubAccess(status({}))).toBe("unknown");
  });
});

describe("links", () => {
  it("point at the repo's pages on GitHub, which the app is allowed to open", () => {
    expect(repoAccessUrl("a/b")).toBe("https://github.com/a/b/settings/access");
    expect(repoInvitationUrl("a/b")).toBe("https://github.com/a/b/invitations");
  });
});
