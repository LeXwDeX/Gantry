/**
 * `api_request` 的 36 条 UI 可调路由。
 *
 * 来源：docs/modules/desktop-shell.md §6.4、docs/interfaces/api.md §4.1。
 * 编号 #2–#25、#29、#30、#32、#35–#38、#42–#46（共 36 条）。
 * 手工转写；契约 schema 到位后以生成物为准。
 */
import type {
  ApiList,
  Approval,
  Attempt,
  BudgetLedger,
  Conversation,
  Delivery,
  Device,
  InboxItem,
  InboxSummary,
  Me,
  Message,
  NodeStatus,
  Project,
  SettingsCatalog,
  SettingsDocument,
  SettingsStatus,
  Stage,
  StageStatus,
  Workflow,
  WorkflowNode,
  Worktree,
  Level,
} from "./types";
import type { MessagesPostBody } from "./shell";

export type RouteId =
  | "me.get"
  | "projects.list"
  | "projects.get"
  | "deliveries.list"
  | "deliveries.get"
  | "deliveries.patch"
  | "deliveries.abandon"
  | "deliveries.budget"
  | "conversations.list"
  | "messages.list"
  | "messages.post"
  | "worktrees.list"
  | "worktrees.get"
  | "worktrees.lock_put"
  | "worktrees.lock_delete"
  | "workflows.list"
  | "workflows.get"
  | "attempts.list"
  | "attempts.get"
  | "inbox.list"
  | "inbox.get"
  | "inbox.dismiss"
  | "approvals.list"
  | "approvals.get"
  | "approvals.deny"
  | "settings.get"
  | "project_settings.get"
  | "settings.catalog"
  | "settings.status"
  | "devices.list"
  | "devices.rename"
  | "inbox.choose"
  | "deliveries.pause"
  | "deliveries.resume"
  | "nodes.retry"
  | "inbox.summary";

export type RouteQuery = Record<
  string,
  string | number | boolean | Array<string | number> | undefined
>;

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export type RouteRole = "viewer" | "operator" | "approver";

export interface RouteMeta<A> {
  readonly method: HttpMethod;
  readonly path: (a: A) => string;
  readonly query?: (a: A) => RouteQuery;
  readonly role: RouteRole;
  readonly idempotent?: boolean;
}

type Empty = Record<string, never>;

export interface PageArgs {
  cursor?: string;
  limit?: number;
  order?: "asc" | "desc";
}

export type RouteArgs = {
  "me.get": Empty;
  "projects.list": PageArgs;
  "projects.get": { projectId: string };
  "deliveries.list": PageArgs & {
    projectId?: string;
    stage?: Stage[];
    stageStatus?: StageStatus;
    active?: boolean;
  };
  "deliveries.get": { deliveryId: string };
  "deliveries.patch": { deliveryId: string; levelCap: Level };
  "deliveries.abandon": { deliveryId: string; reason?: string };
  "deliveries.budget": { deliveryId: string };
  "conversations.list": PageArgs & { projectId: string };
  "messages.list": PageArgs & { conversationId: string };
  "messages.post": {
    conversationId: string;
    body: MessagesPostBody;
    idempotencyKey: string;
  };
  "worktrees.list": PageArgs & { projectId?: string; deliveryId?: string };
  "worktrees.get": { worktreeId: string };
  "worktrees.lock_put": { worktreeId: string; leaseSeconds?: number };
  "worktrees.lock_delete": { worktreeId: string };
  "workflows.list": PageArgs & { deliveryId: string; stage?: Stage };
  "workflows.get": { workflowId: string; includeSuperseded?: boolean };
  "attempts.list": PageArgs & { nodeId: string };
  "attempts.get": { attemptId: string };
  "inbox.list": PageArgs & {
    projectId?: string;
    status?: "open" | "resolved" | "dismissed";
    kind?: string;
  };
  "inbox.get": { inboxId: string };
  "inbox.dismiss": { inboxId: string; note?: string };
  "approvals.list": PageArgs & { status?: string; deliveryId?: string };
  "approvals.get": { approvalId: string };
  "approvals.deny": {
    approvalId: string;
    reason?: string;
    idempotencyKey: string;
  };
  "settings.get": { resource: string; rev?: number };
  "project_settings.get": {
    projectId: string;
    resource: string;
    rev?: number;
    view?: "effective";
  };
  "settings.catalog": Empty;
  "settings.status": Empty;
  "devices.list": PageArgs;
  "devices.rename": { deviceId: string; name: string };
  "inbox.choose": {
    inboxId: string;
    optionId: string;
    note?: string;
    idempotencyKey: string;
  };
  "deliveries.pause": { deliveryId: string; idempotencyKey: string };
  "deliveries.resume": { deliveryId: string; idempotencyKey: string };
  "nodes.retry": { nodeId: string; idempotencyKey: string };
  "inbox.summary": Empty;
};

