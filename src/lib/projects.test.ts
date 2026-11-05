import { describe, expect, it } from "vitest";
import type { Attempt, AttemptState, Project } from "./api";
import { accountToSelect, commandNotFound, needsSetup, shortPath, trialOutcome, withSetup } from "./projects";

const project = (fields: Partial<Project>) => ({ id: "p", name: "demo", path: "/w", githubAccount: null, githubRepo: null, setupCommand: null, checkCommand: null, allowedDomains: [], allowedRead: [], services: [], env: [], defaultWorkflow: null, ...fields }) as Project;

describe("whether a project still has setup to finish", () => {
  it("is done with a check and, on GitHub, an account", () => {
    expect(needsSetup(project({ checkCommand: "make test", githubRepo: "acme/app", githubAccount: "you" }))).toBe(false);
  });

  it("is done with a check when it isn't on GitHub, since there's no account to choose", () => {
    expect(needsSetup(project({ checkCommand: "make test" }))).toBe(false);
  });

  it("keeps asking for a check, whatever else it has", () => {
    expect(needsSetup(project({}))).toBe(true);
    expect(needsSetup(project({ githubRepo: "acme/app", githubAccount: "you" }))).toBe(true);
  });

  it("keeps asking for an account when the repo is on GitHub", () => {
    expect(needsSetup(project({ checkCommand: "make test", githubRepo: "acme/app" }))).toBe(true);
  });
});

describe("what saving sends", () => {
  it("replaces only what changed, including fields it doesn't know about", () => {
    const before = project({
      allowedDomains: ["pypi.org"],
      allowedRead: ["~/.nvm"],
      services: [{ type: "bridge", name: "db", port: 5432, socket: null }],
      env: [{ name: "DB", value: "x" }],
      defaultWorkflow: "mine",
      ...({ addedLater: 1 } as object),
    });
    const sent = withSetup(before, { githubAccount: "you", setupCommand: "npm ci", checkCommand: "npm test" });
    expect(sent).toEqual({ ...before, githubAccount: "you", setupCommand: "npm ci", checkCommand: "npm test" });
    expect(sent.defaultWorkflow).toBe("mine");
    expect(sent.env).toEqual([{ name: "DB", value: "x" }]);
  });
});

describe("the account shown as chosen", () => {
  it("is what's saved, whoever else can push", () => {
    expect(accountToSelect("you", [])).toBe("you");
    expect(accountToSelect("you", ["a", "b"])).toBe("you");
  });

  it("is the only one that can push", () => {
    expect(accountToSelect(null, ["a"])).toBe("a");
  });

  it("is nobody with several, because that's the person's call", () => {
    expect(accountToSelect(null, ["a", "b"])).toBeNull();
  });

  it("is nobody when none can push", () => {
    expect(accountToSelect(null, [])).toBeNull();
  });
});

describe("trying a command", () => {
  const attempt = (kind: Attempt["kind"], state: AttemptState, exitCode: number | null = null) => ({ kind, state, exitCode }) as Attempt;

  it("is running until the check has an answer", () => {
    expect(trialOutcome([]).state).toBe("running");
    expect(trialOutcome([attempt("setup", "running")]).state).toBe("running");
    expect(trialOutcome([attempt("setup", "succeeded")]).state).toBe("running");
    expect(trialOutcome([attempt("setup", "succeeded"), attempt("check", "queued")]).state).toBe("running");
  });

  it("passes only when the check does", () => {
    expect(trialOutcome([attempt("setup", "succeeded"), attempt("check", "succeeded")]).state).toBe("passed");
    expect(trialOutcome([attempt("check", "succeeded")]).state).toBe("passed");
  });

  it("fails at whichever step failed", () => {
    expect(trialOutcome([attempt("setup", "failed")])).toMatchObject({ state: "failed", attempt: { kind: "setup" } });
    expect(trialOutcome([attempt("setup", "succeeded"), attempt("check", "failed")])).toMatchObject({ state: "failed", attempt: { kind: "check" } });
    expect(trialOutcome([attempt("check", "stopped")]).state).toBe("failed");
  });

  it("recognizes the codes a shell uses when it can't run a command", () => {
    expect(commandNotFound(attempt("check", "failed", 127))).toBe(true);
    expect(commandNotFound(attempt("check", "failed", 126))).toBe(true);
    expect(commandNotFound(attempt("check", "failed", 1))).toBe(false);
  });
});

describe("a path as a person says it", () => {
  it("shortens the home folder and nothing else", () => {
    expect(shortPath("/Users/you/www/demo")).toBe("~/www/demo");
    expect(shortPath("/Users/you")).toBe("~");
    expect(shortPath("/opt/demo")).toBe("/opt/demo");
  });
});
