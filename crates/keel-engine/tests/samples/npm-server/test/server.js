const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");

test("a server the test starts on localhost answers it", async () => {
  const server = http.createServer((request, response) => response.end("ok"));
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  try {
    const { port } = server.address();
    const response = await fetch(`http://127.0.0.1:${port}/`);
    assert.strictEqual(await response.text(), "ok");
  } finally {
    server.close();
  }
});
