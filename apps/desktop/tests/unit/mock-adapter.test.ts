import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { StreamFrame } from "@/api/shell";
import type { Approval, Me, Message } from "@/api/types";
import type { RouteId } from "@/api/routes";
import { MockIpcAdapter } from "@/ipc/mock/adapter";
import { startMockApi, type RunningMockApi } from "./mockApi";

const CONVERSATION = "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC";
const APPROVAL = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const SELF = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const PHONE = "dev_01JB6Z4M8S3T6X9N2Q5V7W0YZN";

let mock: RunningMockApi;
let adapter: MockIpcAdapter;
let frames: StreamFrame[];

function until<T>(probe: () => T | undefined | null | false, timeoutMs = 3000): Promise<T> {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = (): void => {
      const value = probe();
      if (value) return resolve(value);
      if (Date.now() - started > timeoutMs) return reject(new Error("timeout"));
      setTimeout(tick, 10);
    };
    tick();
  });
}

async function openLive(afterSeq?: number): Promise<void> {
  await adapter.eventsOpen({ afterSeq }, (frame) => {
    frames.push(frame);
    if (frame.t === "event" || frame.t === "heartbeat") {
      void adapter.eventsAck(frame.t === "event" ? frame.envelope.seq : frame.seq);
    }
  });
  await until(() => frames.some((f) => f.t === "heartbeat"));
}

async function messageCount(): Promise<number> {
  const page = await adapter.api("messages.list", { conversationId: CONVERSATION, limit: 100 });
  return page.ok ? (page.body?.items.length ?? 0) : -1;
}

beforeEach(async () => {
  mock = await startMockApi();
  adapter = new MockIpcAdapter({ baseUrl: mock.baseUrl, reconnectDelayMs: 20 });
  frames = [];
});

afterEach(async () => {
  await adapter.eventsClose();
  await mock.close();
});

describe("MockIpcAdapter：api_request", () => {
  it("只放行白名单路由，未知路由在发出请求前被拒", async () => {
    await expect(adapter.api("approvals.grant" as RouteId, {} as never)).rejects.toMatchObject({
      code: "shell.unknown_route",
    });
    expect(await mock.requests()).toHaveLength(0);
  });

  it("响应映射为 ApiResult，带 X-Event-Seq 水位线", async () => {
    const result = await adapter.api("me.get", {});
    expect(result).toMatchObject({ ok: true, status: 200, headers: { x_event_seq: 18240 } });
    const missing = await adapter.api("deliveries.get", { deliveryId: "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZZ" });
    expect(missing).toMatchObject({ ok: false, status: 404, problem: { code: "not_found" } });
  });

  it("重复数组查询参数按多值发送", async () => {
    const result = await adapter.api("deliveries.list", {
      stage: ["closed", "abandoned"],
      limit: 10,
    });
    expect(result.ok).toBe(true);
  });

  it("写请求带 Idempotency-Key；响应丢失后重试沿用同一键只产生 1 条消息（E2E-17）", async () => {
    const before = await messageCount();
    const body = { text: "离线测试", refs: [] };
    adapter.swallowNextWriteResponse();
    const lost = await adapter.api("messages.post", {
      conversationId: CONVERSATION,
      body,
      idempotencyKey: "intent_0000000000000001",
    });
    expect(lost).toEqual({ ok: false, network: "timeout" });
    const retried = await adapter.api("messages.post", {
      conversationId: CONVERSATION,
      body,
      idempotencyKey: "intent_0000000000000001",
    });
    expect(retried).toMatchObject({ ok: true, status: 201, headers: { idempotent_replayed: true } });
    expect(await messageCount()).toBe(before + 1);
    expect((retried.ok && (retried.body as Message).text) || "").toBe("离线测试");
  });

  it("断网时不发请求，直接返回 network: offline", async () => {
    adapter.disconnect();
    expect(await adapter.api("me.get", {})).toEqual({ ok: false, network: "offline" });
    expect(await mock.requests()).toHaveLength(0);
  });
});

