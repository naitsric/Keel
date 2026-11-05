import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Modal } from "../ui/primitives";
import { resolveKey, useKeymap } from "./keymap";
import { holdKeymap, keymapHeld } from "./keymapHold";
import { formatKeybinding, KEYS } from "./keys";
import { registerCommands, type Command } from "./registry";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const command = (id: string, extra: Partial<Command> = {}): Command => ({
  id,
  title: id,
  category: "Test",
  keybinding: "$mod+KeyN",
  run: () => {},
  ...extra,
});

describe("resolveKey", () => {
  const newRun = command("run.new");
  const newTask = command("board.newTask", { context: "board" });
  const changes = command("run.showChanges", { keybinding: "$mod+KeyD", context: "run" });

  it("runs the command that applies", () => {
    expect(resolveKey([newRun, newTask], "board", false)).toEqual({ action: "run", command: newTask });
    expect(resolveKey([newRun, newTask], undefined, false)).toEqual({ action: "run", command: newRun });
    expect(resolveKey([command("idle", { enabled: () => false })], undefined, false)).toEqual({ action: "pass" });
  });

  it("runs nothing while held, and suppresses the keys bound where the person is", () => {
    expect(resolveKey([newRun, newTask], undefined, true)).toEqual({ action: "suppress" });
    expect(resolveKey([command("idle", { enabled: () => false })], undefined, true)).toEqual({ action: "suppress" });
    expect(resolveKey([changes], undefined, true)).toEqual({ action: "pass" });
  });
});

describe("holding the keymap", () => {
  it("holds while any holder does", () => {
    const first = holdKeymap();
    const second = holdKeymap();
    first();
    expect(keymapHeld()).toBe(true);
    second();
    second();
    expect(keymapHeld()).toBe(false);
  });
});

describe("formatKeybinding", () => {
  it("writes each key as the Mac does", () => {
    expect(formatKeybinding(KEYS.searchEverywhere)).toBe("⇧⇧");
    expect(formatKeybinding(KEYS.findAction)).toBe("⇧⌘A");
    expect(formatKeybinding(KEYS.previousWaiting)).toBe("⇧⌘G");
    expect(formatKeybinding(KEYS.runs)).toBe("⌘1");
    expect(formatKeybinding(KEYS.settings)).toBe("⌘,");
    expect(formatKeybinding(KEYS.back)).toBe("⌘[");
    expect(formatKeybinding(KEYS.backOutsideFields)).toBe("⌘↑");
    expect(formatKeybinding(KEYS.stopStep)).toBe("⌘F2");
    expect(formatKeybinding("Escape")).toBe("⎋");
  });
});

