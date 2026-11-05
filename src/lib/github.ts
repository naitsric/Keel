import type { GithubStatus } from "./api";

export type GithubAccess = "canPush" | "readOnly" | "none" | "unknown";

const CAN_PUSH = ["ADMIN", "MAINTAIN", "WRITE"];

/** What an account can do in the project's repo. "unknown" when there's nothing to go on yet. */
export function githubAccess(status: GithubStatus | null): GithubAccess {
  if (status?.error?.code === "deliveryFailed" && status.error.kind === "noPermission") return "none";
  const permission = status?.info?.permission;
  if (!permission) return "unknown";
  return CAN_PUSH.includes(permission) ? "canPush" : "readOnly";
}

/** Where a repo's admin adds or removes people. GitHub asks for its own confirmations there. */
export const repoAccessUrl = (repo: string) => `https://github.com/${repo}/settings/access`;

/** Where an invited account accepts. */
export const repoInvitationUrl = (repo: string) => `https://github.com/${repo}/invitations`;