describe("MockIpcAdapter：事件流", () => {
  it("按游标回放 (after, head]，随后心跳；断开后从已 ack 的游标续流且无重复", async () => {
    await openLive(18237);
    const replayed = frames.flatMap((f) => (f.t === "event" ? [f.envelope.seq] : []));
    expect(replayed).toEqual([18238, 18239, 18240]);
    expect(frames[0]).toMatchObject({ t: "state", state: { phase: "live", epoch: 1 } });
    expect(adapter.ackedSeq).toBe(18240);

    await mock.inject("message.posted", "msg_injected", { conversation_id: CONVERSATION });
    await until(() => frames.some((f) => f.t === "event" && f.envelope.seq === 18241));
    const all = frames.flatMap((f) => (f.t === "event" ? [f.envelope.seq] : []));
    expect(all).toEqual([18238, 18239, 18240, 18241]);
    expect(frames.some((f) => f.t === "state" && f.state.phase === "reconnecting")).toBe(true);
    const lastState = [...frames].reverse().find((f) => f.t === "state");
    expect(lastState).toMatchObject({ state: { phase: "live", epoch: 2 } });
  });

  it("游标超前 → resync_required(cursor_ahead)", async () => {
    await adapter.eventsOpen({ afterSeq: 99_999 }, (frame) => frames.push(frame));
    await until(() => frames.some((f) => f.t === "resync_required"));
    expect(frames.find((f) => f.t === "resync_required")).toEqual({
      t: "resync_required",
      reason: "cursor_ahead",
    });
  });

  it("心跳里的 log_epoch 变化以 state 帧送达，旧游标作废（P-67）", async () => {
    await adapter.identityStatus();
    await openLive(18240);
    await mock.set("logEpoch", 2);
    await mock.dropStreams();
    await until(() => frames.some((f) => f.t === "state" && f.state.log_epoch === 2));
  });
});

describe("MockIpcAdapter：审批签名", () => {
  it("approver 设备：prepare 取 challenge 生成展示模型，sign 只带 preparedId 并 grant", async () => {
    await openLive();
    const sheet = await adapter.approvalPrepare({ approvalId: APPROVAL });
    expect(sheet.can_sign).toEqual({ ok: true });
    expect(sheet.signed.subject_digest).toMatch(/^[0-9a-f]{40}$/);
    expect(sheet.signed.target_label).toBe("acme/web · PR #12");
    expect(sheet.prepared_id).toMatch(/^prp_/);
    const result = await adapter.approvalSign({ preparedId: sheet.prepared_id });
    expect(result).toMatchObject({ ok: true, body: { status: "granted" } });
    const ops = await mock.operations();
    expect(ops).toContain("getApprovalChallenge");
    expect(ops.at(-1)).toBe("grantApproval");
    const signCall = adapter.calls.find((c) => c.command === "approval_sign");
    expect(Object.keys(signCall?.args ?? {})).toEqual(["preparedId"]);
    await expect(adapter.approvalSign({ preparedId: sheet.prepared_id })).rejects.toMatchObject({
      code: "shell.prepared_expired",
    });
  });

  it("非审批设备只读展示，不请求 challenge（E2E-06）", async () => {
    await mock.mutate("devices", SELF, { role: "operator" });
    const sheet = await adapter.approvalPrepare({ approvalId: APPROVAL });
    expect(sheet.can_sign).toEqual({ ok: false, reason: "not_approver" });
    expect(await mock.operations()).not.toContain("getApprovalChallenge");
  });

  it("连接中断时签名被拒，不产生 grant（E2E-04）", async () => {
    await openLive();
    const sheet = await adapter.approvalPrepare({ approvalId: APPROVAL });
    adapter.disconnect();
    await expect(adapter.approvalSign({ preparedId: sheet.prepared_id })).rejects.toMatchObject({
      code: "shell.not_connected",
    });
    expect(await mock.operations()).not.toContain("grantApproval");
  });

  it("approval_sign 带额外字段 → shell.bad_argument", async () => {
    await expect(
      adapter.approvalSign({ preparedId: "x", signature: "y" } as { preparedId: string }),
    ).rejects.toMatchObject({ code: "shell.bad_argument" });
  });

  it("已非待签的审批返回 not_pending 且不取 challenge", async () => {
    await mock.mutate("approvals", APPROVAL, { status: "denied" });
    const sheet = await adapter.approvalPrepare({ approvalId: APPROVAL });
    expect(sheet.can_sign).toEqual({ ok: false, reason: "not_pending" });
    expect(await mock.operations()).not.toContain("getApprovalChallenge");
  });
});

