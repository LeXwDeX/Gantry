import {
  IpcError,
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
  ApiList,
  Approval as ApprovalEntity,
  Attempt,
  BudgetLedger,
  Delivery,
  Device,
  EventEnvelope,
  InboxItem,
  Level,
  Message,
  Project,
  Ref,
  SettingsCatalog,
  SettingsDocument,
  SettingsIssue,
  UntrustedText,
  Workflow,
  WorkflowNode,
} from "@/api/types";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import type {
  Simulation,
  SimulationAction,
  SimulationAwaiting,
  SimulationErrorKind,
  SimulationScript,
  SimulationScriptId,
  SimulationSnapshot,
  SimulationToolActivity,
  SimulationToolStatus,
} from "@/demo/types";
import { SIMULATION_SCRIPTS } from "@/demo/types";
import {
  ACTION_DEFINITIONS,
  DEFAULT_PREFS,
  DEFAULT_PROJECT_UI,
  SCENARIO_LEVEL_BUDGETS,
  SCENARIO_PR9_SHA,
  SCENARIO_PR9_STALE_SHA,
  SCENARIO_SERVER_TIME,
  buildScenarioData,
  summarizeInbox,
  SCENARIO_PROJECT_COUNTS,
  type ScenarioData,
  type ScenarioName,
} from "./data";

type PageQuery = {
  cursor?: string | number;
  limit?: number;
  order?: "asc" | "desc";
};

type PendingWrite = {
  args: GuardedWriteArgs;
  value: unknown | null;
  secretRefs: string[];
  ifMatch: string;
  requiredRole: GuardedWritePreview["required_role"];
  requiresSignature: boolean;
};

type SimCreated = {
  messages: Set<string>;
  deliveries: Set<string>;
  workflows: Set<string>;
  inbox: Set<string>;
  approvals: Set<string>;
  attempts: Set<string>;
};

type SimStep = {
  delayMs: number;
  label: string;
  run: () => void;
  after?: () => void;
};

type SimTask = {
  delivery: Delivery;
  workflow: Workflow;
  plan: WorkflowNode;
  build: WorkflowNode;
  verify: WorkflowNode;
  buildAttempt: Attempt;
  verifyAttempt: Attempt | null;
};

const EMPTY_CREATED = (): SimCreated => ({
  messages: new Set(),
  deliveries: new Set(),
  workflows: new Set(),
  inbox: new Set(),
  approvals: new Set(),
  attempts: new Set(),
});

const EMPTY_SIMULATION: SimulationSnapshot = {
  demo: true,
  script_id: null,
  project_id: null,
  run_id: 0,
  status: "idle",
  phase: "idle",
  turn: 0,
  total_turns: 0,
  drafts: [],
  activities: [],
  summaries: [],
  pending_approval_id: null,
  awaiting: null,
  last_error: null,
};

/** 开发场景替身：只在显式 `--mode scenario` 下由工厂动态导入。 */
export class ScenarioIpcAdapter implements IpcAdapter {
  readonly scenario: ScenarioName;
  readonly isScenario = true;
  readonly externalCalls: string[] = [];
  readonly githubCalls: OpenGithubArgs[] = [];
  /**
   * 临时演示边界。它不是正式 IpcAdapter 的一部分：正式实体仍由 api() 与事件流提供，
   * 工具活动、公开摘要和分段生成只在本对象内。
   */
  readonly simulation: Simulation;

  private data: ScenarioData;
  private acked: number | null = null;
  private prefs: Prefs = clone(DEFAULT_PREFS);
  private uiStates = new Map<string, ProjectUiState>();
  private listener: ((frame: StreamFrame) => void) | null = null;
  private seq = 18_240;
  private errorInjected = false;
  private injectedServerError = false;
  private conflictOnNextSubmit = false;
  private pendingWrites = new Map<string, PendingWrite>();
  private pendingSeq = 0;
  private pendingPairing: { code: string; device: Device } | null = null;
  private failNextPairingAttempt = false;
  private idempotentMessages = new Map<
    string,
    { conversationId: string; text: string; message: Message }
  >();

  private simulationState: SimulationSnapshot = EMPTY_SIMULATION;
  private readonly simulationListeners = new Set<() => void>();
  private simScript: SimulationScriptId | null = null;
  private simSteps: SimStep[] = [];
  private simIndex = 0;
  private simTimer: ReturnType<typeof setTimeout> | null = null;
  private simAdvancing = false;
  private simRun = 0;
  private simCreated: SimCreated = EMPTY_CREATED();
  private simTask: SimTask | null = null;
  private simRetryTask: SimTask | null = null;
  private simAwaiting: SimulationAwaiting = null;
  private simLastMessage: Message | null = null;
  private simPendingChoice: InboxItem | null = null;

  constructor(scenario: ScenarioName = "default") {
    this.scenario = scenario;
    this.data = buildScenarioData(scenario);
    this.simulation = {
      scripts: (): readonly SimulationScript[] => SIMULATION_SCRIPTS,
      getSnapshot: () => this.simulationState,
      subscribe: (listener: () => void): (() => void) => {
        this.simulationListeners.add(listener);
        return () => {
          this.simulationListeners.delete(listener);
        };
      },
      select: (id, opts) => this.simSelect(id, opts),
      play: () => this.simPlay(),
      step: () => this.simStep(),
      pause: () => this.simPause(),
      resume: () => this.simResume(),
      cancel: () => this.simCancel(),
      reset: () => this.simReset(),
      advance: (ms) => this.simAdvance(ms ?? 0),
      simulateApproval: (approvalId) => this.simApproval(approvalId),
      simulateDenial: (approvalId) => this.simDenial(approvalId),
      failNextPairing: () => {
        this.failNextPairingAttempt = true;
        return {
          ok: true,
          message: "下一次配对预检将模拟失败；不会生成密钥或证书。",
        };
      },
      expireApproval: (approvalId) => this.simExpire(approvalId),
      setHeadChanged: (approvalId, changed) =>
        this.simHeadChanged(approvalId, changed ?? true),
      setConnection: (phase) => {
        this.setConnection(phase);
        return { ok: true };
      },
      setRole: (role) => {
        this.setRole(role);
        return { ok: true };
      },
      injectError: (kind) => this.simInjectError(kind),
    };
  }

  async identityStatus(): Promise<IdentityStatus> {
    return clone(this.data.identity);
  }

  async pairingBegin(a: {
    pairingString: string;
    deviceName: string;
  }): Promise<{ confirmationCode: string; device: Device }> {
    const name = a.deviceName.trim();
    if (!a.pairingString.trim() || !name) {
      throw new IpcError("shell.bad_argument", "配对串和设备名不能为空。");
    }
    if (name.length > 64) {
      throw new IpcError("shell.bad_argument", "设备名必须为 1–64 字符。");
    }
    if (this.failNextPairingAttempt) {
      this.failNextPairingAttempt = false;
      throw new IpcError(
        "shell.pairing_failed",
        "模拟配对预检失败：未联系服务端，也未生成密钥、证书或系统认证。可再次重试。",
      );
    }
    // 只在内存保留确认码与公开设备字段；不生成密钥、证书或持久化配对串。
    const code = "492 713";
    const device: Device = {
      id: "dev_scenario_pending",
      name,
      platform: "macos",
      cert_serial: "test-cert-0000000000000",
      pubkey_fpr: "sha256:test-only-device-key-fingerprint-000000000000000000",
      approval_pubkey_fpr:
        "sha256:test-only-approval-key-fingerprint-0000000000000000000",
      role: "operator",
      status: "active",
      last_seen: new Date().toISOString(),
    };
    this.pendingPairing = { code, device };
    return { confirmationCode: code, device: clone(device) };
  }

  async pairingConfirm(a: { matches: boolean }): Promise<IdentityStatus> {
    if (!this.pendingPairing) {
      throw new IpcError("shell.bad_argument", "没有待确认的配对请求。");
    }
    const pending = this.pendingPairing;
    this.pendingPairing = null;
    if (!a.matches) {
      this.data.identity = {
        ...this.data.identity,
        state: "unpaired",
        device: null,
        cert: null,
      };
      this.setConnection("unpaired");
      return clone(this.data.identity);
    }
    const device = { ...pending.device, status: "active" as const };
    this.data.devices = [
      ...this.data.devices.filter((entry) => entry.id !== device.id),
      device,
    ];
    this.data.me = {
      ...this.data.me,
      device,
      certificate: {
        not_after: "2026-11-24T12:00:00.000Z",
        renew_after: "2026-11-14T12:00:00.000Z",
      },
      server: { ...this.data.me.server, time: new Date().toISOString() },
    };
    this.data.identity = {
      state: "paired",
      server_host: this.data.identity.server_host ?? "gantry.example.com:8443",
      ca_fingerprint: this.data.identity.ca_fingerprint,
      device: {
        id: device.id,
        name: device.name,
        platform: device.platform,
        pubkey_fpr: device.pubkey_fpr,
        approval_pubkey_fpr: device.approval_pubkey_fpr,
      },
      cert: {
        serial: device.cert_serial,
        not_after: "2026-11-24T12:00:00.000Z",
        renew_after: "2026-11-14T12:00:00.000Z",
      },
      key_backend: "test_software",
    };
    this.data.connection = {
      ...this.data.connection,
      phase: "live",
      epoch: this.data.connection.epoch + 1,
      last_bytes_age_ms: 300,
      cert_not_after: "2026-11-24T12:00:00.000Z",
    };
    this.emitEvent("device.paired", device.id, null, { device_id: device.id });
    this.emit({ t: "state", state: clone(this.data.connection) });
    return clone(this.data.identity);
  }

  async identityReset(): Promise<IdentityStatus> {
    this.pendingPairing = null;
    // 旧服务端的项目 ID 不再适用；偏好保留。
    this.uiStates.clear();
    this.data.identity = {
      ...this.data.identity,
      state: "unpaired",
      device: null,
      cert: null,
    };
    this.setConnection("unpaired");
    return clone(this.data.identity);
  }

  async connectionState(): Promise<ConnectionState> {
    return clone(this.data.connection);
  }

