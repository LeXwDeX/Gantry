import type {
  Approval,
  ApprovalStatus,
  Delivery,
  PullRequestRef,
  Ref,
} from "@/api/types";
import type {
  ApprovalSheetModel,
  ConnectionState,
  SignBlockReason,
} from "@/api/shell";
import type { Me } from "@/api/types";
import type { Tone } from "@/theme/tone";
import { approvalActionLabel } from "@/i18n";

export interface SignAvailabilityInput {
  conn: ConnectionState;
  me: Me;
  approval: Approval;
  pr: PullRequestRef | null;
  sheet: ApprovalSheetModel | null;
  /** 校正后的服务端时间（毫秒）。 */
  now: number;
  approverDeviceName?: string;
}

export type SignAvailability =
  | { ok: true }
  | {
      ok: false;
      reason: SignBlockReason;
      gates?: string[];
      approverDeviceName?: string;
    };

export interface MergeGateIssue {
  code:
    | "checks"
    | "mergeable"
    | "draft"
    | "state"
    | "review"
    | "signature";
  label: string;
}

/**
 * GitHub 合并闸门的 M1 临时规则（desktop-ui §7.8、OQ-U4）。
 * 界面只据此禁用按钮；服务端会在 grant 与消费时独立复核。
 */
export function mergeGateIssues(pr: PullRequestRef): MergeGateIssue[] {
  const issues: MergeGateIssue[] = [];
  if (!pr.checks.required_ok) {
    issues.push({ code: "checks", label: "必需 checks 未通过" });
  }
  if (
    ["dirty", "blocked", "behind", "draft", "unknown"].includes(
      pr.mergeable_state,
    )
  ) {
    issues.push({
      code: "mergeable",
      label: `GitHub 合并状态为 ${pr.mergeable_state}`,
    });
  }
  if (pr.draft) issues.push({ code: "draft", label: "PR 仍是草稿" });
  if (pr.state !== "open") {
    issues.push({ code: "state", label: "PR 已不是打开状态" });
  }
  if (pr.review_state === "changes_requested") {
    issues.push({ code: "review", label: "GitHub review 要求修改" });
  }
  return dedupe(issues);
}

function dedupe(items: MergeGateIssue[]): MergeGateIssue[] {
  const seen = new Set<string>();
  return items.filter((item) => {
    if (seen.has(item.code)) return false;
    seen.add(item.code);
    return true;
  });
}

/** 签名可用性；第一个命中的条件生效。 */
export function signAvailability(input: SignAvailabilityInput): SignAvailability {
  const { conn, me, approval, pr, sheet, now, approverDeviceName } = input;

  if (conn.phase !== "live") return { ok: false, reason: "offline" };

  if (me.profile !== "desktop" || me.device.role !== "approver") {
    return {
      ok: false,
      reason: "not_approver",
      approverDeviceName: approverDeviceName ?? "审批设备",
    };
  }

  if (approval.status !== "pending") {
    return { ok: false, reason: "not_pending" };
  }

  if (approval.target.kind === "pr") {
    if (!pr) return { ok: false, reason: "self_check_failed" };
    if (pr.head_sha !== approval.subject_digest) {
      return { ok: false, reason: "stale" };
    }
    const gates = mergeGateIssues(pr);
    if (gates.length > 0) {
      return {
        ok: false,
        reason: "gates_unmet",
        gates: gates.map((gate) => gate.label),
      };
    }
  }

  if (!sheet) return { ok: false, reason: "expired" };

  if (
    sheet.signed.tech.approval_id !== approval.id ||
    sheet.signed.subject_digest !== approval.subject_digest
  ) {
    return { ok: false, reason: "self_check_failed" };
  }

  if (Date.parse(sheet.challenge_expires_at) <= now) {
    return { ok: false, reason: "expired" };
  }

  if (!sheet.can_sign.ok) {
    switch (sheet.can_sign.reason) {
      case "not_connected":
        return { ok: false, reason: "offline" };
      case "not_approver":
        return {
          ok: false,
          reason: "not_approver",
          approverDeviceName: approverDeviceName ?? "审批设备",
        };
      case "not_pending":
        return { ok: false, reason: "not_pending" };
      case "expired":
        return { ok: false, reason: "expired" };
      case "self_check_failed":
        return { ok: false, reason: "self_check_failed" };
      default:
        return { ok: false, reason: "self_check_failed" };
    }
  }

  return { ok: true };
}

