import { describe, expect, it } from "vitest";
import type { Preflight } from "./api";
import { onboardingState } from "./onboarding";

const ready: Preflight = {
  claudeFound: true,
  gitFound: true,
  ghFound: true,
  version: { found: "2.1.318" } as Preflight["version"],
  auth: { loggedIn: true } as Preflight["auth"],
  ignoredEnv: [],
  blocker: null,
};
const account = { login: "you", active: true, ok: true, scopes: ["repo"] };

describe("what a new Mac still needs", () => {
  it("waits while it checks", () => {
    expect(onboardingState(null, null).claude).toBe("checking");
    expect(onboardingState(null, null).canAddProject).toBe(false);
  });

  it("is ready when everything is there", () => {
    expect(onboardingState(ready, [account])).toEqual({ claude: "ready", claudeProblem: null, git: "ready", github: "ready", canAddProject: true });
  });

  it("tells installing Claude Code from signing in, and from a version it can't use", () => {
    expect(onboardingState({ ...ready, claudeFound: false, auth: null }, []).claude).toBe("missing");
    expect(onboardingState({ ...ready, auth: { loggedIn: false } as Preflight["auth"] }, []).claude).toBe("signedOut");
    const blocker = { code: "cliVersionTooOld", found: "1.0.0", minimum: "2.1.318" } as unknown as Preflight["blocker"];
    const state = onboardingState({ ...ready, blocker }, []);
    expect(state.claude).toBe("problem");
    expect(state.claudeProblem).toBe(blocker);
  });

  it("doesn't call a version problem something it isn't", () => {
    const blocker = { code: "gitMissing" } as Preflight["blocker"];
    expect(onboardingState({ ...ready, gitFound: false, blocker }, []).claude).toBe("ready");
  });

  it("can add a project with only git, since the rest can come later", () => {
    const state = onboardingState({ ...ready, claudeFound: false, ghFound: false, auth: null }, null);
    expect(state.canAddProject).toBe(true);
    expect(state.github).toBe("missing");
    expect(onboardingState({ ...ready, gitFound: false }, null).canAddProject).toBe(false);
  });

  it("knows gh can be installed with nobody signed in", () => {
    expect(onboardingState(ready, []).github).toBe("signedOut");
    expect(onboardingState(ready, null).github).toBe("checking");
  });
});
