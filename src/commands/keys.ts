/**
 * Every key, in tinykeys syntax: commands bind them and tooltips show them.
 *
 * Not here on purpose: Esc, which must stay Radix's to close a dialog (a dialog holding the keymap
 * would prevent its default, and Radix skips closing when it sees that), and what the native menu
 * takes for its own items (see src/menu/menuModel.ts), or src/main.tsx blocks. `settings` is the
 * menu's: it's only shown.
 */
export const KEYS = {
  searchEverywhere: "Shift Shift",
  findAction: "$mod+Shift+KeyA",
  goToRun: "$mod+KeyO",
  newRun: "$mod+KeyN",
  nextWaiting: "$mod+KeyG",
  previousWaiting: "$mod+Shift+KeyG",
  runs: "$mod+Digit1",
  board: "$mod+Digit2",
  workflows: "$mod+Digit3",
  brain: "$mod+Digit4",
  settings: "$mod+Comma",
  back: "$mod+BracketLeft",
  backOutsideFields: "$mod+ArrowUp",
  showChanges: "$mod+KeyD",
  stopStep: "$mod+F2",
} as const;

const SYMBOLS: Record<string, string> = {
  $mod: "⌘",
  Control: "⌃",
  Alt: "⌥",
  Shift: "⇧",
  Escape: "⎋",
  ArrowUp: "↑",
  BracketLeft: "[",
  Comma: ",",
};

const MODIFIER_ORDER = ["Control", "Alt", "Shift", "$mod"];

function formatPress(press: string): string {
  const parts = press.split("+");
  const key = parts.pop() as string;
  const modifiers = parts.sort((a, b) => MODIFIER_ORDER.indexOf(a) - MODIFIER_ORDER.indexOf(b));
  return modifiers.map((m) => SYMBOLS[m] ?? m).join("") + (SYMBOLS[key] ?? key.replace(/^Key/, "").replace(/^Digit/, ""));
}

/** `$mod+Shift+KeyA` → `⇧⌘A`, in macOS modifier order. */
export const formatKeybinding = (binding: string) => binding.split(" ").map(formatPress).join("");

/** A tooltip that says the key too. */
export const withKey = (label: string, binding: string) => `${label}  ${formatKeybinding(binding)}`;