describe("MockIpcAdapter：受保护写入", () => {
  it("设备角色变更走 发起审批 → challenge → grant → 带 Gantry-Approval 重发", async () => {
    await openLive();
    const preview = await adapter.guardedWritePrepare({
      kind: "device_role",
      deviceId: PHONE,
      role: "viewer",
    });
    expect(preview).toMatchObject({ valid: true, requires_signature: true, can_submit: { ok: true } });
    const result = await adapter.guardedWriteSubmit({ pendingId: preview.pending_id });
    expect(result).toMatchObject({ ok: true, body: { id: PHONE, role: "viewer" } });
    const chain = new Set(["createApproval", "getApprovalChallenge", "grantApproval", "setDeviceRole"]);
    expect((await mock.operations()).filter((op) => chain.has(op))).toEqual([
      "createApproval",
      "getApprovalChallenge",
      "grantApproval",
      "setDeviceRole",
    ]);
    const approvals = await adapter.api("approvals.list", { limit: 100 });
    const created = approvals.ok
      ? (approvals.body?.items ?? []).find((a: Approval) => a.action === "device_role_change")
      : undefined;
    expect(created?.status).toBe("granted");
    expect(created?.subject_digest).toMatch(/^sha256:[0-9a-f]{64}$/);
  });

  it("设置写入先 dry_run；凭据值不进入 IPC 录制与结果", async () => {
    await openLive();
    const doc = await adapter.api("settings.get", { resource: "models" });
    if (!doc.ok) throw new Error("settings.get failed");
    const etag = doc.headers.etag ?? "";
    const value = structuredClone((doc.body as { value: Record<string, unknown> }).value);
    (value.providers as Record<string, { credential: unknown }>).anthropic.credential = {
      type: "api_key",
      secret_ref: "model.anthropic.main",
      value: "sk-test-placeholder-0000",
    };
    const preview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value,
      ifMatch: etag,
    });
    expect(preview.errors).toEqual([]);
    const result = await adapter.guardedWriteSubmit({ pendingId: preview.pending_id });
    expect(result).toMatchObject({ ok: true, status: 200 });
    expect(JSON.stringify(adapter.calls)).not.toContain("sk-test-placeholder");
    expect(JSON.stringify(result)).not.toContain("sk-test-placeholder");
    const ops = await mock.operations();
    expect(ops.filter((op) => op === "putSettings")).toHaveLength(2);
  });

  it("If-Match 过期 → 预览带错误，不可提交", async () => {
    const preview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: {},
      ifMatch: '"rev-1"',
    });
    expect(preview.valid).toBe(false);
    expect(preview.can_submit).toEqual({ ok: false, reason: "invalid" });
  });
});

describe("MockIpcAdapter：身份", () => {
  it("已配对时 identity 来自 /v1/me", async () => {
    const identity = await adapter.identityStatus();
    const me = (await adapter.api("me.get", {})) as { body: Me };
    expect(identity).toMatchObject({ state: "paired", device: { id: me.body.device.id } });
  });

  it("未配对 → 配对向导：确认码 → 一致 → paired；不一致 → unpaired", async () => {
    const fresh = new MockIpcAdapter({ baseUrl: mock.baseUrl, paired: false });
    expect((await fresh.identityStatus()).state).toBe("unpaired");
    await expect(fresh.api("me.get", {})).rejects.toMatchObject({ code: "shell.not_paired" });
    const begun = await fresh.pairingBegin({ pairingString: "gantry-pair://token", deviceName: "测试机" });
    expect(begun.confirmationCode).toMatch(/^\d{3} \d{3}$/);
    expect((await fresh.identityStatus()).state).toBe("pending_confirm");
    expect((await fresh.pairingConfirm({ matches: true })).state).toBe("paired");

    const other = new MockIpcAdapter({ baseUrl: mock.baseUrl, paired: false });
    await other.pairingBegin({ pairingString: "gantry-pair://token", deviceName: "测试机" });
    expect((await other.pairingConfirm({ matches: false })).state).toBe("unpaired");
  });

  it("吊销后关流并拒绝后续请求（E2E-14）", async () => {
    await openLive();
    adapter.revoke();
    expect(frames.at(-1)).toMatchObject({ t: "state", state: { phase: "revoked" } });
    const count = (await mock.requests()).length;
    await expect(adapter.api("me.get", {})).rejects.toMatchObject({ code: "shell.not_paired" });
    expect((await mock.requests()).length).toBe(count);
    expect((await adapter.identityStatus()).state).toBe("revoked");
  });
});
