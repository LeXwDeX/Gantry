import assert from "node:assert/strict";
import test from "node:test";
import { startMockApi } from "../server.mjs";

const PROJECT = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY_RUNNING = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const DELIVERY_CLOSED = "dlv_01JB7M5P9R3X7V2N6Q4T8W1YZD";
const APPROVAL = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const NODE = "nod_01JB7M7W5X3N9R6T8Q2K4V1YZH";
const OPEN_INBOX = "inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ";
const ESCALATION_INBOX = "inb_01JB7P3M7S1T4X6N9Q2V5W8YZK";
const OTHER_DEVICE = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZN";
const SIGNATURE =
  "QAzQTiBfxzxWOP2G_q8M4i405Cr8GOZvAFZlbZGCC96ICBp04L9nG2vAWEbWF-jQAkPYVrwZjbBeJGf8nUdDLg";

async function withMock(fn) {
  const mock = await startMockApi({ port: 0, heartbeatMs: 40 });
  try {
    return await fn(mock);
  } finally {
    await mock.close();
  }
}

function request(mock, path, init = {}) {
  return fetch(`${mock.baseUrl}${path}`, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init.body ? { "Content-Type": "application/json" } : {}),
      ...(init.headers || {}),
    },
  });
}

function deliveryControl(mock, action, key) {
  return request(mock, `/v1/deliveries/${DELIVERY_RUNNING}/${action}`, {
    method: "POST",
    headers: { "Idempotency-Key": key },
    body: "{}",
  });
}

async function assertDeliveryStatus(mock, action, key, expected) {
  const response = await deliveryControl(mock, action, key);
  assert.equal(response.status, 200);
  assert.equal((await response.json()).stage_status, expected);
}

function dismissInbox(mock, id) {
  return request(mock, `/v1/inbox/${id}/dismiss`, { method: "POST", body: "{}" });
}

function chooseInboxOption(mock, id, optionId, key) {
  return request(mock, `/v1/inbox/${id}/choose`, {
    method: "POST",
    headers: { "Idempotency-Key": key },
    body: JSON.stringify({ option_id: optionId }),
  });
}

