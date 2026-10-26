/**
 * Gantry API 实体类型。
 *
 * 来源：docs/interfaces/api.md、docs/data-model.md、docs/interfaces/config.md。
 * 共享机器可读 schema（schemas/openapi.yaml）尚未交付，因此本文件是**手工转写**，
 * 不是生成代码；schema 到位后应由生成类型取代。
 */

export type Level = "None" | "Low" | "Medium" | "High" | "Max" | "Ultimate";

export type Stage =
  | "spec"
  | "plan"
  | "implement"
  | "propose"
  | "review"
  | "merge"
  | "closed"
  | "abandoned";

export type StageStatus = "running" | "waiting_human" | "blocked" | "paused" | "done";

export type AgentKind = "planner" | "worker" | "utility";

export type MessageRole = "user" | "agent" | "event";

export type MessageTone = "info" | "ok" | "run" | "warn";

export interface UntrustedText {
  text: string;
  source: string;
  truncated: boolean;
}

export interface IssueRef {
  repo: string;
  number: number;
  node_id: string;
  title: UntrustedText;
  state: "open" | "closed";
  state_reason: string | null;
  synced_at: string;
}

export interface PullRequestRef {
  repo: string;
  number: number;
  node_id: string;
  title: UntrustedText;
  state: "open" | "closed" | "merged";
  head_sha: string;
  base_ref: string;
  base_sha: string;
  draft: boolean;
  mergeable_state: string;
  in_merge_queue: boolean;
  checks: CheckSummary;
  review_state: "none" | "approved" | "changes_requested";
  synced_at: string;
}

export interface CheckSummary {
  passed: number;
  failed: number;
  pending: number;
  required_ok: boolean;
}

export type Ref =
  | { kind: "issue"; repo: string; number: number }
  | { kind: "pr"; repo: string; number: number }
  | { kind: "worktree"; id: string }
  | { kind: "delivery"; id: string };

export interface RefOption {
  id: string;
  label: string;
  primary?: boolean;
}

export type Card =
  | {
      kind: "plan";
      delivery_id: string;
      workflow_id: string;
      summary: UntrustedText;
    }
  | { kind: "approval"; approval_id: string }
  | { kind: "escalation"; inbox_id: string }
  | { kind: "choice"; inbox_id: string; options: RefOption[] }
  | {
      kind: "summary";
      delivery_id?: string | null;
      items: Array<{ text: UntrustedText; ref?: Ref | null }>;
    }
  | { kind: string; [key: string]: unknown };

export interface Project {
  id: string;
  repo: string;
  installation_id: number;
  bot_user_id: number;
  default_branch: string;
  allowed_merge_methods: Array<"merge" | "squash" | "rebase">;
  merge_queue_enabled: boolean;
  created_at?: string;
  updated_at?: string;
}

export interface Delivery {
  id: string;
  project_id: string;
  title: string;
  level_cap: Level;
  level_effective: Level;
  stage: Stage;
  stage_status: StageStatus;
  issues: IssueRef[];
  pr: PullRequestRef | null;
  worktree_id: string | null;
  parent_id: string | null;
  depends_on: string[];
  origin: "conversation" | "github_label" | "github_command";
  created_at?: string;
  updated_at?: string;
}

export interface Worktree {
  id: string;
  project_id: string;
  branch: string;
  base_ref: string;
  path: string;
  lock: {
    holder: "engine" | "guardian" | "human";
    delivery_id: string;
    acquired_at: string;
    lease_until: string;
  } | null;
}

export type NodeRole =
  | "plan"
  | "build"
  | "verify"
  | "single"
  | "reasoner"
  | "arbiter"
  | "summarize"
  | "admission"
  | "heal"
  | "answer"
  | "intent"
  | "classify"
  | `review:${string}`;

export type NodeStatus =
  | "pending"
  | "queued"
  | "running"
  | "succeeded"
  | "failed"
  | "skipped"
  | "cancelled"
  | "superseded";