/**
 * 响应体映射。本基线实际消费的路由给出精确类型；
 * 尚未接入界面的路由先保留为 unknown，不猜字段。
 */
export type RouteBody = {
  "me.get": Me;
  "projects.list": ApiList<Project>;
  "projects.get": Project;
  "deliveries.list": ApiList<Delivery>;
  "deliveries.get": Delivery;
  "deliveries.patch": Delivery;
  "deliveries.abandon": Delivery;
  "deliveries.budget": BudgetLedger;
  "conversations.list": ApiList<Conversation>;
  "messages.list": ApiList<Message>;
  "messages.post": Message;
  "worktrees.list": ApiList<Worktree>;
  "worktrees.get": Worktree;
  "worktrees.lock_put": Worktree;
  "worktrees.lock_delete": Worktree;
  "workflows.list": ApiList<Workflow>;
  "workflows.get": Workflow;
  "attempts.list": ApiList<Attempt>;
  "attempts.get": Attempt;
  "inbox.list": ApiList<InboxItem>;
  "inbox.get": InboxItem;
  "inbox.dismiss": InboxItem;
  "approvals.list": ApiList<Approval>;
  "approvals.get": Approval;
  "approvals.deny": Approval;
  "settings.get": SettingsDocument;
  "project_settings.get": SettingsDocument;
  "settings.catalog": SettingsCatalog;
  "settings.status": SettingsStatus;
  "devices.list": ApiList<Device>;
  "devices.rename": Device;
  "inbox.choose": InboxItem;
  "deliveries.pause": Delivery;
  "deliveries.resume": Delivery;
  "nodes.retry": WorkflowNode & { status: NodeStatus };
  "inbox.summary": InboxSummary;
};

const page = (a: PageArgs): RouteQuery => ({
  cursor: a.cursor,
  limit: a.limit,
  order: a.order,
});

