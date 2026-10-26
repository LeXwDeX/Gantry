import {
  IpcError,
  type ApiResult,
  type ApprovalSheetModel,
  type ConnectionState,
  type IdentityStatus,
  type IpcAdapter,
  type OutboxItem,
  type ProjectUiState,
  type Selection,
  type StreamFrame,
} from "@/api/shell";
import type {
  Approval,
  Attempt,
  BudgetLedger,
  Conversation,
  Delivery,
  InboxItem,
  InboxSummary,
  Level,
  Me,
  Message,
  Project,
  Workflow,
  Worktree,
} from "@/api/types";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import { xEventSeq } from "@/api/result";
import { errorMessage } from "@/i18n";
import { defaultProjectUi, mergeProjectUi } from "./ui";
import { emptyProjectData, initialState, type DesktopState, type ProjectData } from "./state";
import { needsYou, type NeedsYouProjection } from "./needsYou";
import { buildSend, canQueueSend } from "./send";
import {
  isSnapshotStale,
  isBelowWatermark,
  maxEventSeq,
  minEventSeq,
} from "@/sync/watermark";
import {
  isKnownEventType,
  resourcesForEvent,
  type ResourceKey,
} from "@/sync/events";
import { RefreshScheduler } from "@/sync/refreshScheduler";
import { fetchAllPages, mapLimit } from "@/sync/paging";
import {
  latestWorkflow,
  mergeDeliveries,
  mergeInbox,
  mergeMessages,
  mergeWorktrees,
} from "@/sync/collections";
import {
  approvalStatusLabel,
  isPrApproval,
  mergeGateIssues,
  signAvailability as computeSignAvailability,
  signBlockText as formatSignBlock,
  shortSha,
  type SignAvailability,
  type SignAvailabilityInput,
} from "./sign";
import type { Tone } from "@/theme/tone";
import {
  canAct,
  type Action,
} from "./authz";

export interface DesktopControllerOptions {
  refreshDelayMs?: number;
  persistDelayMs?: number;
  retryDelaysMs?: number[];
  now?: () => number;
  prefetchWorkflows?: boolean;
}

type HydrateReason = "start" | "refresh" | "resync";

type ProjectSnapshot = {
  seqs: number[];
};

const DEFAULT_RETRY_DELAYS = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000];

/**
 * 框架无关的桌面状态控制器。
 * `subscribe` / `getSnapshot` 是稳定函数，可直接交给 useSyncExternalStore。
 */
export class DesktopController {
  readonly ipc: IpcAdapter;
  private readonly options: Required<DesktopControllerOptions>;
  private state: DesktopState = initialState();
  private readonly listeners = new Set<() => void>();

  private running = false;
  private generation = 0;
  private streamToken = 0;
  private appliedSeq = 0;
  private logEpoch: number | null = null;
  private clockOffsetMs = 0;
  private catchUp: { epoch: number; count: number } | null = null;
  private resyncing = false;
  private terminal = false;

  private readonly scheduler: RefreshScheduler;
  private readonly persistTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly retryTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly retryAttempts = new Map<string, number>();
  private readonly flushing = new Map<string, Promise<void>>();
  private readonly loadingProjects = new Map<string, Promise<void>>();
  private readonly loadedProjects = new Set<string>();
  /** 身份代际：重置身份后，旧代际的待发项必须由用户显式处理。 */
  private identityEpoch = 0;
  /** 成功重置时的代际阈值：更低代际的旧身份项不得写回存储。 */
  private isolatedOutboxBeforeEpoch = 0;
  private identityFingerprint: string | null = null;
  private readonly outboxIdentity = new Map<string, number>();
  private resetPending = false;

  constructor(ipc: IpcAdapter, options: DesktopControllerOptions = {}) {
    this.ipc = ipc;
    this.options = {
      refreshDelayMs: options.refreshDelayMs ?? 100,
      persistDelayMs: options.persistDelayMs ?? 500,
      retryDelaysMs: options.retryDelaysMs ?? DEFAULT_RETRY_DELAYS,
      now: options.now ?? (() => Date.now()),
      prefetchWorkflows: options.prefetchWorkflows ?? true,
    };
    this.scheduler = new RefreshScheduler({
      delayMs: this.options.refreshDelayMs,
      concurrency: 4,
      run: (key) => this.runRefresh(key),
    });
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): DesktopState => this.state;

  // ---------------------------------------------------------------- 生命周期

  async start(): Promise<void> {
    if (this.running) return;
    this.running = true;
    this.terminal = false;
    this.scheduler.reset();
    this.setState((state) => ({ ...state, status: "loading", error: null }));
    await this.bootstrap();
  }

  async stop(): Promise<void> {
    this.running = false;
    this.generation += 1;
    this.streamToken += 1;
    this.scheduler.dispose();
    for (const timer of this.persistTimers.values()) clearTimeout(timer);
    this.persistTimers.clear();
    for (const timer of this.retryTimers.values()) clearTimeout(timer);
    this.retryTimers.clear();
    await this.flushPersist();
    try {
      await this.ipc.eventsClose();
    } catch {
      // 关闭失败不影响卸载。
    }
  }

  private async bootstrap(): Promise<void> {
    try {
      const identity = await this.ipc.identityStatus();
      if (!this.running) return;
      const connection = await this.ipc.connectionState();
      if (!this.running) return;
      this.noteIdentity(identity);
      this.logEpoch = connection.log_epoch;
      this.setState((state) => ({ ...state, identity, connection }));
      if (this.identityBlocked(identity.state) || this.connectionBlocked(connection.phase)) {
        if (this.isTerminalPhase(connection.phase)) this.enterTerminalState();
        else this.setState((state) => ({ ...state, status: "unpaired" }));
        return;
      }
      // 任何终态之后重新配对都会使旧队列失效；即使身份指纹相同也不再自动发送。
      if (this.terminal) this.breakOutboxIdentity();
      this.terminal = false;
      await this.hydrate("start");
    } catch (error) {
      if (!this.running) return;
      this.setState((state) => ({
        ...state,
        status: "error",
        error: errorMessageFrom(error),
      }));
    }
  }

  private identityBlocked(state: string): boolean {
    return state !== "paired";
  }

  private connectionBlocked(phase: ConnectionState["phase"]): boolean {
    return (
      phase === "revoked" ||
      phase === "cert_expired" ||
      phase === "unpaired" ||
      phase === "handshake_failed"
    );
  }

  private isTerminalPhase(
    phase: ConnectionState["phase"] | null | undefined,
  ): boolean {
    return phase === "revoked" || phase === "cert_expired";
  }

  private isTerminalConnection(): boolean {
    return this.terminal || this.isTerminalPhase(this.state.connection?.phase);
  }

  /**
   * 吊销 / 证书过期是不可恢复的终态：立即停流、停刷新与发送，清实体缓存；
   * 保留身份、界面状态和待发队列，重新配对后仍可使用。
   */
  private enterTerminalState(): void {
    if (this.terminal) return;
    this.breakOutboxIdentity();
    this.terminal = true;
    this.generation += 1;
    this.streamToken += 1;
    this.scheduler.clear();
    this.appliedSeq = 0;
    this.catchUp = null;
    this.resyncing = false;
    this.loadedProjects.clear();
    for (const timer of this.persistTimers.values()) clearTimeout(timer);
    this.persistTimers.clear();
    for (const timer of this.retryTimers.values()) clearTimeout(timer);
    this.retryTimers.clear();
    this.retryAttempts.clear();
    this.setState((state) => ({
      ...state,
      status: "unpaired",
      error: null,
      me: null,
      projects: [],
      currentProjectId: null,
      projectData: {},
      inbox: [],
      inboxSummary: null,
      approvals: {},
      workflows: {},
      attemptsByNode: {},
      budgetsByDelivery: {},
      notice: null,
      resyncing: false,
      lastResyncReason: null,
      ui: requeueSending(state.ui),
    }));
    void this.ipc.eventsClose().catch(() => undefined);
  }

