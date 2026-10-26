import type { DesktopState } from "./state";

/** 界面角色门槛矩阵（desktop-ui §7.13）。只用于体验；服务端仍会 403。 */
export type Role = "viewer" | "operator" | "approver";

export type Action =
  | "browse"
  | "send"
  | "choose"
  | "dismiss"
  | "control"
  | "retry"
  | "renameDevice"
  | "readSettings"
  | "writeSettings"
  | "sign"
  | "deny"
  | "changeRole"
  | "revoke";

const OPERATOR_ACTIONS: ReadonlySet<Action> = new Set([
  "send",
  "choose",
  "dismiss",
  "control",
  "retry",
  "renameDevice",
  "readSettings",
  "writeSettings",
]);

const APPROVER_ACTIONS: ReadonlySet<Action> = new Set([
  "sign",
  "deny",
  "changeRole",
  "revoke",
]);

export function can(
  role: Role | null | undefined,
  action: Action,
): boolean {
  if (!role) return false;
  if (action === "browse") return true;
  if (role === "approver") return true;
  if (role === "operator") return !APPROVER_ACTIONS.has(action);
  return !OPERATOR_ACTIONS.has(action) && !APPROVER_ACTIONS.has(action);
}
export function canAct(
  state: Pick<DesktopState, "me">,
  action: Action,
): boolean {
  return can(state.me?.device.role, action);
}
