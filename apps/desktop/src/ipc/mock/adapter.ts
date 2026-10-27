/**
 * 浏览器 / 测试用外壳替身：直连 Mock API 的 HTTP 层（desktop-ui §6.1、§12）。
 *
 * 只在 `--mode mock` 构建与测试中加载；正式构建里该分支被静态折叠。
 * 它模拟外壳的可见行为——路由白名单、`ApprovalSheetModel`、受保护写入链路、
 * 事件流的游标续传与重连——但**不持有任何密钥**：签名是随机占位串，
 * 只用于驱动 Mock 的 `grant` 状态机，不能当作真实签名。
 */
import {
  IpcError,
  type ApiHeaders,
  type ApiResult,
  type ApprovalSheetModel,
  type ConnectionState,
  type GuardedWriteArgs,
  type GuardedWritePreview,
  type IdentityStatus,
  type IpcAdapter,
  type OpenGithubArgs,
  type Prefs,
  type ProjectUiState,
  type StreamFrame,
} from "@/api/shell";
import type {
  Approval,
  ApprovalTarget,
  Delivery,
  Device,
  EventEnvelope,
  Me,
  SettingsIssue,
} from "@/api/types";
import {
  ROUTES,
  isRouteId,
  type RouteArgs,
  type RouteBody,
  type RouteId,
  type RouteQuery,
} from "@/api/routes";
import { httpBody } from "../routeBody";
import { SseParser } from "./sse";

export interface MockIpcOptions {
  /** Mock API 根地址；浏览器同源代理时为空串。 */
  baseUrl?: string;
  /** 测试注入；默认用全局实现。 */
  fetchImpl?: typeof globalThis.fetch;
  /** 初始是否已配对；默认 true。 */
  paired?: boolean;
  /** 事件流断开后的重连间隔。 */
  reconnectDelayMs?: number;
  platform?: "macos" | "windows";
}

/** 录制的 IPC 调用；用于断言“不回传待签内容”（E2E-09）。凭据值不入日志。 */
export interface IpcCall {
  command: string;
  args: Record<string, unknown>;
}

interface PreparedSigning {
  approvalId: string;
  nonce: string;
  action: string;
  target: ApprovalTarget;
  subjectDigest: string;
  expiresAt: number;
}

interface Signing {
  action: "settings_update" | "device_role_change" | "device_revoke";
  target: ApprovalTarget;
  subject_digest: string;
}

interface PendingWrite {
  args: GuardedWriteArgs;
  requiresSignature: boolean;
  signing: Signing | null;
}

type Problem = Extract<ApiResult, { problem: unknown }>;

const KNOWN_ACTIONS = new Set([
  "merge_pr",
  "force_push",
  "push_outside_prefix",
  "resolve_review_thread_human",
  "patch_apply_protected",
  "settings_update",
  "device_role_change",
  "device_revoke",
]);

const ACTION_LABELS: Record<string, string> = {
  merge_pr: "合并",
  force_push: "强制推送",
  push_outside_prefix: "推送到前缀外分支",
  resolve_review_thread_human: "解决人工评审线程",
  patch_apply_protected: "应用受保护补丁",
  settings_update: "修改设置",
  device_role_change: "修改设备角色",
  device_revoke: "吊销设备",
};

const SIGNING_PREFIX = "gantry/approval/v1\n";
const MAX_PREPARED = 8;

const DEFAULT_PREFS: Prefs = {
  theme: "dark",
  density: "compact",
  language: "zh-CN",
  systemNotifications: true,
  layout: {
    leftWidth: 240,
    inspectorWidth: 300,
    leftCollapsed: false,
    inspectorCollapsed: false,
  },
};

export class MockIpcAdapter implements IpcAdapter {
  readonly isMock = true;
  readonly calls: IpcCall[] = [];
  readonly externalOpens: Array<{ url: string }> = [];
  readonly githubOpens: OpenGithubArgs[] = [];

  private readonly baseUrl: string;
  private readonly fetchImpl: typeof globalThis.fetch;
  private readonly reconnectDelayMs: number;
  private readonly platform: "macos" | "windows";

  private paired: boolean;
  private revoked = false;
  private certExpired = false;
  private me: Me | null = null;
  private clockSkewMs = 0;
  private connection: ConnectionState;
  private pendingPairing: { device: Device } | null = null;

  private onFrame: ((f: StreamFrame) => void) | null = null;
  private streamGen = 0;
  private streamAbort: AbortController | null = null;
  private acked: number | null = null;
  private cursor: number | null = null;
  private offline = false;
  private streamHeld = false;
  private swallowNext = false;

