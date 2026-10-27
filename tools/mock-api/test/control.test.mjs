import assert from "node:assert/strict";
import test from "node:test";
import { startMockApi } from "../server.mjs";

const CONVERSATION = "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC";
const DELIVERY = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const APPROVAL = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const SELF = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const PHONE = "dev_01JB6Z4M8S3T6X9N2Q5V7W0YZN";
const WEB = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";

let mock;

test.beforeEach(async () => {
  mock = await startMockApi({ port: 0, heartbeatMs: 60_000 });
});

test.afterEach(async () => {
  await mock.close();
});

function request(path, init = {}) {
  return fetch(`${mock.baseUrl}${path}`, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init.body ? { "Content-Type": "application/json" } : {}),
      ...(init.headers || {}),
    },
  });
}

async function post(path, body, headers = {}) {
  return request(path, { method: "POST", body: JSON.stringify(body ?? {}), headers });
}

/** 打开事件流并收集帧；`until` 等到满足条件的帧出现。 */
async function stream(after) {
  const controller = new AbortController();
  const response = await fetch(`${mock.baseUrl}/v1/events?after=${after}`, {
    headers: { Accept: "text/event-stream" },
    signal: controller.signal,
  });
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const frames = [];
  let buffer = "";
  let ended = false;
  const pump = (async () => {
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let index = buffer.indexOf("\n\n");
        while (index !== -1) {
          const block = buffer.slice(0, index);
          buffer = buffer.slice(index + 2);
          const frame = {};
          for (const line of block.split("\n")) {
            const colon = line.indexOf(":");
            if (colon > 0) frame[line.slice(0, colon)] = line.slice(colon + 1).trim();
          }
          if (frame.data) frames.push({ event: frame.event, id: Number(frame.id), data: JSON.parse(frame.data) });
          index = buffer.indexOf("\n\n");
        }
      }
    } catch {
      // 中止。
    } finally {
      ended = true;
    }
  })();
  async function until(predicate, timeoutMs = 2000) {
    const started = Date.now();
    while (Date.now() - started < timeoutMs) {
      const found = frames.find(predicate);
      if (found) return found;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(`frame not received; got ${JSON.stringify(frames.map((f) => f.event))}`);
  }
  return {
    frames,
    until,
    get ended() {
      return ended;
    },
    async close() {
      controller.abort();
      await pump;
    },
  };
}

test("writes append schema-valid events and push them live without reconnecting", async () => {
  const events = await stream(18240);
  try {
    await events.until((f) => f.event === "heartbeat");
    const created = await post(
      `/v1/conversations/${CONVERSATION}/messages`,
      { text: "实时推送", refs: [] },
      { "Idempotency-Key": "live-push-message-0001" },
    );
    assert.equal(created.status, 201);
    const message = await created.json();
    const frame = await events.until((f) => f.event === "message.posted");
    assert.equal(frame.id, 18241);
    assert.equal(frame.data.subject, message.id);
    assert.equal(frame.data.project_id, WEB);
    assert.equal(frame.data.data.text, "实时推送");
    assert.equal(created.headers.get("x-event-seq"), "18241");

    // 幂等重放不再产生事件。
    const replay = await post(
      `/v1/conversations/${CONVERSATION}/messages`,
      { text: "实时推送", refs: [] },
      { "Idempotency-Key": "live-push-message-0001" },
    );
    assert.equal(replay.headers.get("idempotent-replayed"), "true");
    assert.equal(mock.state.head, 18241);
  } finally {
    await events.close();
  }
});

test("delivery, inbox, node, device and worktree writes each emit their event type", async () => {
  const before = mock.state.head;
  const key = (n) => `control-write-key-${String(n).padStart(6, "0")}`;
  assert.equal((await post(`/v1/deliveries/${DELIVERY}/pause`, {}, { "Idempotency-Key": key(1) })).status, 200);
  assert.equal((await post(`/v1/deliveries/${DELIVERY}/resume`, {}, { "Idempotency-Key": key(2) })).status, 200);
  assert.equal(
    (await request(`/v1/deliveries/${DELIVERY}`, { method: "PATCH", body: JSON.stringify({ level_cap: "Low" }) })).status,
    200,
  );
  assert.equal(
    (await request(`/v1/devices/${PHONE}`, { method: "PATCH", body: JSON.stringify({ name: "新名字" }) })).status,
    200,
  );
  assert.equal(
    (await request(`/v1/devices/${PHONE}/role`, { method: "PUT", body: JSON.stringify({ role: "viewer" }) })).status,
    200,
  );
  const escalation = mock.state.inbox.items.find((item) => item.kind === "escalation");
  const option = escalation.payload.options[0].id;
  assert.equal(
    (await post(`/v1/inbox/${escalation.id}/choose`, { option_id: option }, { "Idempotency-Key": key(3) })).status,
    200,
  );
  assert.equal((await post(`/v1/approvals/${APPROVAL}/deny`, {}, { "Idempotency-Key": key(4) })).status, 200);
  assert.equal((await post(`/v1/deliveries/${DELIVERY}/abandon`, {})).status, 200);
  const worktree = "/v1/worktrees/wtr_01JB7M8R3X6V0N4R7T2K5W0YZF/lock";
  assert.equal((await request(worktree, { method: "PUT", body: "{}" })).status, 200);
  assert.equal((await request(worktree, { method: "DELETE" })).status, 200);
  const types = mock.state.events.filter((event) => event.seq > before).map((event) => event.type);
  assert.deepEqual(types, [
    "delivery.paused",
    "delivery.resumed",
    "delivery.level_changed",
    "device.updated",
    "device.updated",
    "inbox.resolved",
    "approval.denied",
    "delivery.abandoned",
    "worktree.locked",
    "worktree.unlocked",
  ]);
  const seqs = mock.state.events.filter((event) => event.seq > before).map((event) => event.seq);
  assert.deepEqual(seqs, types.map((_, index) => before + index + 1));
});

