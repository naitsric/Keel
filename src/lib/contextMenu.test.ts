import { afterEach, describe, expect, it } from "vitest";
import { keepsContextMenu } from "./contextMenu";

const selected = { isCollapsed: false };
const nothingSelected = { isCollapsed: true };

const inPage = (html: string, selector: string) => {
  document.body.innerHTML = html;
  return document.querySelector(selector);
};

describe("the browser's right-click menu", () => {
  afterEach(() => document.body.replaceChildren());

  it.each([
    ["an input", "<input />", "input"],
    ["a text area", "<textarea></textarea>", "textarea"],
    ["something editable", '<div contenteditable="true"><b>text</b></div>', "b"],
  ])("stays on %s, selected or not, for Paste", (_name, html, selector) => {
    expect(keepsContextMenu(inPage(html, selector), nothingSelected)).toBe(true);
    expect(keepsContextMenu(inPage(html, selector), selected)).toBe(true);
  });

  it.each([
    ["selectable text", '<p class="selectable">text</p>', "p"],
    ["what's inside selectable text", '<p class="selectable"><code>text</code></p>', "code"],
    ["a block of output", "<pre>text</pre>", "pre"],
    ["a line of a diff", "<pre><div>text</div></pre>", "div"],
  ])("stays on %s only with something selected, for Copy", (_name, html, selector) => {
    expect(keepsContextMenu(inPage(html, selector), selected)).toBe(true);
    expect(keepsContextMenu(inPage(html, selector), nothingSelected)).toBe(false);
    expect(keepsContextMenu(inPage(html, selector), null)).toBe(false);
  });

  it.each([
    ["a button", "<button>Approve</button>", "button"],
    ["plain text", "<p>text</p>", "p"],
    ["a table row", "<table><tbody><tr><td>run</td></tr></tbody></table>", "td"],
  ])("goes from %s, even with text selected elsewhere", (_name, html, selector) => {
    expect(keepsContextMenu(inPage(html, selector), selected)).toBe(false);
  });

  it("goes when it isn't on an element", () => {
    expect(keepsContextMenu(null, selected)).toBe(false);
    expect(keepsContextMenu(window, selected)).toBe(false);
  });
});