  private prepared = new Map<string, PreparedSigning>();
  private signing = false;
  private pendingWrites = new Map<string, PendingWrite>();
  private prefs: Prefs = structuredClone(DEFAULT_PREFS);
  private uiStates = new Map<string, ProjectUiState>();

  constructor(options: MockIpcOptions = {}) {
    this.baseUrl = (options.baseUrl ?? "").replace(/\/$/, "");
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch.bind(globalThis);
    this.reconnectDelayMs = options.reconnectDelayMs ?? 1000;
    this.platform = options.platform ?? "macos";
    this.paired = options.paired ?? true;
    this.connection = {
      phase: this.paired ? "connecting" : "unpaired",
      epoch: 0,
      last_bytes_age_ms: null,
      cert_not_after: null,
      cert_expiring: false,
      log_epoch: null,
      clock_skew_ms: null,
    };
  }

  // ------------------------------------------------------------ 身份与配对

  async identityStatus(): Promise<IdentityStatus> {
    this.log("identity_status");
    if (this.revoked) return this.identity("revoked");
    if (this.certExpired) return this.identity("cert_expired");
    if (!this.paired) return this.identity(this.pendingPairing ? "pending_confirm" : "unpaired");
    await this.refreshMe();
    return this.identity("paired");
  }

  async pairingBegin(a: {
    pairingString: string;
    deviceName: string;
  }): Promise<{ confirmationCode: string; device: Device }> {
    this.log("pairing_begin", { device_name: a.deviceName });
    const name = a.deviceName.trim();
    if (!a.pairingString.trim() || !name || name.length > 64) {
      throw new IpcError("shell.bad_argument", "配对串和设备名必须为 1–64 字符。");
    }
    const result = await this.http<{ device: Device; confirmation_code: string }>(
      "POST",
      "/v1/pair",
      {
        body: {
          token: a.pairingString.trim(),
          csr: "mock-csr-not-a-real-certificate-request",
          approval_pubkey: "mock-approval-pubkey",
          device: { name, platform: this.platform },
        },
      },
    );
    if (!result.ok || !result.body) {
      // 配对令牌的各种失败原因对界面一律显示同一提示（desktop-ui §12）。
      throw new IpcError("pair_token_invalid", "配对串无效或已过期。");
    }
    this.pendingPairing = { device: result.body.device };
    const code = result.body.confirmation_code;
    return {
      confirmationCode: `${code.slice(0, 3)} ${code.slice(3)}`,
      device: result.body.device,
    };
  }

  async pairingConfirm(a: { matches: boolean }): Promise<IdentityStatus> {
    this.log("pairing_confirm", { matches: a.matches });
    if (!this.pendingPairing) {
      throw new IpcError("shell.bad_argument", "没有待确认的配对请求。");
    }
    this.pendingPairing = null;
    if (!a.matches) {
      this.paired = false;
      this.setPhase("unpaired");
      return this.identity("unpaired");
    }
    this.paired = true;
    this.revoked = false;
    this.certExpired = false;
    this.setPhase("connecting");
    await this.refreshMe();
    return this.identity("paired");
  }

  async identityReset(): Promise<IdentityStatus> {
    this.log("identity_reset");
    this.closeStream();
    this.paired = false;
    this.revoked = false;
    this.certExpired = false;
    this.pendingPairing = null;
    this.me = null;
    this.uiStates.clear();
    this.setPhase("unpaired");
    return this.identity("unpaired");
  }

  async connectionState(): Promise<ConnectionState> {
    this.log("connection_state");
    return structuredClone(this.connection);
  }

  // ------------------------------------------------------------ api_request

