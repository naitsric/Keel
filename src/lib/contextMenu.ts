/**
 * Whether the browser's right-click menu stays. It offers Reload on anything else, which a released
 * app must not (see main.tsx), so it stays where it's useful: a field, for Paste, and selected text
 * that's meant to be copied, for Copy. Right-clicking text on a Mac selects the word first.
 */
export function keepsContextMenu(target: EventTarget | null, selection: Pick<Selection, "isCollapsed"> | null = window.getSelection()): boolean {
  if (!(target instanceof Element)) return false;
  if (target.closest("input, textarea, [contenteditable]")) return true;
  return target.closest("pre, .selectable") !== null && selection !== null && !selection.isCollapsed;
}