async function openSse(mock, path, headers = {}) {
  const controller = new AbortController();
  const response = await fetch(`${mock.baseUrl}${path}`, {
    headers: { Accept: "text/event-stream", ...headers },
    signal: controller.signal,
  });
  assert.equal(response.status, 200);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  async function nextFrame() {
    while (true) {
      const separator = buffer.indexOf("\n\n");
      if (separator !== -1) {
        const block = buffer.slice(0, separator);
        buffer = buffer.slice(separator + 2);
        const frame = { event: "message", data: null, id: null };
        for (const line of block.split("\n")) {
          if (line.startsWith("event: ")) frame.event = line.slice(7);
          else if (line.startsWith("data: ")) frame.data = line.slice(6);
          else if (line.startsWith("id: ")) frame.id = line.slice(4);
        }
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
    nextFrame,
    close() {
      controller.abort();
      reader.cancel().catch(() => {});
    },
  };
}

test("path and query parameters follow the OpenAPI patterns without hiding unknown settings", async () => {
  await withMock(async (mock) => {
    const badQuery = await request(mock, "/v1/deliveries?project_id=oops");
    assert.equal(badQuery.status, 400);
    assert.equal((await badQuery.json()).code, "invalid_request");

    const badPath = await request(mock, "/v1/projects/oops");
    assert.equal(badPath.status, 400);
    assert.equal((await badPath.json()).code, "invalid_request");

    const badEventQuery = await request(mock, "/v1/events?project_id=oops");
    assert.equal(badEventQuery.status, 400);
    assert.equal((await badEventQuery.json()).code, "invalid_request");

    const unknownGlobal = await request(mock, "/v1/settings/nope");
    assert.equal(unknownGlobal.status, 404);
    assert.equal((await unknownGlobal.json()).code, "not_found");

    const unknownProject = await request(mock, `/v1/projects/${PROJECT}/settings/nope`);
    assert.equal(unknownProject.status, 404);
    assert.equal((await unknownProject.json()).code, "not_found");
  });
});

test("idempotency fingerprints the concrete path and scopes by device", async () => {
  await withMock(async (mock) => {
    const key = "pause-key-shared-00001";
    const pathA = `/v1/deliveries/${DELIVERY_RUNNING}/pause`;
    const pathB = `/v1/deliveries/${DELIVERY_CLOSED}/pause`;

    const first = await request(mock, pathA, {
      method: "POST",
      headers: { "Idempotency-Key": key },
      body: "{}",
    });
    assert.equal(first.status, 200);

    const replay = await request(mock, pathA, {
      method: "POST",
      headers: { "Idempotency-Key": key },
      body: "{}",
    });
    assert.equal(replay.status, 200);
    assert.equal(replay.headers.get("idempotent-replayed"), "true");

    const crossResource = await request(mock, pathB, {
      method: "POST",
      headers: { "Idempotency-Key": key },
      body: "{}",
    });
    assert.equal(crossResource.status, 422);
    assert.equal((await crossResource.json()).code, "idempotency_key_reuse");

    const originalDevice = mock.state.me.device.id;
    mock.state.me.device.id = OTHER_DEVICE;
    try {
      const otherDevice = await request(mock, pathA, {
        method: "POST",
        headers: { "Idempotency-Key": key },
        body: "{}",
      });
      assert.equal(otherDevice.status, 200);
      assert.equal(otherDevice.headers.get("idempotent-replayed"), null);
    } finally {
      mock.state.me.device.id = originalDevice;
    }
  });
});

test("Last-Event-ID wins over after but does not bypass other parameter checks", async () => {
  await withMock(async (mock) => {
    const stream = await openSse(mock, "/v1/events?after=not-a-number", {
      "Last-Event-ID": "18236",
    });
    try {
      let first = await stream.nextFrame();
      while (first.data === null) first = await stream.nextFrame();
      assert.equal(first.id, "18237");
      assert.equal(first.event, "inbox.raised");
    } finally {
      stream.close();
    }

    const invalidAfter = await request(mock, "/v1/events?after=not-a-number");
    assert.equal(invalidAfter.status, 400);
    assert.equal((await invalidAfter.json()).code, "invalid_request");

    const invalidOther = await request(
      mock,
      "/v1/events?after=not-a-number&project_id=oops",
      { headers: { "Last-Event-ID": "18236" } },
    );
    assert.equal(invalidOther.status, 400);
    assert.equal((await invalidOther.json()).code, "invalid_request");
  });
});

test("node retry rejects superseded nodes and paused or terminal deliveries", async () => {
  await withMock(async (mock) => {
    const workflow = mock.state.workflows.items[0];
    const node = workflow.nodes.find((item) => item.id === NODE);
    const delivery = mock.state.deliveries.items.find((item) => item.id === workflow.delivery_id);
    assert.ok(delivery);
    // Mock precondition: the fixture ships this node as succeeded.
    node.status = "failed";

    const retry = (key) =>
      request(mock, `/v1/nodes/${NODE}/retry`, {
        method: "POST",
        headers: { "Idempotency-Key": key },
        body: "{}",
      });
    const expectInvalidState = async (key) => {
      const nodesBefore = structuredClone(mock.state.workflows.items);
      const attemptsBefore = structuredClone(mock.state.attempts.items);
      const response = await retry(key);
      assert.equal(response.status, 409);
      const problem = await response.json();
      assert.equal(problem.code, "conflict");
      assert.equal(problem.reason, "invalid_state");
      assert.deepEqual(mock.state.workflows.items, nodesBefore);
      assert.deepEqual(mock.state.attempts.items, attemptsBefore);
    };

    const nodesBefore = structuredClone(mock.state.workflows.items);
    const attemptsBefore = structuredClone(mock.state.attempts.items);
    const retried = await retry("retry-key-normal-00001");
    assert.equal(retried.status, 202);
    const retriedBody = await retried.json();
    assert.equal(retriedBody.id, NODE);
    assert.equal(retriedBody.status, "failed");
    assert.deepEqual(mock.state.workflows.items, nodesBefore);
    assert.deepEqual(mock.state.attempts.items, attemptsBefore);

    const replay = await retry("retry-key-normal-00001");
    assert.equal(replay.status, 202);
    assert.equal(replay.headers.get("idempotent-replayed"), "true");
    assert.equal((await replay.json()).status, "failed");
    assert.deepEqual(mock.state.workflows.items, nodesBefore);
    assert.deepEqual(mock.state.attempts.items, attemptsBefore);

    node.status = "succeeded";
    await expectInvalidState("retry-key-not-failed-01");
    node.status = "failed";

    // Mock precondition: inject superseded / paused / terminal states the fixture does not contain.
    node.superseded_by = "nod_01JB7M7V4X2N9R5T8Q3K6W1YZG";
    await expectInvalidState("retry-key-superseded-1");
    node.superseded_by = null;

    delivery.stage_status = "paused";
    await expectInvalidState("retry-key-paused-000001");
    delivery.stage_status = "running";

    delivery.stage = "closed";
    await expectInvalidState("retry-key-closed-000001");
    delivery.stage = "abandoned";
    await expectInvalidState("retry-key-abandoned-01");
    delivery.stage = "review";

    // Mock precondition: a workflow without a delivery stays retryable.
    workflow.delivery_id = null;
    const detachedNodes = structuredClone(mock.state.workflows.items);
    const detachedAttempts = structuredClone(mock.state.attempts.items);
    const detached = await retry("retry-key-detached-001");
    assert.equal(detached.status, 202);
    assert.equal((await detached.json()).status, "failed");
    assert.deepEqual(mock.state.workflows.items, detachedNodes);
    assert.deepEqual(mock.state.attempts.items, detachedAttempts);
  });
});

test("resume keeps waiting_human for no action, dismissed and empty blocker lists", async () => {
  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    // Mock precondition: start from waiting_human; the mock has no engine to raise blockers.
    delivery.stage_status = "waiting_human";

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000001", "paused");
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000001", "waiting_human");

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000002", "paused");
    for (const id of [OPEN_INBOX, ESCALATION_INBOX]) {
      const dismissed = await dismissInbox(mock, id);
      assert.equal(dismissed.status, 200);
    }
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000002", "waiting_human");

    // The previous dismissed snapshot still keeps the delivery waiting.
    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000003", "paused");
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000003", "waiting_human");

    // A delivery paused while running still returns to running.
    delivery.stage_status = "running";
    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000004", "paused");
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000004", "running");
  });

  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    for (const id of [OPEN_INBOX, ESCALATION_INBOX]) {
      const dismissed = await dismissInbox(mock, id);
      assert.equal(dismissed.status, 200);
    }
    delivery.stage_status = "waiting_human";

    // No blocker is open when the pause snapshot is created.
    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000012", "paused");
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000012", "waiting_human");
  });
});

