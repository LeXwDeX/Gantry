/**
 * 向 Mock 注入事件时使用的合法 data（unit 与 e2e 共用）。
 *
 * Mock 会按 schemas/events/<type>.json 校验注入的 data；这里的模板取自
 * crates/gantry-core/fixtures/events/cases.json 的正例，只替换与夹具相关的 ID 和值。
 */
import type { Message } from "@/api/types";

export const WEB = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
export const CONVERSATION = "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC";
export const DELIVERY = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
export const APPROVAL = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
export const APPROVAL_INBOX = "inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ";
export const ESCALATION_INBOX = "inb_01JB7P3M7S1T4X6N9Q2V5W8YZK";
export const SPEC_INBOX = "inb_01JB7P4N8T2X5V7P9Q3W6X0YZM";
export const SELF = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
export const PR_HEAD = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";

export interface InjectedEvent {
  type: string;
  subject: string;
  data: unknown;
  project_id?: string | null;
  correlation_id?: string | null;
  actor?: { kind: string; id: string };
}

const forgeActor = { login: "alice", github_id: 7, type: "user", association: "OWNER" };

function forgeEnvelopeData(kind: string, data: Record<string, unknown>, number = 12) {
  return {
    schema: "forge.event.v1",
    kind,
    dedup_key: `dedup:${kind}:${Date.now()}`,
    source: "webhook",
    delivery_ids: [],
    project_id: WEB,
    repo: "acme/web",
    installation_id: 1,
    delivery_id: DELIVERY,
    target: { type: "pr", number, node_id: "PR_x" },
    actor: forgeActor,
    origin: "external",
    caused_by_action: null,
    occurred_at: "2026-10-24T12:41:03.120Z",
    observed_at: "2026-10-24T12:41:03.120Z",
    stale: false,
    data,
  };
}

export const events = {
  messagePosted(message: Message, projectId: string = WEB): InjectedEvent {
    return {
      type: "message.posted",
      subject: message.id,
      data: message,
      project_id: projectId,
      correlation_id: message.conversation_id,
    };
  },
  inboxRaised(item: Record<string, unknown> & { id: string }): InjectedEvent {
    return {
      type: "inbox.raised",
      subject: item.id,
      data: { item },
      project_id: (item.project_id as string | null) ?? null,
      correlation_id: (item.delivery_id as string | null) ?? item.id,
    };
  },
  inboxResolved(inboxId: string, resolution: string, extra: Record<string, unknown> = {}): InjectedEvent {
    return {
      type: "inbox.resolved",
      subject: inboxId,
      data: { inbox_id: inboxId, resolution, ...extra },
      project_id: WEB,
      correlation_id: DELIVERY,
    };
  },
  forgeSynced(changes: Array<{ field: string; from: unknown; to: unknown }>): InjectedEvent {
    return {
      type: "forge.synced",
      subject: DELIVERY,
      data: {
        target: { type: "pr", number: 12, node_id: "PR_x" },
        source: "reconcile",
        changes,
        synced_at: "2026-10-24T12:41:03.120Z",
      },
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "forge", id: "forge" },
    };
  },
  approvalInvalidated(previous: string, current: string): InjectedEvent {
    return {
      type: "approval.invalidated",
      subject: APPROVAL,
      data: { reason: "head_changed", previous_head_sha: previous, current_head_sha: current },
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "engine", id: "engine" },
    };
  },
  approvalConsumed(consumedBy: string): InjectedEvent {
    return {
      type: "approval.consumed",
      subject: APPROVAL,
      data: { approval_id: APPROVAL, consumed_by: consumedBy, subject_digest: PR_HEAD },
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "engine", id: "engine" },
    };
  },
  stageChanged(from: string, to: string, stageStatus = "running"): InjectedEvent {
    return {
      type: "delivery.stage_changed",
      subject: DELIVERY,
      data: { from, to, stage_status: stageStatus },
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "engine", id: "engine" },
    };
  },
  prMerged(mergeSha: string): InjectedEvent {
    return {
      type: "forge.pr_merged",
      subject: DELIVERY,
      data: forgeEnvelopeData("pr_merged", {
        merge_commit_sha: mergeSha,
        head_sha: PR_HEAD,
        base_ref: "main",
        merged_by: forgeActor,
        via_merge_queue: false,
        merge_method: "squash",
      }),
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "forge", id: "forge" },
    };
  },
  issueClosed(number: number): InjectedEvent {
    return {
      type: "forge.issue_closed",
      subject: DELIVERY,
      data: forgeEnvelopeData(
        "issue_closed",
        { number, state_reason: "completed", closed_by: forgeActor, closed_via_pr: 12 },
        number,
      ),
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "forge", id: "forge" },
    };
  },
  policyDenied(reasonCode: string): InjectedEvent {
    return {
      type: "policy.denied",
      subject: DELIVERY,
      data: {
        origin: "action",
        action_id: "push_branch",
        kind: "push_branch",
        target: { kind: "branch", repo: "acme/web", branch: "gantry/12-login-timeout" },
        reason_code: reasonCode,
        subject: "d7d018f867d6c5b78e82906386420d45a4d84486",
        config_rev: 18240,
      },
      project_id: WEB,
      correlation_id: DELIVERY,
      actor: { kind: "system", id: "policy" },
    };
  },
};

/** 一条可推入 Mock `messages` 集合的消息；ID 按时间递增，排在夹具消息之后。 */
export function mockMessage(seq: number, overrides: Partial<Message> = {}): Message {
  const id = `msg_01JB7P${String(seq).padStart(20, "0")}`;
  const ts = new Date(Date.parse("2026-10-24T12:45:00.000Z") + seq * 1000).toISOString();
  return {
    id,
    conversation_id: CONVERSATION,
    role: "agent",
    agent_kind: "planner",
    tone: null,
    text: `消息 ${seq}`,
    level_cap: null,
    refs: [],
    cards: [],
    delivery_id: null,
    created_at: ts,
    updated_at: ts,
    ...overrides,
  } as Message;
}