export type Verdict = "ACCEPT" | "REVISE" | "REJECT" | "BLOCKED";

export interface WorkflowNode {
  id: string;
  workflow_id: string;
  role: NodeRole;
  tier: "utility" | "planner" | "worker";
  exec: "sandbox" | "sandbox_ro" | "direct";
  depends_on: string[];
  required: boolean;
  status: NodeStatus;
  verdict: Verdict | null;
  superseded_by: string | null;
}

export interface Workflow {
  id: string;
  delivery_id: string | null;
  conversation_id: string | null;
  stage: Stage | null;
  graph_rev: number;
  status: "running" | "paused" | "succeeded" | "failed" | "cancelled";
  template: string;
  nodes?: WorkflowNode[];
}

export interface Attempt {
  id: string;
  node_id: string;
  n: number;
  sandbox_id: string | null;
  status: "preparing" | "running" | "succeeded" | "failed" | "cancelled";
  started_at: string;
  ended_at: string | null;
  result_ref: string;
  failure_class:
    | "timeout"
    | "exec_failed"
    | "verdict_fail"
    | "policy_denied"
    | "cancelled"
    | "budget_exhausted"
    | "infra_error"
    | null;
  failure_detail: string | null;
  usage: {
    input_tokens: number;
    output_tokens: number;
    cache_read_tokens: number;
    cache_write_tokens: number;
    requests: number;
  };
}

export interface Conversation {
  id: string;
  project_id: string;
  created_at?: string;
  updated_at?: string;
}

export interface Message {
  id: string;
  conversation_id: string;
  role: MessageRole;
  agent_kind: AgentKind | null;
  tone: MessageTone | null;
  text: string;
  level_cap: Level | null;
  refs: Ref[];
  cards: Card[];
  delivery_id: string | null;
  created_at?: string;
  updated_at?: string;
}

export type InboxKind =
  | "approval_needed"
  | "decision_needed"
  | "spec_needed"
  | "budget_exhausted"
  | "escalation"
  | "security_finding";

export interface InboxOption {
  id: string;
  label: string;
  primary?: boolean;
}

export interface DecisionNeededPayload {
  kind: "gate_reject" | "manual_action" | "review_dispute" | "other" | (string & {});
  reason_code: string;
  options: InboxOption[];
  node_id?: string | null;
  patch?: { ref: string; paths: string[] };
}

export interface EscalationPayload {
  reason_code: string;
  node_id?: string | null;
  attempt_id?: string | null;
  target_level?: Level;
  evidence?: Array<Ref | { kind: "result"; ref: string }>;
  options?: InboxOption[];
}

export interface BudgetExhaustedPayload {
  dimension: string;
  limit: number;
  used: number;
  options: InboxOption[];
}

export interface SpecNeededPayload {
  missing: Array<{ aspect: string; question: string }>;
}

export interface SecurityFindingPayload {
  source: Ref;
  finding: {
    severity: string;
    category: string;
    title: UntrustedText;
  };
  attempt_id?: string | null;
}

export type InboxPayload =
  | { approval_id: string }
  | DecisionNeededPayload
  | EscalationPayload
  | BudgetExhaustedPayload
  | SpecNeededPayload
  | SecurityFindingPayload
  | Record<string, unknown>;

export interface InboxItem {
  id: string;
  project_id: string | null;
  delivery_id: string | null;
  kind: InboxKind | (string & {});
  severity: "info" | "warn" | "blocker";
  summary: string;
  payload: InboxPayload;
  status: "open" | "resolved" | "dismissed";
  dedup_key: string;
  created_at?: string;
  updated_at?: string;
}

export interface InboxSummaryEntry {
  project_id: string | null;
  open: number;
  max_severity: "info" | "warn" | "blocker";
}

export interface InboxSummary {
  total_open: number;
  by_project: InboxSummaryEntry[];
}

export type ApprovalStatus =
  | "pending"
  | "granted"
  | "consumed"
  | "invalidated"
  | "expired"
  | "denied";