test("resume does not treat a removed blocker as resolved", async () => {
  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    // Mock precondition: start from waiting_human; the mock has no engine to raise blockers.
    delivery.stage_status = "waiting_human";

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000005", "paused");

    // Mock precondition: remove the recorded blockers to simulate items that disappeared.
    mock.state.inbox.items = mock.state.inbox.items.filter(
      (item) => item.delivery_id !== DELIVERY_RUNNING,
    );

    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000005", "waiting_human");
  });
});

test("resume clears waiting_human only when every recorded blocker resolves", async () => {
  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    // Mock precondition: start from waiting_human; the mock has no engine to raise blockers.
    delivery.stage_status = "waiting_human";

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000006", "paused");
    const chosen = await chooseInboxOption(
      mock,
      ESCALATION_INBOX,
      "upgrade",
      "b8-choose-00000000001",
    );
    assert.equal(chosen.status, 200);
    assert.equal((await chosen.json()).status, "resolved");

    // One of the two recorded blockers is still open.
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000006", "waiting_human");
  });

  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    // Mock precondition: start from waiting_human; the mock has no engine to raise blockers.
    delivery.stage_status = "waiting_human";

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000007", "paused");
    const chosen = await chooseInboxOption(
      mock,
      ESCALATION_INBOX,
      "upgrade",
      "b8-choose-00000000002",
    );
    assert.equal(chosen.status, 200);
    // Mock precondition: approvals resolve through their own route, which this mock does not wire
    // back to the inbox item.
    mock.state.inbox.items.find((item) => item.id === OPEN_INBOX).status = "resolved";

    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000007", "running");
  });

  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    delivery.stage_status = "waiting_human";

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000008", "paused");
    const chosen = await chooseInboxOption(
      mock,
      ESCALATION_INBOX,
      "upgrade",
      "b8-choose-00000000003",
    );
    assert.equal(chosen.status, 200);

    // A partial resume keeps the recorded blockers so a later resume can re-check them.
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000008", "waiting_human");
    mock.state.inbox.items.find((item) => item.id === OPEN_INBOX).status = "resolved";
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000009", "running");
  });

  await withMock(async (mock) => {
    const delivery = mock.state.deliveries.items.find((item) => item.id === DELIVERY_RUNNING);
    delivery.stage_status = "waiting_human";

    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000010", "paused");
    const dismissed = await dismissInbox(mock, ESCALATION_INBOX);
    assert.equal(dismissed.status, 200);
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000010", "waiting_human");

    // Re-pausing after a partial resume must retain the dismissed blocker in the snapshot.
    await assertDeliveryStatus(mock, "pause", "b8-pause-000000000011", "paused");
    mock.state.inbox.items.find((item) => item.id === OPEN_INBOX).status = "resolved";
    await assertDeliveryStatus(mock, "resume", "b8-resume-00000000011", "waiting_human");
  });
});