  async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    this.log("api_request", { route });
    if (!isRouteId(route)) {
      throw new IpcError("shell.unknown_route", "路由不在白名单内。");
    }
    this.requirePaired();
    const meta = ROUTES[route];
    const record = (args ?? {}) as { body?: unknown; idempotencyKey?: string };
    return this.http<RouteBody[R]>(meta.method, meta.path(args as never), {
      query: meta.query ? meta.query(args as never) : undefined,
      body: httpBody(route, args, record.body),
      idempotencyKey: record.idempotencyKey,
    });
  }

  // ------------------------------------------------------------ 事件流

  async eventsOpen(
    a: { afterSeq?: number },
    onFrame: (f: StreamFrame) => void,
  ): Promise<void> {
    this.log("events_open", a.afterSeq === undefined ? {} : { after_seq: a.afterSeq });
    this.requirePaired();
    this.closeStream();
    this.onFrame = onFrame;
    this.acked = null;
    this.cursor = a.afterSeq ?? null;
    const gen = ++this.streamGen;
    void this.runStream(gen);
  }

  async eventsAck(seq: number): Promise<void> {
    this.acked = this.acked === null ? seq : Math.max(this.acked, seq);
    this.cursor = this.cursor === null ? seq : Math.max(this.cursor, seq);
  }

  async eventsClose(): Promise<void> {
    this.log("events_close");
    this.closeStream();
    this.onFrame = null;
  }

  get ackedSeq(): number | null {
    return this.acked;
  }

  // ------------------------------------------------------------ 审批签名

  async approvalPrepare(a: { approvalId: string }): Promise<ApprovalSheetModel> {
    this.log("approval_prepare", { approval_id: a.approvalId });
    this.requirePaired();
    const approval = await this.mustGet<Approval>(`/v1/approvals/${a.approvalId}`);
    const me = await this.refreshMe();
    const delivery = approval.delivery_id
      ? await this.optionalGet<Delivery>(`/v1/deliveries/${approval.delivery_id}`)
      : null;
    const model = this.sheetModel(approval, me, delivery);
    if (approval.status !== "pending") {
      model.can_sign = { ok: false, reason: "not_pending" };
      return model;
    }
    if (me.device.role !== "approver") {
      // 非审批设备只返回只读展示，不取 challenge（desktop-shell §7.4.1 步骤 2）。
      model.can_sign = { ok: false, reason: "not_approver" };
      return model;
    }
    const challenge = await this.mustGet<{
      approval_id: string;
      payload: {
        action: string;
        approval_id: string;
        delivery_id?: string;
        device_id: string;
        expires_at: string;
        nonce: string;
        subject_digest: string;
        target: ApprovalTarget;
        v: number;
      };
      signing: { alg: string; canonicalization: string; prefix: string };
    }>(`/v1/approvals/${a.approvalId}/challenge`);
    const p = challenge.payload;
    const failures: string[] = [];
    if (challenge.approval_id !== approval.id) failures.push("approval_id_mismatch");
    if (p.device_id !== me.device.id) failures.push("device_id_mismatch");
    if (p.v !== 1) failures.push("unknown_version");
    if (
      challenge.signing.alg !== "ES256" ||
      challenge.signing.canonicalization !== "JCS" ||
      challenge.signing.prefix !== SIGNING_PREFIX
    ) {
      failures.push("unsupported_signing");
    }
    if (!KNOWN_ACTIONS.has(p.action)) failures.push("unknown_action");
    if (
      p.action !== approval.action ||
      p.subject_digest !== approval.subject_digest ||
      jcs(p.target) !== jcs(approval.target) ||
      (p.delivery_id ?? null) !== approval.delivery_id
    ) {
      failures.push("approval_data_mismatch");
    }
    const expiresAt = Date.parse(p.expires_at);
    if (expiresAt - this.serverNow() < 10_000) failures.push("challenge_expired");
    if (failures.length > 0) {
      throw new IpcError("shell.self_check_failed", `展示前自检失败：${failures.join(", ")}`);
    }
    const preparedId = `prp_${randomToken(12)}`;
    this.prepared.set(preparedId, {
      approvalId: approval.id,
      nonce: p.nonce,
      action: approval.action,
      target: approval.target,
      subjectDigest: approval.subject_digest,
      expiresAt,
    });
    while (this.prepared.size > MAX_PREPARED) {
      const oldest = this.prepared.keys().next().value;
      if (oldest === undefined) break;
      this.prepared.delete(oldest);
    }
    model.prepared_id = preparedId;
    model.signed.tech.nonce = p.nonce;
    model.signed.device_id = p.device_id;
    model.challenge_expires_at = p.expires_at;
    model.can_sign =
      this.connection.phase === "live" ? { ok: true } : { ok: false, reason: "not_connected" };
    return model;
  }

  async approvalSign(a: { preparedId: string }): Promise<ApiResult<Approval>> {
    this.log("approval_sign", { ...a });
    if (Object.keys(a).some((key) => key !== "preparedId")) {
      throw new IpcError("shell.bad_argument", "approval_sign 只接受 prepared_id。");
    }
    const prepared = this.prepared.get(a.preparedId);
    if (!prepared || prepared.expiresAt <= this.serverNow()) {
      this.prepared.delete(a.preparedId);
      throw new IpcError("shell.prepared_expired", "签名准备已过期，请重新载入审批单。");
    }
    if (this.connection.phase !== "live") {
      throw new IpcError("shell.not_connected", "连接中断，暂不能签名。");
    }
    if (this.signing) throw new IpcError("shell.signing_busy", "已有签名确认进行中。");
    this.signing = true;
    try {
      const current = await this.mustGet<Approval>(`/v1/approvals/${prepared.approvalId}`);
      if (
        current.status === "pending" &&
        (current.action !== prepared.action ||
          current.subject_digest !== prepared.subjectDigest ||
          jcs(current.target) !== jcs(prepared.target))
      ) {
        throw new IpcError("shell.self_check_failed", "审批内容已变化，请重新载入。");
      }
      return await this.http<Approval>("POST", `/v1/approvals/${prepared.approvalId}/grant`, {
        body: { nonce: prepared.nonce, signature: placeholderSignature() },
        idempotencyKey: `grant_${a.preparedId}`,
      });
    } finally {
      this.signing = false;
      this.prepared.delete(a.preparedId);
    }
  }

  async approvalDiscard(a: { preparedId: string }): Promise<void> {
    this.log("approval_discard", { ...a });
    this.prepared.delete(a.preparedId);
  }

  // ------------------------------------------------------------ 受保护写入

  async guardedWritePrepare(a: GuardedWriteArgs): Promise<GuardedWritePreview> {
    this.log("guarded_write_prepare", redactWrite(a));
    this.requirePaired();
    const me = await this.refreshMe();
    const pendingId = `pnd_${randomToken(12)}`;
    let errors: SettingsIssue[] = [];
    let warnings: SettingsIssue[] = [];
    let relaxes: string[] = [];
    let requiredRole: GuardedWritePreview["required_role"] = "operator";
    let requiresSignature = false;
    let signing: Signing | null = null;

    if (a.kind === "settings_put") {
      const result = await this.http<{
        valid: boolean;
        errors: SettingsIssue[];
        warnings: SettingsIssue[];
        relaxes: string[];
        required_role: GuardedWritePreview["required_role"];
        requires_signature: boolean;
        signing?: Signing;
      }>("PUT", settingsPath(a.scope, a.projectId, a.resource), {
        query: { dry_run: true },
        body: { value: a.value },
        ifMatch: a.ifMatch,
      });
      if (result.ok && result.body) {
        errors = result.body.errors;
        warnings = result.body.warnings;
        relaxes = result.body.relaxes;
        requiredRole = result.body.required_role;
        requiresSignature = result.body.requires_signature;
        signing = result.body.signing ?? null;
        if (!result.body.valid && errors.length === 0) {
          errors = [{ pointer: "", code: "invalid", message: "服务端判定无效。" }];
        }
      } else {
        errors = problemIssues(result);
      }
    } else if (a.kind === "settings_delete") {
      // DELETE 没有 dry_run（OQ-S11）：先按无需签名预览，提交时遇 403 再转签名链。
    } else {
      requiredRole = "approver";
      requiresSignature = true;
      const body = a.kind === "device_role" ? { role: a.role } : {};
      signing = {
        action: a.kind === "device_role" ? "device_role_change" : "device_revoke",
        target: { kind: "device", id: a.deviceId },
        subject_digest: `sha256:${await sha256Hex(jcs(body))}`,
      };
    }

    const canSubmit: GuardedWritePreview["can_submit"] =
      errors.length > 0
        ? { ok: false, reason: "invalid" }
        : this.connection.phase !== "live"
          ? { ok: false, reason: "not_connected" }
          : requiresSignature && me.device.role !== "approver"
            ? { ok: false, reason: "not_approver" }
            : { ok: true };
    if (errors.length === 0) {
      this.pendingWrites.set(pendingId, { args: a, requiresSignature, signing });
    }
    return {
      pending_id: pendingId,
      valid: errors.length === 0,
      errors,
      warnings,
      relaxes,
      required_role: requiredRole,
      requires_signature: requiresSignature,
      preview: {
        kind: a.kind,
        resource: "resource" in a ? a.resource : undefined,
        scope: a.kind === "settings_put" ? a.scope : a.kind === "settings_delete" ? "project" : undefined,
      },
      can_submit: canSubmit,
    };
  }

  async guardedWriteSubmit(a: { pendingId: string }): Promise<ApiResult<unknown>> {
    this.log("guarded_write_submit", { ...a });
    const pending = this.pendingWrites.get(a.pendingId);
    if (!pending) {
      return localProblem(409, "pending_not_found", "写入预览已失效，请重新保存。");
    }
    this.pendingWrites.delete(a.pendingId);
    if (this.connection.phase !== "live") {
      throw new IpcError("shell.not_connected", "连接中断，暂不能提交。");
    }
    let approvalId: string | null = null;
    if (pending.requiresSignature && pending.signing) {
      const granted = await this.signInternal(pending.signing, a.pendingId);
      if (!granted.ok) return granted;
      approvalId = granted.approvalId;
    }
    let result = await this.sendWrite(pending.args, approvalId);
    if (
      !result.ok &&
      "problem" in result &&
      result.status === 403 &&
      result.problem.code === "signature_required" &&
      approvalId === null
    ) {
      const signing = result.problem.signing as Signing | undefined;
      if (signing) {
        const granted = await this.signInternal(signing, a.pendingId);
        if (!granted.ok) return granted;
        result = await this.sendWrite(pending.args, granted.approvalId);
      }
    }
    return result;
  }

  async guardedWriteDiscard(a: { pendingId: string }): Promise<void> {
    this.log("guarded_write_discard", { ...a });
    this.pendingWrites.delete(a.pendingId);
  }

  // ------------------------------------------------------------ 本机存储

  async prefsGet(): Promise<Prefs> {
    return structuredClone(this.prefs);
  }

  async prefsSet(a: { patch: Partial<Prefs> }): Promise<Prefs> {
    this.prefs = {
      ...this.prefs,
      ...a.patch,
      layout: { ...this.prefs.layout, ...(a.patch.layout ?? {}) },
    };
    return structuredClone(this.prefs);
  }

  async uiStateGet(a: { projectId: string }): Promise<ProjectUiState | null> {
    const value = this.uiStates.get(a.projectId);
    return value ? structuredClone(value) : null;
  }

  async uiStateSet(a: { projectId: string; state: ProjectUiState }): Promise<void> {
    this.uiStates.set(a.projectId, structuredClone(a.state));
  }

  async uiStateList(): Promise<Record<string, ProjectUiState>> {
    return Object.fromEntries(
      [...this.uiStates.entries()].map(([key, value]) => [key, structuredClone(value)]),
    );
  }

  async openExternal(a: { url: string }): Promise<void> {
    this.log("open_external", { ...a });
    this.externalOpens.push({ ...a });
  }

  async openGithub(a: OpenGithubArgs): Promise<void> {
    this.log("open_github", { ...a });
    this.githubOpens.push({ ...a });
  }

  // ------------------------------------------------------------ 测试钩子

  /** 断网：事件流中止、HTTP 一律 `network: offline`，直到 `reconnect()`。 */
  disconnect(): void {
    this.offline = true;
    this.streamAbort?.abort();
    this.setPhase("reconnecting");
  }

  /** 只中断事件流（HTTP 仍可达），直到 `reconnect()`：对应流已断、尚未重连成功的窗口。 */
  interruptStream(): void {
    this.streamHeld = true;
    this.streamAbort?.abort();
    this.setPhase("reconnecting");
  }

  /** 恢复网络：从已 ack 的游标续流（desktop-shell §7.3）。 */
  reconnect(): void {
    this.offline = false;
    this.streamHeld = false;
    if (this.onFrame && !this.revoked && !this.certExpired) {
      const gen = ++this.streamGen;
      void this.runStream(gen);
    }
  }

  /** 吊销：关流并转 `revoked`（外壳在心跳间隔内感知，desktop-shell §12）。 */
  revoke(): void {
    this.revoked = true;
    this.closeStream();
    this.setPhase("revoked");
  }

  expireCertificate(): void {
    this.certExpired = true;
    this.closeStream();
    this.setPhase("cert_expired");
  }

  /** 下一次写请求照常送达，但响应“丢失”（E2E-17）。 */
  swallowNextWriteResponse(): void {
    this.swallowNext = true;
  }

  /** 让当前事件流断开一次（模拟网络抖动），随后按退避自动重连。 */
  dropStream(): void {
    this.streamAbort?.abort();
  }

  // ------------------------------------------------------------ 内部

  private async runStream(gen: number): Promise<void> {
    while (gen === this.streamGen) {
      if (this.offline || this.streamHeld) return;
      const abort = new AbortController();
      this.streamAbort = abort;
      const query: RouteQuery = this.cursor === null ? {} : { after: this.cursor };
      try {
        const response = await this.fetchImpl(this.url("/v1/events", query), {
          headers: { Accept: "text/event-stream" },
          signal: abort.signal,
        });
        if (gen !== this.streamGen) return;
        if (response.status === 409 || response.status === 410) {
          const problem = (await response.json().catch(() => ({}))) as { code?: string };
          const reason = problem.code === "cursor_expired" ? "cursor_expired" : "cursor_ahead";
          this.emit({ t: "resync_required", reason });
          return;
        }
        if (!response.ok || !response.body) throw new Error(`events ${response.status}`);
        this.connection = {
          ...this.connection,
          phase: "live",
          epoch: this.connection.epoch + 1,
          last_bytes_age_ms: 0,
        };
        this.emit({ t: "state", state: structuredClone(this.connection) });
        await this.readStream(gen, response.body);
      } catch {
        // 中止或网络失败：落到下方的重连。
      }
      if (gen !== this.streamGen) return;
      this.setPhase("reconnecting");
      await sleep(this.reconnectDelayMs);
    }
  }

  private async readStream(gen: number, body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    const parser = new SseParser();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done || gen !== this.streamGen) return;
        for (const message of parser.push(decoder.decode(value, { stream: true }))) {
          if (gen !== this.streamGen) return;
          this.handleSse(message.event, message.data);
        }
      }
    } finally {
      reader.releaseLock();
    }
  }

  private handleSse(event: string, data: string): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(data);
    } catch {
      return;
    }
    if (event === "heartbeat") {
      const beat = parsed as { seq: number; log_epoch?: number };
      if (
        beat.log_epoch !== undefined &&
        this.connection.log_epoch !== null &&
        beat.log_epoch !== this.connection.log_epoch
      ) {
        // 纪元变化：旧游标作废，界面经 state 帧触发全量重同步（P-67）。
        this.connection = { ...this.connection, log_epoch: beat.log_epoch };
        this.cursor = null;
        this.emit({ t: "state", state: structuredClone(this.connection) });
        return;
      }
      if (beat.log_epoch !== undefined && this.connection.log_epoch === null) {
        this.connection = { ...this.connection, log_epoch: beat.log_epoch };
      }
      this.emit({ t: "heartbeat", seq: beat.seq, log_epoch: beat.log_epoch });
      return;
    }
    this.emit({ t: "event", envelope: parsed as EventEnvelope });
  }

  private emit(frame: StreamFrame): void {
    this.onFrame?.(frame);
  }

  private closeStream(): void {
    this.streamGen += 1;
    this.streamAbort?.abort();
    this.streamAbort = null;
  }

  private setPhase(phase: ConnectionState["phase"]): void {
    if (this.connection.phase === phase) return;
    this.connection = {
      ...this.connection,
      phase,
      last_bytes_age_ms: phase === "live" ? 0 : this.connection.last_bytes_age_ms,
    };
    this.emit({ t: "state", state: structuredClone(this.connection) });
  }

  private requirePaired(): void {
    if (this.revoked || this.certExpired || !this.paired) {
      throw new IpcError("shell.not_paired", "本机尚未配对或证书已失效。");
    }
  }

  private async refreshMe(): Promise<Me> {
    const me = await this.mustGet<Me>("/v1/me");
    this.me = me;
    this.clockSkewMs = Date.parse(me.server.time) - Date.now();
    const notAfter = me.certificate.not_after;
    this.connection = {
      ...this.connection,
      cert_not_after: notAfter,
      cert_expiring: Date.parse(notAfter) - this.serverNow() < 7 * 86_400_000,
      log_epoch: this.connection.log_epoch ?? me.server.log_epoch,
      clock_skew_ms: this.clockSkewMs,
    };
    return me;
  }

  private serverNow(): number {
    return Date.now() + this.clockSkewMs;
  }

  private identity(state: IdentityStatus["state"]): IdentityStatus {
    const device = state === "paired" ? this.me?.device ?? null : null;
    return {
      state,
      server_host: state === "unpaired" ? null : "mock-api.localhost",
      ca_fingerprint: null,
      device: device
        ? {
            id: device.id,
            name: device.name,
            platform: device.platform,
            pubkey_fpr: device.pubkey_fpr,
            approval_pubkey_fpr: device.approval_pubkey_fpr,
          }
        : null,
      cert:
        device && this.me
          ? {
              serial: device.cert_serial,
              not_after: this.me.certificate.not_after,
              renew_after: this.me.certificate.renew_after,
            }
          : null,
      key_backend: state === "paired" ? "mock_none" : null,
    };
  }

  private sheetModel(approval: Approval, me: Me, delivery: Delivery | null): ApprovalSheetModel {
    const pr = delivery?.pr ?? null;
    return {
      prepared_id: "",
      signed: {
        action: approval.action,
        action_label: ACTION_LABELS[approval.action] ?? approval.action,
        target: approval.target,
        target_label: targetLabel(approval.target),
        subject_digest: approval.subject_digest,
        digest_head12: approval.subject_digest.replace(/^sha256:/, "").slice(0, 12),
        expires_at: approval.expires_at,
        device_id: me.device.id,
        device_name: me.device.name,
        delivery_id: approval.delivery_id,
        tech: { approval_id: approval.id, nonce: "", v: 1 },
      },
      unsigned: {
        delivery_title: delivery
          ? { text: delivery.title, source: "delivery.title", truncated: false }
          : null,
        pr: pr
          ? {
              title: pr.title,
              checks: pr.checks,
              review_state: pr.review_state,
              mergeable_state: pr.mergeable_state,
              in_merge_queue: pr.in_merge_queue,
              synced_at: pr.synced_at,
            }
          : null,
        changes: null,
        note: null,
      },
      preview: { kind: approval.action },
      links: approval.target.kind === "pr" ? { commit: true, pr: true } : {},
      can_sign: { ok: false, reason: "not_pending" },
      challenge_expires_at: approval.expires_at,
      server_time_offset_ms: this.clockSkewMs,
    };
  }

  private async signInternal(
    signing: Signing,
    pendingId: string,
  ): Promise<{ ok: true; approvalId: string } | Problem | Extract<ApiResult, { network: unknown }>> {
    const created = await this.http<Approval>("POST", "/v1/approvals", {
      body: signing,
      idempotencyKey: `apr_${pendingId}`,
    });
    if (!created.ok) return created;
    if (!created.body) return localProblem(502, "internal", "审批创建缺少响应体。");
    const sheet = await this.approvalPrepare({ approvalId: created.body.id });
    if (!sheet.can_sign.ok) {
      return localProblem(403, "forbidden_role", "需要审批设备签名。");
    }
    const granted = await this.approvalSign({ preparedId: sheet.prepared_id });
    if (!granted.ok) return granted;
    return { ok: true, approvalId: created.body.id };
  }

  private sendWrite(args: GuardedWriteArgs, approvalId: string | null): Promise<ApiResult<unknown>> {
    const approval = approvalId ?? undefined;
    switch (args.kind) {
      case "settings_put":
        return this.http("PUT", settingsPath(args.scope, args.projectId, args.resource), {
          body: { value: args.value },
          ifMatch: args.ifMatch,
          approval,
        });
      case "settings_delete":
        return this.http("DELETE", settingsPath("project", args.projectId, args.resource), {
          ifMatch: args.ifMatch,
          approval,
        });
      case "device_role":
        return this.http("PUT", `/v1/devices/${args.deviceId}/role`, {
          body: { role: args.role },
          approval,
        });
      case "device_revoke":
        return this.http("POST", `/v1/devices/${args.deviceId}/revoke`, {
          body: {},
          approval,
        });
    }
  }

  private async mustGet<T>(path: string): Promise<T> {
    const result = await this.http<T>("GET", path);
    if (result.ok && result.body !== null) return result.body;
    if ("network" in result) {
      throw new IpcError("shell.not_connected", "无法连接服务端。");
    }
    throw new IpcError(
      result.ok ? "shell.bad_argument" : result.problem.code,
      result.ok ? "响应缺少内容。" : String(result.problem.detail ?? result.problem.title),
    );
  }

  private async optionalGet<T>(path: string): Promise<T | null> {
    const result = await this.http<T>("GET", path);
    return result.ok ? result.body : null;
  }

  private async http<T>(
    method: string,
    path: string,
    options: {
      query?: RouteQuery;
      body?: unknown;
      idempotencyKey?: string;
      ifMatch?: string;
      approval?: string;
    } = {},
  ): Promise<ApiResult<T>> {
    if (this.offline) return { ok: false, network: "offline" };
    const headers: Record<string, string> = { Accept: "application/json" };
    if (options.body !== undefined) headers["Content-Type"] = "application/json";
    if (options.idempotencyKey) headers["Idempotency-Key"] = options.idempotencyKey;
    if (options.ifMatch) headers["If-Match"] = options.ifMatch;
    if (options.approval) headers["Gantry-Approval"] = options.approval;
    let response: Response;
    try {
      response = await this.fetchImpl(this.url(path, options.query), {
        method,
        headers,
        body: options.body === undefined ? undefined : JSON.stringify(options.body),
      });
    } catch {
      return { ok: false, network: "offline" };
    }
    if (method !== "GET" && this.swallowNext) {
      this.swallowNext = false;
      return { ok: false, network: "timeout" };
    }
    const text = await response.text();
    const json = text ? safeJson(text) : null;
    if (response.ok || response.status === 304) {
      return {
        ok: true,
        status: response.status,
        headers: responseHeaders(response),
        body: (json as T | null) ?? null,
      };
    }
    const problem = (json ?? {}) as Record<string, unknown>;
    return {
      ok: false,
      status: response.status,
      problem: {
        ...problem,
        code: typeof problem.code === "string" ? problem.code : `http_${response.status}`,
        title: typeof problem.title === "string" ? problem.title : response.statusText,
        request_id:
          typeof problem.request_id === "string"
            ? problem.request_id
            : (response.headers.get("X-Request-Id") ?? ""),
      },
    };
  }

  private url(path: string, query?: RouteQuery): string {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query ?? {})) {
      if (value === undefined) continue;
      for (const item of Array.isArray(value) ? value : [value]) {
        params.append(key, String(item));
      }
    }
    const search = params.toString();
    return `${this.baseUrl}${path}${search ? `?${search}` : ""}`;
  }

  private log(command: string, args: Record<string, unknown> = {}): void {
    this.calls.push({ command, args });
    if (this.calls.length > 1000) this.calls.shift();
  }
}

