import { describe, expect, it } from "vitest";
import { parseEnv } from "./SettingsView";

describe("parseEnv", () => {
  it("reads NAME=value lines, keeping any = in the value", () => {
    expect(parseEnv("A=1\n\n  B = x=y  \n")).toEqual({
      env: [
        { name: "A", value: "1" },
        { name: "B", value: "x=y" },
      ],
      invalid: null,
    });
  });

  it("says which line isn't a variable", () => {
    expect(parseEnv("A=1\nnope").invalid).toBe("nope");
    expect(parseEnv("=x").invalid).toBe("=x");
  });
});
