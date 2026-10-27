import type {
  Delivery,
  InboxItem,
  Message,
  Workflow,
  WorkflowSummary,
  Worktree,
} from "@/api/types";

/** 合并最新一页消息：用同 id 的新版本覆盖旧值，新 id 追加到末尾。 */
export function mergeMessages(
  existing: Message[],
  incoming: Message[],
  limit = 1000,
): Message[] {
  const incomingById = new Map(incoming.map((message) => [message.id, message]));
  const existingIds = new Set(existing.map((message) => message.id));
  const merged = existing.map((message) => incomingById.get(message.id) ?? message);
  for (const message of incoming) {
    if (!existingIds.has(message.id)) merged.push(message);
  }
  return merged.length > limit ? merged.slice(merged.length - limit) : merged;
}

export function mergeDeliveries(
  existing: Delivery[],
  incoming: Delivery[],
): Delivery[] {
  const byId = new Map(existing.map((delivery) => [delivery.id, delivery]));
  for (const delivery of incoming) byId.set(delivery.id, delivery);
  return [...byId.values()];
}

export function mergeWorktrees(
  existing: Worktree[],
  incoming: Worktree[],
): Worktree[] {
  const byId = new Map(existing.map((worktree) => [worktree.id, worktree]));
  for (const worktree of incoming) byId.set(worktree.id, worktree);
  return [...byId.values()];
}

export function mergeInbox(
  existing: InboxItem[],
  incoming: InboxItem[],
): InboxItem[] {
  const byId = new Map(existing.map((item) => [item.id, item]));
  for (const item of incoming) byId.set(item.id, item);
  return [...byId.values()];
}

export function mergeWorkflow(
  existing: Record<string, Workflow>,
  workflow: Workflow,
): Record<string, Workflow> {
  return { ...existing, [workflow.id]: workflow };
}

export function latestWorkflow<T extends WorkflowSummary>(workflows: T[]): T | null {
  if (workflows.length === 0) return null;
  return workflows.reduce((latest, item) =>
    item.graph_rev > latest.graph_rev ? item : latest,
  );
}

export function uniqueById<T extends { id: string }>(items: T[]): T[] {
  const byId = new Map(items.map((item) => [item.id, item]));
  return [...byId.values()];
}

export function removeInboxItem(items: InboxItem[], id: string): InboxItem[] {
  return items.filter((item) => item.id !== id);
}