function settingsPath(
  scope: "global" | "project",
  projectId: string | undefined,
  resource: string,
): string {
  return scope === "global"
    ? `/v1/settings/${resource}`
    : `/v1/projects/${projectId ?? ""}/settings/${resource}`;
}

function targetLabel(target: ApprovalTarget): string {
  switch (target.kind) {
    case "pr":
      return `${target.repo} · PR #${target.number}`;
    case "branch":
      return `${target.repo} · ${target.branch}`;
    case "review_thread":
      return `${target.repo} · PR #${target.number} 评审线程`;
    case "patch":
      return `${target.repo} · 补丁`;
    case "settings":
      return target.scope === "global"
        ? `全局设置 · ${target.resource}`
        : `项目设置 · ${target.resource}`;
    case "device":
      return `设备 ${target.id}`;
  }
}

function responseHeaders(response: Response): ApiHeaders {
  const headers: ApiHeaders = {};
  const etag = response.headers.get("ETag");
  if (etag) headers.etag = etag;
  const seq = response.headers.get("X-Event-Seq");
  if (seq !== null && seq !== "") headers.x_event_seq = Number(seq);
  if (response.headers.get("Idempotent-Replayed") === "true") headers.idempotent_replayed = true;
  const retry = response.headers.get("Retry-After");
  if (retry) headers.retry_after = Number(retry);
  return headers;
}

