import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("OpenAPI exposes exactly 48 contract operations", async () => {
  const raw = await readFile(new URL("../../../schemas/openapi.yaml", import.meta.url), "utf8");
  const spec = JSON.parse(raw);
  const methods = new Set(["get", "post", "put", "patch", "delete"]);
  const operations = Object.values(spec.paths).flatMap((pathItem) =>
    Object.entries(pathItem).filter(([method]) => methods.has(method)),
  );
  assert.equal(operations.length, 48);
});

test("mock API answers GET /v1/me", async () => {
  const { startMockApi } = await import("../server.mjs");
  const mock = await startMockApi({ port: 0, heartbeatMs: 50 });
  try {
    const response = await fetch(`${mock.baseUrl}/v1/me`);
    assert.equal(response.status, 200);
    const body = await response.json();
    assert.equal(body.server.api_version, "v1");
  } finally {
    await mock.close();
  }
});
