/**
 * 工作区纯视图模型。
 *
 * 所有数据都来自 DesktopState / ProjectData；这里只做选择、排序、投影和
 * 文案映射，不读取 fixture，不写服务端状态。见 docs/modules/desktop-ui.md
 * §7.4–§7.10 与 docs/prototype.md §2。
 */
import type {
  Approval,
  Card,
  Delivery,
  InboxItem,
  InboxOption,
  Level,
  Message,
  Project,
  PullRequestRef,
  Ref,
  Stage,
  UntrustedText,
  Workflow,
  WorkflowNode,
  Worktree,
} from "@/api/types";
import type { Selection } from "@/api/shell";
import type { ProjectData } from "@/store/state";
import { approvalStatusLabel, shortSha } from "@/store/sign";
import {
  agentKindLabel,
  inboxKindLabel,
  stageLabel,
} from "@/i18n";
import { cleanText } from "@/render";
import type { Tone } from "@/theme/tone";

export const LEVELS: Level[] = [
  "None",
  "Low",
  "Medium",
  "High",
  "Max",
  "Ultimate",
];

export const LEVEL_HINTS: Record<Level, string> = {
  None: "只问答，不建交付",
  Low: "单节点实现，开 PR，只等 CI",
  Medium: "plan → build → verify，单 reviewer",
  High: "需求准入 + 多角色并行 review",
  Max: "reasoner 预演 + 对抗 review + arbiter 裁决",
  Ultimate: "按 Milestone 拆成多个交付并行",
};

export const STAGES: Stage[] = [
  "spec",
  "plan",
  "implement",
  "propose",
  "review",
  "merge",
  "closed",
];

export const STAGE_LABELS = STAGES.map((stage) => stageLabel(stage));

export function projectName(project: Project | null): string {
  if (!project) return "—";
  const parts = project.repo.split("/");
  return cleanText(parts.length > 1 ? parts[1] : project.repo, true);
}

export function isActiveDelivery(delivery: Delivery): boolean {
  return delivery.stage !== "closed" && delivery.stage !== "abandoned";
}

export function isTerminalDelivery(delivery: Delivery): boolean {
  return !isActiveDelivery(delivery);
}