function problemIssues(result: ApiResult<unknown>): SettingsIssue[] {
  if (result.ok) return [];
  if ("network" in result) {
    return [{ pointer: "", code: "shell.not_connected", message: "无法连接服务端。" }];
  }
  const errors = result.problem.errors;
  if (Array.isArray(errors) && errors.length > 0) return errors as SettingsIssue[];
  return [
    {
      pointer: "",
      code: result.problem.code,
      message: String(result.problem.detail ?? result.problem.title),
    },
  ];
}

function localProblem(status: number, code: string, detail: string): Problem {
  return { ok: false, status, problem: { code, title: code, detail, request_id: "" } };
}

function redactWrite(a: GuardedWriteArgs): Record<string, unknown> {
  // 设置值可能含凭据：日志只记录键，不记录值（desktop-shell S-2）。
  const { value: _value, ...rest } = a as GuardedWriteArgs & { value?: unknown };
  void _value;
  return { ...rest };
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

/** RFC 8785 的最小子集：对象键排序，其余交给 JSON.stringify；足够覆盖摘要输入。 */
export function jcs(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(jcs).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .filter((key) => record[key] !== undefined)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${jcs(record[key])}`)
    .join(",")}}`;
}

async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function randomToken(bytes: number): string {
  const buffer = new Uint8Array(bytes);
  crypto.getRandomValues(buffer);
  return [...buffer].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** 64 字节随机数的 base64url（86 字符）；只满足 Mock 的格式校验，不是签名。 */
function placeholderSignature(): string {
  const buffer = new Uint8Array(64);
  crypto.getRandomValues(buffer);
  let binary = "";
  for (const b of buffer) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