  async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    const result = await this.handle(route, args as unknown);
    return result as ApiResult<RouteBody[R]>;
  }

  async eventsOpen(
    a: { afterSeq?: number },
    onFrame: (f: StreamFrame) => void,
  ): Promise<void> {
    void a;
    this.listener = onFrame;
    onFrame({ t: "state", state: clone(this.data.connection) });
  }

  async eventsAck(seq: number): Promise<void> {
    this.acked = this.acked === null ? seq : Math.max(this.acked, seq);
  }

  get ackedSeq(): number | null {
    return this.acked;
  }

  async eventsClose(): Promise<void> {
    this.listener = null;
    this.simStopTimer();
    if (this.simulationState.status === "running") {
      this.simPatch({ status: "paused", phase: "paused_by_close" });
    }
  }

  async approvalPrepare(a: { approvalId: string }): Promise<ApprovalSheetModel> {
    const approval = this.findApproval(a.approvalId);
    if (!approval) throw new IpcError("shell.bad_argument", "示例数据中没有该审批。");
    const delivery = approval.delivery_id
      ? this.data.deliveries.find((d) => d.id === approval.delivery_id) ?? null
      : null;
    return {
      prepared_id: `prepared_${this.scenario}_${approval.id}`,
      signed: {
        action: approval.action,
        action_label: "合并",
        target: approval.target,
        target_label:
          approval.target.kind === "pr"
            ? `${approval.target.repo} · PR #${approval.target.number}`
            : "示例目标",
        subject_digest: approval.subject_digest,
        digest_head12: approval.subject_digest.slice(0, 12),
        expires_at: approval.expires_at,
        device_id: this.data.me.device.id,
        device_name: this.data.me.device.name,
        delivery_id: approval.delivery_id,
        tech: {
          approval_id: approval.id,
          nonce: "c2NlbmFyaW8tbm9uY2UtcGxhY2Vob2xkZXI",
          v: 1,
        },
      },
      unsigned: {
        delivery_title: delivery
          ? { text: delivery.title, source: "delivery.title", truncated: false }
          : null,
        pr: delivery?.pr
          ? {
              title: delivery.pr.title,
              checks: delivery.pr.checks,
              review_state: delivery.pr.review_state,
              mergeable_state: delivery.pr.mergeable_state,
              in_merge_queue: delivery.pr.in_merge_queue,
              synced_at: delivery.pr.synced_at,
            }
          : null,
        changes: null,
        note: "示例模式：未连接真实外壳；改动明细未提供。",
      },
      preview: { kind: approval.action },
      links: { commit: true, pr: true },
      can_sign: { ok: false, reason: "not_connected" },
      challenge_expires_at: new Date(Date.now() + 120_000).toISOString(),
      server_time_offset_ms: 0,
    };
  }

  async approvalSign(_a: { preparedId: string }): Promise<ApiResult<ApprovalEntity>> {
    void _a;
    throw new IpcError(
      "shell.key_unavailable",
      "示例模式没有审批签名密钥，也不会修改审批状态。",
    );
  }

  async approvalDiscard(_a: { preparedId: string }): Promise<void> {
    void _a;
  }

  async guardedWritePrepare(a: GuardedWriteArgs): Promise<GuardedWritePreview> {
    return this.prepareWrite(a);
  }

  async guardedWriteSubmit(a: {
    pendingId: string;
  }): Promise<ApiResult<unknown>> {
    return this.submitWrite(a.pendingId);
  }

  async guardedWriteDiscard(a: { pendingId: string }): Promise<void> {
    this.pendingWrites.delete(a.pendingId);
  }

  async prefsGet(): Promise<Prefs> {
    return clone(this.prefs);
  }

  async prefsSet(a: { patch: Partial<Prefs> }): Promise<Prefs> {
    this.prefs = mergePrefs(this.prefs, a.patch);
    return clone(this.prefs);
  }

  async uiStateGet(a: { projectId: string }): Promise<ProjectUiState | null> {
    const value = this.uiStates.get(a.projectId);
    return value ? clone(value) : null;
  }

  async uiStateSet(a: {
    projectId: string;
    state: ProjectUiState;
  }): Promise<void> {
    this.uiStates.set(a.projectId, clone(a.state));
  }

  async uiStateList(): Promise<Record<string, ProjectUiState>> {
    const result: Record<string, ProjectUiState> = {};
    for (const [key, value] of this.uiStates) result[key] = clone(value);
    return result;
  }

  async openExternal(a: { url: string }): Promise<void> {
    let parsed: URL;
    try {
      parsed = new URL(a.url);
    } catch {
      throw new IpcError("shell.bad_argument", "外链不是合法 URL。");
    }
    if (parsed.protocol !== "https:") {
      throw new IpcError("shell.bad_argument", "只允许在系统浏览器打开 https 外链。");
    }
    this.externalCalls.push(a.url);
  }

  async openGithub(a: OpenGithubArgs): Promise<void> {
    if (!/^[^/\s]+\/[^/\s]+$/.test(a.repo)) {
      throw new IpcError("shell.bad_argument", "repo 必须是 owner/repo。");
    }
    this.githubCalls.push(clone(a));
  }

  /** 测试钩子：切换连接状态并以 state 帧通知界面。 */
  setConnection(phase: ConnectionState["phase"]): void {
    this.data.connection = {
      ...this.data.connection,
      phase,
      epoch: this.data.connection.epoch + (phase === "live" ? 1 : 0),
      last_bytes_age_ms: phase === "live" ? 400 : 65_000,
    };
    this.emit({ t: "state", state: clone(this.data.connection) });
    if (phase === "live" && this.simulationState.awaiting?.kind === "connection") {
      this.simResumeConnection();
    }
  }

  /** 测试钩子：把本机设备切换成其他角色并发出 device.updated 提示。 */
  setRole(role: "viewer" | "operator" | "approver"): void {
    this.data.me = {
      ...this.data.me,
      device: { ...this.data.me.device, role },
    };
    if (this.data.identity.device) {
      this.data.identity = {
        ...this.data.identity,
        device: { ...this.data.identity.device },
      };
    }
    this.data.devices = this.data.devices.map((d) =>
      d.id === this.data.me.device.id ? { ...d, role } : d,
    );
    this.emitEvent("device.updated", this.data.me.device.id, null, {
      device_id: this.data.me.device.id,
    });
  }

  /** 测试钩子：注入任意流帧。 */
  emitFrame(frame: StreamFrame): void {
    this.emit(frame);
  }

  /** 测试钩子：模拟服务端日志纪元变化（通过 state 帧送达）。 */
  bumpLogEpoch(): void {
    const next = (this.data.connection.log_epoch ?? 1) + 1;
    this.data.connection = { ...this.data.connection, log_epoch: next };
    this.data.me = {
      ...this.data.me,
      server: { ...this.data.me.server, log_epoch: next },
    };
    this.emit({ t: "state", state: clone(this.data.connection) });
  }

  private prepareWrite(args: GuardedWriteArgs): GuardedWritePreview {
    const pendingId = `pending_scenario_${++this.pendingSeq}`;
    const warnings: SettingsIssue[] = [
      {
        pointer: "",
        code: "scenario_mode",
        message:
          "示例模式：本次只会修改内存中的模拟设置；不产生真实签名、凭据、证书或网络写入。",
      },
    ];
    let errors: SettingsIssue[] = [];
    let relaxes: string[] = [];
    let requiredRole: GuardedWritePreview["required_role"] = "operator";
    let requiresSignature = false;
    let ifMatch = "";
    let value: unknown | null = null;
    let secretRefs: string[] = [];

    if (args.kind === "settings_put" || args.kind === "settings_delete") {
      const scope = args.kind === "settings_put" ? args.scope : "project";
      const projectId = args.kind === "settings_put" ? args.projectId : args.projectId;
      if (!SETTINGS_RESOURCE_IDS.has(args.resource)) {
        errors = [{ pointer: "", code: "unknown_field", message: "未知设置资源。" }];
      } else if (scope === "project" && !projectId) {
        errors = [{ pointer: "", code: "required", message: "缺少项目 ID。" }];
      } else if (
        scope === "project" &&
        !this.data.projects.some((project) => project.id === projectId)
      ) {
        errors = [{ pointer: "", code: "not_found", message: "项目不存在。" }];
      } else if (scope === "project" && !PROJECT_OVERRIDABLE.has(args.resource)) {
        errors = [
          {
            pointer: "",
            code: "not_overridable",
            message: `${args.resource} 只能全局配置，不能按项目覆盖。`,
          },
        ];
      } else {
        const doc = this.currentSettingsDoc(scope, projectId, args.resource);
        if (!doc) {
          errors = [{ pointer: "", code: "not_found", message: "设置资源不存在。" }];
        } else {
          ifMatch = args.kind === "settings_put" ? args.ifMatch : args.ifMatch;
          if (ifMatch !== etagOf(doc)) {
            errors = [
              {
                pointer: "",
                code: "precondition_failed",
                message: "设置已被其他写入更新；请重新载入后再编辑。",
              },
            ];
          } else if (args.kind === "settings_put") {
            const prepared = prepareSettingsValue(
              args.resource,
              args.value,
              scope === "project" ? this.data.settings[args.resource]?.value : null,
              scope === "project",
            );
            errors = prepared.errors;
            relaxes = scope === "global" ? computeRelaxes(args.resource, args.value, doc.value) : [];
            secretRefs = prepared.secretRefs;
            value = prepared.clean;
            if (errors.length === 0) {
              requiredRole = secretRefs.length > 0 || relaxes.length > 0 ? "approver" : "operator";
              requiresSignature = requiredRole === "approver";
            }
          }
        }
      }
    } else if (args.kind === "device_role") {
      const device = this.data.devices.find((entry) => entry.id === args.deviceId);
      requiredRole = "approver";
      requiresSignature = true;
      if (!device) {
        errors = [{ pointer: "", code: "not_found", message: "设备不存在。" }];
      } else if (device.id === this.data.me.device.id) {
        errors = [
          { pointer: "", code: "self_role_change", message: "不能修改自己的设备角色。" },
        ];
      } else if (device.platform === "ios" && args.role === "approver") {
        errors = [
          { pointer: "", code: "mobile_cannot_approve", message: "手机设备不能成为审批设备。" },
        ];
      } else if (device.status === "revoked") {
        errors = [{ pointer: "", code: "invalid_state", message: "设备已吊销。" }];
      }
    } else if (args.kind === "device_revoke") {
      const device = this.data.devices.find((entry) => entry.id === args.deviceId);
      requiredRole = "approver";
      requiresSignature = true;
      if (!device) {
        errors = [{ pointer: "", code: "not_found", message: "设备不存在。" }];
      } else if (device.id === this.data.me.device.id) {
        errors = [
          { pointer: "", code: "self_revoke", message: "不能吊销当前正在使用的设备。" },
        ];
      } else if (device.status === "revoked") {
        errors = [{ pointer: "", code: "invalid_state", message: "设备已吊销。" }];
      }
    } else {
      errors = [{ pointer: "", code: "bad_argument", message: "未知写入类型。" }];
    }

    const role = this.role();
    const canSubmit =
      errors.length > 0
        ? ({ ok: false, reason: "invalid" } as const)
        : requiredRole === "approver" && role !== "approver"
          ? ({ ok: false, reason: "not_approver" } as const)
          : role === "viewer"
            ? ({ ok: false, reason: "invalid" } as const)
            : ({ ok: true } as const);
    if (canSubmit.ok) {
      this.pendingWrites.set(pendingId, {
        args,
        value,
        secretRefs,
        ifMatch,
        requiredRole,
        requiresSignature,
      });
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
        mode: "scenario",
        kind: args.kind,
        resource: "resource" in args ? args.resource : undefined,
        scope: "scope" in args ? args.scope : "project",
        note: "仅保存在开发场景内存中；回读立即生效。",
      },
      can_submit: canSubmit,
    };
  }

  private async submitWrite(pendingId: string): Promise<ApiResult<unknown>> {
    const pending = this.pendingWrites.get(pendingId);
    if (!pending) {
      return problem(409, "pending_not_found", "写入预览已失效，请重新保存。");
    }
    this.pendingWrites.delete(pendingId);
    const role = this.role();
    if (role === "viewer") {
      return problem(403, "forbidden_role", "此设备为只读。");
    }
    if (pending.requiredRole === "approver" && role !== "approver") {
      return problem(403, "forbidden_role", "此改动需要审批设备角色。");
    }
    if (pending.args.kind === "device_role") {
      const target = pending.args;
      const device = this.data.devices.find(
        (entry) => entry.id === target.deviceId,
      );
      if (!device) return problem(404, "not_found", "Device not found.");
      if (device.id === this.data.me.device.id) {
        return problem(409, "self_role_change", "不能修改自己的设备角色。");
      }
      if (device.platform === "ios" && target.role === "approver") {
        return problem(422, "mobile_cannot_approve", "手机设备不能成为审批设备。");
      }
      device.role = target.role;
      this.emitEvent("device.updated", device.id, null, { device_id: device.id });
      return ok(clone(device));
    }
    if (pending.args.kind === "device_revoke") {
      const target = pending.args;
      const device = this.data.devices.find(
        (entry) => entry.id === target.deviceId,
      );
      if (!device) return problem(404, "not_found", "Device not found.");
      if (device.id === this.data.me.device.id) {
        return problem(409, "self_revoke", "不能吊销当前正在使用的设备。");
      }
      device.status = "revoked";
      this.emitEvent("device.revoked", device.id, null, { device_id: device.id });
      return ok(clone(device));
    }

    const scope = pending.args.kind === "settings_delete" ? "project" : pending.args.scope;
    const projectId =
      pending.args.kind === "settings_delete"
        ? pending.args.projectId
        : pending.args.projectId;
    if (!projectId && scope === "project") {
      return problem(400, "bad_argument", "缺少项目 ID。");
    }
    const current = this.currentSettingsDoc(scope, projectId, pending.args.resource);
    if (!current) return problem(404, "not_found", "Settings resource not found.");
    if (this.conflictOnNextSubmit) {
      this.conflictOnNextSubmit = false;
      if (scope === "global") {
        const doc = this.data.settings[pending.args.resource];
        if (doc) doc.rev += 1;
      } else if (projectId) {
        const doc = this.data.projectSettings[projectId]?.[pending.args.resource];
        if (doc) doc.rev += 1;
      }
    }
    const latest = this.currentSettingsDoc(scope, projectId, pending.args.resource);
    if (!latest || pending.ifMatch !== etagOf(latest)) {
      return problem(
        412,
        "precondition_failed",
        "设置已被其他写入更新；请重新载入后再保存。",
      );
    }
    if (pending.args.kind === "settings_delete") {
      if (projectId) {
        this.data.projectSettings[projectId][pending.args.resource] = {
          resource: pending.args.resource,
          scope: "project",
          project_id: projectId,
          rev: 0,
          value: null,
        };
      }
      this.emitEvent("settings.updated", projectId ?? "settings", null, {
        resource: pending.args.resource,
        scope: "project",
        rev: 0,
      });
      return withHeaders(
        clone(this.data.projectSettings[projectId!][pending.args.resource]),
        200,
        { etag: '"0"', x_event_seq: 0 },
      );
    }

    const next = applySettingValue(
      scope,
      projectId,
      pending.args.resource,
      pending.value,
      latest,
      pending.secretRefs,
    );
    if (scope === "global") {
      this.data.settings[pending.args.resource] = next;
      this.updateSettingsStatus(pending.args.resource, next.rev);
      this.emitEvent("settings.updated", "settings", null, {
        resource: pending.args.resource,
        scope: "global",
        rev: next.rev,
      });
    } else if (projectId) {
      this.data.projectSettings[projectId][pending.args.resource] = next;
      this.emitEvent("settings.updated", projectId, null, {
        resource: pending.args.resource,
        scope: "project",
        rev: next.rev,
      });
    }
    return withHeaders(clone(next), 200, {
      etag: `"${next.rev}"`,
      x_event_seq: next.rev,
    });
  }

  private currentSettingsDoc(
    scope: "global" | "project",
    projectId: string | undefined,
    resource: string,
  ): SettingsDocument | null {
    if (scope === "global") return this.data.settings[resource] ?? null;
    return projectId ? this.data.projectSettings[projectId]?.[resource] ?? null : null;
  }

  private updateSettingsStatus(resource: string, rev: number): void {
    this.data.status.latest_rev = Math.max(this.data.status.latest_rev, rev);
    const module = moduleForResource(resource);
    const entry = this.data.status.modules[module];
    if (entry) entry.applied_rev = rev;
    else this.data.status.modules[module] = { applied_rev: rev, health: "ok" };
  }

  private async handle(route: RouteId, raw: unknown): Promise<ApiResult<unknown>> {
    const args = (raw ?? {}) as Record<string, unknown>;
    if (this.scenario === "error" && route === "me.get" && !this.errorInjected) {
      this.errorInjected = true;
      return problem(500, "internal", "Scenario injected error.");
    }
    if (route === "me.get" && this.injectedServerError) {
      this.injectedServerError = false;
      return problem(500, "internal", "Scenario injected server error.");
    }
    if (
      this.role() === "viewer" &&
      (route.startsWith("settings.") ||
        route.startsWith("project_settings.") ||
        route === "settings.catalog" ||
        route === "settings.status")
    ) {
      return problem(403, "forbidden_role", "Viewer cannot read settings.");
    }
    switch (route) {
      case "me.get":
        return ok(this.data.me);
      case "projects.list":
        return ok(paginate(this.data.projects, args));
      case "projects.get": {
        const project = this.data.projects.find((p) => p.id === args.projectId);
        return project ? ok(project) : problem(404, "not_found", "Project not found.");
      }
      case "deliveries.list": {
        let items = this.data.deliveries;
        if (typeof args.projectId === "string") {
          items = items.filter((d) => d.project_id === args.projectId);
        }
        if (args.active === true) {
          items = items.filter((d) => d.stage !== "closed" && d.stage !== "abandoned");
        }
        if (Array.isArray(args.stage) && args.stage.length > 0) {
          const stages = args.stage as string[];
          items = items.filter((d) => stages.includes(d.stage));
        }
        if (typeof args.stageStatus === "string") {
          items = items.filter((d) => d.stage_status === args.stageStatus);
        }
        return ok(paginate(items, args));
      }
      case "deliveries.get": {
        const item = this.data.deliveries.find((d) => d.id === args.deliveryId);
        return item ? ok(item) : problem(404, "not_found", "Delivery not found.");
      }
      case "deliveries.patch": {
        if (!operator(this.role())) return forbiddenOperator();
        const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
        const level = args.levelCap as Level | undefined;
        if (!delivery || !level) return problem(404, "not_found", "Delivery not found.");
        if (delivery.stage === "closed" || delivery.stage === "abandoned") {
          return problem(409, "conflict", "Delivery is terminal.");
        }
        delivery.level_cap = level;
        if (LEVEL_RANK[level] < LEVEL_RANK[delivery.level_effective]) {
          delivery.level_effective = level;
        }
        this.emitEvent("delivery.level_changed", delivery.id, delivery.project_id, {
          delivery_id: delivery.id,
          level_cap: level,
        });
        return ok(delivery);
      }
      case "deliveries.abandon": {
        if (!operator(this.role())) return forbiddenOperator();
        const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
        if (!delivery) return problem(404, "not_found", "Delivery not found.");
        delivery.stage = "abandoned";
        delivery.stage_status = "done";
        this.workflowsFor(delivery.id).forEach((workflow) => {
          workflow.status = "cancelled";
          for (const entry of workflow.nodes ?? []) {
            if (entry.status === "running" || entry.status === "pending" || entry.status === "queued") {
              entry.status = "cancelled";
            }
          }
        });
        this.syncDeliveryInbox(delivery);
        if (this.simTask?.delivery.id === delivery.id) this.simCancel();
        this.emitEvent("delivery.abandoned", delivery.id, delivery.project_id, {
          delivery_id: delivery.id,
        });
        return ok(delivery);
      }
      case "deliveries.pause": {
        if (!operator(this.role())) return forbiddenOperator();
        const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
        if (!delivery) return problem(404, "not_found", "Delivery not found.");
        if (delivery.stage === "closed" || delivery.stage === "abandoned") {
          return problem(409, "conflict", "Delivery is terminal.");
        }
        if (delivery.stage_status !== "paused") {
          delivery.stage_status = "paused";
          this.workflowsFor(delivery.id).forEach((workflow) => {
            workflow.status = "paused";
          });
          if (this.simTask?.delivery.id === delivery.id) this.simPause();
          this.emitEvent("delivery.paused", delivery.id, delivery.project_id, {
            delivery_id: delivery.id,
          });
        }
        return ok(delivery);
      }
      case "deliveries.resume": {
        if (!operator(this.role())) return forbiddenOperator();
        const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
        if (!delivery) return problem(404, "not_found", "Delivery not found.");
        if (delivery.stage === "closed" || delivery.stage === "abandoned") {
          return problem(409, "conflict", "Delivery is terminal.");
        }
        if (delivery.stage_status !== "paused") {
          return ok(delivery);
        }
        delivery.stage_status = "running";
        this.workflowsFor(delivery.id).forEach((workflow) => {
          if (workflow.status === "paused") workflow.status = "running";
        });
        if (
          this.simTask?.delivery.id === delivery.id &&
          this.simulationState.status === "paused" &&
          !this.simAwaiting
        ) {
          this.simPlay();
        }
        this.emitEvent("delivery.resumed", delivery.id, delivery.project_id, {
          delivery_id: delivery.id,
        });
        return ok(delivery);
      }
      case "deliveries.budget": {
        const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
        if (!delivery) return problem(404, "not_found", "Delivery not found.");
        const ledger = this.data.budgets[delivery.id];
        return ledger
          ? ok(clone(ledger))
          : problem(404, "not_found", "Budget ledger not found.");
      }
      case "conversations.list": {
        const items = Object.values(this.data.conversations).filter(
          (c) => c.project_id === args.projectId,
        );
        return ok(paginate(items, args));
      }
      case "messages.list": {
        const conversationId = String(args.conversationId ?? "");
        const project = Object.values(this.data.conversations).find(
          (c) => c.id === conversationId,
        );
        if (!project) return problem(404, "not_found", "Conversation not found.");
        const messages = this.data.messages[project.project_id] ?? [];
        const page = pageWindow(messages, args, "desc");
        return ok({ items: page.items, next_cursor: page.next_cursor });
      }
      case "messages.post": {
        if (!operator(this.role())) return forbiddenOperator();
        const conversationId = String(args.conversationId ?? "");
        const project = Object.values(this.data.conversations).find(
          (c) => c.id === conversationId,
        );
        if (!project) return problem(404, "not_found", "Conversation not found.");
        const body = args.body as
          | { text?: string; level_cap?: Level; refs?: Ref[] }
          | undefined;
        const text = body?.text?.trim();
        if (!text) return invalid("text 不能为空");
        if (text.length > 20_000) return invalid("text 超过本地演示上限");
        const idempotencyKey = String(args.idempotencyKey ?? "").trim();
        if (idempotencyKey) {
          const previous = this.idempotentMessages.get(idempotencyKey);
          if (previous) {
            if (previous.conversationId !== conversationId || previous.text !== text) {
              return problem(
                409,
                "idempotency_conflict",
                "同一 Idempotency-Key 已用于不同的消息。",
              );
            }
            return withHeaders(clone(previous.message), 201, {
              idempotent_replayed: true,
            });
          }
        }
        const message: Message = {
          id: `msg_scenario_${++this.seq}`,
          conversation_id: conversationId,
          role: "user",
          agent_kind: null,
          tone: null,
          text,
          level_cap: body?.level_cap ?? null,
          refs: body?.refs ?? [],
          cards: [],
          delivery_id: null,
          created_at: new Date().toISOString(),
        };
        this.data.messages[project.project_id] = [
          ...(this.data.messages[project.project_id] ?? []),
          message,
        ];
        if (idempotencyKey) {
          this.idempotentMessages.set(idempotencyKey, {
            conversationId,
            text,
            message: clone(message),
          });
        }
        this.emitEvent("message.posted", message.id, project.project_id, {
          message_id: message.id,
        });
        this.simBeginFromUserMessage(project.project_id, message);
        return ok(message, 201);
      }
      case "worktrees.list": {
        let items = this.data.worktrees;
        if (typeof args.projectId === "string") {
          items = items.filter((w) => w.project_id === args.projectId);
        }
        if (typeof args.deliveryId === "string") {
          const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
          items = delivery
            ? items.filter((w) => w.id === delivery.worktree_id)
            : [];
        }
        return ok(paginate(items, args));
      }
      case "worktrees.get": {
        const item = this.data.worktrees.find((w) => w.id === args.worktreeId);
        return item ? ok(item) : problem(404, "not_found", "Worktree not found.");
      }
      case "worktrees.lock_put": {
        if (!operator(this.role())) return forbiddenOperator();
        const item = this.data.worktrees.find((w) => w.id === args.worktreeId);
        if (!item) return problem(404, "not_found", "Worktree not found.");
        if (item.lock && item.lock.holder !== "human") {
          return problem(409, "worktree_locked", "Worktree is locked.");
        }
        item.lock = {
          holder: "human",
          delivery_id: item.lock?.delivery_id ?? "",
          acquired_at: SCENARIO_SERVER_TIME,
          lease_until: "2026-10-24T13:30:00.000Z",
        };
        this.emitEvent("worktree.locked", item.id, item.project_id, {
          worktree_id: item.id,
        });
        return ok(item);
      }
      case "worktrees.lock_delete": {
        if (!operator(this.role())) return forbiddenOperator();
        const item = this.data.worktrees.find((w) => w.id === args.worktreeId);
        if (!item) return problem(404, "not_found", "Worktree not found.");
        if (item.lock && item.lock.holder !== "human") {
          return problem(409, "worktree_locked", "Worktree is locked.");
        }
        item.lock = null;
        this.emitEvent("worktree.unlocked", item.id, item.project_id, {
          worktree_id: item.id,
        });
        return ok(item);
      }
      case "workflows.list": {
        let items = Object.values(this.data.workflows).filter(
          (w) => w.delivery_id === args.deliveryId,
        );
        if (typeof args.stage === "string") {
          items = items.filter((w) => w.stage === args.stage);
        }
        return ok(
          paginate(
            items.map(({ nodes: _nodes, ...rest }) => rest),
            args,
          ),
        );
      }
      case "workflows.get": {
        const item = this.data.workflows[String(args.workflowId)];
        if (!item) return problem(404, "not_found", "Workflow not found.");
        const includeSuperseded = args.includeSuperseded === true;
        return ok({
          ...clone(item),
          nodes: (item.nodes ?? []).filter(
            (entry) => includeSuperseded || entry.status !== "superseded",
          ),
        });
      }
      case "nodes.retry": {
        if (!operator(this.role())) return forbiddenOperator();
        const node = this.findNode(String(args.nodeId));
        if (!node) return problem(404, "not_found", "Node not found.");
        const workflow = this.data.workflows[node.node.workflow_id];
        const delivery = workflow?.delivery_id
          ? this.data.deliveries.find((entry) => entry.id === workflow.delivery_id)
          : null;
        if (!workflow || !delivery) return problem(404, "not_found", "Workflow not found.");
        if (delivery.stage_status === "paused") {
          return problem(409, "invalid_state", "Delivery is paused.");
        }
        if (delivery.stage === "closed" || delivery.stage === "abandoned") {
          return problem(409, "invalid_state", "Delivery is terminal.");
        }
        if (node.node.status !== "failed" || node.node.superseded_by) {
          return problem(409, "invalid_state", "Only failed nodes can be retried.");
        }
        const previous = this.data.attempts.filter(
          (entry) => entry.node_id === node.node.id,
        );
        const n = Math.max(0, ...previous.map((entry) => entry.n)) + 1;
        const attempt: Attempt = {
          id: `att_scenario_${++this.seq}`,
          node_id: node.node.id,
          n,
          sandbox_id: `sbx_retry_${this.seq}`,
          status: "running",
          started_at: new Date().toISOString(),
          ended_at: null,
          result_ref: "sha256:pending",
          failure_class: null,
          failure_detail: null,
          usage: usageValue(0, 0, 0, 0, 0),
        };
        this.data.attempts.push(attempt);
        this.simCreated.attempts.add(attempt.id);
        node.node.status = "running";
        workflow.status = "running";
        this.emitEvent("node.retry_requested", node.node.id, delivery.project_id, {
          node_id: node.node.id,
          attempt_id: attempt.id,
        }, delivery.id);
        this.emitEvent("attempt.started", attempt.id, delivery.project_id, {
          node_id: node.node.id,
          attempt_id: attempt.id,
        }, delivery.id);
        this.emitEvent("node.started", node.node.id, delivery.project_id, {
          node_id: node.node.id,
          attempt_id: attempt.id,
        }, delivery.id);
        this.simNodeRetried(node.node.id, attempt, delivery, workflow);
        return ok({ ...clone(node.node), status: node.node.status }, 202);
      }
      case "attempts.list": {
        const nodeId = String(args.nodeId ?? "");
        const items = this.data.attempts
          .filter((entry) => entry.node_id === nodeId)
          .sort((a, b) => a.n - b.n);
        const page = pageWindow(items, args, "asc");
        return ok({ items: page.items, next_cursor: page.next_cursor });
      }
      case "attempts.get": {
        const item = this.data.attempts.find(
          (entry) => entry.id === String(args.attemptId),
        );
        return item ? ok(clone(item)) : problem(404, "not_found", "Attempt not found.");
      }
      case "inbox.list": {
        let items = this.data.inbox;
        const status = typeof args.status === "string" ? args.status : "open";
        items = items.filter((i) => i.status === status);
        if (typeof args.projectId === "string") {
          items = items.filter((i) => i.project_id === args.projectId);
        }
        if (typeof args.kind === "string") {
          items = items.filter((i) => i.kind === args.kind);
        }
        return ok(paginate(items, args));
      }
      case "inbox.get": {
        const item = this.findInbox(String(args.inboxId));
        return item ? ok(item) : problem(404, "not_found", "Inbox item not found.");
      }
      case "inbox.dismiss": {
        if (!operator(this.role())) return forbiddenOperator();
        const item = this.findInbox(String(args.inboxId));
        if (!item) return problem(404, "not_found", "Inbox item not found.");
        if (item.status === "dismissed") return ok(item);
        if (item.status === "resolved") {
          return problem(409, "conflict", "Inbox item is already resolved.");
        }
        item.status = "dismissed";
        this.emitEvent("inbox.dismissed", item.id, item.project_id, {
          inbox_id: item.id,
        });
        return ok(item);
      }
      case "inbox.choose": {
        if (!operator(this.role())) return forbiddenOperator();
        const item = this.findInbox(String(args.inboxId));
        if (!item) return problem(404, "not_found", "Inbox item not found.");
        if (item.kind === "approval_needed") {
          return problem(409, "conflict", "Approval items must use approval routes.");
        }
        if (item.status !== "open") {
          return problem(409, "conflict", "Inbox item is not open.");
        }
        const options = optionsOf(item);
        const optionId = String(args.optionId ?? "");
        if (!options.some((option) => option.id === optionId)) {
          return invalid("选项不在 payload.options 中");
        }
        item.status = "resolved";
        if (item.kind === "escalation" && optionId === "upgrade") {
          const delivery = this.data.deliveries.find((d) => d.id === item.delivery_id);
          const target = (item.payload as { target_level?: Level }).target_level;
          if (delivery && target) delivery.level_cap = target;
        }
        if (item.kind === "escalation" && optionId === "abandon") {
          const delivery = this.data.deliveries.find((d) => d.id === item.delivery_id);
          if (delivery) {
            delivery.stage = "abandoned";
            delivery.stage_status = "done";
          }
        }
        this.emitEvent("inbox.resolved", item.id, item.project_id, {
          inbox_id: item.id,
          option_id: optionId,
        });
        this.simChoiceResolved(item, optionId);
        return ok(item);
      }
      case "approvals.list": {
        let items = this.data.approvals;
        if (typeof args.status === "string") {
          items = items.filter((a) => a.status === args.status);
        }
        if (typeof args.deliveryId === "string") {
          items = items.filter((a) => a.delivery_id === args.deliveryId);
        }
        return ok(paginate(items, args));
      }
      case "approvals.get": {
        const item = this.findApproval(String(args.approvalId));
        return item ? ok(item) : problem(404, "not_found", "Approval not found.");
      }
      case "approvals.deny": {
        if (this.role() !== "approver") {
          return problem(403, "forbidden_role", "Approval requires approver role.");
        }
        const item = this.findApproval(String(args.approvalId));
        if (!item) return problem(404, "not_found", "Approval not found.");
        if (item.status !== "pending") {
          return problem(409, "approval_not_pending", "Approval is not pending.");
        }
        item.status = "denied";
        if (item.delivery_id) {
          this.resolveApprovalInbox(item.id, item.delivery_id);
        }
        this.emitEvent("approval.denied", item.id, null, {
          approval_id: item.id,
          reason: args.reason,
        });
        this.simApprovalResolved(item.id, "denied");
        return ok(item);
      }
      case "settings.get": {
        if (!operator(this.role())) return forbiddenOperator();
        const resource = String(args.resource ?? "");
        const doc = this.data.settings[resource];
        if (!doc) return problem(404, "not_found", "Settings resource not found.");
        const rev = typeof args.rev === "number" ? args.rev : doc.rev;
        if (rev !== doc.rev) {
          return problem(404, "not_found", "该历史版本未在示例模式保留。");
        }
        return withHeaders(clone(doc), 200, {
          etag: `"${doc.rev}"`,
          x_event_seq: doc.rev,
        });
      }
      case "project_settings.get": {
        if (!operator(this.role())) return forbiddenOperator();
        const projectId = String(args.projectId ?? "");
        const resource = String(args.resource ?? "");
        if (!this.data.projects.some((project) => project.id === projectId)) {
          return problem(404, "not_found", "Project not found.");
        }
        const doc = this.data.projectSettings[projectId]?.[resource];
        if (!doc) return problem(404, "not_found", "Settings resource not found.");
        const value =
          args.view === "effective"
            ? effectiveDocument(this.data, projectId, resource)
            : doc;
        if (args.view === "effective") {
          return withHeaders(clone(value), 200, {
            etag: `"effective-${value.rev}"`,
            x_event_seq: value.rev,
          });
        }
        return withHeaders(clone(value), 200, {
          etag: `"${doc.rev}"`,
          x_event_seq: doc.rev,
        });
      }
      case "settings.catalog": {
        if (!operator(this.role())) return forbiddenOperator();
        return ok(clone(this.data.catalog));
      }
      case "settings.status": {
        if (!operator(this.role())) return forbiddenOperator();
        return ok(clone(this.data.status));
      }
      case "devices.list":
        return ok(paginate(this.data.devices, args));
      case "devices.rename": {
        if (!operator(this.role())) return forbiddenOperator();
        const device = this.data.devices.find((d) => d.id === args.deviceId);
        if (!device) return problem(404, "not_found", "Device not found.");
        const name = String(args.name ?? "").trim();
        if (!name || name.length > 64) return invalid("设备名必须为 1–64 字符");
        device.name = name;
        if (device.id === this.data.me.device.id) {
          this.data.me = { ...this.data.me, device: { ...device } };
          if (this.data.identity.device) {
            this.data.identity = {
              ...this.data.identity,
              device: { ...this.data.identity.device, name },
            };
          }
        }
        this.emitEvent("device.updated", device.id, null, { device_id: device.id });
        return ok(device);
      }
      case "inbox.summary": {
        const summary = summarizeInbox(this.data.inbox);
        return ok(summary);
      }
      default:
        return problem(404, "not_found", "Unknown scenario route.");
    }
  }

  private role(): "viewer" | "operator" | "approver" {
    return this.data.me.device.role;
  }

  private findInbox(id: string): InboxItem | undefined {
    return this.data.inbox.find((i) => i.id === id);
  }

  private findApproval(id: string): ApprovalEntity | undefined {
    return this.data.approvals.find((a) => a.id === id);
  }

  private findNode(
    id: string,
  ): { node: WorkflowNode; workflow: Workflow } | undefined {
    for (const workflow of Object.values(this.data.workflows)) {
      const node = workflow.nodes?.find((entry) => entry.id === id);
      if (node) return { node, workflow };
    }
    return undefined;
  }

  private workflowsFor(deliveryId: string): Workflow[] {
    return Object.values(this.data.workflows).filter(
      (workflow) => workflow.delivery_id === deliveryId,
    );
  }

  private approvalHeadCurrent(approval: ApprovalEntity): boolean {
    if (approval.target.kind !== "pr") return true;
    const delivery = approval.delivery_id
      ? this.data.deliveries.find((entry) => entry.id === approval.delivery_id)
      : null;
    return delivery?.pr?.head_sha === approval.subject_digest;
  }

  private resolveApprovalInbox(approvalId: string, deliveryId: string): void {
    for (const item of this.data.inbox) {
      if (
        item.delivery_id === deliveryId &&
        item.kind === "approval_needed" &&
        (item.payload as { approval_id?: string }).approval_id === approvalId &&
        item.status === "open"
      ) {
        item.status = "resolved";
        this.emitEvent("inbox.resolved", item.id, item.project_id, {
          inbox_id: item.id,
        });
      }
    }
  }

  private syncDeliveryInbox(delivery: Delivery): void {
    for (const item of this.data.inbox) {
      if (item.delivery_id === delivery.id && item.status === "open") {
        item.status = "resolved";
      }
    }
  }

  private emit(frame: StreamFrame): void {
    this.listener?.(frame);
  }

  private emitEvent(
    type: string,
    subject: string,
    projectId: string | null,
    data: unknown,
    correlationId: string | null = projectId,
  ): void {
    this.seq += 1;
    const envelope: EventEnvelope = {
      id: `evt_scenario_${this.seq}`,
      seq: this.seq,
      ts: new Date().toISOString(),
      type,
      subject,
      project_id: projectId,
      actor: { kind: "device", id: this.data.me.device.id },
      causation_id: null,
      correlation_id: correlationId,
      data,
    };
    this.emit({ t: "event", envelope });
  }

  // ---------------------------------------------------------------- 演示驱动

  private simSelect(
    id: SimulationScriptId,
    opts?: { projectId?: string },
  ): SimulationAction {
    const projectId =
      opts?.projectId ??
      this.data.projects.find((project) => project.id === "prj_gantry")?.id ??
      this.data.projects[0]?.id ??
      null;
    if (!projectId) return { ok: false, message: "场景中没有项目。" };
    this.simStopTimer();
    this.simScript = id;
    this.simSteps = [];
    this.simIndex = 0;
    this.simTask = null;
    this.simRetryTask = null;
    this.simAwaiting = null;
    this.simPendingChoice = null;
    this.simRemoveCreated();
    this.simResetCreated();
    this.simReplace({
      ...EMPTY_SIMULATION,
      script_id: id,
      project_id: projectId,
      run_id: this.simRun + 1,
      status: "idle",
      phase: "selected",
    });
    return { ok: true };
  }

  private simBeginFromUserMessage(projectId: string, message: Message): void {
    if (this.simRunActive()) {
      const current = this.simulationState.project_id;
      if (this.simAwaiting || current !== projectId) {
        // 等待用户操作或跨项目时拒绝替换，只记录消息；旧运行继续。
        this.simPostAgentMessage(
          projectId,
          current && current !== projectId
            ? "另一个项目仍有模拟任务在运行或等待操作；为避免替换并遗留旧交付，本次只记录消息，未启动新的模拟运行。请先完成、取消或重置原演示任务。"
            : "已有模拟任务正在运行或等待操作；本次只记录消息，未启动新的模拟运行。请先继续、取消或重置当前演示。",
          { kind: "utility", deliveryId: null, tone: "warn" },
        );
        return;
      }
      // 同一项目内重新发送会结束旧运行；先结算旧交付、图、尝试和工具。
      this.simCancel();
    }
    this.simLastMessage = message;
    if (!this.simScript) this.simScript = "qa";
    this.simStopTimer();
    const script = this.simScript;
    const run = ++this.simRun;
    this.simTask = null;
    this.simRetryTask = null;
    this.simAwaiting = null;
    this.simPendingChoice = null;
    this.simSteps = [];
    this.simIndex = 0;
    this.simReplace({
      ...EMPTY_SIMULATION,
      script_id: script,
      project_id: projectId,
      run_id: run,
      status: "running",
      phase: "received",
    });
    this.simSteps = this.simBuildSteps(script, projectId, message);
    this.simIndex = 0;
    this.simSchedule();
  }

  private simPlay(): SimulationAction {
    if (!this.simScript) return { ok: false, message: "请先选择一个演示脚本。" };
    if (this.simulationState.status === "running") return { ok: true };
    if (this.simulationState.awaiting) {
      return { ok: false, message: "脚本正在等待用户操作，请先完成当前步骤。" };
    }
    if (this.simIndex >= this.simSteps.length) {
      const projectId =
        this.simulationState.project_id ??
        this.data.projects[0]?.id ??
        null;
      if (!projectId) return { ok: false, message: "场景中没有项目。" };
      const message = this.simLastMessage;
      this.simRun += 1;
      this.simSteps = this.simBuildSteps(this.simScript, projectId, message);
      this.simIndex = 0;
      this.simReplace({
        ...EMPTY_SIMULATION,
        script_id: this.simScript,
        project_id: projectId,
        status: "running",
        phase: "replay",
        run_id: this.simRun,
        total_turns: this.simSteps.length,
      });
      this.simSchedule();
      return { ok: true };
    }
    this.simPatch({ status: "running" });
    this.simSchedule();
    return { ok: true };
  }

  private simStep(): SimulationAction {
    if (this.simulationState.awaiting) {
      return { ok: false, message: "脚本正在等待用户操作。" };
    }
    if (this.simSteps.length === 0 || this.simIndex >= this.simSteps.length) {
      if (!this.simScript) return { ok: false, message: "请先选择一个演示脚本。" };
      const projectId = this.simulationState.project_id;
      if (!projectId) return { ok: false, message: "场景中没有项目。" };
      this.simRun += 1;
      this.simSteps = this.simBuildSteps(this.simScript, projectId, this.simLastMessage);
      this.simIndex = 0;
      this.simTask = null;
      this.simRetryTask = null;
      this.simPatch({
        run_id: this.simRun,
        status: "running",
        phase: "stepping",
        drafts: [],
        activities: [],
        summaries: [],
        pending_approval_id: null,
        awaiting: null,
        last_error: null,
        turn: 0,
        total_turns: this.simSteps.length,
      });
    }
    this.simStopTimer();
    this.simPatch({ status: "running" });
    this.simRunStep();
    return { ok: true };
  }

  private simPause(): SimulationAction {
    if (this.simulationState.status !== "running") return { ok: true };
    this.simStopTimer();
    this.simPatch({ status: "paused", phase: "paused" });
    return { ok: true };
  }

  private simResume(): SimulationAction {
    return this.simPlay();
  }

  private simRunActive(): boolean {
    if (
      this.simulationState.status === "running" ||
      this.simulationState.status === "paused"
    ) {
      return true;
    }
    if (this.simAwaiting) return true;
    for (const id of this.simCreated.deliveries) {
      const delivery = this.data.deliveries.find((entry) => entry.id === id);
      if (delivery && delivery.stage !== "closed" && delivery.stage !== "abandoned") {
        return true;
      }
    }
    return false;
  }

  private simCancel(): SimulationAction {
    this.simStopTimer();
    this.simSettleCancelledTasks();
    this.simIndex = this.simSteps.length;
    this.simPatch({
      status: "cancelled",
      phase: "cancelled",
      awaiting: null,
      pending_approval_id: null,
      drafts: this.simulationState.drafts.map((draft) =>
        draft.status === "streaming" ? { ...draft, status: "cancelled" } : draft,
      ),
      summaries: this.simulationState.summaries.map((summary) =>
        summary.status === "streaming" ? { ...summary, status: "failed" } : summary,
      ),
    });
    this.simAwaiting = null;
    return { ok: true };
  }

  /** 取消时结算模拟创建的交付，避免交付、图、尝试和工具停在运行中。 */
  private simSettleCancelledTasks(): void {
    for (const id of this.simCreated.deliveries) {
      const delivery = this.data.deliveries.find((entry) => entry.id === id);
      if (!delivery || delivery.stage === "closed") continue;
      const workflows = this.workflowsFor(delivery.id);
      const nodeIds = new Set(
        workflows.flatMap((workflow) => (workflow.nodes ?? []).map((node) => node.id)),
      );
      const alreadyAbandoned = delivery.stage === "abandoned";
      if (!alreadyAbandoned) {
        delivery.stage = "abandoned";
        delivery.stage_status = "done";
        delivery.updated_at = new Date().toISOString();
      }
      for (const workflow of workflows) {
        workflow.status = "cancelled";
        for (const node of workflow.nodes ?? []) {
          if (
            node.status === "running" ||
            node.status === "pending" ||
            node.status === "queued"
          ) {
            node.status = "cancelled";
          }
        }
      }
      for (const attempt of this.data.attempts) {
        if (!nodeIds.has(attempt.node_id)) continue;
        if (attempt.status !== "running" && attempt.status !== "preparing") continue;
        attempt.status = "cancelled";
        attempt.ended_at = new Date().toISOString();
        attempt.failure_class = "cancelled";
        attempt.failure_detail = "simulation cancelled";
        this.simEmitAttemptEvent(attempt, delivery, "attempt.finished");
      }
      this.simFinishTools("failed");
      if (!alreadyAbandoned) {
        this.emitEvent("delivery.abandoned", delivery.id, delivery.project_id, {
          delivery_id: delivery.id,
        });
      }
    }
  }

  private simReset(): SimulationAction {
    this.simStopTimer();
    this.simRemoveCreated();
    this.simScript = null;
    this.simSteps = [];
    this.simIndex = 0;
    this.simTask = null;
    this.simRetryTask = null;
    this.simAwaiting = null;
    this.simPendingChoice = null;
    this.simReplace({ ...EMPTY_SIMULATION });
    return { ok: true };
  }

  private simAdvance(ms: number): SimulationAction {
    if (this.simulationState.status !== "running") {
      return { ok: false, message: "脚本未在运行。" };
    }
    this.simStopTimer();
    this.simAdvancing = true;
    let remaining = Math.max(0, ms);
    try {
      while (
        remaining > 0 &&
        this.simIndex < this.simSteps.length &&
        !this.simAwaiting
      ) {
        const step = this.simSteps[this.simIndex];
        if (step.delayMs > remaining) {
          remaining -= step.delayMs;
          break;
        }
        remaining -= step.delayMs;
        this.simPatch({ status: "running" });
        this.simRunStep();
      }
    } finally {
      this.simAdvancing = false;
    }
    if (
      this.simIndex < this.simSteps.length &&
      !this.simAwaiting &&
      this.simulationState.status === "running"
    ) {
      this.simSchedule();
    }
    return { ok: true };
  }

  private simApproval(approvalId: string): SimulationAction {
    return this.simResolveApproval(approvalId, "granted");
  }

  private simDenial(approvalId: string): SimulationAction {
    return this.simResolveApproval(approvalId, "denied");
  }

  private simExpire(approvalId: string): SimulationAction {
    const approval = this.findApproval(approvalId);
    if (!approval) return { ok: false, message: "找不到审批。" };
    if (approval.status !== "pending") {
      return { ok: false, message: "只有待处理审批可以模拟过期。" };
    }
    approval.status = "expired";
    approval.expires_at = new Date(Date.now() - 1_000).toISOString();
    if (approval.delivery_id) this.resolveApprovalInbox(approval.id, approval.delivery_id);
    this.simResolveWaitingChoice();
    this.simAwaiting = null;
    this.emitEvent("approval.expired", approval.id, null, {
      approval_id: approval.id,
    });
    this.simPatch({
      pending_approval_id: null,
      awaiting: null,
      phase: "approval_expired",
      last_error: "审批已过期。",
    });
    this.simPostAgentMessage(
      this.simulationState.project_id ?? this.projectForDelivery(approval.delivery_id),
      "审批已过期；本次演示任务停止。",
      {
        kind: "utility",
        deliveryId: approval.delivery_id,
      },
    );
    this.simPatch({ status: "failed" });
    return { ok: true, message: "已模拟审批过期。" };
  }

  private simHeadChanged(approvalId: string, changed: boolean): SimulationAction {
    const approval = this.findApproval(approvalId);
    if (!approval) return { ok: false, message: "找不到审批。" };
    if (approval.status !== "pending") {
      return { ok: false, message: "只有待处理审批可以模拟 head 变化。" };
    }
    const delivery = approval.delivery_id
      ? this.data.deliveries.find((entry) => entry.id === approval.delivery_id)
      : null;
    if (delivery?.pr && changed) {
      delivery.pr.head_sha = SCENARIO_PR9_STALE_SHA;
      approval.status = "invalidated";
      this.resolveApprovalInbox(approval.id, delivery.id);
      this.simResolveWaitingChoice();
      this.simAwaiting = null;
      this.emitEvent("approval.invalidated", approval.id, null, {
        approval_id: approval.id,
      });
      this.simPatch({
        pending_approval_id: null,
        awaiting: null,
        phase: "head_changed",
        last_error: "PR head 已变化，审批失效。",
      });
      this.simPostAgentMessage(delivery.project_id, "PR head 已变化，审批已失效；本次演示任务停止。", {
        kind: "utility",
        deliveryId: delivery.id,
      });
      this.simPatch({ status: "failed" });
    }
    return { ok: true };
  }

  private simResolveApproval(
    approvalId: string,
    status: "granted" | "denied",
  ): SimulationAction {
    if (this.role() !== "approver") {
      return { ok: false, message: "当前角色不是审批设备，不能模拟批准。" };
    }
    const approval = this.findApproval(approvalId);
    if (!approval) return { ok: false, message: "找不到审批。" };
    if (approval.status !== "pending") {
      return { ok: false, message: "审批已不是待处理状态。" };
    }
    if (Date.parse(approval.expires_at) <= Date.now()) {
      approval.status = "expired";
      return { ok: false, message: "审批已过期。" };
    }
    if (this.data.connection.phase !== "live") {
      return { ok: false, message: "连接无效，不能模拟批准或拒绝。" };
    }
    if (!this.approvalHeadCurrent(approval)) {
      return { ok: false, message: "PR head 已变化，审批载荷不再对应当前代码。" };
    }
    const waitingChoice =
      this.simAwaiting?.kind === "choice" ? this.simAwaiting : null;
    this.simAwaiting = null;
    if (waitingChoice) {
      const item = this.findInbox(waitingChoice.inbox_id);
      if (item && item.status === "open") {
        item.status = "resolved";
        this.emitEvent("inbox.resolved", item.id, item.project_id, {
          inbox_id: item.id,
          option_id: "fix",
        });
      }
    }
    approval.status = status;
    approval.signature = null;
    const delivery = approval.delivery_id
      ? this.data.deliveries.find((entry) => entry.id === approval.delivery_id)
      : null;
    if (approval.delivery_id) this.resolveApprovalInbox(approval.id, approval.delivery_id);
    this.emitEvent(
      status === "granted" ? "approval.granted" : "approval.denied",
      approval.id,
      delivery?.project_id ?? null,
      { approval_id: approval.id, simulated: true, signature: null },
    );
    if (delivery) {
      const workflow = this.workflowsFor(delivery.id)[0];
      this.emitEvent(
        status === "granted" ? "delivery.stage_changed" : "delivery.abandoned",
        delivery.id,
        delivery.project_id,
        { delivery_id: delivery.id },
        delivery.id,
      );
      if (status === "granted") {
        delivery.stage = "closed";
        delivery.stage_status = "done";
        if (workflow) workflow.status = "succeeded";
        this.simPatch({
          pending_approval_id: null,
          awaiting: null,
          phase: "approval_granted",
          last_error: null,
        });
        this.simPostAgentMessage(
          delivery.project_id,
          "模拟批准：未生成 signature，未调用真实签名或 GitHub。演示任务已完成。",
          { kind: "planner", deliveryId: delivery.id, tone: null },
        );
        this.simSettleBudget(delivery.id);
        this.simPatch({ status: "completed" });
      } else {
        delivery.stage = "abandoned";
        delivery.stage_status = "done";
        if (workflow) workflow.status = "cancelled";
        this.simPatch({
          pending_approval_id: null,
          awaiting: null,
          phase: "approval_denied",
          last_error: "审批被拒绝。",
        });
        this.simPostAgentMessage(
          delivery.project_id,
          "审批被拒绝；本次演示任务已停止，没有生成签名。",
          { kind: "utility", deliveryId: delivery.id, tone: "warn" },
        );
        this.simPatch({ status: "completed" });
      }
    } else {
      this.simPatch({
        pending_approval_id: null,
        awaiting: null,
        status: status === "granted" ? "completed" : "failed",
      });
    }
    return {
      ok: true,
      message:
        status === "granted"
          ? "已模拟批准，未生成签名。"
          : "已模拟拒绝，未生成签名。",
    };
  }

  private simChoiceResolved(item: InboxItem, optionId: string): void {
    if (this.simAwaiting?.kind !== "choice" || this.simAwaiting.inbox_id !== item.id) {
      return;
    }
    if (optionId === "keep") {
      // keep 等同停止演示：取消并结算本次任务，移除预创建但从未暴露的待审批入口。
      const deliveryId = this.simPendingChoice?.delivery_id ?? item.delivery_id;
      this.simPendingChoice = null;
      for (const id of [...this.simCreated.approvals]) {
        const approval = this.data.approvals.find((entry) => entry.id === id);
        if (approval?.status !== "pending") continue;
        this.data.approvals = this.data.approvals.filter((entry) => entry.id !== id);
        this.simCreated.approvals.delete(id);
      }
      this.simCancel();
      this.simPostAgentMessage(
        item.project_id,
        "已选择停止演示：本次模拟任务已取消，不会进入审批；未生成签名，也未调用真实 GitHub。",
        { kind: "utility", deliveryId, tone: "warn" },
      );
      return;
    }
    this.simAwaiting = null;
    const approval = this.simPendingChoice
      ? this.data.approvals.find(
          (entry) => entry.delivery_id === this.simPendingChoice?.delivery_id && entry.status === "pending",
        )
      : undefined;
    if (approval) {
      this.emitEvent(
        "approval.requested",
        approval.id,
        item.project_id,
        { approval_id: approval.id },
        approval.delivery_id ?? undefined,
      );
      const approvalInbox: InboxItem = {
        id: `inb_sim_approval_${this.simulationState.run_id}`,
        project_id: item.project_id,
        delivery_id: approval.delivery_id,
        kind: "approval_needed",
        severity: "blocker",
        summary: "模拟审批：确认已选择的演示任务可以进入完成阶段。",
        payload: { approval_id: approval.id },
        status: "open",
        dedup_key: `approval:${approval.id}`,
      };
      this.data.inbox.push(approvalInbox);
      this.simCreated.inbox.add(approvalInbox.id);
      this.emitEvent("inbox.raised", approvalInbox.id, item.project_id, {
        inbox_id: approvalInbox.id,
      });
      this.simAwaiting = { kind: "approval", approval_id: approval.id };
      this.simPatch({
        pending_approval_id: approval.id,
        awaiting: this.simAwaiting,
        phase: "await_approval",
        status: "paused",
      });
      this.simPostAgentMessage(
        item.project_id,
        "选择已记录。现在可以模拟批准或拒绝；模拟批准不会生成 signature。",
        { kind: "utility", deliveryId: approval.delivery_id, tone: null },
      );
    } else {
      this.simPendingChoice = null;
      this.simPatch({
        awaiting: null,
        phase: "choice_resolved",
        status: "completed",
        last_error: null,
      });
    }
  }

  private simNodeRetried(
    nodeId: string,
    attempt: Attempt,
    delivery: Delivery,
    workflow: Workflow,
  ): void {
    const task = this.simTask ?? this.simRetryTask;
    if (!task || task.build.id !== nodeId) return;
    task.buildAttempt = attempt;
    task.workflow = workflow;
    this.simAwaiting = null;
    this.simPatch({
      awaiting: null,
      status: "running",
      phase: "retry_running",
    });
    const steps: SimStep[] = [
      {
        delayMs: 600,
        label: "retry-tools",
        run: () => {
          // 新尝试使用新的调用标识；旧失败调用保持原 attempt 与失败事实。
          this.simStartTool(task, "运行仓库测试", "npm test -- --runInBand");
          this.simStartTool(task, "检查变更范围", "git diff --stat origin/main");
          this.simSetActivities("succeeded");
          task.build.status = "succeeded";
          task.buildAttempt.status = "succeeded";
          task.buildAttempt.ended_at = new Date().toISOString();
          this.emitEvent(
            "attempt.finished",
            task.buildAttempt.id,
            delivery.project_id,
            {
              node_id: task.build.id,
              attempt_id: task.buildAttempt.id,
              status: "succeeded",
            },
            delivery.id,
          );
          this.simPatch({ phase: "retry_verify" });
        },
      },
      {
        delayMs: 500,
        label: "retry-verify",
        run: () => {
          const verifyAttempt = this.simStartAttempt(task.verify, workflow, delivery, "running");
          task.verifyAttempt = verifyAttempt;
          task.verify.status = "running";
          this.emitEvent(
            "node.started",
            task.verify.id,
            delivery.project_id,
            { node_id: task.verify.id, attempt_id: verifyAttempt.id },
            delivery.id,
          );
        },
      },
      {
        delayMs: 500,
        label: "retry-complete",
        run: () => {
          if (task.verifyAttempt) {
            task.verifyAttempt.status = "succeeded";
            task.verifyAttempt.ended_at = new Date().toISOString();
          }
          task.verify.status = "succeeded";
          task.verify.verdict = "ACCEPT";
          workflow.status = "succeeded";
          delivery.stage = "closed";
          delivery.stage_status = "done";
          this.simSettleBudget(delivery.id);
          this.simPostAgentMessage(
            delivery.project_id,
            "重试成功：新尝试已完成，旧失败尝试保持不变。模拟执行摘要已结算。",
            { kind: "worker", deliveryId: delivery.id, tone: null },
          );
          this.simPatch({
            status: "completed",
            phase: "retry_completed",
            awaiting: null,
          });
        },
      },
    ];
    this.simSteps = steps;
    this.simIndex = 0;
    this.simSchedule();
    this.simEmitAttemptEvent(attempt, delivery, "attempt.started");
  }

  private simResumeConnection(): void {
    this.simAwaiting = null;
    this.simPatch({ awaiting: null, status: "running", phase: "reconnected" });
    const projectId = this.simulationState.project_id;
    if (projectId) {
      this.simPostAgentMessage(
        projectId,
        "连接已恢复。演示任务继续执行。",
        { kind: "utility", deliveryId: this.simTask?.delivery.id ?? null },
      );
    }
    const steps: SimStep[] = [
      {
        delayMs: 400,
        label: "offline-complete",
        run: () => {
          this.simPatch({
            status: "completed",
            phase: "completed",
            awaiting: null,
          });
          if (projectId) {
            this.simPostAgentMessage(
              projectId,
              "断线恢复演示完成：未产生重复消息，项目事实仍归原项目。",
              { kind: "utility", deliveryId: this.simTask?.delivery.id ?? null },
            );
          }
        },
      },
    ];
    this.simSteps = steps;
    this.simIndex = 0;
    this.simSchedule();
  }

  private simInjectError(kind: SimulationErrorKind): SimulationAction {
    if (kind === "offline") {
      this.setConnection("reconnecting");
      return { ok: true, message: "已模拟断线。" };
    }
    if (kind === "http_500") {
      this.injectedServerError = true;
      return { ok: true, message: "下一次 me.get 将返回一次 500。" };
    }
    if (kind === "settings_conflict") {
      this.conflictOnNextSubmit = true;
      return { ok: true, message: "下一次设置提交将模拟 412 冲突。" };
    }
    const projectId =
      this.simulationState.project_id ?? this.data.projects[0]?.id ?? null;
    const envelope: EventEnvelope = {
      id: "evt_stale_scenario",
      seq: 0,
      ts: new Date().toISOString(),
      type: "message.posted",
      subject: "msg_stale_scenario",
      project_id: projectId,
      actor: { kind: "system", id: "scenario" },
      causation_id: null,
      correlation_id: null,
      data: { stale: true },
    };
    this.emit({ t: "event", envelope });
    return { ok: true, message: "已投递 seq=1 的旧事件；控制器不得回退。" };
  }

  private simPatch(patch: Partial<SimulationSnapshot>): void {
    this.simReplace({ ...this.simulationState, ...patch });
  }

  private simReplace(next: SimulationSnapshot): void {
    this.simulationState = next;
    for (const listener of [...this.simulationListeners]) listener();
  }

  private simStopTimer(): void {
    if (this.simTimer !== null) {
      clearTimeout(this.simTimer);
      this.simTimer = null;
    }
  }

  private simSchedule(): void {
    if (this.simAdvancing) return;
    if (this.simulationState.status !== "running" || this.simAwaiting) return;
    if (this.simIndex >= this.simSteps.length) return;
    const step = this.simSteps[this.simIndex];
    this.simStopTimer();
    const run = this.simRun;
    this.simTimer = setTimeout(() => {
      this.simTimer = null;
      if (run !== this.simRun || this.simulationState.status !== "running") return;
      this.simRunStep();
    }, Math.max(0, step.delayMs));
  }

  private simRunStep(): void {
    const step = this.simSteps[this.simIndex];
    if (!step) {
      if (this.simulationState.status === "running") {
        this.simPatch({ status: "completed", phase: "completed", awaiting: null });
      }
      return;
    }
    this.simIndex += 1;
    this.simPatch({
      turn: this.simIndex,
      total_turns: this.simSteps.length,
      phase: step.label,
    });
    try {
      step.run();
    } catch (error) {
      this.simPatch({
        status: "failed",
        phase: "error",
        last_error: error instanceof Error ? error.message : "演示脚本失败。",
      });
      this.simStopTimer();
      return;
    }
    if (step.after) step.after();
    if (this.simAwaiting) return;
    if (this.simIndex >= this.simSteps.length) {
      if (this.simulationState.status === "running") {
        this.simPatch({ status: "completed", phase: "completed", awaiting: null });
      }
      return;
    }
    this.simSchedule();
  }

  private simBuildSteps(
    script: SimulationScriptId,
    projectId: string,
    userMessage: Message | null,
  ): SimStep[] {
    switch (script) {
      case "qa":
        return this.simQaSteps(projectId, userMessage);
      case "task":
        return this.simTaskSteps(projectId, userMessage, false);
      case "failure_retry":
        return this.simFailureSteps(projectId, userMessage);
      case "choice_approval":
        return this.simApprovalSteps(projectId, userMessage);
      case "offline_switch":
        return this.simOfflineSteps(projectId, userMessage);
      case "settings_apply":
        return this.simSettingsSteps(projectId);
    }
  }

  private simQaSteps(projectId: string, userMessage: Message | null): SimStep[] {
    const run = this.simRun;
    const prompt = userMessage?.text ?? "请说明当前演示状态。";
    const messageId = `msg_sim_${run}_qa`;
    const draftId = `draft_${run}_qa`;
    const summaryId = `sum_${run}_qa`;
    const pieces = [
      `这是对「${prompt.slice(0, 40)}」的模拟回答。`,
      " 场景只使用内存数据，未调用真实模型或网络。",
    ];
    return [
      {
        delayMs: 250,
        label: "generate",
        run: () => {
          this.simPatch({
            drafts: [
              {
                draft_id: draftId,
                message_id: messageId,
                delivery_id: null,
                node_id: null,
                attempt_id: null,
                status: "streaming",
                segments: [pieces[0]],
                text: pieces[0],
                simulated: true,
              },
            ],
            summaries: [
              {
                summary_id: summaryId,
                status: "streaming",
                text: "正在组织公开回答。",
                simulated: true,
                delivery_id: null,
                node_id: null,
                attempt_id: null,
                message_id: messageId,
              },
            ],
          });
        },
      },
      {
        delayMs: 300,
        label: "segments",
        run: () => {
          this.simPatch({
            drafts: this.simulationState.drafts.map((draft) => ({
              ...draft,
              segments: pieces,
              text: pieces.join(""),
            })),
            summaries: this.simulationState.summaries.map((summary) => ({
              ...summary,
              status: "done",
              text: "公开摘要：读取场景上下文，组织一条模拟回答。",
            })),
          });
        },
      },
      {
        delayMs: 250,
        label: "settle",
        run: () => {
          this.simPostAgentMessage(projectId, pieces.join(""), {
            kind: "planner",
            deliveryId: null,
            messageId,
            tone: null,
          });
          this.simPatch({
            drafts: this.simulationState.drafts.map((draft) => ({
              ...draft,
              status: "done",
            })),
            status: "completed",
            phase: "settled",
          });
        },
      },
    ];
  }

  private simTaskSteps(
    projectId: string,
    userMessage: Message | null,
    withFailure: boolean,
  ): SimStep[] {
    const run = this.simRun;
    let task: SimTask | null = null;
    const receiptId = `msg_sim_${run}_receipt`;
    const planId = `msg_sim_${run}_plan`;
    const finalId = `msg_sim_${run}_final`;
    const draftId = `draft_${run}_task`;
    const summaryId = `sum_${run}_task`;
    const prompt = userMessage?.text ?? "请执行当前演示任务。";
    return [
      {
        delayMs: 200,
        label: "receipt",
        run: () => {
          task = this.simCreateTask(projectId, prompt, withFailure);
          this.simTask = task;
          this.simPatch({
            total_turns: 5,
            drafts: [
              {
                draft_id: draftId,
                message_id: finalId,
                delivery_id: task.delivery.id,
                node_id: task.build.id,
                attempt_id: task.buildAttempt.id,
                status: "streaming",
                segments: ["已接收任务，正在读取场景事实。"],
                text: "已接收任务，正在读取场景事实。",
                simulated: true,
              },
            ],
          });
          this.simPostAgentMessage(projectId, "任务回执：已建立一个模拟交付与计划。", {
            kind: "utility",
            deliveryId: task.delivery.id,
            messageId: receiptId,
            refs: [],
            tone: null,
            cards: [],
          });
        },
      },
      {
        delayMs: 300,
        label: "plan",
        run: () => {
          if (!task) return;
          this.simPatch({
            summaries: [
              {
                summary_id: summaryId,
                status: "streaming",
                text: "公开摘要：拆分计划节点、实现节点和验证节点。",
                simulated: true,
                delivery_id: task.delivery.id,
                node_id: task.plan.id,
                attempt_id: null,
                message_id: planId,
              },
            ],
          });
          task.plan.status = "succeeded";
          task.plan.verdict = null;
          this.simPostAgentMessage(projectId, "计划已就绪：计划 → 实现 → 验证。", {
            kind: "planner",
            deliveryId: task.delivery.id,
            messageId: planId,
            tone: null,
            refs: [{ kind: "delivery", id: task.delivery.id }],
            cards: [
              {
                kind: "plan",
                delivery_id: task.delivery.id,
                workflow_id: task.workflow.id,
                summary: untrustedText(
                  "模拟计划：并行准备工具、实现变更并验证公开执行摘要。",
                  "scenario:plan",
                ),
              },
            ],
          });
          this.emitEvent("node.finished", task.plan.id, projectId, {
            node_id: task.plan.id,
            status: "succeeded",
          }, task.delivery.id);
        },
      },
      {
        delayMs: 350,
        label: "tools",
        run: () => {
          if (!task) return;
          this.simStartTool(task, "运行仓库测试", "npm test -- --runInBand");
          this.simStartTool(task, "检查变更范围", "git diff --stat origin/main");
          this.simPatch({
            drafts: this.simulationState.drafts.map((draft) => ({
              ...draft,
              segments: [...draft.segments, "并行工具正在运行。"],
              text: [...draft.segments, "并行工具正在运行。"].join(""),
            })),
            summaries: this.simulationState.summaries.map((summary) => ({
              ...summary,
              text: "公开摘要：两个工具并行执行，等待结果。",
            })),
          });
        },
      },
      {
        delayMs: 450,
        label: "verify",
        run: () => {
          if (!task) return;
          this.simFinishTools("succeeded");
          task.build.status = "succeeded";
          task.build.verdict = null;
          task.buildAttempt.status = "succeeded";
          task.buildAttempt.ended_at = new Date().toISOString();
          this.emitEvent("node.finished", task.build.id, projectId, {
            node_id: task.build.id,
            status: "succeeded",
          }, task.delivery.id);
          this.emitEvent("attempt.finished", task.buildAttempt.id, projectId, {
            node_id: task.build.id,
            attempt_id: task.buildAttempt.id,
            status: "succeeded",
          }, task.delivery.id);
          task.verify.status = "running";
          task.verifyAttempt = this.simStartAttempt(task.verify, task.workflow, task.delivery, "running");
          this.simPatch({
            summaries: this.simulationState.summaries.map((summary) => ({
              ...summary,
              status: "done",
              text: "公开摘要：工具通过，进入验证。",
            })),
            drafts: this.simulationState.drafts.map((draft) => ({
              ...draft,
              segments: [...draft.segments, "验证节点运行中。"],
              text: [...draft.segments, "验证节点运行中。"].join(""),
            })),
          });
        },
      },
      {
        delayMs: 400,
        label: "complete",
        run: () => {
          if (!task) return;
          if (task.verifyAttempt) {
            task.verifyAttempt.status = "succeeded";
            task.verifyAttempt.ended_at = new Date().toISOString();
            this.emitEvent("attempt.finished", task.verifyAttempt.id, projectId, {
              node_id: task.verify.id,
              attempt_id: task.verifyAttempt.id,
              status: "succeeded",
            }, task.delivery.id);
          }
          task.verify.status = "succeeded";
          task.verify.verdict = "ACCEPT";
          task.workflow.status = "succeeded";
          task.delivery.stage = "closed";
          task.delivery.stage_status = "done";
          this.simSettleBudget(task.delivery.id);
          this.simPatch({
            drafts: this.simulationState.drafts.map((draft) => ({
              ...draft,
              status: "done",
              segments: [...draft.segments, "完成。"],
              text: [...draft.segments, "完成。"].join(""),
            })),
            summaries: this.simulationState.summaries.map((summary) => ({
              ...summary,
              status: "done",
              text: "模拟执行摘要：计划通过，两个工具成功，验证通过。",
            })),
            status: "completed",
            phase: "completed",
          });
          this.simPostAgentMessage(projectId, "模拟任务完成：计划、工具和验证均已成功。", {
            kind: "worker",
            deliveryId: task.delivery.id,
            messageId: finalId,
            tone: null,
            refs: [{ kind: "delivery", id: task.delivery.id }],
            cards: [
              {
                kind: "summary",
                delivery_id: task.delivery.id,
                items: [
                  {
                    text: untrustedText(
                      "模拟执行摘要：计划通过、两个并行工具成功、验证通过。",
                      "scenario:summary",
                    ),
                    ref: { kind: "delivery", id: task.delivery.id },
                  },
                ],
              },
            ],
          });
        },
      },
    ];
  }

  private simFailureSteps(projectId: string, userMessage: Message | null): SimStep[] {
    const steps = this.simTaskSteps(projectId, userMessage, true);
    // 把前三个步骤保留为回执/计划/工具；在验证阶段改为失败并等待重试。
    return steps.slice(0, 4).map((step, index) =>
      index === 3
        ? {
            delayMs: 500,
            label: "failed",
            run: () => {
              const task = this.simTask;
              if (!task) return;
              this.simFinishTools("failed");
              task.build.status = "failed";
              task.buildAttempt.status = "failed";
              task.buildAttempt.ended_at = new Date().toISOString();
              task.buildAttempt.failure_class = "exec_failed";
              task.buildAttempt.failure_detail = "repo.test: exit 1 (simulated)";
              task.workflow.status = "failed";
              this.simAwaiting = {
                kind: "retry",
                node_id: task.build.id,
                delivery_id: task.delivery.id,
              };
              this.simPatch({
                status: "paused",
                phase: "await_retry",
                awaiting: this.simAwaiting,
                last_error: "模拟工具失败；请重试节点。",
                drafts: this.simulationState.drafts.map((draft) => ({
                  ...draft,
                  text: [...draft.segments, "构建失败，等待重试。"].join(""),
                })),
              });
              this.emitEvent("node.finished", task.build.id, projectId, {
                node_id: task.build.id,
                status: "failed",
              }, task.delivery.id);
              this.simPostAgentMessage(projectId, "模拟构建失败：工具返回非零退出码。可以重试该节点。", {
                kind: "worker",
                deliveryId: task.delivery.id,
                tone: "warn",
                refs: [{ kind: "delivery", id: task.delivery.id }],
              });
            },
          }
        : step,
    );
  }

  private simApprovalSteps(projectId: string, _userMessage: Message | null): SimStep[] {
    const run = this.simRun;
    const deliveryId = `dlv_sim_approval_${run}`;
    const workflowId = `wfl_sim_approval_${run}`;
    const nodeId = `nod_sim_approval_${run}`;
    const approvalId = `apr_sim_approval_${run}`;
    const choiceId = `inb_sim_choice_${run}`;
    const messageId = `msg_sim_approval_${run}`;
    return [
      {
        delayMs: 200,
        label: "raise_choice",
        run: () => {
          const delivery = this.simCreateDelivery(projectId, {
            id: deliveryId,
            title: "模拟选择与审批",
            stage: "merge",
            stageStatus: "waiting_human",
            level: "Medium",
          });
          const workflow = this.simCreateWorkflow(delivery, workflowId, [
            this.simNode(`${nodeId}_plan`, workflowId, "plan", "succeeded"),
            this.simNode(`${nodeId}_merge`, workflowId, "single", "pending"),
          ]);
          const choice: InboxItem = {
            id: choiceId,
            project_id: projectId,
            delivery_id: deliveryId,
            kind: "decision_needed",
            severity: "warn",
            summary: "模拟评审争议：是否继续完成这次演示？",
            payload: {
              kind: "review_dispute",
              reason_code: "guardian.scope_expanding_fix",
              options: [
                { id: "fix", label: "继续并模拟审批", primary: true },
                { id: "keep", label: "停止演示" },
              ],
            },
            status: "open",
            dedup_key: `sim:choice:${run}`,
          };
          this.data.inbox.push(choice);
          this.simCreated.inbox.add(choice.id);
          const approval: ApprovalEntity = {
            id: approvalId,
            delivery_id: deliveryId,
            action: "merge_pr",
            target: { kind: "pr", repo: "LeXwDeX/gantry", number: 900 + run },
            subject_digest: SCENARIO_PR9_SHA,
            device_id: null,
            signature: null,
            status: "pending",
            expires_at: new Date(Date.now() + 86_400_000).toISOString(),
          };
          this.data.approvals.push(approval);
          this.simCreated.approvals.add(approval.id);
          delivery.pr = {
            repo: "LeXwDeX/gantry",
            number: 900 + run,
            node_id: `PR_sim_${run}`,
            title: untrustedText("模拟审批用 PR", "scenario:pr"),
            state: "open",
            head_sha: SCENARIO_PR9_SHA,
            base_ref: "main",
            base_sha: "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
            draft: false,
            mergeable_state: "clean",
            in_merge_queue: false,
            checks: { passed: 4, failed: 0, pending: 0, required_ok: true },
            review_state: "approved",
            synced_at: new Date().toISOString(),
          };
          this.simPendingChoice = choice;
          this.simAwaiting = {
            kind: "choice",
            inbox_id: choice.id,
            options: [
              { id: "fix", label: "继续并模拟审批", primary: true },
              { id: "keep", label: "停止演示" },
            ],
          };
          this.emitEvent("inbox.raised", choice.id, projectId, { inbox_id: choice.id });
          this.simPostAgentMessage(
            projectId,
            "这项演示需要你先做选择；选择后可以模拟批准或拒绝，模拟批准不会生成 signature。",
            {
              kind: "planner",
              deliveryId,
              messageId,
              tone: null,
              refs: [{ kind: "delivery", id: deliveryId }],
              cards: [
                { kind: "choice", inbox_id: choice.id, options: this.simAwaiting.options },
              ],
            },
          );
          void workflow;
          this.simPatch({
            status: "paused",
            phase: "await_choice",
            awaiting: this.simAwaiting,
            pending_approval_id: null,
            total_turns: 1,
          });
        },
      },
    ];
  }

  private simOfflineSteps(projectId: string, userMessage: Message | null): SimStep[] {
    const run = this.simRun;
    return [
      {
        delayMs: 200,
        label: "prepare",
        run: () => {
          const task = this.simCreateTask(projectId, userMessage?.text ?? "断线恢复演示", false);
          this.simTask = task;
          this.simPostAgentMessage(projectId, "任务已建立；现在模拟连接中断。", {
            kind: "utility",
            deliveryId: task.delivery.id,
            tone: null,
          });
          this.setConnection("reconnecting");
          this.simAwaiting = { kind: "connection" };
          this.simPatch({
            status: "paused",
            phase: "await_connection",
            awaiting: this.simAwaiting,
            total_turns: 2,
          });
        },
      },
    ];
  }

  private simSettingsSteps(projectId: string): SimStep[] {
    const run = this.simRun;
    const messageId = `msg_sim_${run}_settings`;
    return [
      {
        delayMs: 200,
        label: "settings_summary",
        run: () => {
          this.simPatch({
            drafts: [
              {
                draft_id: `draft_${run}_settings`,
                message_id: messageId,
                delivery_id: null,
                node_id: null,
                attempt_id: null,
                status: "streaming",
                segments: ["设置演示：请在校验预览中修改设置。"],
                text: "设置演示：请在校验预览中修改设置。",
                simulated: true,
              },
            ],
            phase: "settings_ready",
          });
        },
      },
      {
        delayMs: 250,
        label: "settings_done",
        run: () => {
          this.simPostAgentMessage(
            projectId,
            "设置演示边界已就绪：保存只修改内存，冲突会返回 412，回读只显示 present 标记。",
            { kind: "utility", tone: null, messageId, deliveryId: null },
          );
          this.simPatch({
            drafts: this.simulationState.drafts.map((draft) => ({
              ...draft,
              status: "done",
            })),
            status: "completed",
            phase: "completed",
          });
        },
      },
    ];
  }

  private simCreateDelivery(
    projectId: string,
    options: {
      id: string;
      title: string;
      stage: Delivery["stage"];
      stageStatus: Delivery["stage_status"];
      level: Level;
    },
  ): Delivery {
    const delivery: Delivery = {
      id: options.id,
      project_id: projectId,
      title: options.title.slice(0, 80),
      level_cap: options.level,
      level_effective: options.level,
      stage: options.stage,
      stage_status: options.stageStatus,
      issues: [],
      pr: null,
      worktree_id: null,
      parent_id: null,
      depends_on: [],
      origin: "conversation",
      created_at: new Date().toISOString(),
      updated_at: new Date().toISOString(),
    };
    this.data.deliveries.push(delivery);
    this.simCreated.deliveries.add(delivery.id);
    const level = options.level.toLowerCase();
    const budget = SCENARIO_LEVEL_BUDGETS[level] ?? SCENARIO_LEVEL_BUDGETS.medium;
    this.data.budgets[delivery.id] = {
      id: `bdg_${delivery.id}`,
      delivery_id: delivery.id,
      subject: "delivery",
      template: budget.template,
      limits: {
        tokens_by_tier: {
          utility: budget.utility,
          planner: budget.planner,
          worker: budget.worker,
        },
        nodes: budget.nodes,
        rounds: budget.rounds,
        wall_time: budget.wall_time_s,
        forge_writes: budget.forge_writes,
        concurrency: budget.concurrency,
      },
      used: {
        tokens_by_tier: { utility: 0, planner: 0, worker: 0 },
        nodes: 0,
        attempts: 0,
        rounds: 0,
        wall_time: 0,
        forge_writes: 0,
      },
    };
    this.emitEvent("delivery.created", delivery.id, projectId, {
      delivery_id: delivery.id,
    });
    return delivery;
  }

  private simCreateWorkflow(
    delivery: Delivery,
    workflowId: string,
    nodes: WorkflowNode[],
  ): Workflow {
    const workflow: Workflow = {
      id: workflowId,
      delivery_id: delivery.id,
      conversation_id: null,
      stage: delivery.stage,
      graph_rev: 1,
      status: "running",
      template: `${delivery.level_effective.toLowerCase()}.${delivery.stage}`,
      nodes,
    };
    this.data.workflows[workflow.id] = workflow;
    this.simCreated.workflows.add(workflow.id);
    const budget = this.data.budgets[delivery.id];
    if (budget) {
      budget.used = { ...budget.used, nodes: nodes.length };
    }
    this.emitEvent("workflow.created", workflow.id, delivery.project_id, {
      workflow_id: workflow.id,
    }, delivery.id);
    for (const node of nodes) {
      this.emitEvent("node.queued", node.id, delivery.project_id, {
        node_id: node.id,
      }, delivery.id);
    }
    return workflow;
  }

  private simNode(
    id: string,
    workflowId: string,
    role: WorkflowNode["role"],
    status: WorkflowNode["status"],
    dependsOn: string[] = [],
  ): WorkflowNode {
    return {
      id,
      workflow_id: workflowId,
      role,
      tier: role === "plan" || role === "arbiter" ? "planner" : "worker",
      exec: "sandbox",
      depends_on: dependsOn,
      required: true,
      status,
      verdict: null,
      superseded_by: null,
    };
  }

  private simCreateTask(projectId: string, prompt: string, withFailure: boolean): SimTask {
    const run = this.simRun;
    const delivery = this.simCreateDelivery(projectId, {
      id: `dlv_sim_${run}`,
      title: prompt,
      stage: "implement",
      stageStatus: "running",
      level: "Medium",
    });
    const workflowId = `wfl_sim_${run}`;
    const plan = this.simNode(`nod_sim_${run}_plan`, workflowId, "plan", "pending");
    const build = this.simNode(`nod_sim_${run}_build`, workflowId, "build", "running", [plan.id]);
    const verify = this.simNode(`nod_sim_${run}_verify`, workflowId, "verify", "pending", [build.id]);
    const workflow = this.simCreateWorkflow(delivery, workflowId, [plan, build, verify]);
    const buildAttempt = this.simStartAttempt(build, workflow, delivery, "running");
    const task: SimTask = {
      delivery,
      workflow,
      plan,
      build,
      verify,
      buildAttempt,
      verifyAttempt: null,
    };
    this.emitEvent("node.started", build.id, projectId, {
      node_id: build.id,
      attempt_id: buildAttempt.id,
    }, delivery.id);
    if (withFailure) {
      // 失败脚本在后续步骤替换状态，避免在计划未展示前提前失败。
      return task;
    }
    return task;
  }

  private simStartAttempt(
    node: WorkflowNode,
    workflow: Workflow,
    delivery: Delivery,
    status: Attempt["status"],
  ): Attempt {
    const n = this.data.attempts.filter((entry) => entry.node_id === node.id).length + 1;
    const attempt: Attempt = {
      id: `att_sim_${this.simRun}_${node.id.split("_").pop()}_${n}`,
      node_id: node.id,
      n,
      sandbox_id: `sbx_sim_${this.simRun}_${n}`,
      status,
      started_at: new Date().toISOString(),
      ended_at: null,
      result_ref: "sha256:pending",
      failure_class: null,
      failure_detail: null,
      usage: usageValue(0, 0, 0, 0, 0),
    };
    this.data.attempts.push(attempt);
    this.simCreated.attempts.add(attempt.id);
    void workflow;
    this.simEmitAttemptEvent(attempt, delivery, "attempt.started");
    if (delivery.id) {
      const budget = this.data.budgets[delivery.id];
      if (budget) {
        budget.used = {
          ...budget.used,
          attempts: Number(budget.used.attempts ?? 0) + 1,
        };
      }
    }
    return attempt;
  }

  private simEmitAttemptEvent(attempt: Attempt, delivery: Delivery, type: string): void {
    this.emitEvent(type, attempt.id, delivery.project_id, {
      node_id: attempt.node_id,
      attempt_id: attempt.id,
      n: attempt.n,
    }, delivery.id);
  }

  private simStartTool(task: SimTask, name: string, target: string): void {
    const activity: SimulationToolActivity = {
      call_id: `call_${this.simRun}_${this.simulationState.activities.length + 1}`,
      name,
      target,
      status: "running",
      started_at: new Date().toISOString(),
      ended_at: null,
      elapsed_ms: null,
      collapsed_detail: "模拟摘要：命令已启动，输出未提供。",
      simulated: true,
      delivery_id: task.delivery.id,
      node_id: task.build.id,
      attempt_id: task.buildAttempt.id,
      message_id: `msg_sim_${this.simRun}_plan`,
    };
    this.simPatch({
      activities: [...this.simulationState.activities, activity],
    });
  }

  private simFinishTools(status: SimulationToolStatus): void {
    const endedAt = new Date().toISOString();
    this.simPatch({
      activities: this.simulationState.activities.map((activity) =>
        activity.status === "running"
          ? {
              ...activity,
              status,
              ended_at: endedAt,
              elapsed_ms: Math.max(0, Date.parse(endedAt) - Date.parse(activity.started_at)),
              collapsed_detail:
                status === "succeeded"
                  ? "模拟摘要：命令退出码 0（完整输出未提供）。"
                  : "模拟摘要：命令退出码 1；详情由节点失败原因承载。",
            }
          : activity,
      ),
    });
  }

  private simSetActivities(status: SimulationToolStatus): void {
    this.simFinishTools(status);
  }

  private simSettleBudget(deliveryId: string): void {
    const budget = this.data.budgets[deliveryId];
    if (!budget) return;
    budget.used = {
      ...budget.used,
      tokens_by_tier: { utility: 18_400, planner: 52_100, worker: 611_000 },
      rounds: 1,
      wall_time: 940,
      forge_writes: 3,
    };
    this.emitEvent("budget.charged", deliveryId, this.projectForDelivery(deliveryId), {
      delivery_id: deliveryId,
    }, deliveryId);
  }

  private simPostAgentMessage(
    projectId: string | null,
    text: string,
    options: {
      kind: Message["agent_kind"];
      deliveryId: string | null;
      messageId?: string;
      tone?: Message["tone"];
      refs?: Ref[];
      cards?: Message["cards"];
    },
  ): Message | null {
    if (!projectId) return null;
    const conversation = Object.values(this.data.conversations).find(
      (entry) => entry.project_id === projectId,
    );
    if (!conversation) return null;
    const message: Message = {
      id: options.messageId ?? `msg_sim_${++this.seq}`,
      conversation_id: conversation.id,
      role: "agent",
      agent_kind: options.kind,
      tone: options.tone ?? null,
      text,
      level_cap: null,
      refs: options.refs ?? [],
      cards: options.cards ?? [],
      delivery_id: options.deliveryId,
      created_at: new Date().toISOString(),
    };
    const existing = this.data.messages[projectId] ?? [];
    if (!existing.some((entry) => entry.id === message.id)) {
      this.data.messages[projectId] = [...existing, message];
      this.simCreated.messages.add(message.id);
    }
    this.emitEvent("message.posted", message.id, projectId, {
      message_id: message.id,
    });
    return message;
  }

  private simResolveWaitingChoice(): void {
    if (this.simAwaiting?.kind !== "choice") return;
    const item = this.findInbox(this.simAwaiting.inbox_id);
    if (item && item.status === "open") {
      item.status = "resolved";
      this.emitEvent("inbox.resolved", item.id, item.project_id, {
        inbox_id: item.id,
      });
    }
    this.simAwaiting = null;
  }

  private simApprovalResolved(
    approvalId: string,
    status: "granted" | "denied",
  ): void {
    if (this.simulationState.pending_approval_id !== approvalId) return;
    const deliveryId = this.data.approvals.find((entry) => entry.id === approvalId)?.delivery_id;
    this.simPatch({
      pending_approval_id: null,
      awaiting: null,
      status: "completed",
      phase: status === "denied" ? "approval_denied" : "approval_granted",
      last_error: status === "denied" ? "审批被拒绝。" : null,
    });
    if (deliveryId) {
      this.simPostAgentMessage(
        this.projectForDelivery(deliveryId),
        status === "denied"
          ? "审批被拒绝；演示任务已停止。"
          : "模拟批准：未生成 signature，演示任务继续。",
        { kind: "utility", deliveryId, tone: status === "denied" ? "warn" : null },
      );
    }
    this.simAwaiting = null;
  }

  private projectForDelivery(deliveryId: string | null): string | null {
    if (!deliveryId) return null;
    return (
      this.data.deliveries.find((delivery) => delivery.id === deliveryId)?.project_id ??
      null
    );
  }

  private simResetCreated(): void {
    this.simCreated = EMPTY_CREATED();
  }

  private simRemoveCreated(): void {
    for (const id of this.simCreated.messages) {
      for (const [projectId, messages] of Object.entries(this.data.messages)) {
        this.data.messages[projectId] = messages.filter((message) => message.id !== id);
      }
    }
    this.data.deliveries = this.data.deliveries.filter(
      (delivery) => !this.simCreated.deliveries.has(delivery.id),
    );
    for (const id of this.simCreated.deliveries) delete this.data.budgets[id];
    for (const id of this.simCreated.workflows) delete this.data.workflows[id];
    this.data.inbox = this.data.inbox.filter((item) => !this.simCreated.inbox.has(item.id));
    this.data.approvals = this.data.approvals.filter(
      (approval) => !this.simCreated.approvals.has(approval.id),
    );
    this.data.attempts = this.data.attempts.filter(
      (attempt) => !this.simCreated.attempts.has(attempt.id),
    );
    this.simCreated = EMPTY_CREATED();
  }
}

