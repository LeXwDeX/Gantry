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
  BudgetLedger,
  Delivery,
  EventEnvelope,
  InboxItem,
  Level,
  Message,
  Project,
  SettingsDocument,
} from "@/api/types";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import {
  DEFAULT_LEVELS,
  DEFAULT_PREFS,
  DEFAULT_PROJECT_UI,
  SCENARIO_PR9_SHA,
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

/** 开发场景替身：只在显式 `--mode scenario` 下由工厂动态导入。 */
export class ScenarioIpcAdapter implements IpcAdapter {
  readonly scenario: ScenarioName;
  readonly isScenario = true;
  readonly externalCalls: string[] = [];
  readonly githubCalls: OpenGithubArgs[] = [];

  private data: ScenarioData;
  private acked: number | null = null;
  private prefs: Prefs = clone(DEFAULT_PREFS);
  private uiStates = new Map<string, ProjectUiState>();
  private listener: ((frame: StreamFrame) => void) | null = null;
  private seq = 18_240;
  private errorInjected = false;

  constructor(scenario: ScenarioName = "default") {
    this.scenario = scenario;
    this.data = buildScenarioData(scenario);
  }

  async identityStatus(): Promise<IdentityStatus> {
    return clone(this.data.identity);
  }

  async pairingBegin(_a: {
    pairingString: string;
    deviceName: string;
  }): Promise<{ confirmationCode: string; device: import("@/api/types").Device }> {
    void _a;
    throw new IpcError(
      "shell.key_unavailable",
      "示例模式不生成配对密钥或证书；请使用真实外壳完成配对。",
    );
  }

  async pairingConfirm(_a: { matches: boolean }): Promise<IdentityStatus> {
    void _a;
    throw new IpcError(
      "shell.key_unavailable",
      "示例模式没有待确认的配对请求；请使用真实外壳完成配对。",
    );
  }

  async identityReset(): Promise<IdentityStatus> {
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

  async guardedWritePrepare(_a: GuardedWriteArgs): Promise<GuardedWritePreview> {
    void _a;
    return {
      pending_id: "pending_scenario",
      valid: true,
      errors: [],
      warnings: [
        {
          pointer: "",
          code: "scenario_mode",
          message: "示例模式不提交、不保存；受保护写入需要真实外壳签名。",
        },
      ],
      relaxes: [],
      required_role: "operator",
      requires_signature: false,
      preview: { mode: "scenario", note: "示例模式：未连接真实外壳，不能提交。" },
      can_submit: { ok: false, reason: "not_connected" },
    };
  }

  async guardedWriteSubmit(_a: {
    pendingId: string;
  }): Promise<ApiResult<unknown>> {
    void _a;
    throw new IpcError(
      "shell.key_unavailable",
      "示例模式没有审批签名密钥；受保护写入不会提交，rev 不变。",
    );
  }

  async guardedWriteDiscard(_a: { pendingId: string }): Promise<void> {
    void _a;
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

  private async handle(route: RouteId, raw: unknown): Promise<ApiResult<unknown>> {
    const args = (raw ?? {}) as Record<string, unknown>;
    if (this.scenario === "error" && route === "me.get" && !this.errorInjected) {
      this.errorInjected = true;
      return problem(500, "internal", "Scenario injected error.");
    }
    if (this.role() === "viewer" && route.startsWith("settings.")) {
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
        this.syncDeliveryInbox(delivery);
        this.emitEvent("delivery.abandoned", delivery.id, delivery.project_id, {
          delivery_id: delivery.id,
        });
        return ok(delivery);
      }
      case "deliveries.budget": {
        const delivery = this.data.deliveries.find((d) => d.id === args.deliveryId);
        if (!delivery) return problem(404, "not_found", "Delivery not found.");
        const ledger: BudgetLedger = {
          id: "bdg_scenario",
          delivery_id: delivery.id,
          subject: "delivery",
          template: `${delivery.level_cap.toLowerCase()}.scenario`,
          limits: { nodes: 40, rounds: 4, wall_time_s: 10800 },
          used: { nodes: 4, attempts: 5, rounds: 1, wall_time_s: 940 },
        };
        return ok(ledger);
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
          | { text?: string; level_cap?: Level; refs?: Message["refs"] }
          | undefined;
        const text = body?.text?.trim();
        if (!text) return invalid("text 不能为空");
        const message: Message = {
          id: `msg_scenario_${this.seq + 1}`,
          conversation_id: conversationId,
          role: "user",
          agent_kind: null,
          tone: null,
          text,
          level_cap: body?.level_cap ?? null,
          refs: body?.refs ?? [],
          cards: [],
          delivery_id: null,
        };
        this.data.messages[project.project_id] = [
          ...(this.data.messages[project.project_id] ?? []),
          message,
        ];
        this.emitEvent("message.posted", message.id, project.project_id, {
          message_id: message.id,
        });
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
        return item ? ok(item) : problem(404, "not_found", "Workflow not found.");
      }
      case "attempts.list":
        return ok({ items: [], next_cursor: null });
      case "attempts.get":
        return problem(404, "not_found", "Attempt not found.");
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
        this.emitEvent("approval.denied", item.id, null, {
          approval_id: item.id,
          reason: args.reason,
        });
        return ok(item);
      }
      case "settings.get": {
        if (!operator(this.role())) return forbiddenOperator();
        const doc = this.data.settings[String(args.resource)];
        return doc
          ? withHeaders(clone(doc), 200, {
              etag: `"${doc.rev}"`,
              x_event_seq: doc.rev,
            })
          : problem(404, "not_found", "Settings resource not found.");
      }
      case "project_settings.get": {
        if (!operator(this.role())) return forbiddenOperator();
        const byProject = this.data.projectSettings[String(args.projectId)];
        const doc = byProject?.[String(args.resource)];
        if (!doc) return problem(404, "not_found", "Settings resource not found.");
        const value =
          args.view === "effective"
            ? effectiveDocument(this.data, String(args.projectId), String(args.resource))
            : doc;
        return withHeaders(clone(value), 200, {
          etag: `"${value.rev}"`,
          x_event_seq: value.rev,
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
  ): void {
    this.seq += 1;
    const envelope: EventEnvelope = {
      id: `evt_scenario_${this.seq}`,
      seq: this.seq,
      ts: SCENARIO_SERVER_TIME,
      type,
      subject,
      project_id: projectId,
      actor: { kind: "device", id: this.data.me.device.id },
      causation_id: null,
      correlation_id: projectId,
      data,
    };
    this.emit({ t: "event", envelope });
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

function ok<T>(body: T, status = 200): ApiResult<T> {
  return { ok: true, status, headers: {}, body };
}

function withHeaders<T>(
  body: T,
  status: number,
  headers: { etag?: string; x_event_seq?: number },
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
  const local = data.projectSettings[projectId]?.[resource];
  if (local && local.value !== null) return local;
  if (resource === "levels") {
    const global = data.settings.levels;
    return {
      ...global,
      scope: "effective",
      project_id: projectId,
      value: { default_level: DEFAULT_LEVELS[projectId] ?? "Medium" },
      clamped: [],
    };
  }
  const global = data.settings[resource];
  if (global) {
    return { ...global, scope: "effective", project_id: projectId, clamped: [] };
  }
  return {
    resource,
    scope: "effective",
    project_id: projectId,
    rev: 0,
    value: null,
  };
}

export type { ScenarioData };
export {
  SCENARIO_PR9_SHA,
  SCENARIO_PROJECT_COUNTS,
  DEFAULT_PROJECT_UI,
  DEFAULT_PREFS,
};
