import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { useKeymap } from "./keymap";
import { registerCommands } from "./registry";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

// What tinykeys does on macOS 13.0 to 13.2, whose WebKit has no regex lookbehind.
vi.mock("tinykeys", () => ({
  tinykeys: () => {
    throw new SyntaxError("Invalid regular expression: invalid group specifier name");
  },
}));

function Host() {
  useKeymap(() => {});
  return <p>Still here</p>;
}

it("loses the keys and keeps the app, where tinykeys can't load its keys", async () => {
  const error = vi.spyOn(console, "error").mockImplementation(() => {});
  const unregister = registerCommands([{ id: "test", title: "test", category: "test", keybinding: "$mod+KeyN", run: () => {} }]);
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  await act(async () => root.render(<Host />));
  expect(host.textContent).toBe("Still here");
  expect(error).toHaveBeenCalledWith("keyboard shortcuts are off", expect.any(SyntaxError));
  await act(async () => root.unmount());
  unregister();
  error.mockRestore();
  document.body.replaceChildren();
});