function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function mergePrefs(current: Prefs, patch: Partial<Prefs>): Prefs {
  return {
    ...current,
    ...patch,
    layout: { ...current.layout, ...(patch.layout ?? {}) },
  };
}

function optionsOf(item: InboxItem): Array<{ id: string }> {
  const payload = item.payload as { options?: Array<{ id: string }> };
  return Array.isArray(payload.options) ? payload.options : [];
}

const SETTINGS_RESOURCE_IDS = new Set([
  "models",
  "levels",
  "policies",
  "guardian",
  "sandbox",
  "egress",
  "notify",
  "github",
]);

const PROJECT_OVERRIDABLE = new Set([
  "models",
  "levels",
  "policies",
  "guardian",
  "sandbox",
  "egress",
]);

const LEVEL_RANK: Record<string, number> = {
  None: 0,
  Low: 1,
  Medium: 2,
  High: 3,
  Max: 4,
  Ultimate: 5,
};

const DISPOSITION_RANK: Record<string, number> = {
  auto: 0,
  approve: 1,
  deny: 2,
  silent_fix: 0,
  silent_reply: 1,
  batch: 1,
  escalate: 2,
};

function usageValue(
  input: number,
  output: number,
  cacheRead = 0,
  cacheWrite = 0,
  requests = 1,
): Attempt["usage"] {
  return {
    input_tokens: input,
    output_tokens: output,
    cache_read_tokens: cacheRead,
    cache_write_tokens: cacheWrite,
    requests,
  };
}