export function formatClock(iso: string | null | undefined): string {
  if (!iso) return "";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

export function selectionKey(selection: Selection | null): string {
  if (!selection) return "";
  switch (selection.kind) {
    case "base":
      return "base";
    case "worktree":
      return `worktree:${selection.id}`;
    case "issue":
      return `issue:${selection.repo}#${selection.number}`;
    case "pr":
      return `pr:${selection.repo}#${selection.number}`;
  }
}

export function sameSelection(
  a: Selection | null,
  b: Selection | null,
): boolean {
  return selectionKey(a) === selectionKey(b);
}

export function deliveryForSelection(
  selection: Selection | null,
  data: ProjectData,
): Delivery | null {
  if (!selection) return null;
  if (selection.kind === "base") return null;
  if (selection.kind === "worktree") {
    const direct = data.deliveries.find(
      (delivery) => delivery.worktree_id === selection.id,
    );
    if (direct) return direct;
    const worktree = data.worktrees.find((item) => item.id === selection.id);
    if (worktree?.lock?.delivery_id) {
      return (
        data.deliveries.find(
          (delivery) => delivery.id === worktree.lock?.delivery_id,
        ) ?? null
      );
    }
    return null;
  }
  return (
    data.deliveries.find((delivery) => {
      if (selection.kind === "issue") {
        return delivery.issues.some(
          (issue) =>
            issue.repo === selection.repo && issue.number === selection.number,
        );
      }
      return (
        delivery.pr?.repo === selection.repo &&
        delivery.pr.number === selection.number
      );
    }) ?? null
  );
}

export function deliveryForRef(
  ref: Ref | null,
  data: ProjectData,
): Delivery | null {
  if (!ref) return null;
  if (ref.kind === "delivery") {
    return data.deliveries.find((delivery) => delivery.id === ref.id) ?? null;
  }
  return deliveryForSelection(selectionForRef(ref, data), data);
}

export function deliveryForCtx(
  ctx: Ref | null,
  data: ProjectData,
): Delivery | null {
  return deliveryForRef(ctx, data);
}

export function primarySelection(delivery: Delivery): Selection | null {
  if (delivery.pr) {
    return {
      kind: "pr",
      repo: delivery.pr.repo,
      number: delivery.pr.number,
    };
  }
  const issue = delivery.issues[0];
  if (issue) {
    return { kind: "issue", repo: issue.repo, number: issue.number };
  }
  if (delivery.worktree_id) {
    return { kind: "worktree", id: delivery.worktree_id };
  }
  return null;
}

export function selectionForRef(
  ref: Ref,
  data: ProjectData,
): Selection | null {
  switch (ref.kind) {
    case "issue":
      return { kind: "issue", repo: ref.repo, number: ref.number };
    case "pr":
      return { kind: "pr", repo: ref.repo, number: ref.number };
    case "worktree":
      return data.worktrees.some((worktree) => worktree.id === ref.id)
        ? { kind: "worktree", id: ref.id }
        : null;
    case "delivery": {
      const delivery = data.deliveries.find((item) => item.id === ref.id);
      return delivery ? primarySelection(delivery) : null;
    }
  }
}

export interface ChatRow {
  type: "message" | "events";
  key: string;
  message?: Message;
  messages?: Message[];
}

export function buildChatRows(messages: Message[]): ChatRow[] {
  const rows: ChatRow[] = [];
  let buffer: Message[] = [];
  const flush = () => {
    if (buffer.length === 0) return;
    rows.push({
      type: "events",
      key: buffer[0].id,
      messages: buffer,
    });
    buffer = [];
  };
  for (const message of messages) {
    if (message.role === "event") {
      buffer.push(message);
      continue;
    }
    flush();
    rows.push({ type: "message", key: message.id, message });
  }
  flush();
  return rows;
}

export function selectionForMessage(
  message: Message,
  data: ProjectData,
): Selection | null {
  const ref = message.refs[0];
  if (ref) return selectionForRef(ref, data);
  if (message.delivery_id) {
    const delivery = data.deliveries.find(
      (item) => item.id === message.delivery_id,
    );
    if (delivery) return primarySelection(delivery);
  }
  return null;
}

export function refForSelection(selection: Selection | null): Ref | null {
  if (!selection) return null;
  switch (selection.kind) {
    case "base":
      return null;
    case "issue":
    case "pr":
      return {
        kind: selection.kind,
        repo: selection.repo,
        number: selection.number,
      };
    case "worktree":
      return { kind: "worktree", id: selection.id };
  }
}

export function refKey(ref: Ref | null): string {
  if (!ref) return "";
  switch (ref.kind) {
    case "issue":
      return `issue:${ref.repo}#${ref.number}`;
    case "pr":
      return `pr:${ref.repo}#${ref.number}`;
    case "worktree":
      return `worktree:${ref.id}`;
    case "delivery":
      return `delivery:${ref.id}`;
  }
}

export function refLabelFor(
  ref: Ref,
  data: ProjectData,
): string {
  switch (ref.kind) {
    case "issue":
      return `#${ref.number}`;
    case "pr":
      return `PR #${ref.number}`;
    case "worktree": {
      const worktree = data.worktrees.find((item) => item.id === ref.id);
      return worktree ? cleanText(worktree.branch, true) : "工作树";
    }
    case "delivery": {
      const delivery = data.deliveries.find((item) => item.id === ref.id);
      return delivery ? `交付 · ${cleanText(delivery.title)}` : "交付";
    }
  }
}

export function ctxLabelFor(
  ref: Ref | null,
  data: ProjectData,
): string {
  if (!ref) return "全项目";
  if (ref.kind !== "delivery") return refLabelFor(ref, data);
  const delivery = data.deliveries.find((item) => item.id === ref.id);
  if (!delivery) return "交付";
  const primary = primarySelection(delivery);
  const primaryRef = refForSelection(primary);
  return primaryRef ? refLabelFor(primaryRef, data) : "交付";
}

export function messageContextLabel(
  message: Message,
  data: ProjectData,
): string {
  const ref = message.refs[0];
  return ref ? refLabelFor(ref, data) : "全项目";
}

export interface ContextOption {
  key: string;
  label: string;
  ref: Ref | null;
}

export function contextOptions(data: ProjectData): ContextOption[] {
  const options: ContextOption[] = [
    { key: "project", label: "全项目", ref: null },
  ];
  const seen = new Set<string>();
  const add = (ref: Ref, label: string) => {
    const key = refKey(ref);
    if (seen.has(key)) return;
    seen.add(key);
    options.push({ key, label, ref });
  };
  const active = sortedActiveDeliveries(data);
  for (const delivery of active) {
    add({ kind: "delivery", id: delivery.id }, `交付 · ${cleanText(delivery.title)}`);
  }
  for (const delivery of active) {
    for (const issue of delivery.issues) {
      add(
        { kind: "issue", repo: issue.repo, number: issue.number },
        `#${issue.number} ${cleanText(issue.title.text)}`,
      );
    }
    if (delivery.pr) {
      add(
        { kind: "pr", repo: delivery.pr.repo, number: delivery.pr.number },
        `PR #${delivery.pr.number} ${cleanText(delivery.pr.title.text)}`,
      );
    }
    if (delivery.worktree_id) {
      const worktree = data.worktrees.find(
        (item) => item.id === delivery.worktree_id,
      );
      if (worktree) {
        add(
          { kind: "worktree", id: worktree.id },
          cleanText(worktree.branch, true),
        );
      }
    }
  }
  return options;
}

export interface WorkRow {
  key: string;
  kind: "issue" | "pr" | "worktree";
  selection: Selection;
  label: string;
  title: UntrustedText | string;
  titleText: string;
  delivery: Delivery;
  hasNeed: boolean;
  ci: { label: string; tone: Tone } | null;
  rank: number;
}

export function deliveryRank(
  delivery: Delivery,
  needsByDelivery: Map<string, InboxItem[]>,
): number {
  if ((needsByDelivery.get(delivery.id)?.length ?? 0) > 0) return 0;
  if (delivery.stage_status === "running") return 1;
  return 2;
}

export function sortedActiveDeliveries(data: ProjectData): Delivery[] {
  return data.deliveries
    .filter(isActiveDelivery)
    .slice()
    .sort((a, b) => b.id.localeCompare(a.id));
}

export function buildWorkRows(
  data: ProjectData,
  needsByDelivery: Map<string, InboxItem[]>,
): WorkRow[] {
  const rows: WorkRow[] = [];
  const seen = new Set<string>();
  const active = data.deliveries.filter(isActiveDelivery);
  for (const delivery of active) {
    const hasNeed = (needsByDelivery.get(delivery.id)?.length ?? 0) > 0;
    const rank = deliveryRank(delivery, needsByDelivery);
    for (const issue of delivery.issues) {
      const key = `issue:${issue.repo}#${issue.number}`;
      if (seen.has(key)) continue;
      seen.add(key);
      rows.push({
        key,
        kind: "issue",
        selection: {
          kind: "issue",
          repo: issue.repo,
          number: issue.number,
        },
        label: `#${issue.number}`,
        title: issue.title,
        titleText: cleanText(issue.title.text),
        delivery,
        hasNeed,
        ci: null,
        rank,
      });
    }
    if (delivery.pr) {
      const key = `pr:${delivery.pr.repo}#${delivery.pr.number}`;
      if (!seen.has(key)) {
        seen.add(key);
        rows.push({
          key,
          kind: "pr",
          selection: {
            kind: "pr",
            repo: delivery.pr.repo,
            number: delivery.pr.number,
          },
          label: `PR #${delivery.pr.number}`,
          title: delivery.pr.title,
          titleText: cleanText(delivery.pr.title.text),
          delivery,
          hasNeed,
          ci: ciLabel(delivery.pr.checks),
          rank,
        });
      }
    }
    if (delivery.worktree_id) {
      const worktree = data.worktrees.find(
        (item) => item.id === delivery.worktree_id,
      );
      if (worktree) {
        const key = `worktree:${worktree.id}`;
        if (!seen.has(key)) {
          seen.add(key);
          rows.push({
            key,
            kind: "worktree",
            selection: { kind: "worktree" as const, id: worktree.id },
            label: cleanText(worktree.branch, true),
            title: worktree.branch,
            titleText: cleanText(worktree.branch, true),
            delivery,
            hasNeed,
            ci: null,
            rank,
          });
        }
      }
    }
  }
  return rows.sort(
    (a, b) =>
      a.rank - b.rank ||
      b.delivery.id.localeCompare(a.delivery.id) ||
      a.key.localeCompare(b.key),
  );
}

export function ciLabel(
  checks: PullRequestRef["checks"],
): { label: string; tone: Tone } {
  if (checks.failed > 0) return { label: "CI ✗", tone: "warn" };
  if (checks.pending > 0) return { label: "CI 运行中", tone: "run" };
  if (checks.passed + checks.failed + checks.pending === 0) {
    return { label: "无 CI", tone: "idle" };
  }
  return { label: "CI ✓", tone: "ok" };
}

export interface DoneRow {
  key: string;
  selection: Selection;
  label: string;
  titleText: string;
  stateLabel: string;
  delivery: Delivery;
}

export function doneRows(data: ProjectData): DoneRow[] {
  return data.deliveries
    .filter(isTerminalDelivery)
    .map((delivery) => {
      const selection = primarySelection(delivery);
      if (!selection) return null;
      const label = delivery.pr
        ? `PR #${delivery.pr.number}`
        : delivery.issues[0]
          ? `#${delivery.issues[0].number}`
          : "交付";
      const stateLabel =
        delivery.stage === "abandoned"
          ? "已放弃"
          : delivery.pr?.state === "merged"
            ? "已合并"
            : "已关闭";
      return {
        key: `done:${delivery.id}`,
        selection,
        label,
        titleText: cleanText(delivery.title),
        stateLabel,
        delivery,
      };
    })
    .filter((row): row is DoneRow => row !== null);
}

export function approvalIdOfItem(item: InboxItem): string | null {
  const payload = item.payload as { approval_id?: unknown };
  return typeof payload.approval_id === "string" ? payload.approval_id : null;
}

export function inboxOptions(item: InboxItem): InboxOption[] {
  const payload = item.payload as { options?: unknown };
  if (!Array.isArray(payload.options)) return [];
  return payload.options.filter((option): option is InboxOption => {
    if (!option || typeof option !== "object") return false;
    const candidate = option as { id?: unknown; label?: unknown };
    return (
      typeof candidate.id === "string" && typeof candidate.label === "string"
    );
  });
}

export function inboxMissing(
  item: InboxItem,
): Array<{ aspect: string; question: string }> {
  const payload = item.payload as { missing?: unknown };
  if (!Array.isArray(payload.missing)) return [];
  return payload.missing
    .filter((entry): entry is { aspect: string; question: string } => {
      if (!entry || typeof entry !== "object") return false;
      const candidate = entry as { aspect?: unknown; question?: unknown };
      return (
        typeof candidate.aspect === "string" &&
        typeof candidate.question === "string"
      );
    })
    .map((entry) => ({ aspect: entry.aspect, question: entry.question }));
}

export function inboxFinding(
  item: InboxItem,
): { severity: string; category: string; title: UntrustedText } | null {
  const payload = item.payload as {
    finding?: {
      severity?: unknown;
      category?: unknown;
      title?: unknown;
    };
  };
  const finding = payload.finding;
  if (!finding || typeof finding !== "object") return null;
  const title = finding.title as UntrustedText | undefined;
  if (
    !title ||
    typeof title !== "object" ||
    typeof title.text !== "string" ||
    typeof title.source !== "string"
  ) {
    return null;
  }
  return {
    severity: typeof finding.severity === "string" ? finding.severity : "info",
    category: typeof finding.category === "string" ? finding.category : "未知",
    title,
  };
}

export function inboxSource(item: InboxItem): Ref | null {
  const payload = item.payload as { source?: unknown };
  const source = payload.source;
  if (!source || typeof source !== "object") return null;
  const candidate = source as { kind?: unknown };
  if (
    candidate.kind === "issue" ||
    candidate.kind === "pr" ||
    candidate.kind === "worktree" ||
    candidate.kind === "delivery"
  ) {
    return source as Ref;
  }
  return null;
}

export function approvalForDelivery(
  approvals: Record<string, Approval>,
  delivery: Delivery,
): Approval | null {
  return (
    Object.values(approvals)
      .filter((approval) => approval.delivery_id === delivery.id)
      .sort((a, b) => {
        if (a.status === "pending" && b.status !== "pending") return -1;
        if (b.status === "pending" && a.status !== "pending") return 1;
        return (b.updated_at ?? b.created_at ?? "").localeCompare(
          a.updated_at ?? a.created_at ?? "",
        );
      })[0] ?? null
  );
}

export function openApprovalItemForDelivery(
  inbox: InboxItem[],
  delivery: Delivery,
): InboxItem | null {
  return (
    inbox.find(
      (item) =>
        item.status === "open" &&
        item.kind === "approval_needed" &&
        item.delivery_id === delivery.id,
    ) ?? null
  );
}

export interface ApprovalView {
  id: string | null;
  label: string;
  tone: Tone;
  pending: boolean;
}

export function approvalView(
  approvals: Record<string, Approval>,
  inbox: InboxItem[],
  approvalId: string | null,
  delivery: Delivery | null,
): ApprovalView {
  if (approvalId && approvals[approvalId]) {
    const approval = approvals[approvalId];
    const status = approvalStatusLabel(approval, {
      prState: delivery?.pr?.state ?? null,
      deliveryStage: delivery?.stage ?? null,
      headSha: delivery?.pr?.head_sha ?? null,
    });
    const stale =
      approval.target.kind === "pr" &&
      delivery?.pr !== null &&
      delivery?.pr !== undefined &&
      delivery.pr.head_sha !== approval.subject_digest;
    return {
      id: approvalId,
      label: status.label,
      tone: status.tone,
      pending: approval.status === "pending" && !stale,
    };
  }
  if (approvalId) {
    const item = inbox.find(
      (entry) =>
        entry.status === "open" &&
        entry.kind === "approval_needed" &&
        approvalIdOfItem(entry) === approvalId,
    );
    if (item) {
      return { id: approvalId, label: "待签名", tone: "need", pending: true };
    }
  }
  if (delivery) {
    const item = openApprovalItemForDelivery(inbox, delivery);
    if (item) {
      return {
        id: approvalIdOfItem(item),
        label: "待签名",
        tone: "need",
        pending: true,
      };
    }
  }
  return {
    id: approvalId,
    label: "无待签审批",
    tone: "idle",
    pending: false,
  };
}

export function approvalTargetText(approval: Approval | null): string {
  if (!approval) return "";
  if (approval.target.kind === "pr") {
    return `PR #${approval.target.number} @ ${shortSha(approval.subject_digest)}`;
  }
  return approval.target.kind === "settings"
    ? approval.target.resource
    : approval.target.kind;
}

export interface GateRow {
  key: string;
  label: string;
  status: string;
  tone: Tone;
}

export function mergeGates(
  pr: PullRequestRef | null,
  project: Project | null,
  approval: ApprovalView,
): GateRow[] {
  if (!pr) {
    return [
      {
        key: "no-pr",
        label: "合并闸门",
        status: "无 PR，无合并闸门",
        tone: "idle",
      },
    ];
  }
  const total = pr.checks.passed + pr.checks.failed + pr.checks.pending;
  const checksStatus =
    total === 0
      ? "无 checks"
      : `${pr.checks.passed}/${total} 通过${pr.checks.failed > 0 ? `，${pr.checks.failed} 失败` : ""}${pr.checks.pending > 0 ? `，${pr.checks.pending} 运行中` : ""}`;
  const reviewStatus =
    pr.review_state === "approved"
      ? "已批准"
      : pr.review_state === "changes_requested"
        ? "要求修改"
        : "未批准";
  const queueStatus = project?.merge_queue_enabled
    ? pr.in_merge_queue
      ? "已进入 merge queue"
      : "未进入 merge queue"
    : "未开启，直接 merge";
  return [
    {
      key: "checks",
      label: `必需 checks @${shortSha(pr.head_sha)}`,
      status: checksStatus,
      tone: pr.checks.required_ok ? "ok" : "warn",
    },
    {
      key: "review",
      label: "GitHub review",
      status: reviewStatus,
      tone:
        pr.review_state === "approved"
          ? "ok"
          : pr.review_state === "changes_requested"
            ? "warn"
            : "idle",
    },
    {
      key: "queue",
      label: "merge queue",
      status: queueStatus,
      tone: pr.in_merge_queue ? "run" : "idle",
    },
    {
      key: "signature",
      label: "Gantry 签名",
      status: approval.label,
      tone: approval.tone,
    },
  ];
}

export interface DeliveryStateView {
  label: string;
  tone: Tone;
}

export function deliveryState(
  delivery: Delivery,
  hasNeed: boolean,
): DeliveryStateView {
  if (delivery.stage === "abandoned") return { label: "已放弃", tone: "idle" };
  if (hasNeed) return { label: "需要你", tone: "need" };
  if (delivery.stage === "closed") {
    return delivery.pr?.state === "merged"
      ? { label: "已合并", tone: "ok" }
      : { label: "已关闭", tone: "ok" };
  }
  switch (delivery.stage_status) {
    case "running":
      return { label: "运行中", tone: "run" };
    case "waiting_human":
      return { label: "等待人工", tone: "need" };
    case "blocked":
      return { label: "受阻", tone: "warn" };
    case "paused":
      return { label: "已暂停", tone: "idle" };
    case "done":
      return { label: "已完成", tone: "ok" };
    default:
      return { label: "空闲", tone: "idle" };
  }
}

export function worktreeHolderLabel(
  lock: Worktree["lock"],
): string {
  if (!lock) return "—";
  switch (lock.holder) {
    case "engine":
      return "前台 agent";
    case "guardian":
      return "后台守护";
    case "human":
      return `你（至 ${formatClock(lock.lease_until)}）`;
    default:
      return lock.holder;
  }
}

export interface StageCell {
  stage: Stage;
  label: string;
  state: "done" | "current" | "future" | "skipped" | "abandoned";
}

export function stageCells(delivery: Delivery): StageCell[] {
  const current =
    delivery.stage === "abandoned" ? -1 : STAGES.indexOf(delivery.stage);
  const skipPlan = delivery.level_cap === "Low";
  return STAGES.map((stage, index) => {
    if (delivery.stage === "abandoned") {
      return { stage, label: STAGE_LABELS[index], state: "abandoned" };
    }
    if (index === 1 && skipPlan) {
      return { stage, label: "跳过", state: "skipped" };
    }
    if (delivery.stage === "closed") {
      return { stage, label: STAGE_LABELS[index], state: "done" };
    }
    if (index < current) {
      return { stage, label: STAGE_LABELS[index], state: "done" };
    }
    if (index === current) {
      return { stage, label: STAGE_LABELS[index], state: "current" };
    }
    return { stage, label: STAGE_LABELS[index], state: "future" };
  });
}

export function workflowForDelivery(
  workflows: Record<string, Workflow>,
  delivery: Delivery,
): Workflow | null {
  if (!isActiveDelivery(delivery)) return null;
  const matches = Object.values(workflows).filter(
    (workflow) =>
      workflow.delivery_id === delivery.id && workflow.stage === delivery.stage,
  );
  matches.sort((a, b) => b.graph_rev - a.graph_rev);
  return matches[0] ?? null;
}

export function nodeRoleLabel(role: string): string {
  if (role.startsWith("review:")) {
    return `评审 · ${role.slice("review:".length)}`;
  }
  switch (role) {
    case "plan":
      return "计划";
    case "build":
      return "实现";
    case "verify":
      return "验证";
    case "single":
      return "单节点实现";
    case "reasoner":
      return "预演";
    case "arbiter":
      return "裁决";
    case "summarize":
      return "汇总";
    case "admission":
      return "需求准入";
    case "heal":
      return "自愈";
    case "answer":
      return "问答";
    case "intent":
      return "意图";
    case "classify":
      return "分类";
    default:
      return role;
  }
}

export function nodeStatusLabel(node: WorkflowNode): string {
  const status =
    node.status === "running"
      ? "运行中"
      : node.status === "succeeded"
        ? "完成"
        : node.status === "failed"
          ? "失败"
          : node.status === "pending"
            ? "等待"
            : node.status === "queued"
              ? "排队"
              : node.status === "skipped"
                ? "跳过"
                : node.status === "cancelled"
                  ? "取消"
                  : node.status === "superseded"
                    ? "已被替换"
                    : node.status;
  return node.verdict ? `${status} · ${node.verdict}` : status;
}

export function nodeStatusTone(node: WorkflowNode): Tone {
  switch (node.status) {
    case "running":
      return "run";
    case "succeeded":
      return "ok";
    case "failed":
      return "warn";
    default:
      return "idle";
  }
}

export function failedNodes(workflow: Workflow | null): WorkflowNode[] {
  if (!workflow?.nodes) return [];
  return workflow.nodes.filter(
    (node) => node.status === "failed" && !node.superseded_by,
  );
}

export function deliveryChain(delivery: Delivery, data: ProjectData): string {
  const parts: string[] = [];
  for (const issue of delivery.issues) parts.push(`#${issue.number}`);
  if (delivery.pr) parts.push(`PR #${delivery.pr.number}`);
  if (delivery.worktree_id) {
    const worktree = data.worktrees.find(
      (item) => item.id === delivery.worktree_id,
    );
    if (worktree) parts.push(cleanText(worktree.branch, true));
  }
  return parts.join(" → ");
}

export function levelLine(delivery: Delivery): string {
  return `上限 ${delivery.level_cap} · 实际 ${delivery.level_effective}`;
}

export function levelReason(delivery: Delivery): string {
  if (delivery.level_cap === delivery.level_effective) return "";
  return "实际档位由服务端给出；加码原因未同步到界面。";
}

export function guardianEvents(
  messages: Message[],
  delivery: Delivery,
): Message[] {
  return messages
    .filter(
      (message) =>
        message.role === "event" &&
        (message.delivery_id === delivery.id ||
          message.refs.some((ref) => refMatchesDelivery(ref, delivery))),
    )
    .slice(-10)
    .reverse();
}

export function refMatchesDelivery(
  ref: Ref,
  delivery: Delivery,
): boolean {
  switch (ref.kind) {
    case "delivery":
      return ref.id === delivery.id;
    case "issue":
      return delivery.issues.some(
        (issue) => issue.repo === ref.repo && issue.number === ref.number,
      );
    case "pr":
      return (
        delivery.pr?.repo === ref.repo && delivery.pr.number === ref.number
      );
    case "worktree":
      return delivery.worktree_id === ref.id;
  }
}

export function messageNeedItem(
  message: Message,
  inbox: InboxItem[],
): InboxItem | null {
  for (const card of message.cards) {
    if (card.kind === "approval") {
      const approvalItem = approvalCardInboxItem(card, inbox);
      if (approvalItem) return approvalItem;
      continue;
    }
    const id = cardInboxId(card);
    if (!id) continue;
    const item = inbox.find(
      (entry) => entry.status === "open" && entry.id === id,
    );
    if (item) return item;
  }
  return null;
}

export function cardInboxId(card: Card): string | null {
  if (card.kind === "escalation" || card.kind === "choice") {
    return typeof card.inbox_id === "string" ? card.inbox_id : null;
  }
  return null;
}

export function approvalCardInboxItem(
  card: Card,
  inbox: InboxItem[],
): InboxItem | null {
  if (card.kind !== "approval") return null;
  const approvalId =
    typeof card.approval_id === "string" ? card.approval_id : null;
  if (!approvalId) return null;
  return (
    inbox.find(
      (item) =>
        item.status === "open" &&
        item.kind === "approval_needed" &&
        approvalIdOfItem(item) === approvalId,
    ) ?? null
  );
}

export interface AgentTone {
  label: string;
  className: "planner" | "worker" | "utility" | "unknown";
}

export function agentKindTone(kind: string | null | undefined): AgentTone {
  switch (kind) {
    case "planner":
      return { label: agentKindLabel(kind), className: "planner" };
    case "worker":
      return { label: agentKindLabel(kind), className: "worker" };
    case "utility":
      return { label: agentKindLabel(kind), className: "utility" };
    default:
      return { label: "agent", className: "unknown" };
  }
}

export function inboxKindText(kind: string): string {
  return inboxKindLabel(kind);
}

export interface MapNodeView {
  key: string;
  kind: "issue" | "pr" | "worktree" | "orphan";
  selection: Selection | null;
  label: string;
  title: string;
  level: Level;
  hasNeed: boolean;
  status: DeliveryStateView;
  delivery: Delivery;
}

export interface MapBand {
  id: string;
  delivery: Delivery;
  issues: MapNodeView[];
  pr: MapNodeView | null;
  worktree: MapNodeView | null;
  orphan: boolean;
  rows: number;
  height: number;
  edges: MapEdge[];
  hasNeed: boolean;
  rank: number;
}

export interface MapEdge {
  key: string;
  kind: "closes" | "branch" | "direct" | "orphan";
  d: string;
  highlighted: boolean;
}

export function buildMapBands(
  data: ProjectData,
  needsByDelivery: Map<string, InboxItem[]>,
): MapBand[] {
  const active = data.deliveries
    .filter(isActiveDelivery)
    .slice()
    .sort(
      (a, b) =>
        deliveryRank(a, needsByDelivery) - deliveryRank(b, needsByDelivery) ||
        b.id.localeCompare(a.id),
    );
  return active.map((delivery) => {
    const hasNeed = (needsByDelivery.get(delivery.id)?.length ?? 0) > 0;
    const status = deliveryState(delivery, hasNeed);
    const issues = delivery.issues.map((issue) => ({
      key: `issue:${issue.repo}#${issue.number}`,
      kind: "issue" as const,
      selection: {
        kind: "issue" as const,
        repo: issue.repo,
        number: issue.number,
      },
      label: `#${issue.number}`,
      title: cleanText(issue.title.text),
      level: delivery.level_cap,
      hasNeed,
      status,
      delivery,
    }));
    const pr = delivery.pr
      ? {
          key: `pr:${delivery.pr.repo}#${delivery.pr.number}`,
          kind: "pr" as const,
          selection: {
            kind: "pr" as const,
            repo: delivery.pr.repo,
            number: delivery.pr.number,
          },
          label: `PR #${delivery.pr.number}`,
          title: cleanText(delivery.pr.title.text),
          level: delivery.level_cap,
          hasNeed,
          status,
          delivery,
        }
      : null;
    const worktree = delivery.worktree_id
      ? data.worktrees.find((item) => item.id === delivery.worktree_id) ?? null
      : null;
    const wt = worktree
      ? {
          key: `worktree:${worktree.id}`,
          kind: "worktree" as const,
          selection: { kind: "worktree" as const, id: worktree.id },
          label: cleanText(worktree.branch, true),
          title: cleanText(delivery.title),
          level: delivery.level_cap,
          hasNeed,
          status,
          delivery,
        }
      : null;
    const orphan = Boolean(pr && delivery.issues.length === 0);
    const rows = Math.max(issues.length, pr ? 1 : 0, wt ? 1 : 0, orphan ? 1 : 0, 1);
    const height = Math.max(74, rows * 76 - 2);
    return {
      id: delivery.id,
      delivery,
      issues,
      pr,
      worktree: wt,
      orphan,
      rows,
      height,
      edges: bandEdges(issues.length, Boolean(pr), Boolean(wt), orphan),
      hasNeed,
      rank: deliveryRank(delivery, needsByDelivery),
    };
  });
}

export function bandEdges(
  issueCount: number,
  hasPr: boolean,
  hasWorktree: boolean,
  orphan: boolean,
): MapEdge[] {
  const edges: MapEdge[] = [];
  if (orphan) {
    edges.push({
      key: "orphan",
      kind: "orphan",
      d: "M248 37 H304",
      highlighted: true,
    });
    if (hasWorktree) {
      edges.push({
        key: "branch",
        kind: "branch",
        d: "M536 37 H592",
        highlighted: true,
      });
    }
    return edges;
  }
  for (let index = 0; index < issueCount; index += 1) {
    const y = 37 + index * 76;
    if (hasPr) {
      edges.push({
        key: `closes:${index}`,
        kind: "closes",
        d: y === 37 ? "M248 37 H304" : `M248 ${y} H276 V37 H304`,
        highlighted: true,
      });
    } else if (hasWorktree) {
      edges.push({
        key: `direct:${index}`,
        kind: "direct",
        d: y === 37 ? "M248 37 H592" : `M248 ${y} H420 V37 H592`,
        highlighted: false,
      });
    }
  }
  if (hasPr && hasWorktree) {
    edges.push({
      key: "branch",
      kind: "branch",
      d: "M536 37 H592",
      highlighted: true,
    });
  }
  return edges;
}

export interface WorktreeRow {
  key: string;
  worktree: Worktree;
  delivery: Delivery | null;
  selection: Selection;
  related: string;
  sandbox: string;
  holder: string;
  status: DeliveryStateView;
}

export function worktreeRows(
  data: ProjectData,
  needsByDelivery: Map<string, InboxItem[]>,
): WorktreeRow[] {
  return data.worktrees
    .map((worktree) => {
      const delivery =
        data.deliveries.find((item) => item.worktree_id === worktree.id) ??
        data.deliveries.find(
          (item) => item.id === worktree.lock?.delivery_id,
        ) ??
        null;
      const hasNeed = delivery
        ? (needsByDelivery.get(delivery.id)?.length ?? 0) > 0
        : false;
      const relatedParts: string[] = [];
      if (delivery) {
        for (const issue of delivery.issues) {
          relatedParts.push(`#${issue.number}`);
        }
        if (delivery.pr) relatedParts.push(`PR #${delivery.pr.number}`);
      }
      return {
        key: `worktree:${worktree.id}`,
        worktree,
        delivery,
        selection: { kind: "worktree" as const, id: worktree.id },
        related: relatedParts.length > 0 ? relatedParts.join(" · ") : "—",
        sandbox: "服务端未提供",
        holder: worktreeHolderLabel(worktree.lock),
        status: delivery
          ? deliveryState(delivery, hasNeed)
          : worktree.lock
            ? { label: "已锁定", tone: "run" as const }
            : { label: "空闲", tone: "idle" as const },
      };
    })
    .sort(
      (a, b) =>
        (a.delivery
          ? deliveryRank(a.delivery, needsByDelivery)
          : 3) -
          (b.delivery ? deliveryRank(b.delivery, needsByDelivery) : 3) ||
        a.worktree.id.localeCompare(b.worktree.id),
    );
}

export function inboxCountForProject(
  summary: { by_project: Array<{ project_id: string | null; open: number }> } | null,
  fallback: number,
  projectId: string,
): number {
  if (!summary) return fallback;
  return (
    summary.by_project.find((entry) => entry.project_id === projectId)?.open ??
    0
  );
}