test("/__mock/events validates data, appends and pushes live; invalid data is rejected", async () => {
  const events = await stream(18240);
  try {
    await events.until((f) => f.event === "heartbeat");
    const bad = await post("/__mock/events", { type: "delivery.paused", subject: DELIVERY, data: {} });
    assert.equal(bad.status, 400);
    assert.match((await bad.json()).error, /violates schemas\/events/);
    assert.equal(mock.state.head, 18240);

    const good = await post("/__mock/events", {
      type: "forge.synced",
      subject: DELIVERY,
      project_id: WEB,
      data: {
        target: { type: "pr", number: 12, node_id: "PR_x" },
        source: "reconcile",
        changes: [{ field: "mergeable_state", from: "blocked", to: "clean" }],
        synced_at: "2026-10-24T12:41:03.120Z",
      },
    });
    assert.equal(good.status, 201, JSON.stringify(await good.clone().json()));
    const frame = await events.until((f) => f.event === "forge.synced");
    assert.equal(frame.id, 18241);
  } finally {
    await events.close();
  }
});

test("/__mock/mutate, fail-next, requests and reset", async () => {
  const mutated = await post("/__mock/mutate", { collection: "devices", id: SELF, patch: { role: "viewer" } });
  assert.equal(mutated.status, 200);
  assert.equal((await (await request("/v1/me")).json()).device.role, "viewer");

  await post("/__mock/fail-next", { operation_id: "getMe", status: 503, code: "unavailable" });
  const failed = await request("/v1/me");
  assert.equal(failed.status, 503);
  assert.equal((await failed.json()).code, "unavailable");
  assert.equal((await request("/v1/me")).status, 200);

  const log = await (await request("/__mock/requests")).json();
  assert.deepEqual(log.map((entry) => entry.operationId), ["getMe", "getMe", "getMe"]);

  await post(`/v1/conversations/${CONVERSATION}/messages`, { text: "x", refs: [] }, {
    "Idempotency-Key": "reset-message-key-0001",
  });
  assert.equal(mock.state.head, 18241);
  const reset = await post("/__mock/reset");
  assert.deepEqual(await reset.json(), { head: 18240, log_epoch: 1 });
  assert.equal((await (await request("/v1/me")).json()).device.role, "approver");
  assert.equal((await (await request("/__mock/requests")).json()).length, 1);
});

test("/__mock/clock lets challenges expire", async () => {
  const challenge = await (await request(`/v1/approvals/${APPROVAL}/challenge`)).json();
  const clock = await post("/__mock/clock", { advance_s: 600 });
  assert.equal(clock.status, 200);
  const granted = await post(
    `/v1/approvals/${APPROVAL}/grant`,
    { nonce: challenge.payload.nonce, signature: "A".repeat(86) },
    { "Idempotency-Key": "clock-grant-key-000001" },
  );
  assert.equal(granted.status, 409);
  const problem = await granted.json();
  assert.equal(problem.code, "challenge_invalid");
  assert.equal(problem.reason, "expired");
});

test("a granted settings approval is consumed once and cannot be replayed", async () => {
  const path = "/v1/settings/models";
  const etag = (await request(path)).headers.get("etag");
  const body = JSON.stringify({
    value: {
      providers: {
        anthropic: {
          kind: "anthropic",
          base_url: "https://api.anthropic.com",
          credential: { type: "api_key", secret_ref: "model.anthropic.main", value: "sk-test-0000" },
        },
      },
    },
  });
  const dry = await (
    await request(`${path}?dry_run=true`, { method: "PUT", headers: { "If-Match": etag }, body })
  ).json();
  const approval = await (await post("/v1/approvals", dry.signing, { "Idempotency-Key": "consume-approval-00001" })).json();
  const challenge = await (await request(`/v1/approvals/${approval.id}/challenge`)).json();
  await post(
    `/v1/approvals/${approval.id}/grant`,
    { nonce: challenge.payload.nonce, signature: "A".repeat(86) },
    { "Idempotency-Key": "consume-grant-0000001" },
  );
  // 摘要不匹配（请求体变了）时不能借用这个审批。
  const tampered = await request(path, {
    method: "PUT",
    headers: { "If-Match": etag, "Gantry-Approval": approval.id },
    body: body.replace("sk-test-0000", "sk-test-9999"),
  });
  assert.equal(tampered.status, 403);
  const ok = await request(path, { method: "PUT", headers: { "If-Match": etag, "Gantry-Approval": approval.id }, body });
  assert.equal(ok.status, 200);
  const next = ok.headers.get("etag");
  const replay = await request(path, { method: "PUT", headers: { "If-Match": next, "Gantry-Approval": approval.id }, body });
  assert.equal(replay.status, 403);
  const types = mock.state.events.slice(-3).map((event) => event.type);
  assert.deepEqual(types.slice(0, 3), ["approval.granted", "approval.consumed", "settings.updated"]);
  const updated = mock.state.events.at(-1);
  assert.deepEqual(updated.data.secret_changes, [{ ref: "model.anthropic.main", op: "set", version: 18201 }]);
  assert.equal(JSON.stringify(mock.state.events).includes("sk-test-0000"), false);
});

test("revoking this device ends established event streams", async () => {
  const events = await stream(18240);
  await events.until((f) => f.event === "heartbeat");
  const revoked = await post(`/v1/devices/${SELF}/revoke`, {});
  assert.equal(revoked.status, 200);
  await events.until((f) => f.event === "device.revoked");
  const started = Date.now();
  while (!events.ended && Date.now() - started < 2000) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.equal(events.ended, true);
  await events.close();
});
