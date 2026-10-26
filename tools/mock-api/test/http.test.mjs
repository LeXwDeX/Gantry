import assert from "node:assert/strict";
import test from "node:test";
import { startMockApi } from "../server.mjs";

let mock;

test.before(async () => {
  mock = await startMockApi({ port: 0, heartbeatMs: 40 });
});

test.after(async () => {
  await mock.close();
});

async function request(path, init = {}) {
  return fetch(`${mock.baseUrl}${path}`, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init.body ? { "Content-Type": "application/json" } : {}),
      ...(init.headers || {}),
    },
  });
}

async function json(response) {
  return response.json();
}

function assertNoCors(response) {
  assert.equal(response.headers.get("access-control-allow-origin"), null);
}

function assertProblemHeaders(response) {
  assert.match(response.headers.get("content-type") || "", /^application\/problem\+json/);
  assert.ok(response.headers.get("x-request-id"));
  assert.equal(response.headers.get("x-log-epoch"), "1");
  assert.ok(response.headers.get("ratelimit-limit"));
  assertNoCors(response);
}

async function openSse(path, headers = {}) {
  const controller = new AbortController();
  const response = await fetch(`${mock.baseUrl}${path}`, {
    headers: { Accept: "text/event-stream", ...headers },
    signal: controller.signal,
  });
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") || "", /^text\/event-stream/);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const frames = [];
  async function nextFrame() {
    while (true) {
      const separator = buffer.indexOf("\n\n");
      if (separator !== -1) {
        const block = buffer.slice(0, separator);
        buffer = buffer.slice(separator + 2);
        const frame = { event: "message", data: null, id: null, retry: null };
        for (const line of block.split("\n")) {
          if (line.startsWith("event: ")) frame.event = line.slice(7);
          else if (line.startsWith("data: ")) frame.data = line.slice(6);
          else if (line.startsWith("id: ")) frame.id = line.slice(4);
          else if (line.startsWith("retry: ")) frame.retry = line.slice(7);
        }
        frames.push(frame);
        return frame;
      }
      const { done, value } = await reader.read();
      if (done) {
        throw new Error("SSE stream ended before a complete frame");
      }
      buffer += decoder.decode(value, { stream: true });
    }
  }
  return {
    response,
    frames,
    nextFrame,
    close() {
      controller.abort();
      reader.cancel().catch(() => {});
    },
  };
}

test("404 and 405 use RFC 9457 problem responses and no CORS", async () => {
  const notFound = await request("/v1/nope");
  assert.equal(notFound.status, 404);
  assertProblemHeaders(notFound);
  const notFoundBody = await json(notFound);
  assert.equal(notFoundBody.code, "not_found");
  assert.equal(notFoundBody.type, "urn:gantry:problem:not_found");
  assert.ok(notFoundBody.request_id);

  const wrongMethod = await request("/v1/me", { method: "POST" });
  assert.equal(wrongMethod.status, 405);
  assertProblemHeaders(wrongMethod);
  assert.match(wrongMethod.headers.get("allow") || "", /GET/);
  assert.equal((await json(wrongMethod)).code, "method_not_allowed");

  const options = await request("/v1/me", { method: "OPTIONS" });
  assert.equal(options.status, 405);
  assert.match(options.headers.get("allow") || "", /GET/);

  const pairOnApiListener = await request("/v1/pair");
  assert.equal(pairOnApiListener.status, 404);
  assert.equal((await json(pairOnApiListener)).code, "not_found");
});

test("static settings routes are not swallowed by {resource}", async () => {
  const catalog = await request("/v1/settings/catalog");
  assert.equal(catalog.status, 200);
  assert.equal((await json(catalog)).schema_version, 1);

  const status = await request("/v1/settings/status");
  assert.equal(status.status, 200);
  assert.equal((await json(status)).latest_rev, 18238);

  const unknownResource = await request("/v1/settings/nope");
  assert.equal(unknownResource.status, 404);
});

