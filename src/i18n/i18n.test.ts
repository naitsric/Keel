import { describe, expect, it } from "vitest";
import en from "./en.json";
import es from "./es.json";

const flatten = (value: object, prefix = ""): string[] =>
  Object.entries(value).flatMap(([key, child]) => (typeof child === "object" ? flatten(child, `${prefix}${key}.`) : [`${prefix}${key}`]));

const values = (value: object, prefix = ""): [string, string][] =>
  Object.entries(value).flatMap(([key, child]): [string, string][] => (typeof child === "object" ? values(child, `${prefix}${key}.`) : [[`${prefix}${key}`, String(child)]]));

describe("translations", () => {
  it("have the same keys in both languages", () => {
    expect(flatten(es).sort()).toEqual(flatten(en).sort());
  });

  it("use the same placeholders in both languages", () => {
    const placeholders = (text: string) => (text.match(/{{\w+}}/g) ?? []).sort();
    const spanish = new Map(values(es));
    for (const [key, text] of values(en)) {
      expect(placeholders(spanish.get(key) ?? ""), key).toEqual(placeholders(text));
    }
  });

  it("are not left empty", () => {
    for (const [key, text] of [...values(en), ...values(es)]) {
      expect(text.trim(), key).not.toBe("");
    }
  });
});