function untrustedText(text: string, source: string): UntrustedText {
  return { text, source, truncated: false };
}

function etagOf(doc: SettingsDocument | null | undefined): string {
  return `"${doc?.rev ?? 0}"`;
}

function moduleForResource(resource: string): string {
  switch (resource) {
    case "models":
      return "models";
    case "levels":
      return "engine";
    case "policies":
      return "policy";
    case "guardian":
      return "guardian";
    case "sandbox":
      return "sandbox";
    case "egress":
      return "egress";
    case "notify":
      return "notify";
    case "github":
      return "forge";
    default:
      return "models";
  }
}

function issue(pointer: string, code: string, message?: string): SettingsIssue {
  return { pointer, code, message };
}

function prepareSettingsValue(
  resource: string,
  raw: unknown,
  globalValue: unknown | null,
  project: boolean,
): { clean: unknown; errors: SettingsIssue[]; secretRefs: string[] } {
  const errors: SettingsIssue[] = [];
  const secretRefs: string[] = [];
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    return {
      clean: raw,
      errors: [issue("", "type", "设置值必须是一个对象。")],
      secretRefs,
    };
  }
  const { clean, secrets } = scrubSecretValues(raw, "", errors);
  secretRefs.push(...secrets);
  if (errors.length === 0) {
    validateResource(resource, clean, project, errors);
    if (project && errors.length === 0 && isRecord(clean)) {
      validateProjectTightening(resource, clean, globalValue, errors);
    }
  }
  return { clean, errors, secretRefs };
}