test("pagination uses opaque keyset cursors and rejects unknown query parameters", async () => {
  const first = await request("/v1/projects?limit=1");
  assert.equal(first.status, 200);
  const firstBody = await json(first);
  assert.equal(firstBody.items.length, 1);
  assert.ok(firstBody.next_cursor);
  assert.ok(Number(first.headers.get("x-event-seq")) >= 0);

  const second = await request(`/v1/projects?limit=1&cursor=${encodeURIComponent(firstBody.next_cursor)}`);
  assert.equal(second.status, 200);
  const secondBody = await json(second);
  assert.notEqual(secondBody.items[0].id, firstBody.items[0].id);

  const badLimit = await request("/v1/projects?limit=0");
  assert.equal(badLimit.status, 400);
  assertProblemHeaders(badLimit);
  assert.equal((await json(badLimit)).code, "invalid_request");

  const unknown = await request("/v1/projects?unexpected=1");
  assert.equal(unknown.status, 400);
  assertProblemHeaders(unknown);
  assert.equal((await json(unknown)).code, "invalid_request");

  const badCursor = await request("/v1/projects?cursor=not-a-cursor");
  assert.equal(badCursor.status, 400);
  assert.equal((await json(badCursor)).code, "invalid_cursor");

  const messages = await request(
    "/v1/conversations/cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC/messages",
  );
  assert.equal(messages.status, 200);
  const messageBody = await json(messages);
  assert.ok(messageBody.items.length >= 2);
  assert.ok(messageBody.items[0].id > messageBody.items[1].id);
});

test("device responses never expose approval_pubkey", async () => {
  const devices = await request("/v1/devices");
  assert.equal(devices.status, 200);
  const text = JSON.stringify(await json(devices));
  assert.equal(text.includes('"approval_pubkey"'), false);
  const me = JSON.stringify(await json(await request("/v1/me")));
  assert.equal(me.includes('"approval_pubkey"'), false);
  assert.ok(me.includes("approval_pubkey_fpr"));
});

