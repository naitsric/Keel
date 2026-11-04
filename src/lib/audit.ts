import type { AuditEvent } from "./api";

export interface CommandRun {
  id: string;
  command: string;
  description: string | null;
  status: "ok" | "failed" | "running";
  exitCode: number | null;
  output: string;
  truncated: boolean;
  at: string;
}

/** The commands a step ran, each with how it ended. A pending result means it's still going. */
export function commandsFrom(events: AuditEvent[]): CommandRun[] {
  const results = new Map(events.filter((e) => e.kind === "toolResult").map((e) => [e.data.id as string, e]));
  return events
    .filter((e) => e.kind === "toolCall" && e.data.name === "Bash")
    .map((call) => {
      const result = results.get(call.data.id);
      const failed = result?.data.isError === true;
      return {
        id: call.data.id,
        command: String(call.data.input?.command ?? ""),
        description: call.data.input?.description ?? null,
        status: !result ? "running" : failed ? "failed" : "ok",
        exitCode: result?.data.exitCode ?? null,
        output: String(result?.data.text ?? ""),
        truncated: result?.data.truncated === true,
        at: call.at,
      };
    });
}

export interface ActivityItem {
  key: string;
  at: string;
  kind: string;
  event: AuditEvent;
  /** For a tool call, what came back. */
  result?: AuditEvent;
}

/** Pairs each tool call with its result, so a call and its output read as one thing. */
export function activityFrom(events: AuditEvent[]): ActivityItem[] {
  const results = new Map(events.filter((e) => e.kind === "toolResult").map((e) => [e.data.id as string, e]));
  return events
    .filter((e) => e.kind !== "toolResult")
    .map((event) => ({
      key: String(event.seq),
      at: event.at,
      kind: event.kind,
      event,
      result: event.kind === "toolCall" ? results.get(event.data.id) : undefined,
    }));
}

/** The most useful line to show for a tool call. */
export function toolSummary(name: string, input: Record<string, any> | null | undefined): string {
  if (!input) return "";
  if (name === "Bash") return String(input.command ?? "");
  return String(input.file_path ?? input.path ?? input.pattern ?? JSON.stringify(input));
}

/** Everything the run's steps changed, without repeats. */
export function filesChangedBy(attempts: { filesChanged: string[] | null }[]): string[] {
  return [...new Set(attempts.flatMap((a) => a.filesChanged ?? []))];
}

/** Files that can run with the repo's secrets as soon as the branch is pushed. */
export const touchesWorkflows = (files: string[]) => files.some((f) => f.startsWith(".github/workflows/"));

/** The attempt whose final message explains the run: the last one that could change files, else the last agent. */
export function explainingAttempt<T extends { kind: string; tools: string; resultText: string | null }>(attempts: T[]): T | undefined {
  const agents = attempts.filter((a) => a.kind === "agent" && a.resultText);
  return [...agents].reverse().find((a) => a.tools === "build") ?? agents[agents.length - 1];
}