  /**
   * 身份重置必须经过控制器：外壳删除本机身份后，旧身份的待发队列
   * 保留到用户明确丢弃或编辑，不会借新身份自动发送。
   */
  async resetIdentity(): Promise<IdentityStatus> {
    // 外壳清理期间身份仍是 paired；暂停持久化，避免定时器把旧草稿写回。
    this.resetPending = true;
    try {
      const identity = await this.ipc.identityReset();
      // 先接受已成功的重置，避免连接回读失败后以旧身份回写界面。
      this.noteIdentity(identity);
      // 重置成功后现有队列都属于旧身份；即使指纹未变也先定代际并抬高隔离阈值。
      this.breakOutboxIdentity();
      this.isolatedOutboxBeforeEpoch = this.identityEpoch;
      this.setState((state) => ({
        ...state,
        identity,
        ui: Object.fromEntries(
          Object.entries(state.ui).map(([projectId, ui]) => [
            projectId,
            { ...defaultProjectUi(), outbox: ui.outbox },
          ]),
        ),
      }));
      // 外壳重置后连接已断开；state 帧可能随旧事件流一起消失，因此直接回读真实状态。
      const connection = await this.ipc.connectionState();
      this.setState((state) => ({ ...state, connection }));
      return identity;
    } finally {
      this.resetPending = false;
    }
  }

  /** 旧身份遗留、当前不会自动发送的待发项数量。 */
  staleOutboxCount(): number {
    return Object.values(this.state.ui).reduce(
      (count, ui) => count + ui.outbox.filter((item) => this.isOutboxStale(item)).length,
      0,
    );
  }

  /** 用户明确丢弃旧身份的待发队列；返回删除项数。 */
  discardStaleOutbox(): number {
    let removed = 0;
    for (const [projectId, ui] of Object.entries(this.state.ui)) {
      const keep = ui.outbox.filter((item) => {
        if (!this.isOutboxStale(item)) return true;
        this.outboxIdentity.delete(item.local_id);
        removed += 1;
        return false;
      });
      if (keep.length !== ui.outbox.length) {
        this.updateUi({ outbox: keep }, projectId);
      }
    }
    return removed;
  }

  isOutboxStale(item: OutboxItem): boolean {
    const epoch = this.outboxIdentity.get(item.local_id);
    return epoch !== undefined && epoch < this.identityEpoch;
  }

  private isOutboxSendable(item: OutboxItem): boolean {
    return !this.isOutboxStale(item);
  }

  /** 已成功重置的旧身份项：只留在内存，终态也不能写回存储。 */
  private isOutboxIsolated(item: OutboxItem): boolean {
    const epoch = this.outboxIdentity.get(item.local_id);
    return epoch !== undefined && epoch < this.isolatedOutboxBeforeEpoch;
  }

  private noteIdentity(identity: IdentityStatus): void {
    const fingerprint = `${identity.state}|${identity.server_host ?? ""}|${
      identity.device?.id ?? ""
    }`;
    if (this.identityFingerprint !== null && this.identityFingerprint !== fingerprint) {
      this.breakOutboxIdentity();
    }
    this.identityFingerprint = fingerprint;
  }

  /** 记录现有待发项所属身份，然后开启新一代身份。 */
  private breakOutboxIdentity(): void {
    for (const ui of Object.values(this.state.ui)) {
      for (const item of ui.outbox) {
        if (!this.outboxIdentity.has(item.local_id)) {
          this.outboxIdentity.set(item.local_id, this.identityEpoch);
        }
      }
    }
    this.identityEpoch += 1;
  }

  // ---------------------------------------------------------------- 选择与界面状态

  selectProject(projectId: string): void {
    if (!this.state.projects.some((project) => project.id === projectId)) return;
    if (this.state.currentProjectId === projectId) return;
    const previous = this.state.currentProjectId;
    if (previous) this.queuePersist(previous);
    this.setState((state) => ({
      ...state,
      currentProjectId: projectId,
      ui: state.ui[projectId]
        ? state.ui
        : { ...state.ui, [projectId]: defaultProjectUi() },
    }));
    void this.ensureProjectLoaded(projectId);
  }

  updateUi(patch: Partial<ProjectUiState>, projectId?: string): void {
    const target = projectId ?? this.state.currentProjectId;
    if (!target) return;
    const current = this.state.ui[target] ?? defaultProjectUi();
    const next = mergeProjectUi(current, patch);
    this.setState((state) => ({
      ...state,
      ui: { ...state.ui, [target]: next },
    }));
    this.queuePersist(target);
  }

  private uiFor(projectId: string): ProjectUiState {
    return this.state.ui[projectId] ?? defaultProjectUi();
  }

  private dataFor(projectId: string): ProjectData {
    return this.state.projectData[projectId] ?? emptyProjectData();
  }

  private setProjectData(projectId: string, next: ProjectData): void {
    this.setState((state) => ({
      ...state,
      projectData: { ...state.projectData, [projectId]: next },
    }));
  }

  notify(message: string | null): void {
    this.setState((state) => ({ ...state, notice: message }));
  }

  // ---------------------------------------------------------------- 水合

  private async hydrate(reason: HydrateReason, focusProjectId?: string): Promise<void> {
    const gen = ++this.generation;
    this.scheduler.clear();
    if (reason === "start") {
      this.setState((state) => ({ ...state, status: "loading", resyncing: false }));
    } else if (reason === "resync") {
      this.setState((state) => ({ ...state, resyncing: true }));
    }

    try {
      const meResult = await this.read("me.get", {});
      if (gen !== this.generation) return;
      if (!meResult.ok) {
        if (reason === "refresh" && this.state.status === "ready") {
          this.notify(failureText(meResult));
          return;
        }
        throw new IpcError(problemCodeOf(meResult), failureText(meResult));
      }
      const me = meResult.body as Me;
      this.clockOffsetMs = Date.parse(me.server.time) - this.options.now();
      if (
        reason !== "resync" &&
        this.logEpoch !== null &&
        me.server.log_epoch !== this.logEpoch
      ) {
        this.logEpoch = me.server.log_epoch;
        this.scheduleResync("log_epoch_changed");
        return;
      }
      this.logEpoch = me.server.log_epoch;

      const projectsResult = await this.fetchAllPages<Project>((cursor) =>
        this.read("projects.list", { cursor, limit: 200 }),
      );
      if (gen !== this.generation) return;
      if (projectsResult.error) {
        throw new IpcError(
          problemCodeOf(projectsResult.error),
          failureText(projectsResult.error),
        );
      }
      const projects = dedupeById(projectsResult.items);
      const seqs: number[] = [];
      for (const page of projectsResult.pages) {
        const seq = xEventSeq(page);
        if (seq !== null) seqs.push(seq);
      }

      let ui = this.state.ui;
      try {
        const stored = await this.ipc.uiStateList();
        if (gen !== this.generation) return;
        ui = mergeStoredUi(stored, this.state.ui);
      } catch {
        if (gen !== this.generation) return;
        ui = this.state.ui;
      }

      // 读取界面状态期间用户可能已切换项目：按最新选择解析，避免旧响应把它改回去。
      const currentProjectId = this.resolveProjectId(projects, focusProjectId);
      this.setState((state) => ({
        ...state,
        me,
        projects,
        currentProjectId,
        ui: ensureUi(ui, projects, currentProjectId),
      }));

      this.loadedProjects.clear();
      if (currentProjectId) {
        const snapshot = await this.hydrateProject(gen, currentProjectId);
        if (gen !== this.generation) return;
        seqs.push(...snapshot.seqs);
        this.loadedProjects.add(currentProjectId);
      }

      const inboxResult = await this.fetchAllPages<InboxItem>((cursor) =>
        this.read("inbox.list", { status: "open", cursor, limit: 200 }),
      );
      if (gen !== this.generation) return;
      let inbox = this.state.inbox;
      if (!inboxResult.error) {
        inbox = dedupeById(inboxResult.items);
        for (const page of inboxResult.pages) {
          const seq = xEventSeq(page);
          if (seq !== null) seqs.push(seq);
        }
      } else if (reason === "start") {
        this.notify(failureText(inboxResult.error));
      }

      const summaryResult = await this.read("inbox.summary", {});
      if (gen !== this.generation) return;
      let inboxSummary = this.state.inboxSummary;
      if (summaryResult.ok) {
        inboxSummary = summaryResult.body as InboxSummary;
        const seq = xEventSeq(summaryResult);
        if (seq !== null) seqs.push(seq);
      }

      this.setState((state) => ({
        ...state,
        inbox,
        inboxSummary,
        status: "ready",
        error: null,
        resyncing: false,
        lastResyncReason: reason === "resync" ? state.lastResyncReason : null,
      }));

      const afterSeq = minEventSeq(seqs);
      await this.openStream(gen, afterSeq ?? undefined);
      if (gen !== this.generation) return;
      if (this.state.connection?.phase === "live") {
        void this.flushAllOutboxes();
      }
      if (currentProjectId) void this.prefetchWorkflows(gen, currentProjectId);
    } catch (error) {
      if (gen !== this.generation) return;
      if (reason === "refresh" && this.state.status === "ready") {
        this.notify(errorMessageFrom(error));
        this.setState((state) => ({ ...state, resyncing: false }));
        return;
      }
      this.setState((state) => ({
        ...state,
        status: "error",
        error: errorMessageFrom(error),
        resyncing: false,
      }));
    }
  }

