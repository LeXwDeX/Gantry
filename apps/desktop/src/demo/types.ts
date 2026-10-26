/**
 * 本地演示边界：只描述场景驱动与应用内临时视图模型。
 *
 * 这些类型不属于生产协议。正式实体（Message/Card/EventEnvelope/Attempt 等）
 * 仍由 `@/api/types` 定义；工具活动、公开执行摘要和分段生成只存在演示边界内，
 * 完成后由场景驱动写入标准 Message。来源：apps/desktop/DESIGN.md
 * 「接口缺口与临时展示模型」。
 */
import type { Device } from "@/api/types";
import type { ConnectionState } from "@/api/shell";

export type SimulationScriptId =
  | "qa"
  | "task"
  | "failure_retry"
  | "choice_approval"
  | "offline_switch"
  | "settings_apply";

export type SimulationStatus =
  | "idle"
  | "running"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export type SimulationDraftStatus = "streaming" | "done" | "failed" | "cancelled";

export type SimulationToolStatus = "running" | "succeeded" | "failed";

export interface SimulationDraft {
  draft_id: string;
  message_id: string;
  delivery_id: string | null;
  node_id: string | null;
  attempt_id: string | null;
  status: SimulationDraftStatus;
  /** 已生成的公开片段；渲染时按顺序拼接。 */
  segments: string[];
  text: string;
  simulated: true;
}

export interface SimulationToolActivity {
  call_id: string;
  name: string;
  /** 命令或路径摘要，不含完整输出。 */
  target: string;
  status: SimulationToolStatus;
  started_at: string;
  ended_at: string | null;
  elapsed_ms: number | null;
  collapsed_detail: string;
  simulated: true;
  delivery_id: string;
  node_id: string;
  attempt_id: string;
  message_id: string;
}

export interface SimulationSummary {
  summary_id: string;
  status: "streaming" | "done" | "failed";
  /** 只展示可公开的简短执行摘要，不是私有思维链。 */
  text: string;
  simulated: true;
  delivery_id: string | null;
  node_id: string | null;
  attempt_id: string | null;
  message_id: string;
}

export type SimulationAwaiting =
  | null
  | {
      kind: "choice";
      inbox_id: string;
      options: Array<{ id: string; label: string; primary?: boolean }>;
    }
  | { kind: "approval"; approval_id: string }
  | { kind: "retry"; node_id: string; delivery_id: string }
  | { kind: "connection" }
  | { kind: "settings" };

export interface SimulationSnapshot {
  demo: true;
  script_id: SimulationScriptId | null;
  project_id: string | null;
  run_id: number;
  status: SimulationStatus;
  phase: string;
  turn: number;
  total_turns: number;
  drafts: readonly SimulationDraft[];
  activities: readonly SimulationToolActivity[];
  summaries: readonly SimulationSummary[];
  pending_approval_id: string | null;
  awaiting: SimulationAwaiting;
  last_error: string | null;
}

export interface SimulationAction {
  ok: boolean;
  message?: string;
}

export interface SimulationScript {
  id: SimulationScriptId;
  label: string;
  description: string;
}

export type SimulationErrorKind =
  | "offline"
  | "http_500"
  | "settings_conflict"
  | "late_event";

export const SIMULATION_SCRIPTS: readonly SimulationScript[] = [
  {
    id: "qa",
    label: "普通问答",
    description: "用户消息触发分段生成，结束后结算为正式 agent 消息。",
  },
  {
    id: "task",
    label: "多节点任务",
    description: "回执、公开摘要、计划、并行工具、验证和结束摘要关联同一交付。",
  },
  {
    id: "failure_retry",
    label: "失败后重试",
    description: "首个尝试失败，nodes.retry 追加新尝试，旧尝试保持不变。",
  },
  {
    id: "choice_approval",
    label: "选择与模拟审批",
    description: "选择上报项后进入模拟审批；批准不生成签名，支持拒绝与失效。",
  },
  {
    id: "offline_switch",
    label: "断线与恢复",
    description: "模拟连接中断，恢复后继续同一交付；其他项目可独立运行。",
  },
  {
    id: "settings_apply",
    label: "设置应用",
    description: "配合设置页完成校验、冲突、模拟保存与回读；不改真实配置。",
  },
];

/** 由场景适配器额外暴露；不并入正式 IpcAdapter。 */
export interface Simulation {
  scripts(): readonly SimulationScript[];
  getSnapshot(): SimulationSnapshot;
  /** 调用方用 useSyncExternalStore 订阅；返回退订函数。 */
  subscribe(listener: () => void): () => void;
  select(
    id: SimulationScriptId,
    opts?: { projectId?: string },
  ): SimulationAction;
  play(): SimulationAction;
  step(): SimulationAction;
  pause(): SimulationAction;
  resume(): SimulationAction;
  cancel(): SimulationAction;
  reset(): SimulationAction;
  /** 手工推进虚拟时间；正常界面使用 play/step，测试可用 fake timers。 */
  advance(ms?: number): SimulationAction;
  simulateApproval(approvalId: string): SimulationAction;
  simulateDenial(approvalId: string): SimulationAction;
  /** 只让下一次配对预检失败，用于演示错误恢复；不生成密钥或证书。 */
  failNextPairing(): SimulationAction;
  expireApproval(approvalId: string): SimulationAction;
  setHeadChanged(approvalId: string, changed?: boolean): SimulationAction;
  setConnection(phase: ConnectionState["phase"]): SimulationAction;
  setRole(role: Device["role"]): SimulationAction;
  injectError(kind: SimulationErrorKind): SimulationAction;
}
