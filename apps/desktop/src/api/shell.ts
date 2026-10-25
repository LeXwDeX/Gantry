/**
 * 外壳 IPC 数据形状。
 *
 * 来源：docs/modules/desktop-shell.md §6、docs/modules/desktop-ui.md §6–§7。
 * 手工转写，待 M0 的 schemas/openapi.yaml 生成类型取代。
 */
import type {
  Approval,
  ApprovalTarget,
  CheckSummary,
  Level,
  Ref,
  SettingsIssue,
  UntrustedText,
} from "./types";

export interface ApiHeaders {
  etag?: string;
  x_event_seq?: number;
  idempotent_replayed?: boolean;
  retry_after?: number;
}

export interface ApiProblem {
  code: string;
  title: string;
  request_id: string;
  [extension: string]: unknown;
}

export type ApiResult<T = unknown> =
  | { ok: true; status: number; headers: ApiHeaders; body: T | null }
  | { ok: false; status: number; problem: ApiProblem }
  | { ok: false; network: "timeout" | "tls" | "offline" | "aborted" };

export interface ConnectionState {
  phase:
    | "unpaired"
    | "connecting"
    | "live"
    | "reconnecting"
    | "handshake_failed"
    | "revoked"
    | "cert_expired";
  /** 每次（重）连 +1，用于统计恢复后补齐的事件数。 */
  epoch: number;
  last_bytes_age_ms: number | null;
  cert_not_after: string | null;
  cert_expiring: boolean;
  /** 服务端事件日志纪元号；变化即全量重同步（P-67）。 */
  log_epoch: number | null;
  clock_skew_ms: number | null;
}

export interface IdentityDevice {
  id: string;
  name: string;
  platform: "windows" | "macos" | "ios";
  pubkey_fpr: string;
  approval_pubkey_fpr: string | null;
}

export interface IdentityCertificate {
  serial: string;
  not_after: string;
  renew_after: string;
}

export interface IdentityStatus {
  state: "unpaired" | "pending_confirm" | "paired" | "revoked" | "cert_expired";
  server_host: string | null;
  ca_fingerprint: string | null;
  device: IdentityDevice | null;
  cert: IdentityCertificate | null;
  /** 原始值留扩展位，UI 只展示。 */
  key_backend: string | null;
}

export type StreamFrame =
  | { t: "event"; envelope: import("./types").EventEnvelope }
  | { t: "heartbeat"; seq: number; log_epoch?: number }
  | { t: "state"; state: ConnectionState }
  | {
      t: "resync_required";
      reason: "cursor_ahead" | "cursor_expired" | "log_epoch_changed";
    }
  | { t: "navigate"; inbox_id: string };

export type SignBlockReason =
  | "offline"
  | "not_approver"
  | "not_pending"
  | "stale"
  | "gates_unmet"
  | "expired"
  | "self_check_failed";

export type ApprovalCanSign =
  | { ok: true }
  | {
      ok: false;
      reason:
        | "not_connected"
        | "not_approver"
        | "not_pending"
        | "expired"
        | "self_check_failed";
    };

export interface ApprovalSheetModel {
  prepared_id: string;
  /** 已签名区：全部来自外壳从同一份已解析 payload 生成的展示模型。 */
  signed: {
    action: string;
    action_label: string;
    target: ApprovalTarget;
    target_label: string;
    subject_digest: string;
    digest_head12: string;
    expires_at: string;
    device_id: string;
    device_name: string;
    delivery_id: string | null;
    tech: { approval_id: string; nonce: string; v: number };
  };
  /** 未签名区：含不可信文本，界面必须视觉分离并标注“未签名信息，仅供参考”。 */
  unsigned: {
    delivery_title?: UntrustedText | null;
    pr?: {
      title: UntrustedText;
      checks: CheckSummary;
      review_state: "none" | "approved" | "changes_requested";
      mergeable_state: string;
      in_merge_queue: boolean;
      synced_at: string;
    } | null;
    changes?: unknown | null;
    note?: string | null;
  };
  preview?: unknown;
  links: { commit?: true; pr?: true };
  can_sign: ApprovalCanSign;
  /** challenge 过期时间；与审批自身 expires_at 分开显示。 */
  challenge_expires_at: string;
  server_time_offset_ms: number;
}

export interface GuardedWritePreview {
  pending_id: string;
  valid: boolean;
  errors: SettingsIssue[];
  warnings: SettingsIssue[];
  relaxes: string[];
  required_role: "viewer" | "operator" | "approver";
  requires_signature: boolean;
  preview: unknown;
  can_submit:
    | { ok: true }
    | { ok: false; reason: "not_connected" | "not_approver" | "invalid" };
}