  private resolveProjectId(projects: Project[], focus?: string): string | null {
    if (focus && projects.some((project) => project.id === focus)) return focus;
    const current = this.state.currentProjectId;
    if (current && projects.some((project) => project.id === current)) return current;
    return projects[0]?.id ?? null;
  }

  /** 读取一个项目快照；结果按 gen 丢弃。 */
  private async hydrateProject(
    gen: number,
    projectId: string,
  ): Promise<ProjectSnapshot> {
    const seqs: number[] = [];
    const previous = this.dataFor(projectId);
    // 读取期间发送成功的消息已确认；提交较早快照时不能把它们覆盖掉。
    const startedMessageIds = new Set(previous.messages.map((m) => m.id));
    const local: {
      deliveries?: Delivery[];
      worktrees?: Worktree[];
      defaultLevel?: Level | null;
      asOf: Partial<ProjectData["asOf"]>;
    } = { asOf: {} };

    const conversations = await this.read("conversations.list", {
      projectId,
      limit: 50,
    });
    if (gen !== this.generation) return { seqs };
    let conversation: Conversation | null = previous.conversation;
    if (conversations.ok) {
      conversation = conversations.body?.items[0] ?? null;
      const seq = xEventSeq(conversations);
      if (seq !== null) {
        seqs.push(seq);
        local.asOf.conversation = seq;
      }
    }

    let messages = previous.messages;
    let messageCursor = previous.messageCursor;
    if (conversations.ok && conversation) {
      const result = await this.read("messages.list", {
        conversationId: conversation.id,
        limit: 50,
        order: "desc",
      });
      if (gen !== this.generation) return { seqs };
      if (result.ok) {
        const list = result.body;
        messages = list ? [...list.items].reverse() : [];
        messageCursor = list?.next_cursor ?? null;
        const seq = xEventSeq(result);
        if (seq !== null) {
          seqs.push(seq);
          local.asOf.messages = seq;
        }
      }
    }

    const role = this.state.me?.device.role;
    const canReadSettings = role === "operator" || role === "approver";
    const tasks: Array<() => Promise<void>> = [
      async () => {
        const [active, done] = await Promise.all([
          this.read("deliveries.list", {
            projectId,
            active: true,
            limit: 200,
          }),
          this.read("deliveries.list", {
            projectId,
            stage: ["closed", "abandoned"],
            limit: 10,
          }),
        ]);
        if (gen !== this.generation) return;
        const items: Delivery[] = [];
        const seqCandidates: Array<number | null> = [];
        if (active.ok) {
          items.push(...(active.body?.items ?? []));
          seqCandidates.push(xEventSeq(active));
        }
        if (done.ok) {
          items.push(...(done.body?.items ?? []));
          seqCandidates.push(xEventSeq(done));
        }
        local.deliveries = mergeDeliveries(previous.deliveries, dedupeById(items));
        const seq = minEventSeq(seqCandidates);
        if (seq !== null) {
          seqs.push(seq);
          local.asOf.deliveries = seq;
        }
      },
      async () => {
        const result = await this.read("worktrees.list", {
          projectId,
          limit: 200,
        });
        if (gen !== this.generation) return;
        if (!result.ok) return;
        local.worktrees = mergeWorktrees(
          previous.worktrees,
          result.body?.items ?? [],
        );
        const seq = xEventSeq(result);
        if (seq !== null) {
          seqs.push(seq);
          local.asOf.worktrees = seq;
        }
      },
    ];

    if (canReadSettings) {
      tasks.push(async () => {
        const result = await this.read("project_settings.get", {
          projectId,
          resource: "levels",
          view: "effective",
        });
        if (gen !== this.generation || !result.ok) return;
        local.defaultLevel = readDefaultLevel(result.body?.value);
        const seq = xEventSeq(result);
        if (seq !== null) {
          seqs.push(seq);
          local.asOf.settings = seq;
        }
      });
    }

    await mapLimit(tasks, 4, async (task) => {
      await task();
    });
    if (gen !== this.generation) return { seqs };

    const assembled: ProjectData = {
      conversation,
      messages,
      messageCursor,
      deliveries: local.deliveries ?? previous.deliveries,
      worktrees: local.worktrees ?? previous.worktrees,
      defaultLevel:
        local.defaultLevel !== undefined
          ? local.defaultLevel
          : previous.defaultLevel,
      asOf: {
        conversation:
          local.asOf.conversation ?? previous.asOf.conversation,
        messages: local.asOf.messages ?? previous.asOf.messages,
        deliveries: local.asOf.deliveries ?? previous.asOf.deliveries,
        worktrees: local.asOf.worktrees ?? previous.asOf.worktrees,
        settings: local.asOf.settings ?? previous.asOf.settings,
      },
    };
    // 同一对话内只补回读取开始后才出现的确认消息；同 id 的返回版本仍以快照为准。
    const current = this.dataFor(projectId);
    const fetchedIds = new Set(assembled.messages.map((m) => m.id));
    const confirmed = current.conversation?.id === assembled.conversation?.id
      ? current.messages.filter(
          (m) => !startedMessageIds.has(m.id) && !fetchedIds.has(m.id),
        )
      : [];
    if (confirmed.length > 0) {
      assembled.messages = mergeMessages(assembled.messages, confirmed);
    }
    this.setProjectData(projectId, assembled);
    if (this.state.connection?.phase === "live") {
      void this.flushOutbox(projectId);
    }
    return { seqs };
  }

  private async ensureProjectLoaded(projectId: string): Promise<void> {
    if (this.loadedProjects.has(projectId)) return;
    const existing = this.loadingProjects.get(projectId);
    if (existing) return existing;
    const gen = this.generation;
    const promise = (async () => {
      await this.hydrateProject(gen, projectId);
      if (gen !== this.generation) return;
      this.loadedProjects.add(projectId);
      void this.prefetchWorkflows(gen, projectId);
    })().finally(() => {
      this.loadingProjects.delete(projectId);
    });
    this.loadingProjects.set(projectId, promise);
    return promise;
  }

  // ---------------------------------------------------------------- 事件流

  private async openStream(gen: number, afterSeq?: number): Promise<void> {
    this.streamToken += 1;
    const token = this.streamToken;
    try {
      await this.ipc.eventsClose();
    } catch {
      // 忽略旧流关闭失败。
    }
    if (!this.running || gen !== this.generation || token !== this.streamToken) {
      return;
    }
    try {
      await this.ipc.eventsOpen({ afterSeq }, (frame) => {
        // 旧流一律失效；当前流在刷新期间收到不可恢复终态时立即停流并进入未配对。
        if (token !== this.streamToken) return;
        if (gen !== this.generation) {
          if (frame.t === "state" && this.isTerminalPhase(frame.state.phase)) {
            this.handleFrame(frame);
          }
          return;
        }
        this.handleFrame(frame);
      });
    } catch (error) {
      if (gen !== this.generation) return;
      this.notify(errorMessageFrom(error));
    }
  }