export type ApprovalAction =
  | "merge_pr"
  | "force_push"
  | "push_outside_prefix"
  | "resolve_review_thread_human"
  | "patch_apply_protected"
  | "settings_update"
  | "device_role_change"
  | "device_revoke";

export type ApprovalTarget =
  | { kind: "pr"; repo: string; number: number }
  | {
      kind: "branch";
      repo: string;
      branch: string;
      expected_remote_oid: string;
    }
  | {
      kind: "review_thread";
      repo: string;
      number: number;
      thread_node_id: string;
    }
  | {
      kind: "patch";
      repo: string;
      attempt_id: string;
      rules: string[];
      paths: string[];
    }
  | {
      kind: "settings";
      scope: "global" | "project";
      project_id?: string;
      resource: string;
      base_rev: number;
    }
  | { kind: "device"; id: string };

export interface Approval {
  id: string;
  delivery_id: string | null;
  action: ApprovalAction;
  target: ApprovalTarget;
  subject_digest: string;
  device_id: string | null;
  signature: string | null;
  status: ApprovalStatus;
  expires_at: string;
  consumed_by?: string | null;
  created_at?: string;
  updated_at?: string;
}

export interface Device {
  id: string;
  name: string;
  platform: "windows" | "macos" | "ios";
  cert_serial: string;
  pubkey_fpr: string;
  approval_pubkey_fpr: string | null;
  role: "viewer" | "operator" | "approver";
  status: "active" | "revoked";
  last_seen: string | null;
}

export interface Me {
  device: Device;
  profile: "desktop" | "mobile";
  certificate: { not_after: string; renew_after: string };
  server: {
    api_version: string;
    version: string;
    time: string;
    log_epoch: number;
  };
}

export interface BudgetLedger {
  id: string;
  delivery_id?: string;
  subject: "delivery" | "conversation";
  template: string | null;
  limits: Record<string, unknown>;
  used: Record<string, unknown>;
}

export interface ApiList<T> {
  items: T[];
  next_cursor: string | null;
}

export interface SettingsIssue {
  pointer: string;
  code: string;
  message?: string;
}

export interface SettingsDocument {
  resource: string;
  scope: "global" | "project" | "effective";
  project_id?: string;
  rev: number;
  value: unknown | null;
  warnings?: SettingsIssue[];
  clamped?: string[];
}

export interface SettingsCatalogModel {
  tools: boolean;
  reasoning_efforts: string[];
  context_window?: number;
}

export interface SettingsCatalog {
  schema_version: number;
  models: {
    capabilities: Record<string, SettingsCatalogModel>;
    adapters: Record<string, { provider_kinds: string[]; supports_fallback: boolean }>;
  };
  levels?: { graph_templates?: Record<string, string[]> };
  policies?: {
    actions?: Record<
      string,
      { default: string; allowed: string[]; dangerous: boolean; reserved: boolean }
    >;
  };
  guardian?: {
    triage?: Record<string, { default?: string; allowed: string[]; locked: boolean; value?: string }>;
  };
  roles?: { approver_keys?: string[] };
  egress?: { suggested_groups?: unknown; forbidden_hosts?: string[] };
  notify?: { channels?: Record<string, { available: boolean; reason?: string }> };
}

export interface SettingsStatus {
  latest_rev: number;
  modules: Record<
    string,
    {
      applied_rev: number;
      health: "ok" | "degraded" | "unhealthy";
      detail?: string;
      unresolved_secret_refs?: string[];
    }
  >;
}

export interface EventEnvelope {
  id: string;
  seq: number;
  ts: string;
  type: string;
  subject: string;
  project_id: string | null;
  actor: {
    kind: "engine" | "guardian" | "user" | "device" | "forge" | "system";
    id: string;
  };
  causation_id: string | null;
  correlation_id: string | null;
  data: unknown;
}

export type { RefOption as CardOption };
