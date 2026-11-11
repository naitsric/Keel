const test = require("node:test");
const assert = require("node:assert");
const ms = require("ms");

test("a dependency setup installed is there", () => {
  assert.strictEqual(ms("2h"), 7200000);
});