  private handleFrame(frame: StreamFrame): void {
    switch (frame.t) {
      case "event": {
        const seq = frame.envelope.seq;
        if (seq <= this.appliedSeq) return;
        this.appliedSeq = seq;
        if (this.catchUp) this.catchUp.count += 1;
        const envelope = frame.envelope;
        if (!isKnownEventType(envelope.type)) {
          this.ack(seq);
          return;
        }
        const keys = resourcesForEvent(envelope, {
          currentProjectId: this.state.currentProjectId,
          deviceId: this.state.me?.device.id ?? null,
          projectForDelivery: (deliveryId) =>
            this.projectForDelivery(deliveryId),
          deliveryForCorrelation: (correlationId) =>
            this.deliveryForCorrelation(correlationId),
        });
        let nextSettings = this.state.settingsRevision;
        if (envelope.type === "settings.updated") {
          nextSettings += 1;
        }
        const filtered = keys.filter(
          (key) => !isBelowWatermark(seq, this.asOfForKey(key)),
        );
        for (const key of filtered) this.scheduler.mark(key);
        if (nextSettings !== this.state.settingsRevision) {
          this.setState((state) => ({
            ...state,
            settingsRevision: nextSettings,
          }));
        }
        this.ack(seq);
        return;
      }
      case "heartbeat": {
        this.appliedSeq = maxEventSeq(this.appliedSeq, frame.seq) ?? this.appliedSeq;
        if (this.catchUp) {
          const count = this.catchUp.count;
          this.catchUp = null;
          if (count > 0) this.notify(`已补齐 ${count} 件事件`);
        }
        this.ack(frame.seq);
        return;
      }
      case "state": {
        const previous = this.state.connection;
        const next = frame.state;
        this.setState((state) => ({ ...state, connection: next }));
        if (this.isTerminalPhase(next.phase)) {
          this.enterTerminalState();
          return;
        }
        if (
          previous?.log_epoch != null &&
          next.log_epoch != null &&
          previous.log_epoch !== next.log_epoch
        ) {
          this.logEpoch = next.log_epoch;
          this.scheduleResync("log_epoch_changed");
          return;
        }
        if (
          next.phase === "live" &&
          (previous === null ||
            previous.phase !== "live" ||
            next.epoch > previous.epoch)
        ) {
          this.catchUp = { epoch: next.epoch, count: 0 };
          void this.flushAllOutboxes();
        }
        return;
      }
      case "resync_required": {
        this.scheduleResync(frame.reason);
        return;
      }
      case "navigate": {
        // M2：从系统通知定位到上报项。
        return;
      }
    }
  }

  private ack(seq: number): void {
    void this.ipc.eventsAck(seq).catch(() => {
      // ack 失败由外壳的重连与重放兜底。
    });
  }

  private scheduleResync(
    reason: "cursor_ahead" | "cursor_expired" | "log_epoch_changed",
  ): void {
    if (this.resyncing) return;
    void this.resync(reason);
  }

  private async resync(
    reason: "cursor_ahead" | "cursor_expired" | "log_epoch_changed",
  ): Promise<void> {
    if (this.resyncing) return;
    this.resyncing = true;
    this.generation += 1;
    this.streamToken += 1;
    this.scheduler.clear();
    this.appliedSeq = 0;
    this.loadedProjects.clear();
    this.setState((state) => ({
      ...state,
      projectData: {},
      inbox: [],
      inboxSummary: null,
      approvals: {},
      workflows: {},
      attemptsByNode: {},
      budgetsByDelivery: {},
      resyncing: true,
      lastResyncReason: reason,
      notice:
        reason === "log_epoch_changed"
          ? "服务端已从备份恢复，界面已重新同步。"
          : "界面已重新同步。",
    }));
    try {
      await this.ipc.eventsClose().catch(() => undefined);
      await this.hydrate("resync");
    } catch (error) {
      this.setState((state) => ({
        ...state,
        status: "error",
        error: errorMessageFrom(error),
        resyncing: false,
      }));
    } finally {
      this.resyncing = false;
      this.setState((state) => ({ ...state, resyncing: false }));
    }
  }

  // ---------------------------------------------------------------- 刷新

  private async runRefresh(key: string): Promise<void> {
    const gen = this.generation;
    try {
      if (key === "projects") await this.refreshProjects(gen);
      else if (key === "me") await this.refreshMe(gen);
      else if (key === "inbox") await this.refreshInbox(gen);
      else if (key === "summary") await this.refreshSummary(gen);
      else if (key === "approvals") await this.refreshApprovals(gen);
      else if (key.startsWith("messages:")) {
        const projectId = key.slice("messages:".length);
        if (!this.loadedProjects.has(projectId)) await this.ensureProjectLoaded(projectId);
        else await this.refreshMessages(gen, projectId);
      } else if (key.startsWith("conversation:")) {
        const projectId = key.slice("conversation:".length);
        if (!this.loadedProjects.has(projectId)) await this.ensureProjectLoaded(projectId);
        else await this.refreshConversation(gen, projectId);
      } else if (key.startsWith("deliveries:")) {
        const projectId = key.slice("deliveries:".length);
        if (!this.loadedProjects.has(projectId)) await this.ensureProjectLoaded(projectId);
        else await this.refreshDeliveries(gen, projectId);
      } else if (key.startsWith("worktrees:")) {
        const projectId = key.slice("worktrees:".length);
        if (!this.loadedProjects.has(projectId)) await this.ensureProjectLoaded(projectId);
        else await this.refreshWorktrees(gen, projectId);
      } else if (key.startsWith("settings:")) {
        await this.refreshDefaultLevel(gen, key.slice("settings:".length));
      } else if (key.startsWith("approval:")) {
        await this.refreshApproval(gen, key.slice("approval:".length));
      } else if (key.startsWith("attempts:")) {
        await this.refreshAttempts(gen, key.slice("attempts:".length));
      } else if (key.startsWith("budget:")) {
        await this.refreshBudget(gen, key.slice("budget:".length));
      } else if (key.startsWith("workflow:")) {
        await this.refreshWorkflow(key.slice("workflow:".length));
      }
    } catch {
      // 事件触发的刷新失败保留旧数据，由用户手动刷新或下一次事件重试。
    }
  }

  private async refreshProjects(gen: number): Promise<void> {
    const result = await this.fetchAllPages<Project>((cursor) =>
      this.read("projects.list", { cursor, limit: 200 }),
    );
    if (gen !== this.generation || result.error) return;
    const projects = dedupeById(result.items);
    const current = this.resolveProjectId(projects);
    this.setState((state) => ({
      ...state,
      projects,
      currentProjectId: current,
      ui: ensureUi(state.ui, projects, current),
    }));
    if (current) void this.ensureProjectLoaded(current);
  }

  private async refreshMe(gen: number): Promise<void> {
    const result = await this.read("me.get", {});
    if (gen !== this.generation || !result.ok) return;
    const me = result.body as Me;
    this.clockOffsetMs = Date.parse(me.server.time) - this.options.now();
    if (this.logEpoch !== null && me.server.log_epoch !== this.logEpoch) {
      this.logEpoch = me.server.log_epoch;
      this.scheduleResync("log_epoch_changed");
      return;
    }
    this.logEpoch = me.server.log_epoch;
    this.setState((state) => ({ ...state, me }));
  }

  private async refreshInbox(gen: number): Promise<void> {
    const result = await this.fetchAllPages<InboxItem>((cursor) =>
      this.read("inbox.list", { status: "open", cursor, limit: 200 }),
    );
    if (gen !== this.generation || result.error) return;
    const inbox = dedupeById(result.items);
    this.setState((state) => ({ ...state, inbox }));
  }

  private async refreshSummary(gen: number): Promise<void> {
    const result = await this.read("inbox.summary", {});
    if (gen !== this.generation || !result.ok) return;
    this.setState((state) => ({
      ...state,
      inboxSummary: result.body as InboxSummary,
    }));
  }

  private async refreshApprovals(gen: number): Promise<void> {
    const result = await this.read("approvals.list", { status: "pending", limit: 200 });
    if (gen !== this.generation || !result.ok) return;
    const items = result.body?.items ?? [];
    this.setState((state) => {
      const approvals = { ...state.approvals };
      for (const approval of items) approvals[approval.id] = approval;
      return { ...state, approvals };
    });
  }

  private async refreshMessages(gen: number, projectId: string): Promise<void> {
    const data = this.dataFor(projectId);
    if (!data.conversation) return;
    const conversationId = data.conversation.id;
    const result = await this.read("messages.list", {
      conversationId,
      limit: 50,
      order: "desc",
    });
    if (gen !== this.generation || !result.ok) return;
    // 读取期间可能已切到新对话或发送成功；按当前缓存合并，避免旧列表覆盖新消息。
    const current = this.dataFor(projectId);
    if (current.conversation?.id !== conversationId) return;
    const seq = xEventSeq(result);
    if (isSnapshotStale(seq, current.asOf.messages)) return;
    const items = result.body ? [...result.body.items].reverse() : [];
    const next: ProjectData = {
      ...current,
      messages: mergeMessages(current.messages, items),
      asOf: { ...current.asOf, messages: seq ?? current.asOf.messages },
    };
    this.setProjectData(projectId, next);
  }