export type GuardedWriteArgs =
  | {
      kind: "settings_put";
      scope: "global" | "project";
      projectId?: string;
      resource: string;
      value: unknown;
      ifMatch: string;
    }
  | {
      kind: "settings_delete";
      projectId: string;
      resource: string;
      ifMatch: string;
    }
  | { kind: "device_role"; deviceId: string; role: "viewer" | "operator" | "approver" }
  | { kind: "device_revoke"; deviceId: string };

export interface Prefs {
  theme: "dark" | "light" | "system";
  density: "compact" | "comfortable";
  language: string;
  systemNotifications: boolean;
  layout: {
    leftWidth: number;
    inspectorWidth: number;
    leftCollapsed: boolean;
    inspectorCollapsed: boolean;
  };
}

export type ProjectView = "chat" | "map" | "worktrees";

export type Selection =
  | { kind: "issue"; repo: string; number: number }
  | { kind: "pr"; repo: string; number: number }
  | { kind: "worktree"; id: string }
  | { kind: "base" };

export interface ChatScroll {
  atBottom: boolean;
  anchor_message_id: string | null;
  offset: number;
}

export interface OutboxItem {
  local_id: string;
  idempotency_key: string;
  text: string;
  level_cap?: Level;
  refs: Ref[];
  created_at: string;
  state: "queued" | "sending" | "failed";
  /** state = failed 时的本地化依据；字符串是 problem.code 或 shell.*。 */
  error_code?: string | null;
}

/** 按项目保存的界面状态（desktop-ui §7.3）。 */
export interface ProjectUiState {
  view: ProjectView;
  sel: Selection | null;
  draft: string;
  ctx: Ref | null;
  levelOverride: Level | null;
  needsOnly: boolean;
  showAll: { issues: boolean; prs: boolean; worktrees: boolean };
  /** 事件折叠组的键（首条事件的 msg_ ID）；用数组保持 JSON 友好。 */
  expandedGroups: string[];
  lastViewed?: { message_id: string; at: string } | null;
  chatScroll: ChatScroll;
  outbox: OutboxItem[];
}

export interface MessagesPostBody {
  text: string;
  level_cap?: Level;
  refs: Ref[];
}

export type ShellErrorCode =
  | "shell.not_paired"
  | "shell.not_connected"
  | "shell.bad_argument"
  | "shell.unknown_route"
  | "shell.too_large"
  | "shell.user_cancelled"
  | "shell.presence_failed"
  | "shell.signing_busy"
  | "shell.prepared_expired"
  | "shell.self_check_failed"
  | "shell.key_unavailable"
  | "shell.unavailable";

export class IpcError extends Error {
  readonly code: ShellErrorCode | (string & {});

  constructor(code: ShellErrorCode | (string & {}), message: string) {
    super(message);
    this.name = "IpcError";
    this.code = code;
  }
}

export type PairingBeginResult = {
  confirmationCode: string;
  device: import("./types").Device;
};

export interface OpenGithubArgs {
  kind: "pr" | "issue" | "commit";
  repo: string;
  number?: number;
  sha?: string;
}

export interface ApprovalPrepareArgs {
  approvalId: string;
}

export interface IpcAdapter {
  identityStatus(): Promise<IdentityStatus>;
  pairingBegin(a: {
    pairingString: string;
    deviceName: string;
  }): Promise<PairingBeginResult>;
  pairingConfirm(a: { matches: boolean }): Promise<IdentityStatus>;
  identityReset(): Promise<IdentityStatus>;
  connectionState(): Promise<ConnectionState>;
  api<R extends import("./routes").RouteId>(
    r: R,
    a: import("./routes").RouteArgs[R],
  ): Promise<ApiResult<import("./routes").RouteBody[R]>>;
  eventsOpen(
    a: { afterSeq?: number },
    onFrame: (f: StreamFrame) => void,
  ): Promise<void>;
  eventsAck(seq: number): Promise<void>;
  eventsClose(): Promise<void>;
  approvalPrepare(a: ApprovalPrepareArgs): Promise<ApprovalSheetModel>;
  approvalSign(a: { preparedId: string }): Promise<ApiResult<Approval>>;
  approvalDiscard(a: { preparedId: string }): Promise<void>;
  guardedWritePrepare(a: GuardedWriteArgs): Promise<GuardedWritePreview>;
  guardedWriteSubmit(a: { pendingId: string }): Promise<ApiResult<unknown>>;
  guardedWriteDiscard(a: { pendingId: string }): Promise<void>;
  prefsGet(): Promise<Prefs>;
  prefsSet(a: { patch: Partial<Prefs> }): Promise<Prefs>;
  uiStateGet(a: { projectId: string }): Promise<ProjectUiState | null>;
  uiStateSet(a: { projectId: string; state: ProjectUiState }): Promise<void>;
  uiStateList(): Promise<Record<string, ProjectUiState>>;
  openExternal(a: { url: string }): Promise<void>;
  openGithub(a: OpenGithubArgs): Promise<void>;
}
