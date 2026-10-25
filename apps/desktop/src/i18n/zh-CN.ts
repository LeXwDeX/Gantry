/** 中文错误与界面词表。只展示本地化后的 `code`，从不展示服务端 `detail`。 */

export const ERROR_MESSAGES: Record<string, string> = {
  invalid_request: "请求无效，请更新客户端后重试。",
  idempotency_key_required: "请求缺少幂等键，属于客户端错误。",
  invalid_cursor: "分页游标无效，列表已从头载入。",
  device_revoked: "此设备已被吊销，需要重新配对。",
  pair_token_invalid: "配对串无效、已使用或已过期，请在服务端重新生成。",
  forbidden_role: "当前设备角色不足。",
  forbidden_profile: "此客户端档案不支持该操作。",
  signature_invalid: "审批签名无效，请重新载入审批单。",
  signature_required: "此写入需要审批设备签名。",
  not_found: "资源不存在或已被移除。",
  method_not_allowed: "请求方法不被支持。",
  conflict: "状态已变化，请刷新后重试。",
  idempotency_in_progress: "上一次请求仍在处理，稍后自动重试。",
  worktree_locked: "工作树被其他持有者锁定。",
  approval_not_pending: "此审批已不在待签状态。",
  challenge_invalid: "确认窗口已失效，请重新载入审批单。",
  stale_head: "代码在你确认期间已更新，审批已失效。",
  approval_mismatch: "审批与本次写入不匹配，请重新发起。",
  cursor_ahead: "事件游标超前，界面将重新同步。",
  cursor_expired: "事件游标已过期，界面将重新同步。",
  precondition_failed: "此页已被其他设备修改，请载入最新值。",
  payload_too_large: "内容过大，无法提交。",
  unsupported_media_type: "不支持的内容类型。",
  validation_failed: "提交内容未通过校验。",
  not_overridable: "该设置项不能按项目覆盖。",
  idempotency_key_reuse: "幂等键被用于不同内容，属于客户端错误。",
  precondition_required: "缺少版本前置条件，属于客户端错误。",
  rate_limited: "服务端繁忙，已按建议时间退避。",
  internal: "服务端内部错误，稍后重试。",
  unavailable: "服务端暂不可用，稍后自动重试。",
  mobile_cannot_approve: "手机档案不能成为审批设备。",
  self_role_change: "不能修改自己的设备角色。",
  override_loosens_global: "项目覆盖不能放宽全局安全限制。",
  secret_ref_missing: "凭据引用不存在，请填写新值。",
  secret_clear_in_use: "使用中的凭据不能被清空。",
  "shell.not_paired": "设备尚未配对，请完成配对。",
  "shell.not_connected": "连接中断，暂不能执行此操作。",
  "shell.bad_argument": "请求参数不合法。",
  "shell.unknown_route": "客户端请求了未知路由。",
  "shell.too_large": "本机界面状态超过大小限制。",
  "shell.user_cancelled": "操作已取消。",
  "shell.presence_failed": "本人在场验证未通过。",
  "shell.signing_busy": "另一个签名确认正在进行。",
  "shell.prepared_expired": "签名句柄已过期，请重新载入。",
  "shell.self_check_failed": "服务端数据不一致，无法签名。",
  "shell.key_unavailable": "审批签名密钥不可用，请检查设备密钥或重新配对。",
  "shell.unavailable": "桌面外壳未连接：请在 Tauri 应用内运行。",
};

export function errorMessage(code: string | null | undefined): string {
  if (!code) return "操作失败，请稍后重试。";
  return ERROR_MESSAGES[code] ?? "操作失败，请稍后重试。";
}

export function conflictReasonMessage(reason: string): string {
  switch (reason) {
    case "self_role_change":
      return "不能修改自己的设备角色。";
    case "mobile_cannot_approve":
      return "手机档案不能成为审批设备。";
    case "invalid_state":
      return "当前状态不允许此操作。";
    case "worktree_locked":
      return "工作树被其他持有者锁定。";
    default:
      return "状态已变化，请刷新后重试。";
  }
}

export function settingsIssueMessage(code: string): string {
  switch (code) {
    case "required":
    case "type":
    case "enum":
    case "range":
    case "unknown_field":
      return "取值为空、类型不符或不在允许范围内。";
    case "override_not_allowed":
      return "该键不能按项目覆盖。";
    case "override_loosens_global":
      return "项目覆盖只能收紧，不能放宽。";
    case "secret_ref_missing":
      return "凭据引用不存在，请填写新值。";
    case "secret_clear_in_use":
      return "使用中的凭据不能被清空。";
    case "scenario_mode":
      return "示例模式不提交、不保存。";
    default:
      return "未通过校验。";
  }
}

const ROLE_LABELS: Record<string, string> = {
  approver: "审批设备",
  operator: "操作设备",
  viewer: "只读设备",
};

export function roleLabel(role: string | null | undefined): string {
  return role ? ROLE_LABELS[role] ?? role : "未知角色";
}

const PHASE_LABELS: Record<string, string> = {
  unpaired: "未配对",
  connecting: "连接中",
  live: "已连接",
  reconnecting: "连接中断 · 重连中",
  handshake_failed: "无法建立安全连接",
  revoked: "此设备已被吊销",
  cert_expired: "证书已过期",
};

export function phaseLabel(phase: string | null | undefined): string {
  return phase ? PHASE_LABELS[phase] ?? phase : "未知连接状态";
}

const STAGE_LABELS: Record<string, string> = {
  spec: "规格",
  plan: "计划",
  implement: "实现",
  propose: "提出",
  review: "评审",
  merge: "合并",
  closed: "关闭",
  abandoned: "放弃",
};

export function stageLabel(stage: string): string {
  return STAGE_LABELS[stage] ?? stage;
}

const STAGE_STATUS_LABELS: Record<string, string> = {
  running: "运行中",
  waiting_human: "需要你",
  blocked: "受阻",
  paused: "已暂停",
  done: "已完成",
};

export function stageStatusLabel(status: string): string {
  return STAGE_STATUS_LABELS[status] ?? status;
}

const AGENT_LABELS: Record<string, string> = {
  planner: "大脑",
  worker: "干活",
  utility: "打杂",
};

export function agentKindLabel(kind: string | null | undefined): string {
  return kind ? AGENT_LABELS[kind] ?? "agent" : "agent";
}

const KIND_LABELS: Record<string, string> = {
  approval_needed: "签名审批",
  decision_needed: "待决定",
  spec_needed: "补规格",
  budget_exhausted: "预算耗尽",
  escalation: "升档",
  security_finding: "安全发现",
};

export function inboxKindLabel(kind: string): string {
  return KIND_LABELS[kind] ?? "上报";
}

export function approvalActionLabel(action: string): string {
  switch (action) {
    case "merge_pr":
      return "合并";
    case "force_push":
      return "强制推送";
    case "push_outside_prefix":
      return "推送到受保护前缀之外";
    case "resolve_review_thread_human":
      return "解决人工评审线程";
    case "patch_apply_protected":
      return "应用受保护补丁";
    case "settings_update":
      return "修改设置";
    case "device_role_change":
      return "修改设备角色";
    case "device_revoke":
      return "吊销设备";
    default:
      return action;
  }
}