test("write paths validate schema, idempotency keys and content type", async () => {
  const path = "/v1/conversations/cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC/messages";
  const missingKey = await request(path, {
    method: "POST",
    body: JSON.stringify({ text: "hello" }),
  });
  assert.equal(missingKey.status, 400);
  assertProblemHeaders(missingKey);
  assert.equal((await json(missingKey)).code, "idempotency_key_required");

  const key = "message-key-0000000001";
  const invalid = await request(path, {
    method: "POST",
    headers: { "Idempotency-Key": key },
    body: JSON.stringify({ text: "hello", model: "forbidden" }),
  });
  assert.equal(invalid.status, 422);
  assertProblemHeaders(invalid);
  const invalidBody = await json(invalid);
  assert.equal(invalidBody.code, "validation_failed");
  assert.ok(invalidBody.errors.some((error) => error.code === "unknown_field" && error.pointer === "/model"));

  const wrongType = await request(path, {
    method: "POST",
    headers: { "Idempotency-Key": "message-key-0000000002", "Content-Type": "text/plain" },
    body: "hello",
  });
  assert.equal(wrongType.status, 415);
  assertProblemHeaders(wrongType);

  const created = await request(path, {
    method: "POST",
    headers: { "Idempotency-Key": "message-key-0000000003" },
    body: JSON.stringify({ text: "继续处理登录超时", level_cap: "High" }),
  });
  assert.equal(created.status, 201);
  assert.equal(created.headers.get("x-event-seq"), "18240");
  const createdBody = await json(created);
  assert.equal(createdBody.role, "user");
  assert.equal(createdBody.text, "继续处理登录超时");
  assert.match(createdBody.created_at, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
  assert.match(createdBody.updated_at, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);

  const replay = await request(path, {
    method: "POST",
    headers: { "Idempotency-Key": "message-key-0000000003" },
    body: JSON.stringify({ text: "继续处理登录超时", level_cap: "High" }),
  });
  assert.equal(replay.status, 201);
  assert.equal(replay.headers.get("idempotent-replayed"), "true");
  assert.equal((await json(replay)).id, createdBody.id);

  const reuse = await request(path, {
    method: "POST",
    headers: { "Idempotency-Key": "message-key-0000000003" },
    body: JSON.stringify({ text: "不同内容" }),
  });
  assert.equal(reuse.status, 422);
  assert.equal((await json(reuse)).code, "idempotency_key_reuse");

  const messages = await json(await request(path));
  assert.equal(messages.items.filter((message) => message.text === "继续处理登录超时").length, 1);
});

test("all read routes answer with schema-valid fixtures", async () => {
  const paths = [
    "/v1/me",
    "/v1/projects",
    "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
    "/v1/deliveries",
    "/v1/deliveries/dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
    "/v1/deliveries/dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB/budget",
    "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA/conversations",
    "/v1/conversations/cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC/messages",
    "/v1/worktrees",
    "/v1/worktrees/wtr_01JB7M8Q2X5V9N3R6T1K4W0YZE",
    "/v1/deliveries/dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB/workflows",
    "/v1/workflows/wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF",
    "/v1/nodes/nod_01JB7M7W5X3N9R6T8Q2K4V1YZH/attempts",
    "/v1/attempts/att_01JB7M9X5N3R8T2Q6K1V4W7YZH",
    "/v1/inbox",
    "/v1/inbox/inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ",
    "/v1/inbox/summary",
    "/v1/approvals",
    "/v1/approvals/apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
    "/v1/approvals/apr_01JB7P4M8T2X6N9R3Q5K1V7WZK/challenge",
    "/v1/settings/models",
    "/v1/settings/levels",
    "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA/settings/levels",
    "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA/settings/levels?view=effective",
    "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA/settings/notify",
    "/v1/settings/catalog",
    "/v1/settings/status",
    "/v1/devices",
  ];
  for (const path of paths) {
    const response = await request(path);
    const text = await response.text();
    assert.equal(response.status, 200, `${path} returned ${response.status}: ${text}`);
    assertNoCors(response);
  }
});

test("pairing and push-token registration stay inside the mock boundary", async () => {
  const pairing = await request("/v1/pair", {
    method: "POST",
    body: JSON.stringify({
      token: "gp1_mock_token",
      csr: "-----BEGIN CERTIFICATE REQUEST-----\nmock\n-----END CERTIFICATE REQUEST-----\n",
      approval_pubkey: "BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7ZaASo8h6hMNHwYgitHzpH-S9kLN8ImOTHNk0q-vuB-sTk",
      device: { name: "Mock Mac", platform: "macos" },
    }),
  });
  assert.equal(pairing.status, 201);
  const missingPubkey = await request("/v1/pair", {
    method: "POST",
    body: JSON.stringify({
      token: "gp1_mock_token",
      csr: "csr",
      device: { name: "Mock Mac", platform: "macos" },
    }),
  });
  assert.equal(missingPubkey.status, 422);
  assert.equal((await json(missingPubkey)).code, "validation_failed");

  const pairingText = JSON.stringify(await json(pairing));
  assert.equal(pairingText.includes('"approval_pubkey"'), false);
  assert.equal(pairingText.includes("approval_pubkey_fpr"), true);

  const token = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
  const registered = await request("/v1/devices/self/push-token", {
    method: "PUT",
    body: JSON.stringify({ token, environment: "production" }),
  });
  assert.equal(registered.status, 204);
  assert.equal(registered.headers.get("content-type"), null);
  const devicesText = JSON.stringify(await json(await request("/v1/devices")));
  assert.equal(devicesText.includes(token), false);
  const events = await openSse("/v1/events");
  try {
    const frame = await events.nextFrame();
    while (frame.data === null) {
      const next = await events.nextFrame();
      if (next.data !== null) {
        assert.equal(next.event, "heartbeat");
        break;
      }
    }
  } finally {
    events.close();
  }
});

test("settings use ETags, strict bodies and never return secret values", async () => {
  const path = "/v1/settings/models";
  const initial = await request(path);
  assert.equal(initial.status, 200);
  const initialEtag = initial.headers.get("etag");
  assert.equal(initialEtag, '"18200"');
  assert.equal(JSON.stringify(await json(initial)).includes("secret-value"), false);

  const noMatch = await request(path, {
    method: "PUT",
    body: JSON.stringify({ value: {} }),
  });
  assert.equal(noMatch.status, 428);
  assert.equal((await json(noMatch)).code, "precondition_required");

  const stale = await request(path, {
    method: "PUT",
    headers: { "If-Match": '"18199"' },
    body: JSON.stringify({ value: {} }),
  });
  assert.equal(stale.status, 412);
  assert.equal(stale.headers.get("etag"), initialEtag);

  const dryRun = await request(`${path}?dry_run=true`, {
    method: "PUT",
    headers: { "If-Match": initialEtag },
    body: JSON.stringify({ value: {} }),
  });
  assert.equal(dryRun.status, 200);
  const dryRunBody = await json(dryRun);
  assert.equal(dryRunBody.valid, true);
  assert.equal(dryRunBody.requires_signature, false);

  const secretValue = "super-secret-value-that-must-not-appear";
  const written = await request(path, {
    method: "PUT",
    headers: { "If-Match": initialEtag },
    body: JSON.stringify({
      value: {
        providers: {
          anthropic: {
            kind: "anthropic",
            base_url: "https://api.anthropic.com",
            credential: {
              type: "api_key",
              secret_ref: "model.anthropic.main",
              value: secretValue,
            },
          },
        },
      },
    }),
  });
  assert.equal(written.status, 200);
  const writtenText = JSON.stringify(await json(written));
  assert.equal(writtenText.includes(secretValue), false);
  const writtenBody = JSON.parse(writtenText);
  assert.equal(writtenBody.value.providers.anthropic.credential.value, undefined);
  assert.equal(writtenBody.value.providers.anthropic.credential.present, true);
  assert.equal(written.headers.get("etag"), '"18201"');

  const fetched = await json(await request(path));
  assert.equal(JSON.stringify(fetched).includes(secretValue), false);
  assert.equal(fetched.value.providers.anthropic.credential.value, undefined);
  assert.equal(JSON.stringify(mock.app.requestLog).includes(secretValue), false);
});

test("project settings provide override and effective views", async () => {
  const base = "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA/settings/levels";
  const override = await request(base);
  assert.equal(override.status, 200);
  const overrideBody = await json(override);
  assert.equal(overrideBody.scope, "project");
  assert.equal(overrideBody.value.high.budget.nodes, 20);

  const effective = await request(`${base}?view=effective`);
  assert.equal(effective.status, 200);
  const effectiveBody = await json(effective);
  assert.equal(effectiveBody.scope, "effective");
  assert.equal(effectiveBody.value.high.budget.nodes, 20);
  assert.equal(effectiveBody.value.low.budget.nodes, 6);

  const notOverridable = await request(
    "/v1/projects/prj_01JB7K2M9Q4X8V3N5R6T1W0YZA/settings/notify",
    {
      method: "PUT",
      headers: { "If-Match": '"0"' },
      body: JSON.stringify({ value: {} }),
    },
  );
  assert.equal(notOverridable.status, 422);
  assert.equal((await json(notOverridable)).code, "not_overridable");
});

test("delivery controls are scripted and idempotent", async () => {
  const path = "/v1/deliveries/dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
  const paused = await request(`${path}/pause`, {
    method: "POST",
    headers: { "Idempotency-Key": "pause-key-000000000001" },
    body: "{}",
  });
  assert.equal(paused.status, 200);
  assert.equal((await json(paused)).stage_status, "paused");

  const resumed = await request(`${path}/resume`, {
    method: "POST",
    headers: { "Idempotency-Key": "resume-key-00000000001" },
    body: "{}",
  });
  assert.equal(resumed.status, 200);
  assert.equal((await json(resumed)).stage_status, "running");

  const patched = await request(path, {
    method: "PATCH",
    body: JSON.stringify({ level_cap: "Low" }),
  });
  assert.equal(patched.status, 200);
  const patchedBody = await json(patched);
  assert.equal(patchedBody.level_cap, "Low");
  assert.equal(patchedBody.level_effective, "Low");
});

test("SSE replays from cursors, honors Last-Event-ID and filters without losing the heartbeat cursor", async () => {
  const all = await openSse("/v1/events?after=18233");
  try {
    assert.equal(all.frames.length, 0);
    const ids = [];
    let heartbeat;
    for (let index = 0; index < 20; index += 1) {
      const frame = await all.nextFrame();
      if (frame.data === null) continue;
      if (frame.event === "heartbeat") {
        heartbeat = frame;
        break;
      }
      ids.push(Number(frame.id));
      const payload = JSON.parse(frame.data);
      assert.equal(payload.seq, Number(frame.id));
    }
    assert.deepEqual(ids, [18234, 18235, 18236, 18237, 18238, 18239, 18240]);
    assert.equal(heartbeat.id, "18240");
    assert.deepEqual(JSON.parse(heartbeat.data), {
      seq: 18240,
      ts: "2026-10-24T12:41:03.456Z",
      log_epoch: 1,
    });
    assert.equal(all.response.headers.get("x-log-epoch"), "1");
    assert.equal(all.response.headers.get("x-event-seq"), "18240");
    assert.equal(all.response.headers.get("x-accel-buffering"), "no");
    assertNoCors(all.response);
  } finally {
    all.close();
  }

  const headerWins = await openSse("/v1/events?after=18234", {
    "Last-Event-ID": "18236",
  });
  try {
    let first = await headerWins.nextFrame();
    while (first.data === null) first = await headerWins.nextFrame();
    assert.equal(first.event, "inbox.raised");
    assert.equal(first.id, "18237");
  } finally {
    headerWins.close();
  }

  const filtered = await openSse(
    "/v1/events?after=18233&types=message.*&project_id=prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  );
  try {
    const ids = [];
    let heartbeat;
    for (let index = 0; index < 20; index += 1) {
      const frame = await filtered.nextFrame();
      if (frame.data === null) continue;
      if (frame.event === "heartbeat") {
        heartbeat = frame;
        break;
      }
      assert.match(frame.event, /^message\./);
      ids.push(Number(frame.id));
    }
    assert.deepEqual(ids, [18235, 18236, 18240]);
    assert.equal(heartbeat.id, "18240");
  } finally {
    filtered.close();
  }

  const global = await openSse(
    "/v1/events?after=18233&types=settings.*&project_id=prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  );
  try {
    let first = await global.nextFrame();
    while (first.data === null) first = await global.nextFrame();
    assert.equal(first.id, "18238");
    assert.equal(first.event, "settings.updated");
    let heartbeat = await global.nextFrame();
    while (heartbeat.data === null) heartbeat = await global.nextFrame();
    assert.equal(heartbeat.event, "heartbeat");
  } finally {
    global.close();
  }

  const noCursor = await openSse("/v1/events");
  try {
    let first = await noCursor.nextFrame();
    while (first.data === null) first = await noCursor.nextFrame();
    assert.equal(first.event, "heartbeat");
    assert.equal(first.id, "18240");
    assert.equal(JSON.parse(first.data).log_epoch, 1);
  } finally {
    noCursor.close();
  }

  const zero = await openSse("/v1/events?after=0&types=delivery.stage_changed");
  try {
    let first = await zero.nextFrame();
    while (first.data === null) first = await zero.nextFrame();
    assert.equal(first.id, "18234");
  } finally {
    zero.close();
  }

  const malformed = await request("/v1/events", { headers: { "Last-Event-ID": "12x" } });
  assert.equal(malformed.status, 400);
  assertProblemHeaders(malformed);
  assert.equal((await json(malformed)).code, "invalid_cursor");

  const ahead = await request("/v1/events", { headers: { "Last-Event-ID": "99999" } });
  assert.equal(ahead.status, 409);
  assertProblemHeaders(ahead);
  assert.equal((await json(ahead)).code, "cursor_ahead");
});