  private async refreshConversation(gen: number, projectId: string): Promise<void> {
    const result = await this.read("conversations.list", {
      projectId,
      limit: 50,
    });
    if (gen !== this.generation || !result.ok) return;
    const conversation = result.body?.items[0] ?? null;
    const data = this.dataFor(projectId);
    const seq = xEventSeq(result);
    this.setProjectData(projectId, {
      ...data,
      conversation,
      asOf: { ...data.asOf, conversation: seq ?? data.asOf.conversation },
    });
    if (conversation) await this.refreshMessages(gen, projectId);
  }

  private async refreshDeliveries(gen: number, projectId: string): Promise<void> {
    const [active, done] = await Promise.all([
      this.read("deliveries.list", { projectId, active: true, limit: 200 }),
      this.read("deliveries.list", {
        projectId,
        stage: ["closed", "abandoned"],
        limit: 10,
      }),
    ]);
    if (gen !== this.generation) return;
    const data = this.dataFor(projectId);
    const items: Delivery[] = [];
    const seqs: Array<number | null> = [];
    if (active.ok) {
      items.push(...(active.body?.items ?? []));
      seqs.push(xEventSeq(active));
    }
    if (done.ok) {
      items.push(...(done.body?.items ?? []));
      seqs.push(xEventSeq(done));
    }
    if (!active.ok && !done.ok) return;
    const seq = minEventSeq(seqs);
    if (isSnapshotStale(seq, data.asOf.deliveries)) return;
    this.setProjectData(projectId, {
      ...data,
      deliveries: mergeDeliveries(data.deliveries, dedupeById(items)),
      asOf: { ...data.asOf, deliveries: seq ?? data.asOf.deliveries },
    });
  }

  private async refreshWorktrees(gen: number, projectId: string): Promise<void> {
    const result = await this.read("worktrees.list", { projectId, limit: 200 });
    if (gen !== this.generation || !result.ok) return;
    const data = this.dataFor(projectId);
    const seq = xEventSeq(result);
    if (isSnapshotStale(seq, data.asOf.worktrees)) return;
    this.setProjectData(projectId, {
      ...data,
      worktrees: mergeWorktrees(data.worktrees, result.body?.items ?? []),
      asOf: { ...data.asOf, worktrees: seq ?? data.asOf.worktrees },
    });
  }

  private async refreshDefaultLevel(gen: number, projectId: string): Promise<void> {
    const role = this.state.me?.device.role;
    if (role !== "operator" && role !== "approver") return;
    const result = await this.read("project_settings.get", {
      projectId,
      resource: "levels",
      view: "effective",
    });
    if (gen !== this.generation || !result.ok) return;
    const data = this.dataFor(projectId);
    const seq = xEventSeq(result);
    if (isSnapshotStale(seq, data.asOf.settings)) return;
    const next: ProjectData = {
      ...data,
      defaultLevel: readDefaultLevel(result.body?.value),
      asOf: { ...data.asOf, settings: seq ?? data.asOf.settings },
    };
    this.setProjectData(projectId, next);
  }

  private async refreshApproval(gen: number, approvalId: string): Promise<void> {
    const result = await this.read("approvals.get", { approvalId });
    if (gen !== this.generation || !result.ok || !result.body) return;
    const approval = result.body;
    this.setState((state) => ({
      ...state,
      approvals: { ...state.approvals, [approval.id]: approval },
    }));
  }

  private async refreshWorkflow(deliveryIdOrWorkflowId: string): Promise<void> {
    const gen = this.generation;
    if (deliveryIdOrWorkflowId.startsWith("wfl_")) {
      await this.loadWorkflow(deliveryIdOrWorkflowId);
      return;
    }
    const deliveryId = deliveryIdOrWorkflowId;
    const projectId = this.projectForDelivery(deliveryId);
    const data = projectId ? this.dataFor(projectId) : null;
    const delivery = data?.deliveries.find((item) => item.id === deliveryId);
    const result = await this.read("workflows.list", {
      deliveryId,
      stage: delivery?.stage,
      limit: 200,
    });
    if (gen !== this.generation || !result.ok) return;
    const latest = latestWorkflow(result.body?.items ?? []);
    if (!latest) return;
    const detail = await this.read("workflows.get", {
      workflowId: latest.id,
      includeSuperseded: true,
    });
    if (gen !== this.generation || !detail.ok || !detail.body) return;
    const workflow = detail.body;
    this.setState((state) => ({
      ...state,
      workflows: { ...state.workflows, [workflow.id]: workflow },
    }));
  }

  private async loadWorkflow(workflowId: string): Promise<void> {
    const gen = this.generation;
    const detail = await this.read("workflows.get", {
      workflowId,
      includeSuperseded: true,
    });
    if (gen !== this.generation || !detail.ok || !detail.body) return;
    const workflow = detail.body;
    this.setState((state) => ({
      ...state,
      workflows: { ...state.workflows, [workflow.id]: workflow },
    }));
  }

  private async refreshAttempts(gen: number, nodeId: string): Promise<void> {
    const result = await this.fetchAllPages<Attempt>((cursor) =>
      this.read("attempts.list", { nodeId, order: "asc", cursor, limit: 200 }),
    );
    if (gen !== this.generation || result.error) return;
    const items = dedupeById(result.items).sort((a, b) => a.n - b.n);
    this.setState((state) => ({
      ...state,
      attemptsByNode: { ...state.attemptsByNode, [nodeId]: items },
    }));
  }

  private async refreshBudget(gen: number, deliveryId: string): Promise<void> {
    const result = await this.read("deliveries.budget", { deliveryId });
    if (gen !== this.generation || !result.ok || !result.body) return;
    const budget = result.body;
    this.setState((state) => ({
      ...state,
      budgetsByDelivery: {
        ...state.budgetsByDelivery,
        [deliveryId]: budget,
      },
    }));
  }

  private async prefetchWorkflows(gen: number, projectId: string): Promise<void> {
    if (!this.options.prefetchWorkflows) return;
    const active = this.dataFor(projectId).deliveries
      .filter((delivery) => delivery.stage !== "closed" && delivery.stage !== "abandoned")
      .slice(0, 20);
    await mapLimit(active, 2, async (delivery) => {
      if (gen !== this.generation) return;
      await this.refreshWorkflow(delivery.id);
    });
  }

  // ---------------------------------------------------------------- 写入

  send(): void {
    if (this.isTerminalConnection()) return;
    const projectId = this.state.currentProjectId;
    if (!projectId) return;
    const role = this.state.me?.device.role;
    if (!canQueueSend(role)) {
      this.notify("此设备为只读，不能发消息。");
      return;
    }
    const ui = this.uiFor(projectId);
    const body = buildSend({
      text: ui.draft,
      ctx: ui.ctx,
      levelOverride: ui.levelOverride,
    });
    if (body.text.trim() === "") return;
    const data = this.dataFor(projectId);
    if (!data.conversation) {
      this.notify("对话尚未载入，请稍后重试。");
      return;
    }
    const item: ProjectUiState["outbox"][number] = {
      local_id: newLocalId(),
      idempotency_key: newIdempotencyKey(),
      text: body.text,
      ...(body.level_cap !== undefined ? { level_cap: body.level_cap } : {}),
      refs: body.refs,
      created_at: new Date(this.options.now()).toISOString(),
      state: "queued",
    };
    this.outboxIdentity.set(item.local_id, this.identityEpoch);
    const nextOutbox = [...ui.outbox, item];
    this.updateUi(
      {
        draft: "",
        ctx: null,
        levelOverride: null,
        outbox: nextOutbox,
      },
      projectId,
    );
    if (this.state.connection?.phase === "live") {
      void this.flushOutbox(projectId);
    }
  }

  async flushOutbox(projectId?: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    const target = projectId ?? this.state.currentProjectId;
    if (!target) return;
    const existing = this.flushing.get(target);
    if (existing) return existing;
    const promise = this.flushProject(target).finally(() => {
      this.flushing.delete(target);
    });
    this.flushing.set(target, promise);
    return promise;
  }

