import { type Attempt, type CiGap, type CiLeftOut, type Guess, isActive, type Project, type ProjectSettings, type ToolFolder } from "./api";

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
export const withSetup = (project: Project, change: Pick<ProjectSettings, "githubAccount" | "setupCommand" | "checkCommand"> & Partial<Pick<ProjectSettings, "allowedRead">>): ProjectSettings => ({ ...project, ...change });

/** One way of writing each folder: with `~` for the home folder and no `/` at the end. */
const sameFolder = (folder: string) => shortPath(folder.trim()).replace(/\/+$/, "");

/** Whether the list already lets this folder be read: it names it, or a folder it's in. */
export function isListed(folder: string, list: string[]): boolean {
  const wanted = sameFolder(folder);
  return list.map(sameFolder).some((listed) => listed !== "" && (wanted === listed || wanted.startsWith(`${listed}/`)));
}

/** What Keel found that the list doesn't let be read yet. */
export const foldersToOffer = (found: ToolFolder[], list: string[]) => found.filter((found) => !isListed(found.folder, list));

/** The list with these folders at the end, each once. */
export const withFolders = (list: string[], folders: string[]) => folders.reduce((all, folder) => (isListed(folder, all) ? all : [...all, folder]), list);

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

/** Whole words, since `--group test` isn't in `--group test-uv`. */
const hasOption = (command: string, option: string) => ` ${command.trim().replace(/\s+/g, " ")} `.includes(` ${option} `);

/** A flag only means something on a single `uv sync`, not on `make deps` or the end of a longer line. */
const BARE_UV_SYNC = /^uv sync\b[^;&|]*$/;

/** Setups chained with `&&` are each a step; the flags belong to the one that is `uv sync`. */
const STEP = " && ";

function uvSyncStep(command: string): { steps: string[]; at: number } | null {
  const steps = command.split("&&").map((step) => step.trim());
  const at = steps.findIndex((step) => BARE_UV_SYNC.test(step));
  return at < 0 ? null : { steps, at };
}

/** The flags left to offer for this setup command. An empty one starts from the guess. */
export function setupOptionsLeft(setup: string, guess: Guess): string[] {
  const command = setup.trim() || (guess.setup ?? "");
  const found = uvSyncStep(command);
  if (!found) return [];
  const step = found.steps[found.at];
  return guess.setupOptions.filter((option) => !hasOption(step, option));
}

export function withSetupOption(setup: string, guess: Guess, option: string): string {
  const command = setup.trim() || (guess.setup ?? "");
  const found = uvSyncStep(command);
  if (!found) return `${command} ${option}`;
  found.steps[found.at] = `${found.steps[found.at]} ${option}`;
  return found.steps.join(STEP);
}

export function checkOptionsLeft(check: string, guess: Guess): string[] {
  return guess.checkOptions.filter((option) => !hasOption(check, option));
}

/** Where a comment starts under `sh -c`: a `#` that begins a word. After a quote one could be quoted, so none is looked for. */
function commentAt(command: string): number {
  for (let at = 0; at < command.length; at++) {
    const c = command[at];
    if (c === "'" || c === '"') return -1;
    if (c === "#" && (at === 0 || /\s/.test(command[at - 1]))) return at;
  }
  return -1;
}

/** Each one has to pass, so a failure anywhere fails the check. It goes before a comment, which would hide it. */
export function withCheckOption(check: string, option: string): string {
  const trimmed = check.trim();
  if (!trimmed) return option;
  const at = commentAt(trimmed);
  if (at < 0) return `${trimmed} && ${option}`;
  const before = trimmed.slice(0, at).trim();
  return `${before ? `${before} && ` : ""}${option} ${trimmed.slice(at)}`;
}

/** pyright from PyPI fetches Node the first time unless it's already installed, which a check with no network can't. */
export const needsNode = (commands: string[]) => commands.some((command) => /\bpyright\b/.test(command));

/** What the CI runs that isn't in this check yet, as it's being typed. */
export function ciCommandsLeft(check: string, commands: CiGap["commands"]): string[] {
  return commands.filter((command) => !hasOption(check, command));
}

/** What the CI runs that Keel left out, unless the check runs it anyway. */
export function ciLeftOutLeft(check: string, leftOut: CiLeftOut[]): CiLeftOut[] {
  return leftOut.filter((left) => !hasOption(check, left.command));
}