const SECRET_KEY = /(?:^|[_-])(api[_-]?key|token|password|secret|private[_-]?key)$/i;

function scrubSecretValues(
  value: unknown,
  pointer: string,
  errors: SettingsIssue[],
): { clean: unknown; secrets: string[] } {
  if (Array.isArray(value)) {
    return {
      clean: value.map((entry, index) =>
        scrubSecretValues(entry, `${pointer}/${index}`, errors).clean,
      ),
      secrets: [],
    };
  }
  if (!value || typeof value !== "object") {
    if (typeof value === "string" && looksLikePem(value)) {
      errors.push(issue(pointer, "inline_secret_detected", "不得内联私钥或凭据。"));
    }
    return { clean: value, secrets: [] };
  }
  const object = value as Record<string, unknown>;
  const secrets: string[] = [];
  const clean: Record<string, unknown> = {};
  for (const [key, entry] of Object.entries(object)) {
    const path = `${pointer}/${key}`;
    if (
      key === "value" &&
      typeof object.secret_ref === "string" &&
      entry !== undefined
    ) {
      if (entry === null) {
        errors.push(issue(path, "secret_clear_in_use", "使用中的凭据不能被清空。"));
      } else if (typeof entry !== "string") {
        errors.push(issue(path, "type", "凭据值必须是字符串。"));
      } else if (!isTestPlaceholderSecret(entry)) {
        errors.push(
          issue(
            path,
            "placeholder_required",
            "示例模式只接受明确标注的测试占位值；请勿输入真实凭据。",
          ),
        );
      } else {
        secrets.push(object.secret_ref);
      }
      continue;
    }
    if (
      typeof entry === "string" &&
      SECRET_KEY.test(key) &&
      key !== "secret_ref" &&
      !(key === "base_url" || key === "webhook_secret_previous")
    ) {
      if (looksLikePem(entry) || entry.length > 24) {
        errors.push(issue(path, "inline_secret_detected", "不得内联凭据。"));
        continue;
      }
    }
    const nested = scrubSecretValues(entry, path, errors);
    clean[key] = nested.clean;
    secrets.push(...nested.secrets);
  }
  return { clean, secrets };
}