test("challenges are reused per device, expire, reject mismatches and cannot be replayed", async () => {
  await withMock(async (mock) => {
    const challengePath = `/v1/approvals/${APPROVAL}/challenge`;
    const grantPath = `/v1/approvals/${APPROVAL}/grant`;

    const first = await (await request(mock, challengePath)).json();
    const second = await (await request(mock, challengePath)).json();
    assert.equal(second.payload.nonce, first.payload.nonce);
    assert.equal(second.payload.expires_at, first.payload.expires_at);

    const mismatch = await request(mock, grantPath, {
      method: "POST",
      headers: { "Idempotency-Key": "grant-key-wrong-00001" },
      body: JSON.stringify({ nonce: "A".repeat(43), signature: SIGNATURE }),
    });
    assert.equal(mismatch.status, 409);
    const mismatchBody = await mismatch.json();
    assert.equal(mismatchBody.code, "challenge_invalid");
    assert.equal(mismatchBody.reason, "mismatch");

    mock.state.me.server.time = first.payload.expires_at;
    const expired = await request(mock, grantPath, {
      method: "POST",
      headers: { "Idempotency-Key": "grant-key-expired-001" },
      body: JSON.stringify({ nonce: first.payload.nonce, signature: SIGNATURE }),
    });
    assert.equal(expired.status, 409);
    const expiredBody = await expired.json();
    assert.equal(expiredBody.code, "challenge_invalid");
    assert.equal(expiredBody.reason, "expired");

    const renewed = await (await request(mock, challengePath)).json();
    assert.notEqual(renewed.payload.nonce, first.payload.nonce);

    const granted = await request(mock, grantPath, {
      method: "POST",
      headers: { "Idempotency-Key": "grant-key-expired-002" },
      body: JSON.stringify({ nonce: renewed.payload.nonce, signature: SIGNATURE }),
    });
    assert.equal(granted.status, 200);
    assert.equal((await granted.json()).status, "granted");

    const sameKey = await request(mock, grantPath, {
      method: "POST",
      headers: { "Idempotency-Key": "grant-key-expired-002" },
      body: JSON.stringify({ nonce: renewed.payload.nonce, signature: SIGNATURE }),
    });
    assert.equal(sameKey.status, 200);
    assert.equal(sameKey.headers.get("Idempotent-Replayed"), "true");

    const replay = await request(mock, grantPath, {
      method: "POST",
      headers: { "Idempotency-Key": "grant-key-expired-003" },
      body: JSON.stringify({ nonce: renewed.payload.nonce, signature: SIGNATURE }),
    });
    assert.equal(replay.status, 409);
    const replayBody = await replay.json();
    assert.equal(replayBody.code, "challenge_invalid");
    assert.equal(replayBody.reason, "used");

    mock.state.me.device.id = OTHER_DEVICE;
    const otherDevice = await request(mock, grantPath, {
      method: "POST",
      headers: { "Idempotency-Key": "grant-key-expired-004" },
      body: JSON.stringify({ nonce: renewed.payload.nonce, signature: SIGNATURE }),
    });
    assert.equal(otherDevice.status, 409);
    assert.equal((await otherDevice.json()).code, "approval_not_pending");
  });
});

