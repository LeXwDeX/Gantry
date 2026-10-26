import type { ReactNode } from "react";
import {
  useCallback,
  useMemo,
  useState,
  useSyncExternalStore,
} from "react";
import type { Message } from "@/api/types";
import type {
  Simulation,
  SimulationDraft,
  SimulationSnapshot,
  SimulationSummary,
  SimulationToolActivity,
} from "@/demo/types";
import { UntrustedText } from "@/render";
import type { DesktopController } from "@/store/controller";
import "./conversation-simulation.css";

/** 模拟视图模型的空快照；引用稳定，供 `useSyncExternalStore` 使用。 */
const EMPTY_SIMULATION: SimulationSnapshot = {
  demo: true,
  script_id: null,
  project_id: null,
  run_id: 0,
  status: "idle",
  phase: "",
  turn: 0,
  total_turns: 0,
  drafts: [],
  activities: [],
  summaries: [],
  pending_approval_id: null,
  awaiting: null,
  last_error: null,
};

const NOOP = (): void => undefined;

/**
 * 只从适配层的可选演示边界读取；正式适配器没有 `isScenario`/`simulation`，
 * 这里返回 null，界面不回退到任何示例事实。
 */
export function simulationFor(
  controller: DesktopController,
): Simulation | null {
  const adapter = controller.ipc as {
    isScenario?: boolean;
    simulation?: Simulation;
  };
  return adapter.isScenario === true && adapter.simulation
    ? adapter.simulation
    : null;
}

export function useSimulationClient(
  controller: DesktopController,
): Simulation | null {
  return useMemo(() => simulationFor(controller), [controller]);
}