function looksLikePem(value: string): boolean {
  return /-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(value);
}

function isTestPlaceholderSecret(value: string): boolean {
  if (!value || looksLikePem(value)) return false;
  return /(test|example|placeholder|fake|dummy|sample)/i.test(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function positiveNumber(value: unknown): boolean {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

function positiveInteger(value: unknown): boolean {
  return typeof value === "number" && Number.isInteger(value) && value > 0;
}

function rejectUnknownKeys(
  value: Record<string, unknown>,
  pointer: string,
  allowed: string[],
  errors: SettingsIssue[],
): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) {
      errors.push(issue(`${pointer}/${key}`, "unknown_field", `未知字段 ${key}。`));
    }
  }
}

function validateResource(
  resource: string,
  raw: unknown,
  project: boolean,
  errors: SettingsIssue[],
): void {
  if (!isRecord(raw)) {
    errors.push(issue("", "type", "设置值必须是一个对象。"));
    return;
  }
  const globalKeys: Record<string, string[]> = {
    models: ["providers", "retry", "utility", "planner", "worker"],
    levels: ["default_level", "token_weights", "none", "low", "medium", "high", "max", "ultimate"],
    policies: ["actions", "unlisted_action", "paths", "allow_globs", "escalation", "approval", "patch"],
    guardian: ["triage", "no_progress", "status_checklist", "patrol_interval_s"],
    sandbox: [
      "driver",
      "runtime",
      "base_image",
      "limits",
      "max_concurrent_sandboxes",
      "prewarm",
      "devcontainer",
      "idle_timeout_s",
      "kill_grace_s",
      "io",
    ],
    egress: ["groups", "profiles", "default_profile", "injection"],
    notify: ["channels", "routes"],
    github: ["app", "commit_identity"],
  };
  const projectKeys: Record<string, string[]> = {
    models: ["utility", "planner", "worker"],
    levels: ["default_level", "none", "low", "medium", "high", "max", "ultimate"],
    policies: ["actions", "unlisted_action", "paths", "escalation", "approval", "patch"],
    guardian: ["triage", "no_progress", "status_checklist"],
    sandbox: ["limits", "devcontainer"],
    egress: ["profiles"],
  };
  const allowed = project ? projectKeys[resource] ?? [] : globalKeys[resource];
  if (!allowed) {
    errors.push(issue("", "unknown_field", "未知设置资源。"));
    return;
  }
  rejectUnknownKeys(raw, "", allowed, errors);
  switch (resource) {
    case "models":
      validateModels(raw, project, errors);
      break;
    case "levels":
      validateLevels(raw, project, errors);
      break;
    case "policies":
      validatePolicies(raw, project, errors);
      break;
    case "guardian":
      validateGuardian(raw, project, errors);
      break;
    case "sandbox":
      validateSandbox(raw, project, errors);
      break;
    case "egress":
      validateEgress(raw, project, errors);
      break;
    case "notify":
      validateNotify(raw, errors);
      break;
    case "github":
      validateGithub(raw, errors);
      break;
  }
}

function validateModels(
  raw: Record<string, unknown>,
  project: boolean,
  errors: SettingsIssue[],
): void {
  if (!project && raw.providers !== undefined) {
    if (!isRecord(raw.providers)) {
      errors.push(issue("/providers", "type", "providers 必须是对象。"));
    } else {
      for (const [id, provider] of Object.entries(raw.providers)) {
        if (!isRecord(provider)) {
          errors.push(issue(`/providers/${id}`, "type", "提供方必须是对象。"));
          continue;
        }
        rejectUnknownKeys(provider, `/providers/${id}`, ["kind", "base_url", "credential"], errors);
        if (provider.kind !== undefined && provider.kind !== "anthropic") {
          errors.push(issue(`/providers/${id}/kind`, "enum", "示例只支持 anthropic。"));
        }
        if (provider.base_url !== undefined && !isHttpsUrl(provider.base_url)) {
          errors.push(issue(`/providers/${id}/base_url`, "type", "base_url 必须是 https URL。"));
        }
        const credential = provider.credential;
        if (credential !== undefined) {
          if (!isRecord(credential)) {
            errors.push(issue(`/providers/${id}/credential`, "type", "credential 必须是对象。"));
          } else {
            rejectUnknownKeys(
              credential,
              `/providers/${id}/credential`,
              ["type", "secret_ref", "present"],
              errors,
            );
            if (credential.type !== undefined && credential.type !== "api_key") {
              errors.push(issue(`/providers/${id}/credential/type`, "enum", "示例只支持 api_key。"));
            }
            if (typeof credential.secret_ref !== "string" || !credential.secret_ref) {
              errors.push(issue(`/providers/${id}/credential/secret_ref`, "secret_ref_missing", "缺少 secret_ref。"));
            } else if (!/^[a-z0-9][a-z0-9._-]{1,120}$/i.test(credential.secret_ref)) {
              errors.push(issue(`/providers/${id}/credential/secret_ref`, "secret_ref_missing", "secret_ref 命名不合法。"));
            }
          }
        }
      }
    }
  }
  if (!project && raw.retry !== undefined) {
    if (!isRecord(raw.retry)) {
      errors.push(issue("/retry", "type", "retry 必须是对象。"));
    } else {
      rejectUnknownKeys(raw.retry, "/retry", ["max_attempts", "backoff_ms"], errors);
      if (raw.retry.max_attempts !== undefined && !positiveInteger(raw.retry.max_attempts)) {
        errors.push(issue("/retry/max_attempts", "range", "重试次数必须是正整数。"));
      }
      if (raw.retry.backoff_ms !== undefined && !positiveInteger(raw.retry.backoff_ms)) {
        errors.push(issue("/retry/backoff_ms", "range", "退避时间必须是正整数。"));
      }
    }
  }
  const providers = isRecord(raw.providers) ? Object.keys(raw.providers) : [];
  const catalog = buildCatalogForValidation();
  for (const tier of ["utility", "planner", "worker"] as const) {
    const entry = raw[tier];
    if (entry === undefined) continue;
    if (!isRecord(entry)) {
      errors.push(issue(`/${tier}`, "type", `${tier} 必须是对象。`));
      continue;
    }
    rejectUnknownKeys(
      entry,
      `/${tier}`,
      [
        "provider",
        "model",
        "reasoning_effort",
        "effort_by_level",
        "fallback",
        "adapter",
        "request_timeout_s",
        "total_timeout_s",
        "attempt_wall_s",
        "max_turns",
      ],
      errors,
    );
    if (typeof entry.provider !== "string" || !entry.provider) {
      errors.push(issue(`/${tier}/provider`, "required", "必须选择提供方。"));
    } else if (!project && providers.length > 0 && !providers.includes(entry.provider)) {
      errors.push(issue(`/${tier}/provider`, "enum", "提供方不在 providers 中。"));
    }
    if (typeof entry.model !== "string" || !entry.model) {
      errors.push(issue(`/${tier}/model`, "required", "必须选择模型。"));
      continue;
    }
    const key = `${String(entry.provider ?? "")}/${entry.model}`;
    const capability = catalog.models.capabilities[key];
    const effort = entry.reasoning_effort;
    if (effort !== undefined && typeof effort !== "string") {
      errors.push(issue(`/${tier}/reasoning_effort`, "type", "推理深度必须是字符串。"));
    } else if (typeof effort === "string") {
      const allowed = capability?.reasoning_efforts ?? ["none", "low", "medium", "high", "max"];
      if (tier === "utility" && effort === "by_level") {
        errors.push(issue(`/${tier}/reasoning_effort`, "enum", "utility 不支持 by_level。"));
      } else if (effort === "by_level") {
        if (!isRecord(entry.effort_by_level)) {
          errors.push(issue(`/${tier}/effort_by_level`, "effort_by_level_required", "选择随档位时必须提供完整映射。"));
        } else {
          for (const level of ["low", "medium", "high", "max"]) {
            const mapping = entry.effort_by_level[level];
            if (!isRecord(mapping) || typeof mapping.default !== "string") {
              errors.push(issue(`/${tier}/effort_by_level/${level}`, "effort_by_level_required", "每个档位必须提供 default。"));
            } else if (!allowed.includes(mapping.default)) {
              errors.push(issue(`/${tier}/effort_by_level/${level}/default`, "effort_unsupported", "模型不支持该推理深度。"));
            }
          }
        }
      } else if (!allowed.includes(effort)) {
        errors.push(issue(`/${tier}/reasoning_effort`, "effort_unsupported", "模型不支持该推理深度。"));
      }
    }
    if (tier !== "utility" && capability && !capability.tools) {
      errors.push(issue(`/${tier}/model`, "worker_model_no_tools", "planner 和工作模型必须支持工具调用。"));
    }
    if (!project && entry.adapter !== undefined && typeof entry.adapter !== "string") {
      errors.push(issue(`/${tier}/adapter`, "type", "adapter 必须是字符串。"));
    }
    if (entry.fallback !== undefined) {
      if (!Array.isArray(entry.fallback) || entry.fallback.length > 3) {
        errors.push(issue(`/${tier}/fallback`, "range", "fallback 最多 3 项。"));
      } else if (tier === "worker" && entry.fallback.length > 0) {
        const adapter = catalog.models.adapters[String(entry.adapter ?? "claude-code")];
        if (!adapter?.supports_fallback) {
          errors.push(issue(`/${tier}/fallback`, "fallback_unsupported", "当前适配器不支持降级。"));
        }
      }
    }
    if (entry.request_timeout_s !== undefined && !positiveInteger(entry.request_timeout_s)) {
      errors.push(issue(`/${tier}/request_timeout_s`, "range", "请求超时必须是正整数秒。"));
    }
    if (entry.total_timeout_s !== undefined && !positiveInteger(entry.total_timeout_s)) {
      errors.push(issue(`/${tier}/total_timeout_s`, "range", "总超时必须是正整数秒。"));
    }
    if (entry.attempt_wall_s !== undefined && !positiveInteger(entry.attempt_wall_s)) {
      errors.push(issue(`/${tier}/attempt_wall_s`, "range", "尝试时限必须是正整数秒。"));
    }
    if (entry.max_turns !== undefined && !positiveInteger(entry.max_turns)) {
      errors.push(issue(`/${tier}/max_turns`, "range", "max_turns 必须是正整数。"));
    }
  }
}

function isHttpsUrl(value: unknown): boolean {
  if (typeof value !== "string") return false;
  try {
    return new URL(value).protocol === "https:";
  } catch {
    return false;
  }
}

function validateLevels(
  raw: Record<string, unknown>,
  _project: boolean,
  errors: SettingsIssue[],
): void {
  if (
    raw.default_level !== undefined &&
    (typeof raw.default_level !== "string" || !(raw.default_level in LEVEL_RANK))
  ) {
    errors.push(issue("/default_level", "enum", "不是合法档位。"));
  }
  if (raw.token_weights !== undefined) {
    if (!isRecord(raw.token_weights)) {
      errors.push(issue("/token_weights", "type", "token_weights 必须是对象。"));
    } else {
      for (const key of ["input", "output", "cache_read", "cache_write"]) {
        if (!positiveNumber(raw.token_weights[key])) {
          errors.push(issue(`/token_weights/${key}`, "token_weights_invalid", "权重必须是正数。"));
        }
      }
    }
  }
  for (const level of ["low", "medium", "high", "max", "ultimate"]) {
    const entry = raw[level];
    if (entry === undefined) continue;
    if (!isRecord(entry)) {
      errors.push(issue(`/${level}`, "type", "档位必须是对象。"));
      continue;
    }
    rejectUnknownKeys(entry, `/${level}`, ["graph_template", "budget"], errors);
    if (entry.graph_template !== undefined && typeof entry.graph_template !== "string") {
      errors.push(issue(`/${level}/graph_template`, "type", "图模板必须是字符串。"));
    }
    if (entry.budget !== undefined) {
      validateBudget(entry.budget, `/${level}/budget`, errors);
    }
  }
  const none = raw.none;
  if (none !== undefined) {
    if (!isRecord(none)) errors.push(issue("/none", "type", "none 必须是对象。"));
    else {
      rejectUnknownKeys(none, "/none", ["daily_tokens"], errors);
      if (none.daily_tokens !== undefined && !positiveInteger(none.daily_tokens)) {
        errors.push(issue("/none/daily_tokens", "range", "None 档日上限必须是正整数。"));
      }
    }
  }
}

function validateBudget(value: unknown, pointer: string, errors: SettingsIssue[]): void {
  if (!isRecord(value)) {
    errors.push(issue(pointer, "type", "budget 必须是对象。"));
    return;
  }
  rejectUnknownKeys(
    value,
    pointer,
    [
      "tokens_by_tier",
      "nodes",
      "rounds",
      "wall_time_s",
      "concurrency",
      "forge_writes",
      "forge_writes_per_hour",
    ],
    errors,
  );
  const tiers = value.tokens_by_tier;
  if (tiers !== undefined) {
    if (!isRecord(tiers)) errors.push(issue(`${pointer}/tokens_by_tier`, "type", "tokens_by_tier 必须是对象。"));
    else {
      for (const tier of ["utility", "planner", "worker"]) {
        if (!positiveInteger(tiers[tier])) {
          errors.push(issue(`${pointer}/tokens_by_tier/${tier}`, "budget_not_finite", "预算必须是正整数。"));
        }
      }
    }
  }
  for (const key of ["nodes", "rounds", "wall_time_s", "concurrency", "forge_writes", "forge_writes_per_hour"]) {
    if (value[key] !== undefined && !positiveInteger(value[key])) {
      errors.push(issue(`${pointer}/${key}`, "budget_not_finite", "预算必须是正整数。"));
    }
  }
}