export const ROUTES: { [R in RouteId]: RouteMeta<RouteArgs[R]> } = {
  "me.get": { method: "GET", path: () => "/v1/me", role: "viewer" },
  "projects.list": {
    method: "GET",
    path: () => "/v1/projects",
    query: page,
    role: "viewer",
  },
  "projects.get": {
    method: "GET",
    path: (a) => `/v1/projects/${a.projectId}`,
    role: "viewer",
  },
  "deliveries.list": {
    method: "GET",
    path: () => "/v1/deliveries",
    query: (a) => ({
      project_id: a.projectId,
      stage: a.stage,
      stage_status: a.stageStatus,
      active: a.active,
      ...page(a),
    }),
    role: "viewer",
  },
  "deliveries.get": {
    method: "GET",
    path: (a) => `/v1/deliveries/${a.deliveryId}`,
    role: "viewer",
  },
  "deliveries.patch": {
    method: "PATCH",
    path: (a) => `/v1/deliveries/${a.deliveryId}`,
    role: "operator",
  },
  "deliveries.abandon": {
    method: "POST",
    path: (a) => `/v1/deliveries/${a.deliveryId}/abandon`,
    role: "operator",
  },
  "deliveries.budget": {
    method: "GET",
    path: (a) => `/v1/deliveries/${a.deliveryId}/budget`,
    role: "viewer",
  },
  "conversations.list": {
    method: "GET",
    path: (a) => `/v1/projects/${a.projectId}/conversations`,
    query: page,
    role: "viewer",
  },
  "messages.list": {
    method: "GET",
    path: (a) => `/v1/conversations/${a.conversationId}/messages`,
    query: page,
    role: "viewer",
  },
  "messages.post": {
    method: "POST",
    path: (a) => `/v1/conversations/${a.conversationId}/messages`,
    role: "operator",
    idempotent: true,
  },
  "worktrees.list": {
    method: "GET",
    path: () => "/v1/worktrees",
    query: (a) => ({
      project_id: a.projectId,
      delivery_id: a.deliveryId,
      ...page(a),
    }),
    role: "viewer",
  },
  "worktrees.get": {
    method: "GET",
    path: (a) => `/v1/worktrees/${a.worktreeId}`,
    role: "viewer",
  },
  "worktrees.lock_put": {
    method: "PUT",
    path: (a) => `/v1/worktrees/${a.worktreeId}/lock`,
    role: "operator",
  },
  "worktrees.lock_delete": {
    method: "DELETE",
    path: (a) => `/v1/worktrees/${a.worktreeId}/lock`,
    role: "operator",
  },
  "workflows.list": {
    method: "GET",
    path: (a) => `/v1/deliveries/${a.deliveryId}/workflows`,
    query: (a) => ({ stage: a.stage, ...page(a) }),
    role: "viewer",
  },
  "workflows.get": {
    method: "GET",
    path: (a) => `/v1/workflows/${a.workflowId}`,
    query: (a) => ({ include_superseded: a.includeSuperseded }),
    role: "viewer",
  },
  "attempts.list": {
    method: "GET",
    path: (a) => `/v1/nodes/${a.nodeId}/attempts`,
    query: page,
    role: "viewer",
  },
  "attempts.get": {
    method: "GET",
    path: (a) => `/v1/attempts/${a.attemptId}`,
    role: "viewer",
  },
  "inbox.list": {
    method: "GET",
    path: () => "/v1/inbox",
    query: (a) => ({
      project_id: a.projectId,
      status: a.status,
      kind: a.kind,
      ...page(a),
    }),
    role: "viewer",
  },
  "inbox.get": {
    method: "GET",
    path: (a) => `/v1/inbox/${a.inboxId}`,
    role: "viewer",
  },
  "inbox.dismiss": {
    method: "POST",
    path: (a) => `/v1/inbox/${a.inboxId}/dismiss`,
    role: "operator",
  },
  "approvals.list": {
    method: "GET",
    path: () => "/v1/approvals",
    query: (a) => ({
      status: a.status,
      delivery_id: a.deliveryId,
      ...page(a),
    }),
    role: "viewer",
  },
  "approvals.get": {
    method: "GET",
    path: (a) => `/v1/approvals/${a.approvalId}`,
    role: "viewer",
  },
  "approvals.deny": {
    method: "POST",
    path: (a) => `/v1/approvals/${a.approvalId}/deny`,
    role: "approver",
    idempotent: true,
  },
  "settings.get": {
    method: "GET",
    path: (a) => `/v1/settings/${a.resource}`,
    query: (a) => ({ rev: a.rev }),
    role: "operator",
  },
  "project_settings.get": {
    method: "GET",
    path: (a) => `/v1/projects/${a.projectId}/settings/${a.resource}`,
    query: (a) => ({ rev: a.rev, view: a.view }),
    role: "operator",
  },
  "settings.catalog": {
    method: "GET",
    path: () => "/v1/settings/catalog",
    role: "operator",
  },
  "settings.status": {
    method: "GET",
    path: () => "/v1/settings/status",
    role: "operator",
  },
  "devices.list": {
    method: "GET",
    path: () => "/v1/devices",
    query: page,
    role: "viewer",
  },
  "devices.rename": {
    method: "PATCH",
    path: (a) => `/v1/devices/${a.deviceId}`,
    role: "operator",
  },
  "inbox.choose": {
    method: "POST",
    path: (a) => `/v1/inbox/${a.inboxId}/choose`,
    role: "operator",
    idempotent: true,
  },
  "deliveries.pause": {
    method: "POST",
    path: (a) => `/v1/deliveries/${a.deliveryId}/pause`,
    role: "operator",
    idempotent: true,
  },
  "deliveries.resume": {
    method: "POST",
    path: (a) => `/v1/deliveries/${a.deliveryId}/resume`,
    role: "operator",
    idempotent: true,
  },
  "nodes.retry": {
    method: "POST",
    path: (a) => `/v1/nodes/${a.nodeId}/retry`,
    role: "operator",
    idempotent: true,
  },
  "inbox.summary": {
    method: "GET",
    path: () => "/v1/inbox/summary",
    role: "viewer",
  },
};

export const ROUTE_IDS = Object.keys(ROUTES) as RouteId[];

export function isRouteId(value: string): value is RouteId {
  return Object.prototype.hasOwnProperty.call(ROUTES, value);
}
