import type { InboxItem } from "@/api/types";
import type { DesktopState } from "./state";

export interface NeedsYouProjection {
  byProject: Map<string | null, InboxItem[]>;
  byDelivery: Map<string, InboxItem[]>;
  total: number;
}

/**
 * 单一投影：所有“需要你”表面只读它。
 * 只统计 status = open；全局事项的 project_id 为 null。
 */
export function needsYou(
  state: Pick<DesktopState, "inbox">,
): NeedsYouProjection {
  const byProject = new Map<string | null, InboxItem[]>();
  const byDelivery = new Map<string, InboxItem[]>();
  let total = 0;
  for (const item of state.inbox) {
    if (item.status !== "open") continue;
    total += 1;
    const projectItems = byProject.get(item.project_id);
    if (projectItems) projectItems.push(item);
    else byProject.set(item.project_id, [item]);
    if (item.delivery_id) {
      const deliveryItems = byDelivery.get(item.delivery_id);
      if (deliveryItems) deliveryItems.push(item);
      else byDelivery.set(item.delivery_id, [item]);
    }
  }
  return { byProject, byDelivery, total };
}

export function openItem(
  state: Pick<DesktopState, "inbox">,
  inboxId: string,
): InboxItem | null {
  return (
    state.inbox.find((item) => item.id === inboxId && item.status === "open") ??
    null
  );
}