function validatePolicies(
  raw: Record<string, unknown>,
  project: boolean,
  errors: SettingsIssue[],
): void {
  if (raw.actions !== undefined) {
    if (!isRecord(raw.actions)) errors.push(issue("/actions", "type", "actions 必须是对象。"));
    else {
      for (const [id, disposition] of Object.entries(raw.actions)) {
        const definition = ACTION_DEFINITIONS.find((action) => action.id === id);
        if (!definition) {
          errors.push(issue(`/actions/${id}`, "action_unknown", "未知动作。"));
          continue;
        }
        if (typeof disposition !== "string" || !definition.allowed.includes(disposition)) {
          errors.push(issue(`/actions/${id}`, "disposition_not_allowed", "动作处置不在允许范围。"));
        } else if (definition.dangerous && disposition === "auto") {
          errors.push(issue(`/actions/${id}`, "disposition_not_allowed", "危险动作不能自动放行。"));
        }
      }
    }
  }
  if (raw.unlisted_action !== undefined && !["approve", "deny"].includes(String(raw.unlisted_action))) {
    errors.push(issue("/unlisted_action", "disposition_not_allowed", "未列动作只允许 approve 或 deny。"));
  }
  if (raw.paths !== undefined) {
    if (!isRecord(raw.paths)) errors.push(issue("/paths", "type", "paths 必须是对象。"));
    else {
      for (const [id, rule] of Object.entries(raw.paths)) {
        if (!isRecord(rule)) {
          errors.push(issue(`/paths/${id}`, "type", "路径规则必须是对象。"));
          continue;
        }
        rejectUnknownKeys(rule, `/paths/${id}`, ["globs", "disposition"], errors);
        if (!Array.isArray(rule.globs) || rule.globs.some((glob) => typeof glob !== "string")) {
          errors.push(issue(`/paths/${id}/globs`, "type", "globs 必须是字符串数组。"));
        }
        if (!["approve", "deny"].includes(String(rule.disposition))) {
          errors.push(issue(`/paths/${id}/disposition`, "disposition_not_allowed", "路径规则只允许 approve 或 deny。"));
        }
        if ((id === "workflows" || id === "sensitive_files") && rule.disposition !== "deny") {
          errors.push(issue(`/paths/${id}/disposition`, "invariant_floor", "该规则必须保持 deny。"));
        }
      }
    }
  }
  if (!project && raw.allow_globs !== undefined) {
    if (!Array.isArray(raw.allow_globs) || raw.allow_globs.length === 0) {
      errors.push(issue("/allow_globs", "glob_invalid", "allow_globs 不能为空。"));
    }
  }
  if (raw.escalation !== undefined) {
    if (!isRecord(raw.escalation)) errors.push(issue("/escalation", "type", "escalation 必须是对象。"));
    else {
      rejectUnknownKeys(raw.escalation, "/escalation", ["enabled", "rules"], errors);
      if (raw.escalation.rules !== undefined) {
        if (!isRecord(raw.escalation.rules)) errors.push(issue("/escalation/rules", "type", "rules 必须是对象。"));
        else {
          for (const [id, rule] of Object.entries(raw.escalation.rules)) {
            if (!isRecord(rule)) {
              errors.push(issue(`/escalation/rules/${id}`, "type", "规则必须是对象。"));
              continue;
            }
            rejectUnknownKeys(rule, `/escalation/rules/${id}`, ["when", "raise_to"], errors);
            if (!["Medium", "High", "Max"].includes(String(rule.raise_to))) {
              errors.push(issue(`/escalation/rules/${id}/raise_to`, "enum", "raise_to 不在允许范围。"));
            }
            if (!isRecord(rule.when)) {
              errors.push(issue(`/escalation/rules/${id}/when`, "escalation_when_invalid", "when 必须是一个条件对象。"));
            } else {
              const keys = Object.keys(rule.when);
              if (keys.length !== 1 || !["paths", "diff", "labels"].includes(keys[0])) {
                errors.push(issue(`/escalation/rules/${id}/when`, "escalation_when_invalid", "when 必须恰好包含一种条件。"));
              }
            }
          }
        }
      }
    }
  }
  if (raw.approval !== undefined) {
    if (!isRecord(raw.approval)) errors.push(issue("/approval", "type", "approval 必须是对象。"));
    else {
      rejectUnknownKeys(raw.approval, "/approval", ["ttl_s"], errors);
      if (raw.approval.ttl_s !== undefined) {
        const ttl = raw.approval.ttl_s;
        if (typeof ttl !== "number" || !Number.isInteger(ttl) || ttl < 60 || ttl > 604_800) {
          errors.push(issue("/approval/ttl_s", "range", "审批有效期必须在 60–604800 秒。"));
        }
      }
    }
  }
  if (raw.patch !== undefined) {
    if (!isRecord(raw.patch)) errors.push(issue("/patch", "type", "patch 必须是对象。"));
    else {
      const keys = ["max_files", "max_bytes", "max_changed_lines", "max_file_bytes"];
      rejectUnknownKeys(raw.patch, "/patch", keys, errors);
      for (const key of keys) {
        if (raw.patch[key] !== undefined && !positiveInteger(raw.patch[key])) {
          errors.push(issue(`/patch/${key}`, "range", "补丁上限必须是正整数。"));
        }
      }
    }
  }
}

function validateGuardian(
  raw: Record<string, unknown>,
  project: boolean,
  errors: SettingsIssue[],
): void {
  if (raw.triage !== undefined) {
    if (!isRecord(raw.triage)) errors.push(issue("/triage", "type", "triage 必须是对象。"));
    else {
      for (const [id, disposition] of Object.entries(raw.triage)) {
        const definition = GUARDIAN_TRIAGE_CATALOG[id];
        if (!definition) {
          errors.push(issue(`/triage/${id}`, "triage_value_invalid", "未知分流类。"));
        } else if (definition.locked) {
          errors.push(issue(`/triage/${id}`, "locked_key", "锁定类不可配置。"));
        } else if (typeof disposition !== "string" || !definition.allowed.includes(disposition)) {
          errors.push(issue(`/triage/${id}`, "triage_value_invalid", "分流处置不在允许范围。"));
        }
      }
    }
  }
  if (raw.no_progress !== undefined) {
    if (!isRecord(raw.no_progress)) errors.push(issue("/no_progress", "type", "no_progress 必须是对象。"));
    else {
      rejectUnknownKeys(raw.no_progress, "/no_progress", ["same_class_repeats", "stalled_rounds"], errors);
      for (const key of ["same_class_repeats", "stalled_rounds"]) {
        if (raw.no_progress[key] !== undefined && !positiveInteger(raw.no_progress[key])) {
          errors.push(issue(`/no_progress/${key}`, "range", "阈值必须是正整数。"));
        }
      }
    }
  }
  if (raw.status_checklist !== undefined) {
    if (!isRecord(raw.status_checklist)) errors.push(issue("/status_checklist", "type", "status_checklist 必须是对象。"));
    else {
      rejectUnknownKeys(raw.status_checklist, "/status_checklist", ["enabled", "min_update_interval_s"], errors);
      if (
        raw.status_checklist.min_update_interval_s !== undefined &&
        (typeof raw.status_checklist.min_update_interval_s !== "number" ||
          !Number.isInteger(raw.status_checklist.min_update_interval_s) ||
          raw.status_checklist.min_update_interval_s < 0)
      ) {
        errors.push(issue("/status_checklist/min_update_interval_s", "range", "间隔必须是非负整数。"));
      }
    }
  }
  if (!project && raw.patrol_interval_s !== undefined) {
    if (!positiveInteger(raw.patrol_interval_s) || Number(raw.patrol_interval_s) < 300) {
      errors.push(issue("/patrol_interval_s", "range", "巡检间隔不得小于 300 秒。"));
    }
  }
}

const GUARDIAN_TRIAGE_CATALOG: Record<
  string,
  { allowed: string[]; locked: boolean; value?: string }
> = {
  ci_failure_in_scope: { allowed: ["silent_fix", "escalate"], locked: false },
  bot_finding_valid: { allowed: ["silent_fix", "escalate"], locked: false },
  bot_finding_invalid: { allowed: ["silent_reply", "escalate"], locked: false },
  nit: { allowed: ["batch", "escalate"], locked: false },
  mechanical_conflict: { allowed: ["escalate", "silent_fix"], locked: false },
  security_finding: { allowed: [], locked: true, value: "escalate" },
  touches_protected_path: { allowed: [], locked: true, value: "escalate" },
  scope_expanding_fix: { allowed: [], locked: true, value: "escalate" },
  repeated_or_no_progress: { allowed: [], locked: true, value: "escalate" },
  budget_exhausted: { allowed: [], locked: true, value: "escalate" },
  approval_needed: { allowed: [], locked: true, value: "escalate" },
  push_after_approval: { allowed: [], locked: true, value: "escalate" },
  design_or_api_change: { allowed: [], locked: true, value: "escalate" },
  unknown: { allowed: [], locked: true, value: "escalate" },
};

function validateSandbox(
  raw: Record<string, unknown>,
  project: boolean,
  errors: SettingsIssue[],
): void {
  if (!project) {
    if (raw.driver !== undefined && raw.driver !== "podman") {
      errors.push(issue("/driver", "enum", "第一版只支持 podman。"));
    }
    if (raw.runtime !== undefined && !["runsc", "runc"].includes(String(raw.runtime))) {
      errors.push(issue("/runtime", "enum", "runtime 只允许 runsc 或 runc。"));
    }
    if (raw.base_image !== undefined) {
      if (typeof raw.base_image !== "string" || !/@sha256:[0-9a-f]{64}$/i.test(raw.base_image)) {
        errors.push(issue("/base_image", "image_not_pinned", "基础镜像必须以 @sha256: 固定摘要。"));
      }
    }
    if (raw.max_concurrent_sandboxes !== undefined && !positiveInteger(raw.max_concurrent_sandboxes)) {
      errors.push(issue("/max_concurrent_sandboxes", "range", "并发布盒数必须是正整数。"));
    }
    if (raw.idle_timeout_s !== undefined && !positiveInteger(raw.idle_timeout_s)) {
      errors.push(issue("/idle_timeout_s", "range", "空闲超时必须是正整数。"));
    }
    if (raw.kill_grace_s !== undefined && !positiveInteger(raw.kill_grace_s)) {
      errors.push(issue("/kill_grace_s", "range", "宽限时间必须是正整数。"));
    }
    if (raw.prewarm !== undefined) {
      if (!isRecord(raw.prewarm)) errors.push(issue("/prewarm", "type", "prewarm 必须是对象。"));
      else rejectUnknownKeys(raw.prewarm, "/prewarm", ["enabled", "pool_size"], errors);
    }
    if (raw.io !== undefined && !isRecord(raw.io)) {
      errors.push(issue("/io", "type", "io 必须是对象。"));
    }
  }
  if (raw.limits !== undefined) {
    if (!isRecord(raw.limits)) errors.push(issue("/limits", "type", "limits 必须是对象。"));
    else {
      rejectUnknownKeys(raw.limits, "/limits", ["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"], errors);
      for (const key of ["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"]) {
        if (raw.limits[key] !== undefined && !positiveNumber(raw.limits[key])) {
          errors.push(issue(`/limits/${key}`, "range", "沙盒限制必须是正数。"));
        }
      }
    }
  }
  if (raw.devcontainer !== undefined) {
    if (!isRecord(raw.devcontainer)) errors.push(issue("/devcontainer", "type", "devcontainer 必须是对象。"));
    else {
      rejectUnknownKeys(raw.devcontainer, "/devcontainer", ["mode"], errors);
      if (raw.devcontainer.mode !== undefined && !["off", "safe_subset"].includes(String(raw.devcontainer.mode))) {
        errors.push(issue("/devcontainer/mode", "enum", "devcontainer mode 不在允许范围。"));
      }
    }
  }
}

function validateEgress(
  raw: Record<string, unknown>,
  project: boolean,
  errors: SettingsIssue[],
): void {
  if (!project && raw.groups !== undefined) {
    if (!isRecord(raw.groups)) errors.push(issue("/groups", "type", "groups 必须是对象。"));
    else {
      for (const [name, group] of Object.entries(raw.groups)) {
        if (!isRecord(group) || !Array.isArray(group.hosts)) {
          errors.push(issue(`/groups/${name}`, "type", "分组必须包含 hosts 数组。"));
          continue;
        }
        for (const host of group.hosts) {
          if (typeof host !== "string" || isForbiddenHost(host)) {
            errors.push(issue(`/groups/${name}/hosts`, "egress_forbidden_host", "主机不在允许范围。"));
          }
        }
      }
    }
  }
  const knownGroups = isRecord(raw.groups)
    ? new Set(Object.keys(raw.groups))
    : null;
  if (raw.profiles !== undefined) {
    if (!isRecord(raw.profiles)) errors.push(issue("/profiles", "type", "profiles 必须是对象。"));
    else {
      for (const [name, profile] of Object.entries(raw.profiles)) {
        if (!isRecord(profile) || !Array.isArray(profile.groups)) {
          errors.push(issue(`/profiles/${name}`, "type", "档案必须包含 groups 数组。"));
        } else if (knownGroups) {
          for (const group of profile.groups) {
            if (typeof group !== "string" || !knownGroups.has(group)) {
              errors.push(
                issue(
                  `/profiles/${name}/groups`,
                  "egress_forbidden_host",
                  "档案引用了不存在的分组。",
                ),
              );
            }
          }
        }
      }
    }
  }
  if (!project && raw.default_profile !== undefined) {
    if (typeof raw.default_profile !== "string") {
      errors.push(issue("/default_profile", "type", "default_profile 必须是字符串。"));
    } else if (isRecord(raw.profiles) && !isRecord(raw.profiles[raw.default_profile])) {
      errors.push(issue("/default_profile", "enum", "default_profile 必须引用已有档案。"));
    }
  }
  if (!project && raw.injection !== undefined) {
    if (!isRecord(raw.injection)) errors.push(issue("/injection", "type", "injection 必须是对象。"));
    else {
      const seenInjections = new Set<string>();
      for (const [id, item] of Object.entries(raw.injection)) {
        if (!isRecord(item)) {
          errors.push(issue(`/injection/${id}`, "type", "注入项必须是对象。"));
          continue;
        }
        rejectUnknownKeys(item, `/injection/${id}`, ["hosts", "kind", "header", "credential"], errors);
        if (!["bearer", "header"].includes(String(item.kind))) {
          errors.push(issue(`/injection/${id}/kind`, "enum", "注入类型不在允许范围。"));
        }
        if (isRecord(item.credential) && typeof item.credential.secret_ref !== "string") {
          errors.push(issue(`/injection/${id}/credential/secret_ref`, "secret_ref_missing", "缺少 secret_ref。"));
        }
        if (Array.isArray(item.hosts) && isRecord(raw.groups)) {
          const allHosts = new Set<string>();
          for (const group of Object.values(raw.groups)) {
            if (isRecord(group) && Array.isArray(group.hosts)) {
              for (const host of group.hosts) if (typeof host === "string") allHosts.add(host);
            }
          }
          for (const host of item.hosts) {
            if (typeof host === "string" && !allHosts.has(host)) {
              errors.push(
                issue(
                  `/injection/${id}/hosts`,
                  "injection_host_not_in_group",
                  "注入主机必须属于某个出网分组。",
                ),
              );
            }
          }
        }
        const header =
          typeof item.header === "string" ? item.header.toLowerCase() : "authorization";
        if (["host", "content-length", "transfer-encoding"].includes(header)) {
          errors.push(issue(`/injection/${id}/header`, "injection_conflict", "不能使用逐跳头。"));
        }
        if (Array.isArray(item.hosts)) {
          for (const host of item.hosts) {
            if (typeof host !== "string") continue;
            const key = `${host.toLowerCase()}:${header}`;
            if (seenInjections.has(key)) {
              errors.push(issue(`/injection/${id}/hosts`, "injection_conflict", "同一主机与请求头只能配置一个注入。"));
            }
            seenInjections.add(key);
          }
        }
      }
    }
  }
}

function isForbiddenHost(host: string): boolean {
  const normalized = host.trim().toLowerCase();
  if (!normalized || normalized === "*" || normalized.includes("**")) return true;
  if (/^(?:\d{1,3}\.){3}\d{1,3}$/.test(normalized) || normalized.includes(":")) return true;
  if (normalized === "github.com" || normalized.endsWith(".github.com")) return true;
  if (normalized === "metadata.google.internal") return true;
  if (normalized.startsWith("*.") && normalized.slice(2).split(".").length < 3) return true;
  return false;
}

function validateNotify(raw: Record<string, unknown>, errors: SettingsIssue[]): void {
  if (raw.channels !== undefined) {
    if (!isRecord(raw.channels)) errors.push(issue("/channels", "type", "channels 必须是对象。"));
    else {
      for (const [kind, channel] of Object.entries(raw.channels)) {
        if (!["desktop", "mobile_push"].includes(kind)) {
          errors.push(issue(`/channels/${kind}`, "channel_unavailable", "未知通知渠道。"));
        } else if (isRecord(channel) && channel.enabled === true && kind !== "desktop") {
          errors.push(issue(`/channels/${kind}`, "channel_unavailable", "该渠道在第一版不可用。"));
        }
      }
    }
  }
  if (raw.routes !== undefined) {
    if (!isRecord(raw.routes)) errors.push(issue("/routes", "type", "routes 必须是对象。"));
    else {
      for (const [id, route] of Object.entries(raw.routes)) {
        if (!isRecord(route) || !Array.isArray(route.kinds) || !Array.isArray(route.channels)) {
          errors.push(issue(`/routes/${id}`, "type", "路由必须是对象。"));
          continue;
        }
        if (route.channels.some((channel) => channel !== "desktop")) {
          errors.push(issue(`/routes/${id}/channels`, "channel_unavailable", "示例只支持 desktop 渠道。"));
        }
      }
    }
  }
}

function validateGithub(raw: Record<string, unknown>, errors: SettingsIssue[]): void {
  if (raw.app !== undefined) {
    if (!isRecord(raw.app)) errors.push(issue("/app", "type", "app 必须是对象。"));
    else {
      rejectUnknownKeys(
        raw.app,
        "/app",
        ["app_id", "private_key", "webhook_secret", "webhook_secret_previous"],
        errors,
      );
      if (raw.app.app_id !== undefined && !positiveInteger(raw.app.app_id)) {
        errors.push(issue("/app/app_id", "range", "app_id 必须是正整数。"));
      }
      for (const key of ["private_key", "webhook_secret", "webhook_secret_previous"]) {
        const ref = raw.app[key];
        if (ref !== undefined) {
          if (!isRecord(ref) || typeof ref.secret_ref !== "string" || !ref.secret_ref) {
            errors.push(issue(`/app/${key}`, "secret_ref_missing", "缺少 secret_ref。"));
          }
        }
      }
    }
  }
  if (raw.commit_identity !== undefined) {
    if (
      !isRecord(raw.commit_identity) ||
      typeof raw.commit_identity.name !== "string" ||
      typeof raw.commit_identity.email !== "string"
    ) {
      errors.push(issue("/commit_identity", "type", "commit_identity 需要 name 和 email。"));
    }
  }
}

function buildCatalogForValidation(): SettingsCatalog {
  // 只用能力表与适配器；其余字段由场景 catalog 提供。
  return {
    schema_version: 1,
    models: {
      capabilities: {
        "anthropic/claude-haiku-4-5": { tools: false, reasoning_efforts: ["none", "low", "medium"] },
        "anthropic/claude-sonnet-4-5": { tools: true, reasoning_efforts: ["none", "low", "medium", "high", "max"] },
        "anthropic/claude-sonnet-5": { tools: true, reasoning_efforts: ["low", "medium", "high", "max"] },
        "anthropic/claude-opus-4-1": { tools: true, reasoning_efforts: ["low", "medium", "high", "max"] },
      },
      adapters: {
        "claude-code": { provider_kinds: ["anthropic"], supports_fallback: false },
      },
    },
  };
}