  private async flushAllOutboxes(): Promise<void> {
    const projects = Object.entries(this.state.ui)
      .filter(([, ui]) =>
        ui.outbox.some(
          (item) => item.state === "queued" && this.isOutboxSendable(item),
        ),
      )
      .map(([projectId]) => projectId);
    await Promise.all(projects.map((projectId) => this.flushOutbox(projectId)));
  }

  private async flushProject(projectId: string): Promise<void> {
    const gen = this.generation;
    for (;;) {
      if (gen !== this.generation || this.isTerminalConnection()) return;
      const role = this.state.me?.device.role;
      if (!role) return;
      if (!canQueueSend(role)) {
        this.failOutbox(projectId, "forbidden_role");
        return;
      }
      const ui = this.uiFor(projectId);
      const next = ui.outbox.find(
        (item) => item.state === "queued" && this.isOutboxSendable(item),
      );
      if (!next) return;
      let data = this.dataFor(projectId);
      if (!data.conversation) {
        // 后台项目的对话可能尚未加载（刷新 / 重同步后）：先加载再发送。
        if (!this.state.projects.some((project) => project.id === projectId)) {
          this.failOutbox(projectId, "not_found");
          this.notify(errorMessage("not_found"));
          return;
        }
        await this.ensureProjectLoaded(projectId);
        if (gen !== this.generation || this.isTerminalConnection()) return;
        data = this.dataFor(projectId);
        if (!data.conversation) {
          // 下次刷新或事件仍会重试，不静默丢队列。
          this.loadedProjects.delete(projectId);
          this.notify("对话尚未载入，待发送消息已保留；恢复后会重试。");
          return;
        }
      }
      this.markOutbox(projectId, next.local_id, "sending");
      const body = {
        text: next.text,
        refs: next.refs,
        ...(next.level_cap !== undefined ? { level_cap: next.level_cap } : {}),
      };
      let result: ApiResult<Message>;
      try {
        result = await this.read("messages.post", {
          conversationId: data.conversation.id,
          body,
          idempotencyKey: next.idempotency_key,
        });
      } catch (error) {
        if (gen !== this.generation || this.isTerminalConnection()) return;
        this.markOutbox(projectId, next.local_id, "queued");
        this.scheduleRetry(projectId);
        this.notify(errorMessageFrom(error));
        return;
      }
      if (gen !== this.generation || this.isTerminalConnection()) return;
      if (result.ok) {
        this.removeOutbox(next.local_id, projectId);
        if (result.body) {
          const current = this.dataFor(projectId);
          this.setProjectData(projectId, {
            ...current,
            messages: mergeMessages(current.messages, [result.body]),
          });
        }
        this.retryAttempts.delete(projectId);
        continue;
      }
      if (!result.ok && "network" in result) {
        this.markOutbox(projectId, next.local_id, "queued");
        if (this.state.connection?.phase === "live") this.scheduleRetry(projectId);
        return;
      }
      const code = result.problem.code;
      if (isRetryableResult(result)) {
        this.markOutbox(projectId, next.local_id, "queued");
        if (this.state.connection?.phase === "live") this.scheduleRetry(projectId);
        else this.notify("连接中断，消息会在恢复后发送。");
        return;
      }
      this.markOutbox(
        projectId,
        next.local_id,
        "failed",
        result.problem.code,
      );
      this.notify(errorMessage(code));
      return;
    }
  }

  retryOutbox(localId: string, projectId?: string): void {
    const target = projectId ?? this.state.currentProjectId;
    if (!target) return;
    const ui = this.uiFor(target);
    if (!ui.outbox.some((item) => item.local_id === localId)) return;
    // 重试是用户对旧身份待发项的明确处置：改用当前身份发送。
    this.outboxIdentity.set(localId, this.identityEpoch);
    const next = ui.outbox.map((item) =>
      item.local_id === localId
        ? { ...item, state: "queued" as const, error_code: null }
        : item,
    );
    this.updateUi({ outbox: next }, target);
    this.retryAttempts.delete(target);
    void this.flushOutbox(target);
  }

  editOutbox(localId: string, projectId?: string): void {
    const target = projectId ?? this.state.currentProjectId;
    if (!target) return;
    const ui = this.uiFor(target);
    const item = ui.outbox.find((entry) => entry.local_id === localId);
    if (!item) return;
    this.outboxIdentity.delete(localId);
    const outbox = ui.outbox.filter((entry) => entry.local_id !== localId);
    this.updateUi(
      {
        outbox,
        draft: item.text,
        ctx: item.refs[0] ?? null,
        levelOverride: item.level_cap ?? null,
      },
      target,
    );
  }

  removeOutbox(localId: string, projectId?: string): void {
    const target = projectId ?? this.state.currentProjectId;
    if (!target) return;
    const ui = this.uiFor(target);
    this.outboxIdentity.delete(localId);
    this.updateUi(
      { outbox: ui.outbox.filter((entry) => entry.local_id !== localId) },
      target,
    );
  }

  private failOutbox(projectId: string, code: string): void {
    const ui = this.uiFor(projectId);
    if (!ui.outbox.some((item) => item.state === "queued" || item.state === "sending")) {
      return;
    }
    this.updateUi(
      {
        outbox: ui.outbox.map((item) =>
          item.state === "queued" || item.state === "sending"
            ? { ...item, state: "failed", error_code: code }
            : item,
        ),
      },
      projectId,
    );
  }

  private markOutbox(
    projectId: string,
    localId: string,
    state: "queued" | "sending" | "failed",
    errorCode: string | null = null,
  ): void {
    const ui = this.uiFor(projectId);
    this.updateUi(
      {
        outbox: ui.outbox.map((item) =>
          item.local_id === localId
            ? { ...item, state, error_code: state === "failed" ? errorCode : null }
            : item,
        ),
      },
      projectId,
    );
  }

  private scheduleRetry(projectId: string): void {
    if (this.retryTimers.has(projectId)) return;
    const attempt = (this.retryAttempts.get(projectId) ?? 0) + 1;
    this.retryAttempts.set(projectId, attempt);
    const delays = this.options.retryDelaysMs;
    const delay = delays[Math.min(attempt - 1, delays.length - 1)] ?? 30_000;
    const timer = setTimeout(() => {
      this.retryTimers.delete(projectId);
      void this.flushOutbox(projectId);
    }, delay);
    this.retryTimers.set(projectId, timer);
  }

  async loadOlder(projectId?: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    const gen = this.generation;
    const target = projectId ?? this.state.currentProjectId;
    if (!target) return;
    const data = this.dataFor(target);
    if (!data.conversation || !data.messageCursor) return;
    const result = await this.read("messages.list", {
      conversationId: data.conversation.id,
      cursor: data.messageCursor,
      limit: 50,
      order: "desc",
    });
    if (gen !== this.generation || this.isTerminalConnection()) return;
    if (!result.ok) {
      this.notify(failureText(result));
      return;
    }
    const items = result.body ? [...result.body.items].reverse() : [];
    const current = this.dataFor(target);
    this.setProjectData(target, {
      ...current,
      messages: mergeMessages(items, current.messages, 1000),
      messageCursor: result.body?.next_cursor ?? null,
    });
  }

  async choose(itemId: string, optionId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    if (!canAct(this.state, "choose")) {
      this.notify("此设备为只读，不能处理上报项。");
      return;
    }
    const gen = this.generation;
    const result = await this.read("inbox.choose", {
      inboxId: itemId,
      optionId,
      idempotencyKey: newIdempotencyKey(),
    });
    if (gen !== this.generation || this.isTerminalConnection()) return;
    if (!result.ok) {
      this.notify(failureText(result));
      return;
    }
    if (result.body) {
      this.setState((state) => ({
        ...state,
        inbox: mergeInbox(state.inbox, [result.body as InboxItem]),
      }));
    }
    await this.refreshAfterWrite(
      (result.body as InboxItem | null)?.project_id ?? undefined,
    );
  }

  async dismiss(itemId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    if (!canAct(this.state, "dismiss")) {
      this.notify("此设备为只读，不能忽略上报项。");
      return;
    }
    const gen = this.generation;
    const result = await this.read("inbox.dismiss", { inboxId: itemId });
    if (gen !== this.generation || this.isTerminalConnection()) return;
    if (!result.ok) {
      this.notify(failureText(result));
      return;
    }
    if (result.body) {
      this.setState((state) => ({
        ...state,
        inbox: mergeInbox(state.inbox, [result.body as InboxItem]),
      }));
    }
    await this.refreshAfterWrite(
      (result.body as InboxItem | null)?.project_id ?? undefined,
    );
  }

