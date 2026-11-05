import { describe, expect, it } from "vitest";
import { pickCommand, type Command } from "./registry";

const command = (id: string, extra: Partial<Command> = {}): Command => ({
  id,
  title: id,
  category: "Test",
  keybinding: "$mod+KeyN",
  run: () => {},
  ...extra,
});

describe("shared keybindings", () => {
  const newRun = command("run.new");
  const newTask = command("board.newTask", { context: "board" });

  it("prefers the command bound to the page the person is on", () => {
    expect(pickCommand([newRun, newTask], "board")?.id).toBe("board.newTask");
  });

  it("falls back to global commands when the page's is disabled or elsewhere", () => {
    const idle = command("board.newTask", { context: "board", enabled: () => false });
    expect(pickCommand([newRun, idle], "board")?.id).toBe("run.new");
    expect(pickCommand([newRun, newTask], undefined)?.id).toBe("run.new");
    expect(pickCommand([newRun, newTask], "run")?.id).toBe("run.new");
  });

  it("runs nothing when every candidate is disabled", () => {
    expect(pickCommand([command("a", { enabled: () => false })], undefined)).toBeUndefined();
  });
});
