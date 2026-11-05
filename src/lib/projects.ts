import { type Attempt, isActive, type Project, type ProjectSettings } from "./api";

/**
 * Whether a project still has setup to finish, worked out from the project so it clears itself
 * whichever screen fixes it. A project that isn't on GitHub has no account to choose, so that alone
 * doesn't count. One with no check always does, and it never stops a run.
 */
export const needsSetup = (project: Project) => !project.checkCommand || (Boolean(project.githubRepo) && !project.githubAccount);

/**
 * What to send when only these fields change. `project_update` replaces every field, so everything
 * else goes back as it came, including any field added later.
 */
export const withSetup = (project: Project, change: Pick<ProjectSettings, "githubAccount" | "setupCommand" | "checkCommand">): ProjectSettings => ({ ...project, ...change });

/** The account to show chosen. With several that can push, that's the person's call, not Keel's. */
export function accountToSelect(saved: string | null, canPush: string[]): string | null {
  if (saved) return saved;
  return canPush.length === 1 ? canPush[0] : null;
}

export type Trial = { state: "running"; attempt: Attempt | null } | { state: "passed"; attempt: Attempt } | { state: "failed"; attempt: Attempt };

/** Where trying a command has got to. Setup comes first, and only the check can pass. */
export function trialOutcome(attempts: Attempt[]): Trial {
  const attempt = attempts[attempts.length - 1];
  if (!attempt || isActive(attempt.state)) return { state: "running", attempt: attempt ?? null };
  if (attempt.state !== "succeeded") return { state: "failed", attempt };
  return attempt.kind === "check" ? { state: "passed", attempt } : { state: "running", attempt };
}

/** A shell's way of saying it couldn't find or start a command. */
export const commandNotFound = (attempt: Attempt) => attempt.exitCode === 126 || attempt.exitCode === 127;

/** A path as a person says it. */
export const shortPath = (path: string) => path.replace(/^\/Users\/[^/]+(?=\/|$)/, "~");
