import type { EventEnvelope } from "@/api/types";

export type ResourceKey =
  | "projects"
  | "me"
  | "inbox"
  | "summary"
  | "approvals"
  | `messages:${string}`
  | `conversation:${string}`
  | `deliveries:${string}`
  | `worktrees:${string}`
  | `settings:${string}`
  | `approval:${string}`
  | `workflow:${string}`;

export interface EventLookup {
  currentProjectId: string | null;
  deviceId: string | null;
  projectForDelivery: (deliveryId: string) => string | null;
  deliveryForCorrelation: (correlationId: string | null) => string | null;
}

const KNOWN_TYPES = new Set<string>([
  "project.added",
  "project.updated",
  "settings.updated",
  "conversation.created",
  "delivery.created",
  "delivery.stage_changed",
  "delivery.level_changed",
  "delivery.abandoned",
  "delivery.paused",
  "delivery.resumed",
  "workflow.created",
  "workflow.revised",
  "workflow.finished",
  "node.queued",
  "node.started",
  "node.finished",
  "node.superseded",
  "node.retry_requested",
  "attempt.started",
  "attempt.finished",
  "attempt.progress",
  "patch.validated",
  "patch.rejected",
  "sandbox.created",
  "sandbox.destroyed",
  "worktree.created",
  "worktree.locked",
  "worktree.unlocked",
  "worktree.removed",
  "message.posted",
  "inbox.raised",
  "inbox.resolved",
  "inbox.dismissed",
  "guardian.triaged",
  "guardian.heal_requested",
  "guardian.escalated",
  "approval.requested",
  "approval.granted",
  "approval.consumed",
  "approval.invalidated",
  "approval.expired",
  "approval.denied",
  "outbox.enqueued",
  "outbox.executing",
  "outbox.succeeded",
  "outbox.failed",
  "outbox.cancelled",
  "forge.synced",
  "forge.drift_detected",
  "budget.charged",
  "budget.exhausted",
  "policy.denied",
  "device.paired",
  "device.updated",
  "device.revoked",
]);

export function isKnownEventType(type: string): boolean {
  if (KNOWN_TYPES.has(type)) return true;
  // 开放枚举：forge.<kind> 只作刷新提示。
  return type.startsWith("forge.");
}

/**
 * 事件只作 refetch 提示。这里只读取信封的 type / project_id / subject / correlation_id，
 * 绝不解释 data 内容；未知事件返回空数组，由调用方安全推进游标。
 */
export function resourcesForEvent(
  envelope: EventEnvelope,
  lookup: EventLookup,
): ResourceKey[] {
  const type = envelope.type;
  const projectId = envelope.project_id ?? lookup.currentProjectId;
  const current = lookup.currentProjectId;

  if (type === "project.added" || type === "project.updated") {
    return ["projects"];
  }

  if (type === "settings.updated") {
    return current ? [`settings:${current}`] : [];
  }

  if (type === "conversation.created") {
    return projectId
      ? [`conversation:${projectId}`, `messages:${projectId}`]
      : [];
  }

  if (type === "message.posted") {
    return projectId ? [`messages:${projectId}`] : [];
  }

  if (
    type === "delivery.created" ||
    type === "delivery.stage_changed" ||
    type === "delivery.level_changed" ||
    type === "delivery.abandoned" ||
    type === "delivery.paused" ||
    type === "delivery.resumed"
  ) {
    if (!projectId) return [];
    const keys: ResourceKey[] = [`deliveries:${projectId}`];
    if (type === "delivery.stage_changed" || type === "delivery.abandoned") {
      if (envelope.subject) keys.push(`workflow:${envelope.subject}`);
    }
    return keys;
  }

  if (
    type.startsWith("workflow.") ||
    type.startsWith("node.") ||
    type.startsWith("attempt.")
  ) {
    const deliveryId =
      lookup.deliveryForCorrelation(envelope.correlation_id) ??
      lookup.deliveryForCorrelation(envelope.subject);
    if (deliveryId) return [`workflow:${deliveryId}`];
    return projectId ? [`deliveries:${projectId}`] : [];
  }

  if (type.startsWith("worktree.")) {
    if (!projectId) return [];
    return [`worktrees:${projectId}`, `deliveries:${projectId}`];
  }

  if (
    type === "inbox.raised" ||
    type === "inbox.resolved" ||
    type === "inbox.dismissed"
  ) {
    return ["inbox", "summary"];
  }

  if (type.startsWith("approval.")) {
    const keys: ResourceKey[] = ["inbox", "summary", "approvals"];
    if (envelope.subject) keys.push(`approval:${envelope.subject}`);
    return keys;
  }

  if (type.startsWith("outbox.")) {
    if (!projectId) return ["approvals"];
    return [`deliveries:${projectId}`, "approvals"];
  }

  if (type.startsWith("forge.")) {
    if (!projectId) return ["inbox", "summary", "approvals"];
    return [`deliveries:${projectId}`, "inbox", "summary", "approvals"];
  }

  if (type === "device.updated" || type === "device.revoked" || type === "device.paired") {
    return lookup.deviceId && envelope.subject === lookup.deviceId
      ? ["me"]
      : [];
  }

  if (type.startsWith("budget.")) {
    return projectId ? [`deliveries:${projectId}`] : [];
  }

  if (
    type === "policy.denied" ||
    type === "patch.validated" ||
    type === "patch.rejected" ||
    type.startsWith("sandbox.") ||
    type.startsWith("guardian.")
  ) {
    // 服务端会同时写事件行消息；不需要额外刷新。
    return [];
  }

  return [];
}

export function settingsResourceFromEvent(envelope: EventEnvelope): string | null {
  if (envelope.type !== "settings.updated") return null;
  const data = envelope.data as { resource?: unknown } | null;
  return data && typeof data.resource === "string" ? data.resource : null;
}