function validateProjectTightening(
  resource: string,
  value: Record<string, unknown>,
  globalValue: unknown | null,
  errors: SettingsIssue[],
): void {
  if (!isRecord(globalValue)) return;
  if (resource === "models") {
    const providers = isRecord(globalValue.providers)
      ? Object.keys(globalValue.providers)
      : [];
    for (const tier of ["utility", "planner", "worker"]) {
      const entry = value[tier];
      if (isRecord(entry) && typeof entry.provider === "string" && !providers.includes(entry.provider)) {
        errors.push(issue(`/${tier}/provider`, "override_not_allowed", "项目不能新增提供方。"));
      }
    }
  } else if (resource === "levels") {
    const global = globalValue;
    if (isRecord(value.none) && isRecord(global.none) && typeof value.none.daily_tokens === "number" && typeof global.none.daily_tokens === "number" && value.none.daily_tokens > global.none.daily_tokens) {
      errors.push(issue("/none/daily_tokens", "override_loosens_global", "只能收紧全局上限。"));
    }
    for (const level of ["low", "medium", "high", "max", "ultimate"]) {
      const entry = value[level];
      if (!isRecord(entry)) continue;
      const globalEntry = global[level];
      if (!isRecord(globalEntry)) continue;
      if (entry.graph_template !== undefined) {
        errors.push(issue(`/${level}/graph_template`, "override_not_allowed", "图模板不可按项目覆盖。"));
      }
      if (isRecord(entry.budget) && isRecord(globalEntry.budget)) {
        compareBudgetTightening(entry.budget, globalEntry.budget, `/${level}/budget`, errors);
      }
    }
  } else if (resource === "policies") {
    const global = globalValue;
    if (isRecord(value.actions) && isRecord(global.actions)) {
      for (const [id, disposition] of Object.entries(value.actions)) {
        const current = global.actions[id];
        if (typeof current !== "string") {
          errors.push(issue(`/actions/${id}`, "override_not_allowed", "不能新增全局未列动作。"));
        } else if (rank(disposition) < rank(current)) {
          errors.push(issue(`/actions/${id}`, "override_loosens_global", "动作处置只能收紧。"));
        }
      }
    }
    if (isRecord(value.paths) && isRecord(global.paths)) {
      for (const [id, rule] of Object.entries(value.paths)) {
        const current = global.paths[id];
        if (current === undefined) {
          if (isRecord(rule) && rule.disposition !== "deny") {
            errors.push(issue(`/paths/${id}`, "override_loosens_global", "新增路径规则必须是 deny。"));
          }
        } else if (isRecord(rule) && isRecord(current) && rank(rule.disposition) < rank(current.disposition)) {
          errors.push(issue(`/paths/${id}/disposition`, "override_loosens_global", "路径处置只能收紧。"));
        }
      }
    }
    if (isRecord(value.approval) && isRecord(global.approval)) {
      if (typeof value.approval.ttl_s === "number" && typeof global.approval.ttl_s === "number" && value.approval.ttl_s > global.approval.ttl_s) {
        errors.push(issue("/approval/ttl_s", "override_loosens_global", "审批有效期只能缩短。"));
      }
    }
    if (isRecord(value.patch) && isRecord(global.patch)) {
      for (const key of ["max_files", "max_bytes", "max_changed_lines", "max_file_bytes"]) {
        if (typeof value.patch[key] === "number" && typeof global.patch[key] === "number" && value.patch[key] > global.patch[key]) {
          errors.push(issue(`/patch/${key}`, "override_loosens_global", "补丁上限只能收紧。"));
        }
      }
    }
  } else if (resource === "guardian") {
    const global = globalValue;
    if (isRecord(value.triage) && isRecord(global.triage)) {
      for (const [id, disposition] of Object.entries(value.triage)) {
        const current = global.triage[id];
        if (typeof current === "string" && rank(disposition) < rank(current)) {
          errors.push(issue(`/triage/${id}`, "override_loosens_global", "分流只能提高审视。"));
        }
      }
    }
    if (isRecord(value.no_progress) && isRecord(global.no_progress)) {
      for (const key of ["same_class_repeats", "stalled_rounds"]) {
        if (typeof value.no_progress[key] === "number" && typeof global.no_progress[key] === "number" && value.no_progress[key] > global.no_progress[key]) {
          errors.push(issue(`/no_progress/${key}`, "override_loosens_global", "阈值只能收紧。"));
        }
      }
    }
  } else if (resource === "sandbox") {
    if (isRecord(value.limits) && isRecord(globalValue.limits)) {
      for (const key of ["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"]) {
        if (typeof value.limits[key] === "number" && typeof globalValue.limits[key] === "number" && value.limits[key] > globalValue.limits[key]) {
          errors.push(issue(`/limits/${key}`, "override_loosens_global", "沙盒限制只能收紧。"));
        }
      }
    }
    if (
      isRecord(value.devcontainer) &&
      value.devcontainer.mode === "off" &&
      isRecord(globalValue.devcontainer) &&
      globalValue.devcontainer.mode === "safe_subset"
    ) {
      errors.push(issue("/devcontainer/mode", "override_loosens_global", "不能放宽 devcontainer 安全模式。"));
    }
  } else if (resource === "egress") {
    if (isRecord(value.profiles) && isRecord(globalValue.profiles)) {
      for (const [name, profile] of Object.entries(value.profiles)) {
        const current = globalValue.profiles[name];
        if (current === undefined) {
          errors.push(issue(`/profiles/${name}`, "override_not_allowed", "项目不能新增出网档案。"));
          continue;
        }
        if (
          !isRecord(current) ||
          !Array.isArray(current.groups) ||
          !isRecord(profile) ||
          !Array.isArray(profile.groups)
        ) {
          continue;
        }
        const currentGroups: unknown[] = current.groups;
        const extra = profile.groups.filter((group) => !currentGroups.includes(group));
        if (extra.length > 0) {
          errors.push(issue(`/profiles/${name}/groups`, "override_loosens_global", "出网分组必须是全局档案的子集。"));
        }
      }
    }
  }
}

function rank(value: unknown): number {
  return typeof value === "string" ? DISPOSITION_RANK[value] ?? -1 : -1;
}

function compareBudgetTightening(
  value: Record<string, unknown>,
  global: Record<string, unknown>,
  pointer: string,
  errors: SettingsIssue[],
): void {
  for (const key of ["nodes", "rounds", "wall_time_s", "concurrency", "forge_writes", "forge_writes_per_hour"]) {
    if (typeof value[key] === "number" && typeof global[key] === "number" && value[key] > global[key]) {
      errors.push(issue(`${pointer}/${key}`, "override_loosens_global", "预算只能收紧。"));
    }
  }
  if (isRecord(value.tokens_by_tier) && isRecord(global.tokens_by_tier)) {
    for (const tier of ["utility", "planner", "worker"]) {
      if (typeof value.tokens_by_tier[tier] === "number" && typeof global.tokens_by_tier[tier] === "number" && value.tokens_by_tier[tier] > global.tokens_by_tier[tier]) {
        errors.push(issue(`${pointer}/tokens_by_tier/${tier}`, "override_loosens_global", "预算只能收紧。"));
      }
    }
  }
}

function computeRelaxes(resource: string, next: unknown, current: unknown): string[] {
  const relaxes: string[] = [];
  if (!isRecord(next) || !isRecord(current)) return relaxes;
  if (resource === "policies") {
    if (isRecord(next.actions) && isRecord(current.actions)) {
      for (const [id, disposition] of Object.entries(next.actions)) {
        const previous = current.actions[id];
        if (typeof previous === "string" && rank(disposition) < rank(previous)) {
          relaxes.push(`actions.${id}`);
        }
      }
    }
    if (isRecord(next.approval) && isRecord(current.approval) && Number(next.approval.ttl_s) > Number(current.approval.ttl_s)) {
      relaxes.push("approval.ttl_s");
    }
    if (isRecord(next.patch) && isRecord(current.patch)) {
      for (const key of ["max_files", "max_bytes", "max_changed_lines", "max_file_bytes"]) {
        if (Number(next.patch[key]) > Number(current.patch[key])) relaxes.push(`patch.${key}`);
      }
    }
  } else if (resource === "levels") {
    for (const level of ["low", "medium", "high", "max", "ultimate"]) {
      const entry = next[level];
      const previous = current[level];
      if (isRecord(entry) && isRecord(previous) && isRecord(entry.budget) && isRecord(previous.budget)) {
        for (const key of ["nodes", "rounds", "wall_time_s", "concurrency", "forge_writes"]) {
          if (Number(entry.budget[key]) > Number(previous.budget[key])) {
            relaxes.push(`${level}.budget.${key}`);
          }
        }
      }
    }
  } else if (resource === "guardian") {
    if (isRecord(next.triage) && isRecord(current.triage)) {
      for (const [id, value] of Object.entries(next.triage)) {
        const previous = current.triage[id];
        if (typeof previous === "string" && rank(value) < rank(previous)) relaxes.push(`triage.${id}`);
      }
    }
    if (Number(next.patrol_interval_s) < Number(current.patrol_interval_s)) {
      relaxes.push("patrol_interval_s");
    }
  } else if (resource === "sandbox") {
    if (next.runtime === "runc" && current.runtime !== "runc") relaxes.push("runtime");
    if (typeof next.base_image === "string" && next.base_image !== current.base_image) relaxes.push("base_image");
  } else if (resource === "egress") {
    if (isRecord(next.profiles) && isRecord(current.profiles)) {
      for (const [name, profile] of Object.entries(next.profiles)) {
        if (!isRecord(profile) || !Array.isArray(profile.groups)) continue;
        const previous = current.profiles[name];
        if (isRecord(previous) && Array.isArray(previous.groups)) {
          const previousGroups: unknown[] = previous.groups;
          if (profile.groups.some((group) => !previousGroups.includes(group))) {
            relaxes.push(`profiles.${name}.groups`);
          }
        }
      }
    }
  } else if (resource === "models") {
    if (isRecord(next.providers) && isRecord(current.providers)) {
      for (const id of Object.keys(next.providers)) {
        if (JSON.stringify(next.providers[id]) !== JSON.stringify(current.providers[id])) {
          relaxes.push(`providers.${id}`);
        }
      }
    }
  } else if (resource === "notify") {
    if (isRecord(next.channels) && isRecord(current.channels) && isRecord(next.channels.desktop) && next.channels.desktop.enabled === false) {
      relaxes.push("channels.desktop.enabled");
    }
  } else if (resource === "github") {
    if (JSON.stringify(next) !== JSON.stringify(current)) relaxes.push("github");
  }
  return relaxes;
}

function applySettingValue(
  scope: "global" | "project",
  projectId: string | undefined,
  resource: string,
  value: unknown,
  current: SettingsDocument,
  secretRefs: string[],
): SettingsDocument {
  const clean = clone(value);
  markSecretsPresent(clean, secretRefs);
  return {
    resource,
    scope,
    ...(scope === "project" && projectId ? { project_id: projectId } : {}),
    rev: current.rev + 1,
    value: clean,
  };
}

function markSecretsPresent(value: unknown, secretRefs: string[]): void {
  if (secretRefs.length === 0 || !isRecord(value)) return;
  if (typeof value.secret_ref === "string" && secretRefs.includes(value.secret_ref)) {
    value.present = true;
  }
  for (const entry of Object.values(value)) {
    if (Array.isArray(entry)) {
      for (const item of entry) markSecretsPresent(item, secretRefs);
    } else if (isRecord(entry)) {
      markSecretsPresent(entry, secretRefs);
    }
  }
}

function ok<T>(body: T, status = 200): ApiResult<T> {
  return { ok: true, status, headers: {}, body };
}

function withHeaders<T>(
  body: T,
  status: number,
  headers: {
    etag?: string;
    x_event_seq?: number;
    idempotent_replayed?: boolean;
  },
): ApiResult<T> {
  return { ok: true, status, headers, body };
}

function problem(status: number, code: string, title: string): ApiResult<never> {
  return {
    ok: false,
    status,
    problem: { code, title, request_id: "req_scenario" },
  };
}

function invalid(message: string): ApiResult<never> {
  return problem(422, "validation_failed", message);
}

function forbiddenOperator(): ApiResult<never> {
  return problem(403, "forbidden_role", "此设备为只读。");
}

function operator(role: "viewer" | "operator" | "approver"): boolean {
  return role !== "viewer";
}


function paginate<T>(items: T[], raw: Record<string, unknown>): ApiList<T> {
  const query = raw as PageQuery;
  const limit = Math.min(200, Math.max(1, Number(query.limit) || 50));
  const offset = Number(query.cursor) || 0;
  const slice = items.slice(offset, offset + limit);
  const next = offset + limit < items.length ? String(offset + limit) : null;
  return { items: slice, next_cursor: next };
}

/** messages 默认 desc：从最新一页开始，cursor 指向更早的分页位置。 */
function pageWindow<T>(
  items: T[],
  raw: Record<string, unknown>,
  defaultOrder: "asc" | "desc",
): ApiList<T> {
  const query = raw as PageQuery;
  const limit = Math.min(200, Math.max(1, Number(query.limit) || 50));
  const offset = Number(query.cursor) || 0;
  const order = query.order ?? defaultOrder;
  if (order === "asc") {
    const slice = items.slice(offset, offset + limit);
    return {
      items: slice,
      next_cursor: offset + limit < items.length ? String(offset + limit) : null,
    };
  }
  const end = Math.max(0, items.length - offset);
  const start = Math.max(0, end - limit);
  return {
    items: items.slice(start, end).reverse(),
    next_cursor: start > 0 ? String(offset + limit) : null,
  };
}

function effectiveDocument(
  data: ScenarioData,
  projectId: string,
  resource: string,
): SettingsDocument {
  const global = data.settings[resource];
  const local = data.projectSettings[projectId]?.[resource];
  if (!global) {
    return {
      resource,
      scope: "effective",
      project_id: projectId,
      rev: 0,
      value: null,
      clamped: [],
    };
  }
  if (!local || local.value === null) {
    return {
      ...clone(global),
      scope: "effective",
      project_id: projectId,
      clamped: [],
    };
  }
  if (!isRecord(global.value) || !isRecord(local.value)) {
    return {
      ...clone(global),
      scope: "effective",
      project_id: projectId,
      clamped: [],
    };
  }
  const clamped: string[] = [];
  const value = effectiveValue(resource, global.value, local.value, clamped);
  return {
    resource,
    scope: "effective",
    project_id: projectId,
    rev: global.rev,
    value,
    clamped,
  };
}

function effectiveValue(
  resource: string,
  global: Record<string, unknown>,
  local: Record<string, unknown>,
  clamped: string[],
): Record<string, unknown> {
  if (resource === "notify" || resource === "github") return clone(global);
  const merged = deepMerge(global, local);
  if (resource === "levels" && isRecord(local.none) && isRecord(global.none)) {
    if (
      typeof local.none.daily_tokens === "number" &&
      typeof global.none.daily_tokens === "number" &&
      local.none.daily_tokens > global.none.daily_tokens
    ) {
      (merged.none as Record<string, unknown>).daily_tokens = global.none.daily_tokens;
      clamped.push("none.daily_tokens");
    }
    for (const level of ["low", "medium", "high", "max", "ultimate"]) {
      const localEntry = local[level];
      const globalEntry = global[level];
      if (!isRecord(localEntry) || !isRecord(globalEntry)) continue;
      if (!isRecord(localEntry.budget) || !isRecord(globalEntry.budget)) continue;
      const target = merged[level];
      if (!isRecord(target) || !isRecord(target.budget)) continue;
      for (const key of ["nodes", "rounds", "wall_time_s", "concurrency", "forge_writes", "forge_writes_per_hour"]) {
        if (
          typeof localEntry.budget[key] === "number" &&
          typeof globalEntry.budget[key] === "number" &&
          localEntry.budget[key] > globalEntry.budget[key]
        ) {
          target.budget[key] = globalEntry.budget[key];
          clamped.push(`${level}.budget.${key}`);
        }
      }
      if (isRecord(localEntry.budget.tokens_by_tier) && isRecord(globalEntry.budget.tokens_by_tier)) {
        for (const tier of ["utility", "planner", "worker"]) {
          const localTokens = localEntry.budget.tokens_by_tier[tier];
          const globalTokens = globalEntry.budget.tokens_by_tier[tier];
          if (typeof localTokens === "number" && typeof globalTokens === "number" && localTokens > globalTokens) {
            const targetTokens = target.budget.tokens_by_tier as Record<string, unknown>;
            targetTokens[tier] = globalTokens;
            clamped.push(`${level}.budget.tokens_by_tier.${tier}`);
          }
        }
      }
    }
  }
  if (resource === "policies") {
    if (isRecord(local.actions) && isRecord(global.actions)) {
      const target = merged.actions as Record<string, unknown>;
      for (const [id, disposition] of Object.entries(local.actions)) {
        const current = global.actions[id];
        if (typeof current === "string" && rank(disposition) < rank(current)) {
          target[id] = current;
          clamped.push(`actions.${id}`);
        }
      }
    }
    if (isRecord(local.approval) && isRecord(global.approval)) {
      const localTtl = local.approval.ttl_s;
      const globalTtl = global.approval.ttl_s;
      if (typeof localTtl === "number" && typeof globalTtl === "number" && localTtl > globalTtl) {
        (merged.approval as Record<string, unknown>).ttl_s = globalTtl;
        clamped.push("approval.ttl_s");
      }
    }
    if (isRecord(local.patch) && isRecord(global.patch)) {
      const target = merged.patch as Record<string, unknown>;
      for (const key of ["max_files", "max_bytes", "max_changed_lines", "max_file_bytes"]) {
        const localValue = local.patch[key];
        const globalValue = global.patch[key];
        if (typeof localValue === "number" && typeof globalValue === "number" && localValue > globalValue) {
          target[key] = globalValue;
          clamped.push(`patch.${key}`);
        }
      }
    }
    if (isRecord(local.paths) && isRecord(global.paths)) {
      const target = merged.paths as Record<string, unknown>;
      for (const [id, rule] of Object.entries(local.paths)) {
        const current = global.paths[id];
        if (isRecord(current) && isRecord(rule) && rank(rule.disposition) < rank(current.disposition)) {
          target[id] = clone(current);
          clamped.push(`paths.${id}`);
        }
      }
    }
  }
  if (resource === "guardian") {
    if (isRecord(local.triage) && isRecord(global.triage)) {
      const target = merged.triage as Record<string, unknown>;
      for (const [id, disposition] of Object.entries(local.triage)) {
        const current = global.triage[id];
        if (typeof current === "string" && rank(disposition) < rank(current)) {
          target[id] = current;
          clamped.push(`triage.${id}`);
        }
      }
    }
    if (isRecord(local.no_progress) && isRecord(global.no_progress)) {
      const target = merged.no_progress as Record<string, unknown>;
      for (const key of ["same_class_repeats", "stalled_rounds"]) {
        const localValue = local.no_progress[key];
        const globalValue = global.no_progress[key];
        if (typeof localValue === "number" && typeof globalValue === "number" && localValue > globalValue) {
          target[key] = globalValue;
          clamped.push(`no_progress.${key}`);
        }
      }
    }
  }
  if (resource === "sandbox" && isRecord(local.limits) && isRecord(global.limits)) {
    const target = merged.limits as Record<string, unknown>;
    for (const key of ["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"]) {
      const localValue = local.limits[key];
      const globalValue = global.limits[key];
      if (typeof localValue === "number" && typeof globalValue === "number" && localValue > globalValue) {
        target[key] = globalValue;
        clamped.push(`limits.${key}`);
      }
    }
  }
  if (resource === "egress" && isRecord(local.profiles) && isRecord(global.profiles)) {
    const target = merged.profiles as Record<string, unknown>;
    for (const [name, profile] of Object.entries(local.profiles)) {
      const current = global.profiles[name];
      if (!isRecord(profile) || !Array.isArray(profile.groups) || !isRecord(current) || !Array.isArray(current.groups)) {
        continue;
      }
      const currentGroups: unknown[] = current.groups;
      target[name] = {
        ...(isRecord(target[name]) ? target[name] : {}),
        groups: profile.groups.filter((group) => currentGroups.includes(group)),
      };
      if (profile.groups.some((group) => !currentGroups.includes(group))) {
        clamped.push(`profiles.${name}.groups`);
      }
    }
  }
  return merged;
}

function deepMerge(
  base: Record<string, unknown>,
  patch: Record<string, unknown>,
): Record<string, unknown> {
  const result: Record<string, unknown> = clone(base);
  for (const [key, value] of Object.entries(patch)) {
    const previous = result[key];
    if (isRecord(previous) && isRecord(value)) {
      result[key] = deepMerge(previous, value);
    } else if (Array.isArray(value)) {
      result[key] = clone(value);
    } else if (isRecord(value)) {
      result[key] = deepMerge({}, value);
    } else {
      result[key] = clone(value);
    }
  }
  return result;
}

export type { ScenarioData };
export {
  SCENARIO_PR9_SHA,
  SCENARIO_PROJECT_COUNTS,
  DEFAULT_PROJECT_UI,
  DEFAULT_PREFS,
};