test("challenge TTL never exceeds the approval expiry", async () => {
  await withMock(async (mock) => {
    mock.state.me.server.time = "2026-10-24T14:40:30.000Z";
    const challenge = await (
      await request(mock, `/v1/approvals/${APPROVAL}/challenge`)
    ).json();
    assert.equal(challenge.payload.expires_at, "2026-10-24T14:41:00.000Z");
    assert.match(challenge.payload.nonce, /^[A-Za-z0-9_-]{43}$/);
  });
});

test("create approval only allows the three internal actions", async () => {
  await withMock(async (mock) => {
    const path = "/v1/approvals";
    const digest40 = "a".repeat(40);
    const digestSha = `sha256:${"a".repeat(64)}`;
    const globalSettings = {
      action: "settings_update",
      target: { kind: "settings", scope: "global", resource: "levels", base_rev: 1 },
      subject_digest: digestSha,
    };
    const deviceTarget = { kind: "device", id: OTHER_DEVICE };

    const allowed = [
      { key: "approval-create-global-01", body: globalSettings },
      {
        key: "approval-create-project-02",
        body: {
          ...globalSettings,
          target: { ...globalSettings.target, scope: "project", project_id: PROJECT },
        },
      },
      {
        key: "approval-create-role-0003",
        body: {
          action: "device_role_change",
          target: deviceTarget,
          subject_digest: digestSha,
        },
      },
      {
        key: "approval-create-revoke-04",
        body: {
          action: "device_revoke",
          target: deviceTarget,
          subject_digest: digestSha,
        },
      },
    ];
    for (const { key, body } of allowed) {
      const created = await request(mock, path, {
        method: "POST",
        headers: { "Idempotency-Key": key },
        body: JSON.stringify(body),
      });
      assert.equal(created.status, 201, `${key}: ${JSON.stringify(await created.clone().json())}`);
      assert.equal((await created.json()).action, body.action);
    }

    const before = mock.state.approvals.items.length;
    const forbidden = [
      {
        key: "approval-create-deny-merge",
        action: "merge_pr",
        target: { kind: "pr", repo: "acme/web", number: 12 },
        digest: digest40,
      },
      {
        key: "approval-create-deny-force",
        action: "force_push",
        target: { kind: "branch", repo: "acme/web", branch: "gantry/42-fix" },
        digest: digest40,
      },
      {
        key: "approval-create-deny-prefix",
        action: "push_outside_prefix",
        target: { kind: "branch", repo: "acme/web", branch: "feature/fix" },
        digest: digest40,
      },
      {
        key: "approval-create-deny-patch",
        action: "patch_apply_protected",
        target: {
          kind: "patch",
          repo: "acme/web",
          attempt_id: "att_01JB7M4P8R2X6V9N3Q5T7W1YZB",
          rules: [],
          paths: [],
        },
        digest: digestSha,
      },
    ];
    for (const { key, action, target, digest } of forbidden) {
      const response = await request(mock, path, {
        method: "POST",
        headers: { "Idempotency-Key": key },
        body: JSON.stringify({ action, target, subject_digest: digest }),
      });
      assert.equal(response.status, 422, `${action} must be a validation failure`);
      assert.match(response.headers.get("content-type") || "", /^application\/problem\+json/);
      const problem = await response.json();
      assert.equal(problem.code, "validation_failed", `${action}: ${JSON.stringify(problem)}`);
      assert.ok(problem.errors.length > 0, `${action}: expected validation details`);
    }
    assert.equal(mock.state.approvals.items.length, before);

    const targetErrors = [
      {
        key: "approval-create-target-01",
        body: {
          ...globalSettings,
          target: { ...globalSettings.target, project_id: PROJECT },
        },
      },
      {
        key: "approval-create-target-02",
        body: {
          ...globalSettings,
          target: { kind: "settings", scope: "project", resource: "levels", base_rev: 1 },
        },
      },
    ];
    for (const { key, body } of targetErrors) {
      const response = await request(mock, path, {
        method: "POST",
        headers: { "Idempotency-Key": key },
        body: JSON.stringify(body),
      });
      assert.equal(response.status, 422, `${key} must be a validation failure`);
      const problem = await response.json();
      assert.equal(problem.code, "validation_failed");
      assert.ok(
        problem.errors.some((error) => error.pointer === "/target/project_id"),
        `${key}: expected /target/project_id, got ${JSON.stringify(problem.errors)}`,
      );
    }
    assert.equal(mock.state.approvals.items.length, before);
  });
});
