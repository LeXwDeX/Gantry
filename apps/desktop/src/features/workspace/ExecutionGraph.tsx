/**
 * 只读执行图。工作流、尝试和预算来自 DesktopState 的正式读取路由；
 * 工具与公开摘要只是可选的演示视图模型。这里不提供 DAG 编辑或预算追加。
 */
import type { ReactNode } from "react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type {
  Attempt,
  BudgetLedger,
  Delivery,
  NodeStatus,
  Workflow,
  WorkflowNode,
} from "@/api/types";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import type { Tone } from "@/theme/tone";
import type {
  SimulationSummary,
  SimulationToolActivity,
} from "@/demo/types";
import { Modal } from "@/a11y/Modal";
import { stageLabel } from "@/i18n";
import { cleanText, UntrustedText } from "@/render";
import { nodeRoleLabel, nodeStatusTone } from "./model";
import { ToneDot } from "./parts";
import "./execution-graph.css";

export interface ExecutionGraphProgress {
  tools: readonly SimulationToolActivity[];
  summaries: readonly SimulationSummary[];
}

export interface ExecutionGraphProps {
  controller: DesktopController;
  state: DesktopState;
  workflowId: string;
  deliveryId?: string | null;
  /** 可选的应用内演示进度；正式模式没有数据时整段不显示。 */
  progress?: ExecutionGraphProgress | null;
  onClose: () => void;
}

const NODE_W = 200;
const NODE_H = 88;
const GAP_X = 64;
const GAP_Y = 24;
const PAD = 12;

const STATUS_LABELS: Record<NodeStatus, string> = {
  pending: "等待",
  queued: "排队",
  running: "运行中",
  succeeded: "完成",
  failed: "失败",
  skipped: "跳过",
  cancelled: "取消",
  superseded: "已被替换",
};

const WORKFLOW_STATUS_LABELS: Record<Workflow["status"], string> = {
  running: "运行中",
  paused: "已暂停",
  succeeded: "完成",
  failed: "失败",
  cancelled: "已取消",
};

const ATTEMPT_STATUS_LABELS: Record<Attempt["status"], string> = {
  preparing: "准备中",
  running: "运行中",
  succeeded: "完成",
  failed: "失败",
  cancelled: "取消",
};

const TIER_LABELS: Record<WorkflowNode["tier"], string> = {
  utility: "打杂",
  planner: "大脑",
  worker: "干活",
};

const KNOWN_STATUS = new Set<string>(Object.keys(STATUS_LABELS));

interface Placement {
  node: WorkflowNode;
  level: number;
  row: number;
  x: number;
  y: number;
}

interface Layout {
  placements: Placement[];
  byId: Map<string, Placement>;
  width: number;
  height: number;
}

function layoutNodes(nodes: WorkflowNode[]): Layout {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const levels = new Map<string, number>();
  const visiting = new Set<string>();

  function levelOf(id: string): number {
    const cached = levels.get(id);
    if (cached !== undefined) return cached;
    if (visiting.has(id)) return 0;
    const node = byId.get(id);
    if (!node) return 0;
    visiting.add(id);
    const parents = node.depends_on.filter((parent) => byId.has(parent));
    const level =
      parents.length > 0
        ? Math.max(...parents.map((parent) => levelOf(parent) + 1))
        : 0;
    visiting.delete(id);
    levels.set(id, level);
    return level;
  }

  const rows = new Map<number, number>();
  const placements: Placement[] = [];
  for (const node of nodes) {
    const level = levelOf(node.id);
    const row = rows.get(level) ?? 0;
    rows.set(level, row + 1);
    placements.push({
      node,
      level,
      row,
      x: PAD + level * (NODE_W + GAP_X),
      y: PAD + row * (NODE_H + GAP_Y),
    });
  }
  const maxLevel = placements.reduce(
    (max, item) => Math.max(max, item.level),
    -1,
  );
  const maxRow = placements.reduce((max, item) => Math.max(max, item.row), -1);
  return {
    placements,
    byId: new Map(placements.map((item) => [item.node.id, item])),
    width: PAD * 2 + (maxLevel + 1) * NODE_W + maxLevel * GAP_X,
    height: PAD * 2 + (maxRow + 1) * NODE_H + maxRow * GAP_Y,
  };
}