  async control(
    deliveryId: string,
    action: "pause" | "resume" | "abandon",
  ): Promise<void> {
    if (this.isTerminalConnection()) return;
    if (!canAct(this.state, "control")) {
      this.notify("此设备为只读，不能控制交付。");
      return;
    }
    const gen = this.generation;
    const projectId = this.projectForDelivery(deliveryId);
    const result =
      action === "pause"
        ? await this.read("deliveries.pause", {
            deliveryId,
            idempotencyKey: newIdempotencyKey(),
          })
        : action === "resume"
          ? await this.read("deliveries.resume", {
              deliveryId,
              idempotencyKey: newIdempotencyKey(),
            })
          : await this.read("deliveries.abandon", { deliveryId });
    if (gen !== this.generation || this.isTerminalConnection()) return;
    if (!result.ok) {
      this.notify(failureText(result));
      return;
    }
    if (result.body && projectId) {
      const data = this.dataFor(projectId);
      this.setProjectData(projectId, {
        ...data,
        deliveries: mergeDeliveries(data.deliveries, [result.body as Delivery]),
      });
      await Promise.all([
        this.refreshWorkflow(deliveryId),
        this.refreshBudget(this.generation, deliveryId),
      ]);
    }
  }

  async retryNode(nodeId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    if (!canAct(this.state, "retry")) {
      this.notify("此设备为只读，不能重试节点。");
      return;
    }
    const gen = this.generation;
    const result = await this.read("nodes.retry", {
      nodeId,
      idempotencyKey: newIdempotencyKey(),
    });
    if (gen !== this.generation || this.isTerminalConnection()) return;
    if (!result.ok) {
      this.notify(failureText(result));
      return;
    }
    if (!result.body) return;
    const node = result.body;
    let deliveryId: string | null = null;
    this.setState((state) => {
      const workflows = { ...state.workflows };
      for (const [id, workflow] of Object.entries(workflows)) {
        if (workflow.nodes?.some((entry) => entry.id === node.id)) {
          deliveryId = workflow.delivery_id;
          workflows[id] = {
            ...workflow,
            nodes: workflow.nodes?.map((entry) =>
              entry.id === node.id ? { ...entry, ...node } : entry,
            ),
          };
        }
      }
      return { ...state, workflows };
    });
    await this.ensureAttempts(node.id);
    if (deliveryId) {
      await Promise.all([
        this.refreshWorkflow(deliveryId),
        this.refreshBudget(this.generation, deliveryId),
      ]);
    }
  }

  async approvalPrepare(approvalId: string): Promise<ApprovalSheetModel> {
    return this.ipc.approvalPrepare({ approvalId });
  }

  async approvalSign(preparedId: string): Promise<ApiResult<Approval>> {
    if (this.isTerminalConnection()) {
      return {
        ok: false,
        status: 409,
        problem: {
          code: "device_revoked",
          title: "Device is revoked or certificate expired.",
          request_id: "local",
        },
      };
    }
    const gen = this.generation;
    const result = await this.ipc.approvalSign({ preparedId });
    if (gen !== this.generation || this.isTerminalConnection()) return result;
    if (result.ok && result.body) {
      const approval = result.body;
      this.setState((state) => ({
        ...state,
        approvals: { ...state.approvals, [approval.id]: approval },
      }));
      await this.refreshAfterWrite(this.projectForApproval(approval));
    }
    return result;
  }

  async denyApproval(
    approvalId: string,
    reason?: string,
  ): Promise<ApiResult<Approval>> {
    if (this.isTerminalConnection()) {
      return {
        ok: false,
        status: 409,
        problem: {
          code: "device_revoked",
          title: "Device is revoked or certificate expired.",
          request_id: "local",
        },
      };
    }
    if (!canAct(this.state, "deny")) {
      return {
        ok: false,
        status: 403,
        problem: {
          code: "forbidden_role",
          title: "Approval requires approver role.",
          request_id: "local",
        },
      };
    }
    const gen = this.generation;
    const result = await this.read("approvals.deny", {
      approvalId,
      reason,
      idempotencyKey: newIdempotencyKey(),
    });
    if (gen !== this.generation || this.isTerminalConnection()) return result;
    if (result.ok && result.body) {
      const approval = result.body;
      this.setState((state) => ({
        ...state,
        approvals: { ...state.approvals, [approval.id]: approval },
      }));
      await this.refreshAfterWrite(this.projectForApproval(approval));
    }
    return result;
  }

  async refresh(): Promise<void> {
    if (this.state.status === "unpaired" || this.state.status === "error") {
      await this.bootstrap();
      return;
    }
    await this.hydrate("refresh");
  }

  /** 供审批单在模拟操作后读取最新审批；结果进入同一 DesktopState。 */
  async ensureApproval(approvalId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    await this.refreshApproval(this.generation, approvalId);
  }

  /** 供 DAG 详情按需读取；结果进入同一 DesktopState。 */
  async ensureAttempts(nodeId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    await this.refreshAttempts(this.generation, nodeId);
  }

  async ensureBudget(deliveryId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    await this.refreshBudget(this.generation, deliveryId);
  }

  async ensureWorkflow(workflowId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    await this.loadWorkflow(workflowId);
  }

  /** 供检视器在未预取工作流时按交付读取当前图。 */
  async ensureWorkflowForDelivery(deliveryId: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    await this.refreshWorkflow(deliveryId);
  }

  workflowForDelivery(deliveryId: string): Workflow | null {
    const candidates = Object.values(this.state.workflows).filter(
      (workflow) => workflow.delivery_id === deliveryId,
    );
    return latestWorkflow(candidates);
  }

  signAvailability(
    approval: Approval,
    sheet: ApprovalSheetModel | null,
    pr = this.prForApproval(approval),
    approverDeviceName?: string,
  ): SignAvailability {
    const me = this.state.me;
    const connection = this.state.connection;
    if (!me || !connection) {
      return { ok: false, reason: "offline" };
    }
    const input: SignAvailabilityInput = {
      conn: connection,
      me,
      approval,
      pr,
      sheet,
      now: this.options.now() + this.clockOffsetMs,
      approverDeviceName,
    };
    return computeSignAvailability(input);
  }

  approvalStatus(
    approval: Approval,
  ): { label: string; tone: Tone } {
    const delivery = approval.delivery_id
      ? this.deliveryById(approval.delivery_id)
      : null;
    return approvalStatusLabel(approval, {
      prState: delivery?.pr?.state ?? null,
      deliveryStage: delivery?.stage ?? null,
      headSha: delivery?.pr?.head_sha ?? null,
    });
  }

  mergeGates(approval: Approval): string[] {
    const pr = this.prForApproval(approval);
    return pr ? mergeGateIssues(pr).map((gate) => gate.label) : [];
  }

  signBlockText(
    result: ReturnType<DesktopController["signAvailability"]>,
  ): string {
    return result.ok ? "" : formatSignBlock(result);
  }

  // ---------------------------------------------------------------- 查询辅助

  needsYou(): NeedsYouProjection {
    return needsYou(this.state);
  }

  can(action: Action): boolean {
    return canAct(this.state, action);
  }

  projectForDelivery(deliveryId: string): string | null {
    for (const delivery of this.allDeliveries()) {
      if (delivery.id === deliveryId) return delivery.project_id;
    }
    return null;
  }

  private projectForApproval(approval: Approval): string | undefined {
    if (approval.delivery_id) {
      const projectId = this.projectForDelivery(approval.delivery_id);
      if (projectId) return projectId;
    }
    return this.state.inbox.find((item) => {
      const payload = item.payload as { approval_id?: unknown };
      return payload.approval_id === approval.id;
    })?.project_id ?? undefined;
  }

  deliveryById(deliveryId: string): Delivery | null {
    return this.allDeliveries().find((delivery) => delivery.id === deliveryId) ?? null;
  }

  private deliveryForCorrelation(correlationId: string | null): string | null {
    if (!correlationId) return null;
    if (this.deliveryById(correlationId)) return correlationId;
    return null;
  }

  private allDeliveries(): Delivery[] {
    return Object.values(this.state.projectData).flatMap((data) => data.deliveries);
  }

