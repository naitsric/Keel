import type { GithubAccount, Preflight, Reason } from "./api";

export type Readiness = "checking" | "ready" | "missing" | "signedOut" | "problem";

export interface Onboarding {
  claude: Readiness;
  /** Why Claude Code can't be used, when that's about its version rather than installing it. */
  claudeProblem: Reason | null;
  git: Readiness;
  /** Not needed to start. Without it, only what doesn't deliver can run, and the built-in workflows all do. */
  github: Readiness;
  /** Whether a project can be added now. Only git is needed, but a map also needs Claude Code. */
  canAddProject: boolean;
}

const VERSION_PROBLEMS = ["cliVersionTooOld", "cliVersionUntested"];

export function onboardingState(preflight: Preflight | null, accounts: GithubAccount[] | null): Onboarding {
  if (!preflight) {
    return { claude: "checking", claudeProblem: null, git: "checking", github: "checking", canAddProject: false };
  }
  const versionProblem = preflight.blocker && VERSION_PROBLEMS.includes(preflight.blocker.code) ? preflight.blocker : null;
  const claude: Readiness = !preflight.claudeFound ? "missing" : !preflight.auth?.loggedIn ? "signedOut" : versionProblem ? "problem" : "ready";
  const github: Readiness = !preflight.ghFound ? "missing" : accounts === null ? "checking" : accounts.length === 0 ? "signedOut" : "ready";
  return {
    claude,
    claudeProblem: versionProblem,
    git: preflight.gitFound ? "ready" : "missing",
    github,
    canAddProject: preflight.gitFound,
  };
}
