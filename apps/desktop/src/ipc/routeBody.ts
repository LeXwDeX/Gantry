import type { RouteId } from "@/api/routes";

/**
 * UI 侧为了可读性使用扁平参数；外壳的 HTTP 请求体是每个写路由自己的 JSON。
 * 这里集中把扁平参数映射为协议请求体，避免经过 api_request 时丢字段。
 */
export function httpBody(
  route: RouteId,
  args: unknown,
  explicitBody?: unknown,
): unknown | undefined {
  const a = (args ?? {}) as Record<string, unknown>;
  switch (route) {
    case "messages.post":
      return explicitBody !== undefined ? explicitBody : a.body;
    case "deliveries.patch":
      return { level_cap: a.levelCap };
    case "deliveries.abandon":
      return a.reason !== undefined ? { reason: a.reason } : {};
    case "deliveries.pause":
    case "deliveries.resume":
    case "nodes.retry":
      return {};
    case "worktrees.lock_put":
      return a.leaseSeconds !== undefined
        ? { lease_seconds: a.leaseSeconds }
        : {};
    case "inbox.dismiss":
      return a.note !== undefined ? { note: a.note } : {};
    case "approvals.deny":
      return a.reason !== undefined ? { reason: a.reason } : {};
    case "inbox.choose": {
      const body: Record<string, unknown> = { option_id: a.optionId };
      if (a.note !== undefined) body.note = a.note;
      return body;
    }
    case "devices.rename":
      return { name: a.name };
    default:
      return explicitBody;
  }
}