function statusText(node: WorkflowNode): string {
  const raw = String(node.status);
  const base = KNOWN_STATUS.has(raw)
    ? STATUS_LABELS[node.status]
    : `未知状态 · ${cleanText(raw, true)}`;
  if (!node.verdict) return base;
  return `${base} · ${cleanText(String(node.verdict), true)}`;
}

function roleText(node: WorkflowNode): string {
  return nodeRoleLabel(String(node.role));
}

function workflowStatusText(workflow: Workflow): string {
  const raw = String(workflow.status);
  return Object.hasOwn(WORKFLOW_STATUS_LABELS, raw)
    ? WORKFLOW_STATUS_LABELS[raw as Workflow["status"]]
    : `未知状态 · ${cleanText(raw, true)}`;
}

function workflowStatusTone(workflow: Workflow): Tone {
  switch (workflow.status) {
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

/** `--idle` 作文字色对比度不足 4.5:1，中性状态文字改用 `--text-muted`。 */
function statusTextClass(tone: Tone): string {
  return tone === "idle" ? "eg-status-text--neutral" : `tone-text-${tone}`;
}

function attemptStatusText(attempt: Attempt): string {
  const raw = String(attempt.status);
  return Object.hasOwn(ATTEMPT_STATUS_LABELS, raw)
    ? ATTEMPT_STATUS_LABELS[raw as Attempt["status"]]
    : `未知状态 · ${cleanText(raw, true)}`;
}

function findDelivery(
  state: DesktopState,
  deliveryId: string | null,
): Delivery | null {
  if (!deliveryId) return null;
  for (const data of Object.values(state.projectData)) {
    const delivery = data.deliveries.find((item) => item.id === deliveryId);
    if (delivery) return delivery;
  }
  return null;
}

function formatClock(iso: string | null): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

const NUMBER_FORMAT = new Intl.NumberFormat("en-US");

function formatNumber(value: number): string {
  return NUMBER_FORMAT.format(value);
}

function formatSeconds(value: number): string {
  const seconds = Math.max(0, Math.floor(value));
  if (seconds < 60) return `${seconds} 秒`;
  if (seconds < 3_600) {
    const minutes = Math.floor(seconds / 60);
    return `${minutes} 分 ${seconds % 60} 秒`;
  }
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  return minutes > 0 ? `${hours} 小时 ${minutes} 分` : `${hours} 小时`;
}

function numeric(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function budgetValue(
  value: unknown,
  options: { seconds?: boolean } = {},
): ReactNode {
  if (value === undefined || value === null) return "—";
  const number = numeric(value);
  if (number !== null) {
    return options.seconds ? formatSeconds(number) : formatNumber(number);
  }
  if (typeof value === "string") {
    return <UntrustedText value={value} identifier />;
  }
  return "未知结构";
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

const DIMENSION_META: Record<
  string,
  { label: string; unit: string; seconds?: boolean }
> = {
  nodes: { label: "节点", unit: "个" },
  attempts: { label: "尝试", unit: "次" },
  rounds: { label: "轮次", unit: "轮" },
  wall_time: { label: "运行时长", unit: "秒", seconds: true },
  forge_writes: { label: "Forge 写", unit: "次" },
  concurrency: { label: "并发", unit: "—" },
};

const KNOWN_TIERS = ["utility", "planner", "worker"];

export function ExecutionGraph({
  controller,
  state,
  workflowId,
  deliveryId,
  progress = null,
  onClose,
}: ExecutionGraphProps): ReactNode {
  const workflow = state.workflows[workflowId] ?? null;
  const nodes = useMemo(() => workflow?.nodes ?? [], [workflow]);
  const layout = useMemo(() => layoutNodes(nodes), [nodes]);
  const [workflowLoaded, setWorkflowLoaded] = useState(false);
  const [workflowReload, setWorkflowReload] = useState(0);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [attemptLoaded, setAttemptLoaded] = useState<Record<string, true>>({});
  const [attemptReload, setAttemptReload] = useState(0);
  const [budgetLoaded, setBudgetLoaded] = useState(false);
  const [budgetReload, setBudgetReload] = useState(0);
  const [retryBusy, setRetryBusy] = useState(false);
  const nodeRefs = useRef(new Map<string, HTMLButtonElement>());
  const canvasRef = useRef<HTMLDivElement>(null);
  const viewer = state.me?.device.role === "viewer";
  const isScenario =
    (controller.ipc as { isScenario?: boolean } | undefined)?.isScenario ===
    true;

  const effectiveDeliveryId = deliveryId ?? workflow?.delivery_id ?? null;
  const delivery = findDelivery(state, effectiveDeliveryId);
  const budget = effectiveDeliveryId
    ? state.budgetsByDelivery[effectiveDeliveryId] ?? null
    : null;

  useEffect(() => {
    let alive = true;
    setWorkflowLoaded(false);
    void controller.ensureWorkflow(workflowId).then(
      () => {
        if (alive) setWorkflowLoaded(true);
      },
      () => {
        if (alive) setWorkflowLoaded(true);
      },
    );
    return () => {
      alive = false;
    };
  }, [controller, workflowId, workflowReload]);

  useEffect(() => {
    if (!effectiveDeliveryId) return;
    let alive = true;
    setBudgetLoaded(false);
    void controller.ensureBudget(effectiveDeliveryId).then(
      () => {
        if (alive) setBudgetLoaded(true);
      },
      () => {
        if (alive) setBudgetLoaded(true);
      },
    );
    return () => {
      alive = false;
    };
  }, [controller, effectiveDeliveryId, budgetReload]);

  useEffect(() => {
    if (!selectedId) return;
    let alive = true;
    setAttemptLoaded((current) => {
      if (!(selectedId in current)) return current;
      const next = { ...current };
      delete next[selectedId];
      return next;
    });
    void controller.ensureAttempts(selectedId).then(
      () => {
        if (alive) {
          setAttemptLoaded((current) => ({ ...current, [selectedId]: true }));
        }
      },
      () => {
        if (alive) {
          setAttemptLoaded((current) => ({ ...current, [selectedId]: true }));
        }
      },
    );
    return () => {
      alive = false;
    };
  }, [controller, selectedId, attemptReload]);

  useEffect(() => {
    if (selectedId && !layout.byId.has(selectedId)) setSelectedId(null);
  }, [layout, selectedId]);

  // lint 禁止 JSX 内联样式；布局坐标只在 DOM 上更新。
  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (canvas) {
      canvas.style.width = `${layout.width}px`;
      canvas.style.height = `${layout.height}px`;
    }
    for (const item of layout.placements) {
      const element = nodeRefs.current.get(item.node.id);
      if (!element) continue;
      element.style.left = `${item.x}px`;
      element.style.top = `${item.y}px`;
    }
  }, [layout]);

  const selectedNode = selectedId
    ? nodes.find((node) => node.id === selectedId) ?? null
    : null;
  const attempts = selectedNode
    ? state.attemptsByNode[selectedNode.id]
    : undefined;
  const attemptsKnown = selectedNode ? attemptLoaded[selectedNode.id] : false;
  const nodeLabels = new Map(
    nodes.map((node) => [node.id, roleText(node)] as const),
  );

  function selectNode(nodeId: string): void {
    setSelectedId(nodeId);
  }

  function moveSelection(nodeId: string, delta: number): void {
    const index = layout.placements.findIndex(
      (item) => item.node.id === nodeId,
    );
    if (index < 0) return;
    const next =
      layout.placements[
        (index + delta + layout.placements.length) % layout.placements.length
      ];
    if (!next) return;
    setSelectedId(next.node.id);
    nodeRefs.current.get(next.node.id)?.focus();
  }

  async function retryNode(node: WorkflowNode): Promise<void> {
    setRetryBusy(true);
    try {
      await controller.retryNode(node.id);
      setAttemptLoaded((current) => {
        const next = { ...current };
        delete next[node.id];
        return next;
      });
      setAttemptReload((value) => value + 1);
    } catch {
      // controller 已通过 notice 呈现路由失败；这里只恢复按钮状态。
    } finally {
      setRetryBusy(false);
    }
  }

  const canRetrySelected =
    selectedNode !== null &&
    selectedNode.status === "failed" &&
    !selectedNode.superseded_by &&
    controller.can("retry") &&
    !viewer &&
    delivery?.stage !== "closed" &&
    delivery?.stage !== "abandoned" &&
    delivery?.stage_status !== "paused";

  return (
    <Modal
      open
      label="执行图"
      onClose={onClose}
      variant="fullscreen"
      className="eg-dialog"
    >
      <div className="eg">
        <header className="eg__header">
          <button
            type="button"
            className="btn btn-sm"
            onClick={onClose}
            autoFocus
          >
            ← 返回
          </button>
          <h1 className="eg__title">执行图</h1>
          {workflow ? (
            <span className="eg__meta mono">
              修订 {workflow.graph_rev}
            </span>
          ) : null}
          <span className="eg__demo">
            {isScenario ? "示例数据 · 只读" : "只读"}
          </span>
        </header>

        {!workflow ? (
          <div className="eg__state">
            {workflowLoaded ? (
              <>
                <p>执行图未载入。</p>
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={() => setWorkflowReload((value) => value + 1)}
                >
                  重新读取执行图
                </button>
              </>
            ) : (
              <p role="status">正在读取执行图…</p>
            )}
          </div>
        ) : (
          <div className="eg__body">
            <section className="eg__graph" aria-label="节点">
              <header className="eg__graph-head">
                <span>
                  阶段 ·{" "}
                  {workflow.stage ? (
                    <UntrustedText value={stageLabel(workflow.stage)} />
                  ) : (
                    "未指定阶段"
                  )}
                </span>
                <span className={statusTextClass(workflowStatusTone(workflow))}>
                  {workflowStatusText(workflow)}
                </span>
                <span>
                  模板 <UntrustedText value={workflow.template} identifier />
                </span>
                <span>{nodes.length} 个节点</span>
                {nodes.some((node) => node.status === "superseded") ? (
                  <span>含已替换节点</span>
                ) : null}
              </header>
              <p className="eg__boundary">
                图修订仅展示当前图与已替换节点，不是完整历史快照；生产读模型待补。
              </p>

              {nodes.length === 0 ? (
                <p className="eg__empty">这个工作流没有节点。</p>
              ) : (
                <div
                  className="eg__scroll"
                  role="group"
                  aria-label="执行图节点，可滚动"
                  tabIndex={0}
                >
                  <div className="eg__canvas" ref={canvasRef}>
                    <svg
                      className="eg__edges"
                      width={layout.width}
                      height={layout.height}
                      aria-hidden="true"
                    >
                      {layout.placements.flatMap((item) =>
                        item.node.depends_on.map((parentId) => {
                          const parent = layout.byId.get(parentId);
                          if (!parent) return null;
                          const x1 = parent.x + NODE_W;
                          const y1 = parent.y + NODE_H / 2;
                          const x2 = item.x;
                          const y2 = item.y + NODE_H / 2;
                          return (
                            <path
                              key={`${parentId}:${item.node.id}`}
                              className={`eg-edge${
                                parent.node.status === "superseded"
                                  ? " eg-edge--muted"
                                  : ""
                              }`}
                              d={`M ${x1} ${y1} C ${x1 + 32} ${y1}, ${
                                x2 - 32
                              } ${y2}, ${x2} ${y2}`}
                            />
                          );
                        }),
                      )}
                    </svg>
                    {layout.placements.map((item) => {
                      const node = item.node;
                      const selected = node.id === selectedId;
                      return (
                        <button
                          key={node.id}
                          type="button"
                          ref={(element) => {
                            if (element) nodeRefs.current.set(node.id, element);
                            else nodeRefs.current.delete(node.id);
                          }}
                          data-node-id={node.id}
                          className={`eg-node eg-node--${nodeStatusTone(node)}${
                            node.status === "superseded"
                              ? " eg-node--superseded"
                              : ""
                          }${selected ? " eg-node--selected" : ""}`}
                          aria-pressed={selected}
                          aria-label={`${cleanText(roleText(node))}：${cleanText(
                            statusText(node),
                          )}`}
                          onClick={() => selectNode(node.id)}
                          onFocus={() => selectNode(node.id)}
                          onKeyDown={(event) => {
                            if (event.key === "ArrowRight" || event.key === "ArrowDown") {
                              event.preventDefault();
                              moveSelection(node.id, 1);
                            } else if (
                              event.key === "ArrowLeft" ||
                              event.key === "ArrowUp"
                            ) {
                              event.preventDefault();
                              moveSelection(node.id, -1);
                            }
                          }}
                        >
                          <span className="eg-node__role">
                            <UntrustedText value={roleText(node)} />
                          </span>
                          <span className="eg-node__status">
                            {statusText(node)}
                          </span>
                          <span className="eg-node__foot">
                            <ToneDot tone={nodeStatusTone(node)} />
                            <span className="mono">
                              {node.required ? "必需" : "可选"}
                            </span>
                            {node.superseded_by ? <span>· 已替换</span> : null}
                          </span>
                        </button>
                      );
                    })}
                  </div>
                </div>
              )}
              <p className="eg__hint">
                点击或用 Tab 选择节点；方向键切换。依赖线只读，不提供 DAG 编辑。
              </p>
            </section>

            <aside className="eg__detail" aria-label="节点详情">
              {selectedNode ? (
                <NodeDetail
                  node={selectedNode}
                  attempts={attempts}
                  attemptsKnown={attemptsKnown}
                  canRetry={canRetrySelected}
                  retryBusy={retryBusy}
                  progress={progress}
                  nodeLabels={nodeLabels}
                  onRetry={() => void retryNode(selectedNode)}
                  onReloadAttempts={() => setAttemptReload((value) => value + 1)}
                />
              ) : (
                <p className="eg__empty">选择一个节点查看尝试与用量。</p>
              )}

              <BudgetPanel
                ledger={budget}
                hasDelivery={effectiveDeliveryId !== null}
                loaded={budgetLoaded}
                onReload={() => setBudgetReload((value) => value + 1)}
              />
            </aside>
          </div>
        )}
      </div>
    </Modal>
  );
}

function NodeDetail({
  node,
  attempts,
  attemptsKnown,
  canRetry,
  retryBusy,
  progress,
  nodeLabels,
  onRetry,
  onReloadAttempts,
}: {
  node: WorkflowNode;
  attempts: Attempt[] | undefined;
  attemptsKnown: boolean;
  canRetry: boolean;
  retryBusy: boolean;
  progress: ExecutionGraphProgress | null;
  nodeLabels: Map<string, string>;
  onRetry: () => void;
  onReloadAttempts: () => void;
}): ReactNode {
  const tools = progress
    ? progress.tools.filter((tool) => tool.node_id === node.id)
    : [];
  const summaries = progress
    ? progress.summaries.filter((summary) => summary.node_id === node.id)
    : [];
  const attemptById = new Map(
    (attempts ?? []).map((attempt) => [attempt.id, attempt]),
  );

  return (
    <div className="eg-detail">
      <header className="eg-detail__head">
        <h2 className="eg-detail__title">
          <UntrustedText value={roleText(node)} />
        </h2>
        <span
          className={`eg-detail__status ${statusTextClass(nodeStatusTone(node))}`}
        >
          {statusText(node)}
        </span>
      </header>
      <dl className="eg-kv">
        <dt>层级</dt>
        <dd>
          {Object.hasOwn(TIER_LABELS, String(node.tier))
            ? TIER_LABELS[node.tier]
            : <UntrustedText value={String(node.tier)} identifier />}
        </dd>
        <dt>执行</dt>
        <dd className="mono">
          <UntrustedText value={String(node.exec)} identifier />
        </dd>
        <dt>必需</dt>
        <dd>{node.required ? "是" : "否"}</dd>
        <dt>判定</dt>
        <dd className="mono">
          {node.verdict ? (
            <UntrustedText value={String(node.verdict)} identifier />
          ) : (
            "—"
          )}
        </dd>
        <dt>依赖</dt>
        <dd>
          {node.depends_on.length === 0 ? (
            "无"
          ) : (
            <ul className="eg-deps">
              {node.depends_on.map((parentId) => (
                <li key={parentId} className="eg-dep">
                  <UntrustedText
                    value={nodeLabels.get(parentId) ?? parentId}
                    identifier={!nodeLabels.has(parentId)}
                  />
                  <span className="mono eg-dep__id">
                    <UntrustedText value={parentId} identifier />
                  </span>
                </li>
              ))}
            </ul>
          )}
        </dd>
        {node.superseded_by ? (
          <>
            <dt>已替换为</dt>
            <dd className="mono">
              <UntrustedText value={node.superseded_by} identifier />
            </dd>
          </>
        ) : null}
      </dl>
      {node.superseded_by ? (
        <p className="eg-note">
          该节点来自 include_superseded，旧尝试仍可读；这不是完整历史快照。
        </p>
      ) : null}

      <section className="eg-attempts" aria-label="尝试">
        <h3 className="eg-section-title">尝试</h3>
        {attempts === undefined ? (
          attemptsKnown ? (
            <div className="eg-inline-state">
              <p>尝试记录未载入。</p>
              <button
                type="button"
                className="btn btn-sm"
                onClick={onReloadAttempts}
              >
                重新读取尝试
              </button>
            </div>
          ) : (
            <p role="status" className="eg-muted">
              正在读取尝试…
            </p>
          )
        ) : attempts.length === 0 ? (
          <p className="eg-muted">没有尝试记录。</p>
        ) : (
          <ol className="eg-attempt-list">
            {attempts.map((attempt) => (
              <li key={attempt.id} className="eg-attempt">
                <header className="eg-attempt__head">
                  <h4 className="eg-attempt__title">尝试 #{attempt.n}</h4>
                  <span className={statusTextClass(attemptTone(attempt))}>
                    {attemptStatusText(attempt)}
                  </span>
                </header>
                <dl className="eg-kv eg-kv--tight">
                  <dt>开始</dt>
                  <dd className="mono">{formatClock(attempt.started_at)}</dd>
                  <dt>结束</dt>
                  <dd className="mono">{formatClock(attempt.ended_at)}</dd>
                  <dt>沙盒</dt>
                  <dd className="mono">
                    {attempt.sandbox_id ? (
                      <UntrustedText value={attempt.sandbox_id} identifier />
                    ) : (
                      "—"
                    )}
                  </dd>
                  {attempt.failure_class ? (
                    <>
                      <dt>失败</dt>
                      <dd className="mono">
                        <UntrustedText
                          value={attempt.failure_class}
                          identifier
                        />
                      </dd>
                    </>
                  ) : null}
                  {attempt.failure_detail ? (
                    <>
                      <dt>原因</dt>
                      <dd>
                        <UntrustedText value={attempt.failure_detail} />
                      </dd>
                    </>
                  ) : null}
                  <dt>用量</dt>
                  <dd className="eg-usage">
                    <span>输入 {formatNumber(attempt.usage.input_tokens)}</span>
                    <span>输出 {formatNumber(attempt.usage.output_tokens)}</span>
                    <span>
                      缓存读 {formatNumber(attempt.usage.cache_read_tokens)}
                    </span>
                    <span>
                      缓存写 {formatNumber(attempt.usage.cache_write_tokens)}
                    </span>
                    <span>请求 {formatNumber(attempt.usage.requests)}</span>
                  </dd>
                  <dt>结果</dt>
                  <dd className="mono eg-ref">
                    <UntrustedText value={attempt.result_ref} identifier />
                    <span className="eg-note">
                      仅结果编号；内容读取接口未定义（Q-12）。
                    </span>
                  </dd>
                </dl>
              </li>
            ))}
          </ol>
        )}
        {canRetry ? (
          <div className="eg-retry">
            <button
              type="button"
              className="btn btn-sm"
              disabled={retryBusy}
              onClick={onRetry}
            >
              {retryBusy ? "正在重试…" : "重试失败节点"}
            </button>
            <span className="eg-muted">
              经 nodes.retry 追加新尝试；旧尝试保持不变。
            </span>
          </div>
        ) : null}
      </section>

      {tools.length > 0 ? (
        <section className="eg-activity" aria-label="模拟工具">
          <h3 className="eg-section-title">模拟工具（演示）</h3>
          <ul className="eg-activity__list">
            {tools.map((tool) => (
              <li key={tool.call_id} className="eg-activity__item">
                <details>
                  <summary>
                    <span className="eg-activity__name">
                      <UntrustedText value={tool.name} />
                    </span>
                    <span className="eg-activity__state">
                      {tool.status === "running"
                        ? "运行中"
                        : tool.status === "failed"
                          ? "失败"
                          : "完成"}
                    </span>
                    {tool.elapsed_ms !== null ? (
                      <span className="mono eg-muted">
                        {formatNumber(tool.elapsed_ms)} ms
                      </span>
                    ) : null}
                  </summary>
                  <p className="mono eg-activity__target">
                    <UntrustedText value={tool.target} />
                  </p>
                  <p className="eg-muted">
                    <UntrustedText value={tool.collapsed_detail} />
                  </p>
                  <p className="eg-note mono">
                    <UntrustedText value={tool.call_id} identifier /> · 尝试 #
                    {attemptById.get(tool.attempt_id)?.n ?? "?"}
                  </p>
                </details>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {summaries.length > 0 ? (
        <section className="eg-summary" aria-label="模拟执行摘要">
          <h3 className="eg-section-title">模拟执行摘要（演示）</h3>
          {summaries.map((summary) => (
            <p key={summary.summary_id} className="eg-summary__text">
              <span className="eg-muted">
                {summary.status === "done"
                  ? "完成"
                  : summary.status === "failed"
                    ? "失败"
                    : "生成中"}
                ：
              </span>
              <UntrustedText value={summary.text} />
            </p>
          ))}
          <p className="eg-note">
            只展示可公开的模拟执行摘要，不是模型私有思维链。
          </p>
        </section>
      ) : null}
    </div>
  );
}

function attemptTone(attempt: Attempt): Tone {
  switch (attempt.status) {
    case "running":
    case "preparing":
      return "run";
    case "succeeded":
      return "ok";
    case "failed":
      return "warn";
    default:
      return "idle";
  }
}

function BudgetPanel({
  ledger,
  hasDelivery,
  loaded,
  onReload,
}: {
  ledger: BudgetLedger | null;
  hasDelivery: boolean;
  loaded: boolean;
  onReload: () => void;
}): ReactNode {
  if (!ledger) {
    return (
      <section className="eg-budget" aria-label="只读预算">
        <h3 className="eg-section-title">预算 · 只读</h3>
        {!hasDelivery ? (
          <p className="eg-muted">该工作流没有交付预算台账。</p>
        ) : loaded ? (
          <div className="eg-inline-state">
            <p className="eg-muted">预算台账未载入。</p>
            <button type="button" className="btn btn-sm" onClick={onReload}>
              重新读取预算
            </button>
          </div>
        ) : (
          <p role="status" className="eg-muted">
            正在读取预算…
          </p>
        )}
      </section>
    );
  }

  const limits = asRecord(ledger.limits);
  const used = asRecord(ledger.used);
  const limitTokens = asRecord(limits.tokens_by_tier);
  const usedTokens = asRecord(used.tokens_by_tier);
  const tiers = [
    ...KNOWN_TIERS.filter(
      (tier) => tier in limitTokens || tier in usedTokens,
    ),
    ...Array.from(
      new Set([...Object.keys(limitTokens), ...Object.keys(usedTokens)]),
    )
      .filter((tier) => !KNOWN_TIERS.includes(tier))
      .sort(),
  ];
  const dimensions = [
    ...Object.keys(DIMENSION_META).filter(
      (key) => key in limits || key in used,
    ),
    ...Array.from(
      new Set([
        ...Object.keys(limits),
        ...Object.keys(used),
      ]),
    )
      .filter(
        (key) => key !== "tokens_by_tier" && !(key in DIMENSION_META),
      )
      .sort(),
  ];

  return (
    <section className="eg-budget" aria-label="只读预算">
      <h3 className="eg-section-title">预算 · 只读</h3>
      <p className="eg-budget__template">
        模板{" "}
        {ledger.template ? (
          <UntrustedText value={ledger.template} identifier />
        ) : (
          "未指定"
        )}
      </p>
      <table className="eg-budget__table">
        <thead>
          <tr>
            <th scope="col">维度</th>
            <th scope="col">上限</th>
            <th scope="col">已用</th>
            <th scope="col">单位</th>
            <th scope="col">比例</th>
          </tr>
        </thead>
        <tbody>
          {tiers.map((tier) => {
            const limit = numeric(limitTokens[tier]);
            const spent = numeric(usedTokens[tier]);
            return (
              <tr key={`token:${tier}`}>
                <th scope="row">
                  tokens · <UntrustedText value={tier} identifier />
                </th>
                <td className="mono">{budgetValue(limitTokens[tier])}</td>
                <td className="mono">{budgetValue(usedTokens[tier])}</td>
                <td>tokens</td>
                <td className="eg-budget__meter-cell">
                  <BudgetMeter limit={limit} used={spent} />
                </td>
              </tr>
            );
          })}
          {dimensions.map((key) => {
            const meta = DIMENSION_META[key];
            const limit = numeric(limits[key]);
            const spent = numeric(used[key]);
            return (
              <tr key={`dimension:${key}`}>
                <th scope="row">
                  {meta ? meta.label : <UntrustedText value={key} />}
                </th>
                <td className="mono">
                  {budgetValue(limits[key], { seconds: meta?.seconds })}
                </td>
                <td className="mono">
                  {budgetValue(used[key], { seconds: meta?.seconds })}
                </td>
                <td>{meta ? meta.unit : "—"}</td>
                <td className="eg-budget__meter-cell">
                  <BudgetMeter limit={limit} used={spent} />
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      <p className="eg-note">
        只展示台账已有维度、上限与消耗。预算追加规则（Q-11）尚未定义，这里不提供追加。
      </p>
    </section>
  );
}

function BudgetMeter({
  limit,
  used,
}: {
  limit: number | null;
  used: number | null;
}): ReactNode {
  if (limit === null || used === null || limit <= 0) {
    return <span className="eg-muted">—</span>;
  }
  const ratio = Math.min(1, Math.max(0, used / limit));
  return (
    <progress
      className={`eg-meter${ratio >= 1 ? " eg-meter--full" : ""}`}
      max={limit}
      value={used}
      aria-label={`已用 ${Math.round(ratio * 100)}%`}
    />
  );
}