describe("useKeymap", () => {
  const ran: string[] = [];
  const searched = vi.fn();
  let root: Root;
  let unregister: () => void;
  let onDialogChange: (open: boolean) => void;

  function Host({ dialog = false }: { dialog?: boolean }) {
    useKeymap(searched);
    return (
      <>
        <button type="button">focus</button>
        <textarea />
        <Modal open={dialog} onOpenChange={onDialogChange} title="dialog">
          <button type="button">inside</button>
        </Modal>
      </>
    );
  }

  beforeEach(async () => {
    ran.length = 0;
    searched.mockClear();
    onDialogChange = vi.fn();
    unregister = registerCommands([
      command("run.new", { run: () => void ran.push("run.new") }),
      command("navigate.board", { keybinding: "$mod+Digit2", run: () => void ran.push("navigate.board") }),
      command("navigate.back", { keybinding: "$mod+BracketLeft", keybindingOutsideFields: "$mod+ArrowUp", run: () => void ran.push("navigate.back") }),
    ]);
    const host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => root.render(<Host />));
  });

  afterEach(async () => {
    unregister();
    await act(async () => root.unmount());
    document.body.replaceChildren();
  });

  /** tinykeys reads `$mod` from the platform: ⌘ on macOS, Ctrl elsewhere (happy-dom says Linux). */
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? { metaKey: true } : { ctrlKey: true };

  function press(key: string, code: string, init: KeyboardEventInit = mod, on: Element = document.activeElement ?? document.body) {
    const event = new KeyboardEvent("keydown", { key, code, bubbles: true, cancelable: true, ...init });
    on.dispatchEvent(event);
    return event;
  }

  function tapShift() {
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Shift", code: "ShiftLeft" }));
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Shift", code: "ShiftLeft" }));
  }

  it("runs a command on its key and consumes the key", () => {
    const event = press("n", "KeyN");
    expect(ran).toEqual(["run.new"]);
    expect(event.defaultPrevented).toBe(true);
  });

  it("fires inside a text field too", () => {
    document.querySelector("textarea")?.focus();
    press("n", "KeyN");
    expect(ran).toEqual(["run.new"]);
  });

  it("ignores key repeat, so a held key does one thing", () => {
    press("n", "KeyN");
    press("n", "KeyN", { ...mod, repeat: true });
    expect(ran).toEqual(["run.new"]);
  });

  it("leaves ⌘↑ to a text field, and takes it everywhere else", () => {
    document.querySelector("textarea")?.focus();
    expect(press("ArrowUp", "ArrowUp").defaultPrevented).toBe(false);
    expect(ran).toEqual([]);

    (document.activeElement as HTMLElement).blur();
    expect(press("ArrowUp", "ArrowUp").defaultPrevented).toBe(true);
    expect(ran).toEqual(["navigate.back"]);
  });

  it("takes ⌘[ inside a text field too", () => {
    document.querySelector("textarea")?.focus();
    press("[", "BracketLeft");
    expect(ran).toEqual(["navigate.back"]);
  });

  it("opens on two quick Shift taps, and not when another key comes between", () => {
    tapShift();
    press("A", "KeyA", { shiftKey: true });
    tapShift();
    expect(searched).not.toHaveBeenCalled();
    tapShift();
    expect(searched).toHaveBeenCalledOnce();
  });

  it("runs no shortcut or Shift Shift while a dialog holds it", () => {
    const release = holdKeymap();
    const newRun = press("n", "KeyN");
    press("2", "Digit2");
    tapShift();
    tapShift();
    expect(ran).toEqual([]);
    expect(searched).not.toHaveBeenCalled();
    expect(newRun.defaultPrevented).toBe(true);
    // Keys no command is bound to reach the dialog as they are.
    expect(press("Enter", "Enter", {}).defaultPrevented).toBe(false);
    expect(press("Tab", "Tab", {}).defaultPrevented).toBe(false);

    release();
    press("n", "KeyN");
    press("2", "Digit2");
    tapShift();
    tapShift();
    expect(ran).toEqual(["run.new", "navigate.board"]);
    expect(searched).toHaveBeenCalledOnce();
  });

  it("leaves ⌘↑ to a text field under a dialog too", () => {
    const release = holdKeymap();
    document.querySelector("textarea")?.focus();
    expect(press("ArrowUp", "ArrowUp").defaultPrevented).toBe(false);
    release();
  });

  it("holds only while a dialog is open, because some stay mounted when closed", async () => {
    expect(keymapHeld()).toBe(false);
    await act(async () => root.render(<Host dialog />));
    expect(keymapHeld()).toBe(true);
    press("n", "KeyN");
    expect(ran).toEqual([]);
    await act(async () => root.render(<Host />));
    expect(keymapHeld()).toBe(false);
    press("n", "KeyN");
    expect(ran).toEqual(["run.new"]);
  });

  it("releases the hold when an open dialog unmounts", async () => {
    await act(async () => root.render(<Host dialog />));
    expect(keymapHeld()).toBe(true);
    await act(async () => root.render(<></>));
    expect(keymapHeld()).toBe(false);
  });

  it("still lets Esc close a dialog that holds it", async () => {
    await act(async () => root.render(<Host dialog />));
    press("Escape", "Escape", {}, document.body);
    expect(onDialogChange).toHaveBeenCalledWith(false);
  });
});