export function useSimulationSnapshot(
  simulation: Simulation | null,
): SimulationSnapshot {
  const subscribe = useCallback(
    (listener: () => void) => simulation?.subscribe(listener) ?? NOOP,
    [simulation],
  );
  const getSnapshot = useCallback(
    () => simulation?.getSnapshot() ?? EMPTY_SIMULATION,
    [simulation],
  );
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

export interface SimulationMessageFacts {
  activities: SimulationToolActivity[];
  summaries: SimulationSummary[];
}

/**
 * 把工具与公开摘要挂到它们的 `message_id`。
 * 只有快照属于当前项目时才返回事实；跨项目不串模拟内容。
 */
export function simulationFactsForMessages(
  snapshot: SimulationSnapshot,
  projectId: string | null,
  messages: Message[],
): Map<string, SimulationMessageFacts> {
  const result = new Map<string, SimulationMessageFacts>();
  if (!projectId || snapshot.project_id !== projectId) return result;
  const ids = new Set(messages.map((message) => message.id));
  for (const activity of snapshot.activities) {
    if (!ids.has(activity.message_id)) continue;
    const facts = result.get(activity.message_id) ?? {
      activities: [],
      summaries: [],
    };
    facts.activities = [...facts.activities, activity];
    result.set(activity.message_id, facts);
  }
  for (const summary of snapshot.summaries) {
    if (!ids.has(summary.message_id)) continue;
    const facts = result.get(summary.message_id) ?? {
      activities: [],
      summaries: [],
    };
    facts.summaries = [...facts.summaries, summary];
    result.set(summary.message_id, facts);
  }
  return result;
}

/**
 * 临时生成区：只显示尚未结算成正式 Message 的草稿，以及附着在草稿上的
 * 工具和摘要。消息结算后立即消失，避免与正式正文重复。
 */
export function SimulationTemporary({
  snapshot,
  projectId,
  messages,
}: {
  snapshot: SimulationSnapshot;
  projectId: string | null;
  messages: Message[];
}): ReactNode {
  const settled = useMemo(
    () => new Set(messages.map((message) => message.id)),
    [messages],
  );
  if (!projectId || snapshot.project_id !== projectId) return null;
  const drafts = snapshot.drafts.filter(
    (draft) => !settled.has(draft.message_id),
  );
  const orphanSummaries = snapshot.summaries.filter(
    (summary) => !settled.has(summary.message_id),
  );
  const orphanActivities = snapshot.activities.filter(
    (activity) => !settled.has(activity.message_id),
  );
  const draftMessageIds = new Set(drafts.map((draft) => draft.message_id));
  const unattachedSummaries = orphanSummaries.filter(
    (summary) => !draftMessageIds.has(summary.message_id),
  );
  const unattachedActivities = orphanActivities.filter(
    (activity) => !draftMessageIds.has(activity.message_id),
  );
  const showStatus =
    snapshot.status === "failed" ||
    snapshot.status === "cancelled" ||
    snapshot.status === "paused" ||
    snapshot.awaiting !== null;
  if (
    drafts.length === 0 &&
    unattachedSummaries.length === 0 &&
    unattachedActivities.length === 0 &&
    !showStatus
  ) {
    return null;
  }

  return (
    <section className="cs-live" aria-label="模拟生成">
      <div className="cs-live__head">
        <span className="cs-live__badge">模拟生成</span>
        <span className={`tone-text-${runTone(snapshot.status)}`}>
          {runStatusLabel(snapshot.status)}
        </span>
        {snapshot.total_turns > 0 ? (
          <span className="mono">
            第 {snapshot.turn}/{snapshot.total_turns} 步 · {snapshot.phase}
          </span>
        ) : null}
      </div>
      {showStatus ? (
        <div className="cs-live__status" role="status">
          <span>{awaitingLabel(snapshot)}</span>
          {snapshot.last_error ? (
            <UntrustedText value={snapshot.last_error} />
          ) : null}
        </div>
      ) : null}
      {drafts.map((draft) => (
        <DraftBubble
          key={draft.draft_id}
          draft={draft}
          runStatus={snapshot.status}
          activities={orphanActivities.filter(
            (activity) => activity.message_id === draft.message_id,
          )}
          summaries={orphanSummaries.filter(
            (summary) => summary.message_id === draft.message_id,
          )}
        />
      ))}
      {unattachedActivities.length > 0 ? (
        <SimulationToolList activities={unattachedActivities} />
      ) : null}
      {unattachedSummaries.length > 0 ? (
        <SimulationSummaryList summaries={unattachedSummaries} />
      ) : null}
    </section>
  );
}

function DraftBubble({
  draft,
  runStatus,
  activities,
  summaries,
}: {
  draft: SimulationDraft;
  runStatus: SimulationSnapshot["status"];
  activities: SimulationToolActivity[];
  summaries: SimulationSummary[];
}): ReactNode {
  const label = draftStatusLabel(draft.status, runStatus);
  const tone = draftTone(draft.status, runStatus);
  return (
    <article
      className="cs-live__draft ws-msg ws-msg--agent"
      data-draft-id={draft.draft_id}
      data-status={draft.status}
    >
      <div className="ws-bubble ws-bubble--agent ws-agent--planner">
        <div className="ws-bubble__head">
          <span className="ws-agent__label">模拟 · 大脑</span>
          <span className={`tone-text-${tone}`}>{label}</span>
          <span className="ws-muted">
            已收到 {draft.segments.length} 段
          </span>
        </div>
        <div className="cs-live__text">
          {draft.segments.map((segment, index) => (
            <span key={index} data-segment>
              <UntrustedText value={segment} />
            </span>
          ))}
        </div>
        <SimulationToolList activities={activities} />
        <SimulationSummaryList summaries={summaries} />
      </div>
    </article>
  );
}

export function SimulationToolList({
  activities,
}: {
  activities: readonly SimulationToolActivity[];
}): ReactNode {
  if (activities.length === 0) return null;
  return (
    <section className="cs-tools" aria-label="模拟工具活动">
      <div className="cs-tools__head">工具活动 · 模拟公开摘要</div>
      <ol className="cs-tools__list">
        {activities.map((activity) => (
          <SimulationToolCard key={activity.call_id} activity={activity} />
        ))}
      </ol>
    </section>
  );
}

function SimulationToolCard({
  activity,
}: {
  activity: SimulationToolActivity;
}): ReactNode {
  const [open, setOpen] = useState(false);
  return (
    <li
      className="cs-tool"
      data-call-id={activity.call_id}
      data-status={activity.status}
    >
      <div className="cs-tool__head">
        <span className="cs-tool__name">
          <UntrustedText value={activity.name} />
        </span>
        <span className={`tone-text-${toolTone(activity.status)}`}>
          {toolStatusLabel(activity.status)}
        </span>
        <button
          type="button"
          className="cs-tool__toggle"
          aria-expanded={open}
          onClick={() => setOpen((value) => !value)}
        >
          {open ? "收起" : "详情"}
        </button>
      </div>
      <span className="cs-tool__target mono">
        <UntrustedText value={activity.target} />
      </span>
      <div className="cs-tool__meta mono">
        <span>
          {formatToolClock(activity.started_at)} →{" "}
          {activity.ended_at ? formatToolClock(activity.ended_at) : "进行中"}
        </span>
        <span>
          {activity.elapsed_ms === null
            ? "计时中"
            : `${activity.elapsed_ms} ms`}
        </span>
      </div>
      {open ? (
        <p className="cs-tool__detail">
          <UntrustedText value={activity.collapsed_detail} />
        </p>
      ) : null}
    </li>
  );
}

export function SimulationSummaryList({
  summaries,
}: {
  summaries: readonly SimulationSummary[];
}): ReactNode {
  if (summaries.length === 0) return null;
  return (
    <div className="cs-summary-list" aria-label="模拟执行摘要列表">
      {summaries.map((summary) => (
        <SimulationSummaryCard key={summary.summary_id} summary={summary} />
      ))}
    </div>
  );
}

function SimulationSummaryCard({
  summary,
}: {
  summary: SimulationSummary;
}): ReactNode {
  const [open, setOpen] = useState(false);
  return (
    <section className="cs-summary" data-summary-id={summary.summary_id}>
      <div className="cs-summary__head">
        <span className="cs-summary__badge">模拟执行摘要</span>
        <span className={`tone-text-${summaryTone(summary.status)}`}>
          {summaryStatusLabel(summary.status)}
        </span>
        <button
          type="button"
          className="cs-summary__toggle"
          aria-label="模拟执行摘要"
          aria-expanded={open}
          onClick={() => setOpen((value) => !value)}
        >
          {open ? "收起" : "展开"}
        </button>
      </div>
      {open ? (
        <p className="cs-summary__text">
          <UntrustedText value={summary.text} />
        </p>
      ) : null}
    </section>
  );
}

function formatToolClock(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function runStatusLabel(status: SimulationSnapshot["status"]): string {
  switch (status) {
    case "running":
      return "生成中";
    case "paused":
      return "已暂停";
    case "completed":
      return "已完成";
    case "failed":
      return "失败";
    case "cancelled":
      return "已中断";
    default:
      return "未开始";
  }
}

function runTone(status: SimulationSnapshot["status"]): string {
  if (status === "running") return "run";
  if (status === "completed") return "ok";
  if (status === "failed" || status === "cancelled") return "warn";
  return "idle";
}

function draftStatusLabel(
  status: SimulationDraft["status"],
  runStatus: SimulationSnapshot["status"],
): string {
  if (status === "streaming") {
    if (runStatus === "completed") return "脚本已完成 · 未结算";
    if (runStatus === "failed") return "生成失败";
    if (runStatus === "cancelled") return "已中断";
    return "生成中";
  }
  switch (status) {
    case "done":
      return "已生成";
    case "failed":
      return "失败";
    default:
      return "已中断";
  }
}

function draftTone(
  status: SimulationDraft["status"],
  runStatus: SimulationSnapshot["status"],
): string {
  if (status === "streaming") {
    if (runStatus === "completed") return "warn";
    if (runStatus === "failed" || runStatus === "cancelled") return "warn";
    return "run";
  }
  if (status === "done") return "ok";
  return "warn";
}

function toolStatusLabel(status: SimulationToolActivity["status"]): string {
  if (status === "running") return "运行中";
  if (status === "succeeded") return "成功";
  return "失败";
}

function toolTone(status: SimulationToolActivity["status"]): string {
  if (status === "running") return "run";
  if (status === "succeeded") return "ok";
  return "warn";
}

function summaryStatusLabel(status: SimulationSummary["status"]): string {
  if (status === "streaming") return "生成中";
  if (status === "done") return "已完成";
  return "已失败";
}

function summaryTone(status: SimulationSummary["status"]): string {
  if (status === "streaming") return "run";
  if (status === "done") return "ok";
  return "warn";
}

function awaitingLabel(snapshot: SimulationSnapshot): string {
  switch (snapshot.awaiting?.kind) {
    case "choice":
      return "等待你在对话中选择下一步。";
    case "approval":
      return "等待你在审批单中处理。";
    case "retry":
      return "节点失败，等待在检视器中重试。";
    case "connection":
      return "连接中断，恢复后继续同一任务。";
    case "settings":
      return "等待设置页完成模拟保存。";
    default:
      return "模拟脚本已暂停。";
  }
}