export function signBlockText(
  result: Extract<SignAvailability, { ok: false }>,
): string {
  switch (result.reason) {
    case "offline":
      return "连接中断，暂不能签名。";
    case "not_approver":
      return `此设备不能审批：请在审批设备（${result.approverDeviceName ?? "审批设备"}）上签名。`;
    case "not_pending":
      return "此审批已不在待签状态。";
    case "stale":
      return "代码已更新，审批即将失效。";
    case "gates_unmet":
      return result.gates && result.gates.length > 0
        ? `先在 GitHub 上完成必需的 review 与 checks，才能签名：${result.gates.join("；")}。`
        : "先在 GitHub 上完成必需的 review 与 checks，才能签名。";
    case "expired":
      return "已过期 · 重新载入。";
    case "self_check_failed":
      return "服务端数据不一致，无法签名。";
  }
}

export function signButtonLabel(platform: string | null | undefined): string {
  if (platform === "macos") return "用 Touch ID 签名批准";
  if (platform === "windows") return "用 Windows Hello 签名批准";
  return "签名批准";
}

export function shortSha(sha: string | null | undefined, length = 7): string {
  if (!sha) return "";
  return sha.slice(0, length);
}

export function approvalActionText(action: string): string {
  return approvalActionLabel(action);
}

export function approvalTargetLabel(
  target: Approval["target"],
): string {
  switch (target.kind) {
    case "pr":
      return `${target.repo} · PR #${target.number}`;
    case "branch":
      return `${target.repo} · ${target.branch}`;
    case "review_thread":
      return `${target.repo} · PR #${target.number} · 线程`;
    case "patch":
      return `${target.repo} · 补丁 ${target.attempt_id}`;
    case "settings":
      return `${target.resource}（${target.scope}）`;
    case "device":
      return `设备 ${target.id}`;
  }
}

export interface ApprovalStatusText {
  label: string;
  tone: Tone;
}

/** 一个函数生成审批状态文案；各处只读它。 */
export function approvalStatusLabel(
  approval: Approval,
  options: {
    prState?: PullRequestRef["state"] | null;
    deliveryStage?: Delivery["stage"] | null;
    headSha?: string | null;
  } = {},
): ApprovalStatusText {
  switch (approval.status) {
    case "pending": {
      if (
        approval.target.kind === "pr" &&
        options.headSha &&
        options.headSha !== approval.subject_digest
      ) {
        return {
          label: `代码已更新（${shortSha(approval.subject_digest)} → ${shortSha(options.headSha)}），等待重新评审`,
          tone: "idle",
        };
      }
      return { label: "待签名", tone: "need" };
    }
    case "granted":
      return { label: "已签名 · 排队 merge", tone: "run" };
    case "consumed":
      if (options.prState === "merged") {
        return {
          label: `已合并 ${shortSha(options.headSha ?? approval.subject_digest)}`,
          tone: "ok",
        };
      }
      if (options.deliveryStage === "closed") {
        return { label: "Issue 已关闭 · 交付完成", tone: "ok" };
      }
      return { label: "已执行，等待 GitHub 确认", tone: "run" };
    case "invalidated":
      return {
        label: options.headSha
          ? `已失效（${shortSha(approval.subject_digest)} → ${shortSha(
              options.headSha,
            )}），重新评审中`
          : "已失效，重新评审中",
        tone: "idle",
      };
    case "expired":
      return { label: "已过期", tone: "idle" };
    case "denied":
      return { label: "已拒绝，回到评审", tone: "idle" };
    default:
      return { label: "未知状态", tone: "idle" };
  }
}

export function approvalStatus(
  status: ApprovalStatus,
): { label: string; tone: Tone } {
  switch (status) {
    case "pending":
      return { label: "待签名", tone: "need" };
    case "granted":
      return { label: "已签名 · 排队 merge", tone: "run" };
    case "consumed":
      return { label: "已执行", tone: "ok" };
    case "invalidated":
      return { label: "已失效", tone: "idle" };
    case "expired":
      return { label: "已过期", tone: "idle" };
    case "denied":
      return { label: "已拒绝", tone: "idle" };
    default:
      return { label: "未知状态", tone: "idle" };
  }
}

export function isPrApproval(approval: Approval): boolean {
  return approval.action === "merge_pr" || approval.target.kind === "pr";
}

export function refLabel(ref: Ref): string {
  switch (ref.kind) {
    case "issue":
      return `#${ref.number}`;
    case "pr":
      return `PR #${ref.number}`;
    case "worktree":
      return ref.id;
    case "delivery":
      return ref.id;
  }
}
