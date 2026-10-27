// 契约行为用例集：同一套用例对 Mock API 与真实 `gantry-api` 各跑一遍（api.md §12）。
//
// 只收与夹具数据无关、两边都已接入的路由上的契约行为；依赖 Mock 夹具或真实实现尚未
// 接入的路由（项目 / 交付单元 / 工作流列表等）的用例仍留在 Mock 自己的测试里。
// 只用 Node 内置的 fetch 与 node:test，不依赖 npm 包，以便 Rust 侧的一致性测试直接调用。

import assert from "node:assert/strict";

const PROBLEM = /^application\/problem\+json/;

/**
 * @param {{ test: Function, base: () => string, name: string }} opts
 */
export function sharedBehavior({ test, base, name }) {
  async function request(path, init = {}) {
    return fetch(`${base()}${path}`, {
      ...init,
      headers: {
        Accept: "application/json",
        ...(init.body ? { "Content-Type": "application/json" } : {}),
        ...(init.headers || {}),
      },
    });
  }

  async function problem(response, status, code) {
    assert.equal(response.status, status, `${name}: expected ${status}`);
    assert.match(response.headers.get("content-type") || "", PROBLEM);
    const body = await response.json();
    assert.equal(body.code, code);
    assert.equal(body.status, status);
    assert.equal(body.type, `urn:gantry:problem:${code}`);
    assert.equal(response.headers.get("x-request-id"), body.request_id);
    assert.ok(response.headers.get("x-log-epoch"), "X-Log-Epoch on every response");
    assert.equal(response.headers.get("access-control-allow-origin"), null, "no CORS");
    return body;
  }

  test(`[${name}] 404 / 405 are RFC 9457 problems with Allow and no CORS`, async () => {
    await problem(await request("/v1/nope"), 404, "not_found");
    await problem(await request("/webhook"), 404, "not_found");
    const resp = await request("/v1/me", { method: "DELETE" });
    await problem(resp, 405, "method_not_allowed");
    assert.equal(resp.headers.get("allow"), "GET");
  });

  test(`[${name}] literal settings routes are not swallowed by {resource}`, async () => {
    for (const path of ["/v1/settings/catalog", "/v1/settings/status"]) {
      const resp = await request(path);
      assert.equal(resp.status, 200, path);
      assert.ok(resp.headers.get("x-event-seq"), "X-Event-Seq on success");
    }
    await problem(await request("/v1/settings/nope"), 404, "not_found");
  });

  test(`[${name}] path and query parameters follow the OpenAPI patterns`, async () => {
    await problem(await request("/v1/projects/oops"), 400, "invalid_request");
    await problem(await request("/v1/inbox?unknown=1"), 400, "invalid_request");
    await problem(await request("/v1/inbox?limit=201"), 400, "invalid_request");
    await problem(await request("/v1/events?project_id=oops"), 400, "invalid_request");
  });

  test(`[${name}] write paths check Idempotency-Key and Content-Type`, async () => {
    const path = "/v1/conversations/cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC/messages";
    await problem(
      await request(path, { method: "POST", body: JSON.stringify({ text: "hi" }) }),
      400,
      "idempotency_key_required",
    );
    await problem(
      await request(path, {
        method: "POST",
        body: JSON.stringify({ text: "hi" }),
        headers: { "Idempotency-Key": "short" },
      }),
      400,
      "invalid_request",
    );
    await problem(
      await request(path, {
        method: "POST",
        body: "hi",
        headers: { "Content-Type": "text/plain", "Idempotency-Key": "01JB7P9Z3K5R8T2X6N1Q4V7WYA" },
      }),
      415,
      "unsupported_media_type",
    );
  });

  test(`[${name}] settings use ETags and require If-Match`, async () => {
    const resp = await request("/v1/settings/models");
    assert.equal(resp.status, 200);
    const etag = resp.headers.get("etag");
    assert.match(etag || "", /^"\d+"$/);
    const again = await request("/v1/settings/models", { headers: { "If-None-Match": etag } });
    assert.equal(again.status, 304);
    await problem(
      await request("/v1/settings/models", { method: "PUT", body: JSON.stringify({ value: {} }) }),
      428,
      "precondition_required",
    );
    const stale = await request("/v1/settings/models", {
      method: "PUT",
      body: JSON.stringify({ value: {} }),
      headers: { "If-Match": '"999999999"' },
    });
    await problem(stale, 412, "precondition_failed");
    assert.equal(stale.headers.get("etag"), etag);
  });

  test(`[${name}] event stream cursors`, async () => {
    await problem(await request("/v1/events", { headers: { "Last-Event-ID": "abc" } }), 400, "invalid_cursor");
    await problem(
      await request("/v1/events", { headers: { "Last-Event-ID": "9007199254740990" } }),
      409,
      "cursor_ahead",
    );
    const controller = new AbortController();
    const resp = await fetch(`${base()}/v1/events`, {
      headers: { Accept: "text/event-stream" },
      signal: controller.signal,
    });
    assert.equal(resp.status, 200);
    assert.match(resp.headers.get("content-type") || "", /^text\/event-stream/);
    assert.ok(resp.headers.get("x-log-epoch"));
    const reader = resp.body.getReader();
    const { value } = await reader.read();
    assert.ok(new TextDecoder().decode(value).startsWith("retry: 3000\n\n"));
    controller.abort();
  });

  test(`[${name}] device representations never expose approval_pubkey`, async () => {
    const me = await (await request("/v1/me")).json();
    assert.equal(me.device.approval_pubkey, undefined);
    assert.ok(me.device.approval_pubkey_fpr);
    const devices = await (await request("/v1/devices")).json();
    for (const device of devices.items) {
      assert.equal(device.approval_pubkey, undefined);
    }
  });
}
