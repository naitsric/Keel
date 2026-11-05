import * as Dialog from "@radix-ui/react-dialog";
import { Command as Cmdk, defaultFilter } from "cmdk";
import { Search } from "lucide-react";
import { useRef } from "react";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { useApp } from "../lib/store";
import { paletteContentClass, paletteGroupClass, paletteInputClass, paletteItemClass, paletteListClass, paletteOverlayClass } from "../ui/palette";
import { Kbd } from "../ui/primitives";
import { useKeymapHold } from "./keymapHold";
import { executeCommand, focusContext, isEnabled, pickCommand, useCommands, type Command } from "./registry";
import { openRun } from "./runs";

export type SearchScope = "all" | "actions" | "runs";

export const useSearchEverywhere = create<{ open: boolean; scope: SearchScope; query: string; openedFrom: HTMLElement | null; show: (scope: SearchScope) => void; hide: () => void; setQuery: (query: string) => void }>((set) => ({
  open: false,
  scope: "all",
  query: "",
  openedFrom: null,
  show: (scope) => set({ open: true, scope, query: "", openedFrom: document.activeElement as HTMLElement | null }),
  hide: () => set({ open: false }),
  setQuery: (query) => set({ query }),
}));

/** With nothing typed, "all" lists only the newest runs: 200 of them would push the rest out of sight. */
const RECENT_RUNS = 8;

const withoutAccents = (text: string) => text.normalize("NFD").replace(/\p{M}/gu, "");

/** Searches what the rows say and not their ids, on which a fuzzy match finds anything. "ejecucion" finds "ejecución". */
const filter = (_value: string, search: string, keywords: string[] = []) => defaultFilter(withoutAccents(keywords.join(" ")), withoutAccents(search));

/** The key to show on a row: none if pressing it here would run another command, like ⌘N on the board. */
function keyOf(command: Command, commands: Command[]): string | undefined {
  if (!command.keybinding) return command.displayKey;
  const winner = pickCommand(
    commands.filter((c) => c.keybinding === command.keybinding),
    focusContext(),
  );
  return !winner || winner === command ? command.keybinding : undefined;
}

/**
 * Search Everywhere (⇧⇧), Find Action (⇧⌘A) and Go to Run (⌘O): one palette with scopes. Plain Enter
 * runs a row, so a row only ever goes somewhere or runs a registered command, and what's registered
 * can't decide anything (see registry.ts). It's a dialog like any other, so it holds the keymap.
 */
export function SearchEverywhere() {
  const { t } = useTranslation();
  const { open, scope, query, hide, setQuery } = useSearchEverywhere();
  const registered = useCommands((s) => s.commands);
  const runs = useApp((s) => s.overview.runs);
  const projects = useApp((s) => s.overview.projects);
  const navigate = useApp((s) => s.navigate);
  useKeymapHold(open);

  const commands = Object.values(registered);
  const groups = new Map<string, Command[]>();
  for (const command of commands) {
    groups.set(command.category, [...(groups.get(command.category) ?? []), command]);
  }
  const shownRuns = scope === "all" && !query.trim() ? runs.slice(0, RECENT_RUNS) : runs;

  // Let the dialog unmount before the action runs. Choosing leaves focus nowhere, because a second
  // Enter where it was could press a button, like the one that moves a card to Todo.
  const chose = useRef(false);
  const choose = (action: () => void) => {
    chose.current = true;
    hide();
    requestAnimationFrame(action);
  };

  return (
    <Dialog.Root open={open} onOpenChange={(next) => !next && hide()}>
      <Dialog.Portal>
        <Dialog.Overlay className={paletteOverlayClass} />
        <Dialog.Content
          aria-describedby=""
          className={paletteContentClass}
          onCloseAutoFocus={() => {
            // Radix only gives focus back to a trigger, and this has none. Esc means carry on as you were.
            if (!chose.current) useSearchEverywhere.getState().openedFrom?.focus();
            chose.current = false;
          }}
        >
          <Dialog.Title className="sr-only">{t("palette.label")}</Dialog.Title>
          <Cmdk
            label={t("palette.label")}
            loop
            filter={filter}
            onKeyDown={(e) => {
              // A held Enter doesn't choose. It's still the key that opened this, or will be the next.
              if (e.key === "Enter" && e.repeat) e.preventDefault();
            }}
          >
            <div className="flex items-center gap-2 border-b border-border px-3">
              <Search size={16} className="shrink-0 text-subtle" />
              <Cmdk.Input autoFocus value={query} onValueChange={setQuery} placeholder={t(`palette.placeholder.${scope}`)} className={paletteInputClass} />
              <Kbd binding="Escape" />
            </div>
            <Cmdk.List label={t("palette.label")} className={paletteListClass}>
              <Cmdk.Empty className="px-3 py-6 text-center text-[12.5px] text-muted">{t(scope === "runs" && runs.length === 0 ? "palette.noRuns" : "palette.empty")}</Cmdk.Empty>

              {scope !== "actions" && shownRuns.length > 0 && (
                <Cmdk.Group heading={t("palette.runs")} className={paletteGroupClass}>
                  {shownRuns.map((run) => (
                    <Cmdk.Item key={run.id} value={`run:${run.id}`} keywords={[run.title]} onSelect={() => choose(() => openRun(run.id))} className={paletteItemClass}>
                      <span className="min-w-0 truncate">{run.title}</span>
                      <span className="muted shrink-0 text-[12px] text-subtle">
                        {run.projectName} · {t(`states.${run.state}`)}
                      </span>
                    </Cmdk.Item>
                  ))}
                </Cmdk.Group>
              )}

              {scope === "all" && projects.length > 0 && (
                <Cmdk.Group heading={t("palette.projects")} className={paletteGroupClass}>
                  {projects.map((project) => (
                    <Cmdk.Item key={project.id} value={`project:${project.id}`} keywords={[project.name]} onSelect={() => choose(() => navigate({ name: "board", projectId: project.id }))} className={paletteItemClass}>
                      <span className="min-w-0 truncate">{project.name}</span>
                      <span className="muted shrink-0 text-[12px] text-subtle">{t("app.board")}</span>
                    </Cmdk.Item>
                  ))}
                </Cmdk.Group>
              )}

              {scope !== "runs" &&
                [...groups].map(([category, items]) => (
                  <Cmdk.Group key={category} heading={t(category)} className={paletteGroupClass}>
                    {items.map((command) => {
                      const key = keyOf(command, commands);
                      return (
                        <Cmdk.Item key={command.id} value={`action:${command.id}`} keywords={[t(command.title)]} disabled={!isEnabled(command)} onSelect={() => choose(() => executeCommand(command.id))} className={paletteItemClass}>
                          <span className="min-w-0 truncate">{t(command.title)}</span>
                          {key && <Kbd binding={key} />}
                        </Cmdk.Item>
                      );
                    })}
                  </Cmdk.Group>
                ))}
            </Cmdk.List>
          </Cmdk>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