  private prForApproval(approval: Approval) {
    if (!isPrApproval(approval)) return null;
    if (!approval.delivery_id) return null;
    return this.deliveryById(approval.delivery_id)?.pr ?? null;
  }

  private asOfForKey(key: ResourceKey): number | null {
    if (key.startsWith("messages:")) {
      return this.dataFor(key.slice("messages:".length)).asOf.messages;
    }
    if (key.startsWith("conversation:")) {
      return this.dataFor(key.slice("conversation:".length)).asOf.conversation;
    }
    if (key.startsWith("deliveries:")) {
      return this.dataFor(key.slice("deliveries:".length)).asOf.deliveries;
    }
    if (key.startsWith("worktrees:")) {
      return this.dataFor(key.slice("worktrees:".length)).asOf.worktrees;
    }
    if (key.startsWith("settings:")) {
      return this.dataFor(key.slice("settings:".length)).asOf.settings;
    }
    return null;
  }

  // ---------------------------------------------------------------- 内部工具

  private async read<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    try {
      return await this.ipc.api(route, args);
    } catch (error) {
      const code = error instanceof IpcError ? error.code : "shell.bad_argument";
      return {
        ok: false,
        status: 0,
        problem: {
          code,
          title: error instanceof Error ? error.message : "IPC 调用失败",
          request_id: "local",
        },
      };
    }
  }

  private async fetchAllPages<T>(
    fetchPage: (cursor: string | undefined) => Promise<ApiResult<import("@/api/types").ApiList<T>>>,
  ) {
    return fetchAllPages<T>(fetchPage);
  }

  private async refreshAfterWrite(projectId?: string): Promise<void> {
    if (this.isTerminalConnection()) return;
    await Promise.all([
      this.refreshInbox(this.generation),
      this.refreshSummary(this.generation),
      this.refreshApprovals(this.generation),
    ]);
    const target = projectId ?? this.state.currentProjectId;
    if (target) await this.refreshDeliveries(this.generation, target);
  }

  private queuePersist(projectId: string): void {
    const existing = this.persistTimers.get(projectId);
    if (existing) clearTimeout(existing);
    const timer = setTimeout(() => {
      this.persistTimers.delete(projectId);
      void this.persistUi(projectId);
    }, this.options.persistDelayMs);
    this.persistTimers.set(projectId, timer);
  }

  private async persistUi(projectId: string): Promise<void> {
    // 重置后外壳已删除旧身份的界面状态；停止或迟到的写入不得把它写回。
    if (this.resetPending || this.state.identity?.state === "unpaired") return;
    const state = this.state.ui[projectId];
    if (!state) return;
    // 旧身份的待发项只留在当前会话；但尚未重置的终态仍属原身份，照常保存。
    const outbox = this.isTerminalConnection()
      ? state.outbox.filter((item) => !this.isOutboxIsolated(item))
      : state.outbox.filter((item) => !this.isOutboxStale(item));
    const persisted =
      outbox.length === state.outbox.length ? state : { ...state, outbox };
    try {
      await this.ipc.uiStateSet({ projectId, state: persisted });
    } catch {
      // ui_state 写入失败不阻塞界面；下次编辑会再次尝试。
    }
    if (this.state.ui[projectId] !== state) this.queuePersist(projectId);
  }

  private async flushPersist(): Promise<void> {
    const pending = Object.keys(this.state.ui);
    await Promise.all(
      pending.map(async (projectId) => {
        const timer = this.persistTimers.get(projectId);
        if (timer) {
          clearTimeout(timer);
          this.persistTimers.delete(projectId);
        }
        await this.persistUi(projectId);
      }),
    );
  }

  private setState(updater: (state: DesktopState) => DesktopState): void {
    const next = updater(this.state);
    if (next === this.state) return;
    this.state = next;
    for (const listener of [...this.listeners]) listener();
  }

  // 供 React 组件读取的等价辅助（不依赖 React）。
  get currentProject(): Project | null {
    return (
      this.state.projects.find(
        (project) => project.id === this.state.currentProjectId,
      ) ?? null
    );
  }

  get currentData(): ProjectData {
    return this.state.currentProjectId
      ? this.dataFor(this.state.currentProjectId)
      : emptyProjectData();
  }

  get currentUi(): ProjectUiState {
    return this.state.currentProjectId
      ? this.uiFor(this.state.currentProjectId)
      : defaultProjectUi();
  }

  get approverName(): string {
    return this.state.me?.device.name ?? "审批设备";
  }

  /** 供表单比较：settings.updated 的计数。 */
  get settingsRevision(): number {
    return this.state.settingsRevision;
  }

  /** 只读的 appliedSeq 诊断值（不参与渲染）。 */
  get appliedEventSeq(): number {
    return this.appliedSeq;
  }

  /** 只读的连接校正偏移，供审批倒计时使用。 */
  get clockOffset(): number {
    return this.clockOffsetMs;
  }

  /** 手动触发刷新调度（测试与“刷新失败 · 重试”）。 */
  async flushRefreshes(): Promise<void> {
    await this.scheduler.flushNow();
  }

  /** 测试钩子：读取内部资源键对应的水位线。 */
  watermarkFor(key: ResourceKey): number | null {
    return this.asOfForKey(key);
  }

}

function ensureUi(
  ui: Record<string, ProjectUiState>,
  projects: Project[],
  currentProjectId: string | null,
): Record<string, ProjectUiState> {
  const next: Record<string, ProjectUiState> = { ...ui };
  for (const project of projects) {
    next[project.id] = next[project.id] ?? defaultProjectUi();
  }
  if (currentProjectId && !next[currentProjectId]) {
    next[currentProjectId] = defaultProjectUi();
  }
  return next;
}

function mergeStoredUi(
  stored: Record<string, ProjectUiState>,
  memory: Record<string, ProjectUiState>,
): Record<string, ProjectUiState> {
  const next: Record<string, ProjectUiState> = { ...stored };
  for (const [key, value] of Object.entries(memory)) next[key] = value;
  for (const [key, value] of Object.entries(next)) {
    next[key] = {
      ...defaultProjectUi(),
      ...value,
      showAll: { ...defaultProjectUi().showAll, ...(value.showAll ?? {}) },
      chatScroll: { ...defaultProjectUi().chatScroll, ...(value.chatScroll ?? {}) },
      outbox: (value.outbox ?? []).map((item) =>
        item.state === "sending" ? { ...item, state: "queued" as const } : item,
      ),
    };
  }
  return next;
}

function requeueSending(
  ui: Record<string, ProjectUiState>,
): Record<string, ProjectUiState> {
  return Object.fromEntries(
    Object.entries(ui).map(([projectId, value]) => [
      projectId,
      {
        ...value,
        outbox: value.outbox.map((item) =>
          item.state === "sending" ? { ...item, state: "queued" as const } : item,
        ),
      },
    ]),
  );
}

function dedupeById<T extends { id: string }>(items: T[]): T[] {
  return [...new Map(items.map((item) => [item.id, item])).values()];
}

function readDefaultLevel(value: unknown): Level | null {
  if (value && typeof value === "object" && "default_level" in value) {
    const level = (value as { default_level?: unknown }).default_level;
    if (typeof level === "string") return level as Level;
  }
  return null;
}

function newLocalId(): string {
  return `local_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
}

function newIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `idem_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 12)}`;
}

function errorMessageFrom(error: unknown): string {
  if (error instanceof IpcError) return errorMessage(error.code);
  if (error instanceof Error) {
    const match = /shell\.[a-z_]+/.exec(error.message);
    if (match) return errorMessage(match[0]);
    return error.message;
  }
  return "操作失败，请稍后重试。";
}

function failureText(result: ApiResult<unknown>): string {
  if (!result.ok && "network" in result) {
    return "网络不可用，请检查连接。";
  }
  if (!result.ok) return errorMessage(result.problem.code);
  return "";
}

function problemCodeOf(result: ApiResult<unknown>): string {
  return !result.ok && "problem" in result ? result.problem.code : "internal";
}

function isRetryableResult(result: ApiResult<unknown>): boolean {
  if (!result.ok && "network" in result) return true;
  if (result.ok) return false;
  return (
    result.status >= 500 ||
    result.status === 429 ||
    result.problem.code === "idempotency_in_progress"
  );
}

export type { NeedsYouProjection, Action };
export type { DesktopState, ProjectData } from "./state";
export { needsYou, buildSend, shortSha };
export { signAvailability } from "./sign";
