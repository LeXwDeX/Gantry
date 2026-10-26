import type { ReactNode } from "react";
import { useEffect, useRef, useState } from "react";
import type {
  Approval,
  Delivery,
  InboxItem,
  Message,
  Project,
  Ref,
  Workflow,
  WorkflowNode,
} from "@/api/types";
import type { ProjectUiState, Selection } from "@/api/shell";
import type { DesktopState, ProjectData } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import {
  approvalActionLabel,
  inboxKindLabel,
  stageLabel,
  stageStatusLabel,
} from "@/i18n";
import { cleanText, UntrustedText } from "@/render";
import {
  approvalForDelivery,
  approvalIdOfItem,
  approvalTargetText,
  approvalView,
  deliveryChain,
  deliveryForSelection,
  deliveryState,
  failedNodes,
  formatClock,
  guardianEvents,
  inboxFinding,
  inboxMissing,
  inboxSource,
  levelLine,
  levelReason,
  mergeGates,
  nodeRoleLabel,
  nodeStatusLabel,
  nodeStatusTone,
  openApprovalItemForDelivery,
  refForSelection,
  stageCells,
  worktreeHolderLabel,
  workflowForDelivery,
} from "./model";
import { InboxActions } from "./InboxActions";
import { ToneDot } from "./parts";

export interface InspectorProps {
  controller: DesktopController;
  state: DesktopState;
  project: Project | null;
  data: ProjectData;
  ui: ProjectUiState;
  inbox: InboxItem[];
  needsByDelivery: Map<string, InboxItem[]>;
  isViewer: boolean;
  onCite: (ref: Ref) => void;
  onReplyDelivery: (deliveryId: string) => void;
  onSelect: (selection: Selection) => void;
  onOpenApproval: (approvalId: string) => void;
  /** 打开只读执行图；缺省时不显示入口。 */
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
}
export function Inspector({
  controller,
  state,
  project,
  data,
  ui,
  inbox,
  needsByDelivery,
  isViewer,
  onCite,
  onReplyDelivery,
  onSelect,
  onOpenApproval,
  onOpenWorkflow,
}: InspectorProps): ReactNode {
  const selection = ui.sel;
  if (!selection) {
    return (
      <InspectorEmpty>
        在对话、地图或左侧列表中选择一项，查看交付阶段、当前图与后台守护。
        选中只影响这里，不会改变消息发送的上下文。
      </InspectorEmpty>
    );
  }

  if (selection.kind === "base") {
    return <BaseInspector project={project} data={data} />;
  }

  const delivery = deliveryForSelection(selection, data);
  if (!delivery) {
    return (
      <InspectorEmpty>
        <p>该项不在进行中。</p>
        {selection.kind === "issue" || selection.kind === "pr" ? (
          <button
            type="button"
            className="btn btn-sm"
            onClick={() =>
              void controller.ipc.openGithub({
                kind: selection.kind,
                repo: selection.repo,
                number: selection.number,
              })
            }
          >
            在 GitHub 上打开
          </button>
        ) : null}
      </InspectorEmpty>
    );
  }

  const needItems = needsByDelivery.get(delivery.id) ?? [];
  const status = deliveryState(delivery, needItems.length > 0);
  const ref = refForSelection(selection);
  const workflow = workflowForDelivery(state.workflows, delivery);
  const approval = approvalForDelivery(state.approvals, delivery);
  const openApprovalItem = openApprovalItemForDelivery(inbox, delivery);
  const approvalId =
    approval?.id ??
    (openApprovalItem ? approvalIdOfItem(openApprovalItem) : null);
  const gateApproval = approvalView(
    state.approvals,
    inbox,
    approvalId,
    delivery,
  );
  const gates = mergeGates(delivery.pr, project, gateApproval);
  const failures = failedNodes(workflow);
  const events = guardianEvents(data.messages, delivery);
  const terminal = delivery.stage === "closed" || delivery.stage === "abandoned";

  return (
    <InspectorBody
      controller={controller}
      delivery={delivery}
      selection={selection}
      status={status}
      refValue={ref}
      workflow={workflow}
      failures={failures}
      gates={gates}
      needItems={needItems}
      approval={approval}
      approvalLabel={gateApproval.label}
      approvalPending={gateApproval.pending}
      approvalId={approvalId}
      events={events}
      data={data}
      project={project}
      isViewer={isViewer}
      terminal={terminal}
      onCite={onCite}
      onReplyDelivery={onReplyDelivery}
      onSelect={onSelect}
      onOpenApproval={onOpenApproval}
      onOpenWorkflow={onOpenWorkflow}
    />
  );
}

function InspectorBody({
  controller,
  delivery,
  selection,
  status,
  refValue,
  workflow,
  failures,
  gates,
  needItems,
  approval,
  approvalLabel,
  approvalPending,
  approvalId,
  events,
  data,
  project,
  isViewer,
  terminal,
  onCite,
  onReplyDelivery,
  onSelect,
  onOpenApproval,
  onOpenWorkflow,
}: {
  controller: DesktopController;
  delivery: Delivery;
  selection: Selection;
  status: ReturnType<typeof deliveryState>;
  refValue: Ref | null;
  workflow: Workflow | null;
  failures: WorkflowNode[];
  gates: ReturnType<typeof mergeGates>;
  needItems: InboxItem[];
  approval: Approval | null;
  approvalLabel: string;
  approvalPending: boolean;
  approvalId: string | null;
  events: Message[];
  data: ProjectData;
  project: Project | null;
  isViewer: boolean;
  terminal: boolean;
  onCite: (ref: Ref) => void;
  onReplyDelivery: (deliveryId: string) => void;
  onSelect: (selection: Selection) => void;
  onOpenApproval: (approvalId: string) => void;
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
}): ReactNode {
  const [moreOpen, setMoreOpen] = useState(false);
  const [confirmAbandon, setConfirmAbandon] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const confirmRef = useRef<HTMLDivElement>(null);
  const selectionId = `${selection.kind}:${"id" in selection ? selection.id : "number" in selection ? `${selection.repo}#${selection.number}` : "base"}`;

  useEffect(() => {
    setMoreOpen(false);
    setConfirmAbandon(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectionId]);

  useEffect(() => {
    if (!moreOpen) return;
    const handlePointer = (event: PointerEvent): void => {
      if (!menuRef.current?.contains(event.target as Node)) {
        setMoreOpen(false);
      }
    };
    document.addEventListener("pointerdown", handlePointer);
    return () => document.removeEventListener("pointerdown", handlePointer);
  }, [moreOpen, setMoreOpen]);

  useEffect(() => {
    if (confirmAbandon) confirmRef.current?.focus();
  }, [confirmAbandon]);

  const paused = delivery.stage_status === "paused";
  const canControl = controller.can("control") && !terminal && !isViewer;
  const canRetry = controller.can("retry") && !terminal && !isViewer;
  const abandonText = abandonDescription(delivery, data);
  const worktree = data.worktrees.find(
    (item) => item.id === delivery.worktree_id,
  );

  function togglePause(): void {
    setMoreOpen(false);
    void controller.control(delivery.id, paused ? "resume" : "pause");
  }

  function retryFirst(): void {
    const node = failures[0];
    if (!node) return;
    setMoreOpen(false);
    void controller.retryNode(node.id);
  }

  function abandon(): void {
    setConfirmAbandon(false);
    setMoreOpen(false);
    void controller.control(delivery.id, "abandon");
  }

  return (
    <div className="ws-inspector">
      <header className="ws-inspector__head">
        <div className="ws-inspector__eyebrow">
          <span className="mono">{inspectorNumber(selection, delivery)}</span>
          <span>{inspectorKind(selection)}</span>
          <span className={`tone-text-${status.tone} ws-inspector__state`}>
            {needItems.length > 0 ? "! 需要你" : status.label}
          </span>
        </div>
        <h2 className="ws-inspector__title">
          {inspectorTitle(selection, delivery, data)}
        </h2>
        <p className="ws-inspector__level">{levelLine(delivery)}</p>
        {levelReason(delivery) ? (
          <p className="ws-muted">{levelReason(delivery)}</p>
        ) : null}
        <p className="ws-inspector__chain mono">
          {deliveryChain(delivery, data)}
        </p>

        <div className="ws-inspector__actions">
          <button
            type="button"
            className="btn btn-sm"
            disabled={!refValue || isViewer}
            title={
              isViewer
                ? "此设备为只读"
                : refValue
                  ? undefined
                  : "该选中项没有可引用的 Ref"
            }
            onClick={() => {
              if (refValue) onCite(refValue);
            }}
          >
            在对话中引用
          </button>
          <div
            className="ws-inspector__menu-wrap"
            ref={menuRef}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                setMoreOpen(false);
              }
            }}
          >
            <button
              type="button"
              className="btn btn-sm"
              aria-haspopup="menu"
              aria-expanded={moreOpen}
              aria-label="更多操作"
              onClick={() => {
                setMoreOpen((value) => !value);
                setConfirmAbandon(false);
              }}
            >
              ⋯
            </button>
            {moreOpen ? (
              <div
                role="menu"
                className="ws-inspector__menu"
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    event.preventDefault();
                    setMoreOpen(false);
                  }
                }}
              >
                <button
                  type="button"
                  role="menuitem"
                  disabled={!canControl}
                  onClick={togglePause}
                >
                  {paused ? "继续" : "暂停"}
                </button>
                <button
                  type="button"
                  role="menuitem"
                  disabled={!canRetry || failures.length === 0}
                  onClick={retryFirst}
                >
                  重试失败节点
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="ws-menu-danger"
                  disabled={!canControl}
                  onClick={() => {
                    setConfirmAbandon(true);
                    setMoreOpen(false);
                  }}
                >
                  放弃交付…
                </button>
              </div>
            ) : null}
          </div>
        </div>

        {confirmAbandon ? (
          <div
            role="alertdialog"
            aria-label="确认放弃交付"
            aria-describedby="abandon-description"
            className="ws-confirm-abandon"
            ref={confirmRef}
            tabIndex={-1}
          >
            <p id="abandon-description">{abandonText}</p>
            <div className="ws-confirm-abandon__actions">
              <button
                type="button"
                className="btn btn-danger btn-sm"
                onClick={abandon}
              >
                确认放弃
              </button>
              <button
                type="button"
                className="btn btn-sm"
                autoFocus
                onClick={() => setConfirmAbandon(false)}
              >
                取消
              </button>
            </div>
          </div>
        ) : null}
      </header>

      {needItems.length > 0 ? (
        <section className="ws-inspector__section" aria-label="需要你">
          <h3 className="ws-inspector__heading">需要你</h3>
          {needItems.map((item) => (
            <NeedItemCard
              key={item.id}
              item={item}
              data={data}
              isViewer={isViewer}
              onChoose={(itemId, optionId) => void controller.choose(itemId, optionId)}
              onDismiss={(itemId) => void controller.dismiss(itemId)}
              onReply={() => onReplyDelivery(delivery.id)}
              onSelect={onSelect}
              onOpenApproval={onOpenApproval}
            />
          ))}
        </section>
      ) : null}

      {approval || approvalPending ? (
        <section className="ws-inspector__section" aria-label="审批">
          <h3 className="ws-inspector__heading">审批</h3>
          <div className="ws-inspector__approval">
            <span>
              {approval ? approvalActionLabel(approval.action) : "审批"}{" "}
              <span className="mono">
                {approval
                  ? approvalTargetText(approval)
                  : delivery.pr
                    ? `PR #${delivery.pr.number}`
                    : ""}
              </span>
            </span>
            <span
              className={`tone-text-${
                approvalPending ? "need" : "idle"
              }`}
            >
              {approvalLabel}
            </span>
            {approvalId && approvalPending ? (
              <button
                type="button"
                className="btn btn-sm"
                disabled={isViewer}
                title={isViewer ? "此设备为只读" : undefined}
                onClick={() => onOpenApproval(approvalId)}
              >
                审阅审批…
              </button>
            ) : null}
          </div>
        </section>
      ) : null}

      <section className="ws-inspector__section" aria-label="交付阶段">
        <h3 className="ws-inspector__heading">交付阶段</h3>
        <div className="ws-stages">
          {stageCells(delivery).map((cell) => (
            <div
              key={cell.stage}
              className={`ws-stage ws-stage--${cell.state}`}
            >
              <span className="ws-stage__bar" aria-hidden="true" />
              <span className="ws-stage__label">{cell.label}</span>
            </div>
          ))}
        </div>
        <p className="ws-inspector__sub">
          {delivery.stage === "abandoned"
            ? "交付已放弃"
            : `当前阶段 · ${stageLabel(delivery.stage)} · ${stageStatusLabel(
                delivery.stage_status,
              )}`}
        </p>
      </section>

      {!terminal && onOpenWorkflow ? (
        <WorkflowEntry
          controller={controller}
          delivery={delivery}
          workflow={workflow}
          onOpenWorkflow={onOpenWorkflow}
        />
      ) : null}

      {delivery.stage === "merge" ? (
        <section className="ws-inspector__section" aria-label="合并闸门">
          <h3 className="ws-inspector__heading">合并闸门</h3>
          <ul className="ws-gates">
            {gates.map((gate) => (
              <li key={gate.key} className="ws-gates__row">
                <ToneDot tone={gate.tone} />
                <span className="ws-gates__label">{gate.label}</span>
                <span className="ws-gates__status">{gate.status}</span>
              </li>
            ))}
          </ul>
        </section>
      ) : (
        <section className="ws-inspector__section" aria-label="当前阶段的图">
          <h3 className="ws-inspector__heading">
            {terminal
              ? delivery.stage === "abandoned"
                ? "已放弃"
                : "已完成"
              : `当前阶段 · ${stageLabel(delivery.stage)}`}
          </h3>
          {terminal ? (
            <p className="ws-muted">
              {delivery.stage === "abandoned" ? "交付已放弃。" : "交付已完成。"}
            </p>
          ) : workflow?.nodes && workflow.nodes.length > 0 ? (
            <ul className="ws-graph">
              {workflow.nodes.map((node) => (
                <li key={node.id} className="ws-graph__row">
                  <ToneDot tone={nodeStatusTone(node)} />
                  <span className="ws-graph__name">
                    {nodeRoleLabel(node.role)}
                  </span>
                  <span className="ws-graph__state">
                    {nodeStatusLabel(node)}
                  </span>
                  {node.status === "failed" && !node.superseded_by ? (
                    <button
                      type="button"
                      className="btn btn-sm"
                      disabled={!canRetry}
                      title={isViewer ? "此设备为只读" : undefined}
                      onClick={() => void controller.retryNode(node.id)}
                    >
                      重试
                    </button>
                  ) : null}
                </li>
              ))}
            </ul>
          ) : (
            <p className="ws-muted">
              {delivery.level_cap === "Low" && delivery.stage === "review"
                ? "Low 档评审等待 CI，没有当前阶段图。"
                : "当前阶段的图未载入。"}
            </p>
          )}
          {failures.length > 0 ? (
            <div className="ws-failed">
              <span className="ws-muted">失败节点</span>
              {failures.map((node) => (
                <button
                  key={`failed:${node.id}`}
                  type="button"
                  className="btn btn-sm"
                  disabled={!canRetry}
                  title={isViewer ? "此设备为只读" : undefined}
                  onClick={() => void controller.retryNode(node.id)}
                >
                  重试 {nodeRoleLabel(node.role)}
                </button>
              ))}
            </div>
          ) : null}
        </section>
      )}

      {selection.kind === "worktree" && worktree ? (
        <section className="ws-inspector__section" aria-label="工作树">
          <h3 className="ws-inspector__heading">工作树</h3>
          <dl className="ws-kv">
            <dt>分支</dt>
            <dd className="mono">{cleanText(worktree.branch, true)}</dd>
            <dt>基线</dt>
            <dd className="mono">{cleanText(worktree.base_ref, true)}</dd>
            <dt>写权</dt>
            <dd>{worktreeHolderLabel(worktree.lock)}</dd>
            <dt>沙盒</dt>
            <dd>服务端未提供</dd>
          </dl>
        </section>
      ) : null}

      {delivery.level_cap === "Low" ? (
        <p className="ws-muted ws-inspector__note">
          Low 档不设计划阶段；阶段条中的「跳过」不是执行失败。
        </p>
      ) : null}

      <section className="ws-inspector__section" aria-label="后台守护">
        <h3 className="ws-inspector__heading">后台守护</h3>
        {worktree?.lock?.holder === "guardian" ? (
          <p className="ws-muted">工作树写权：后台守护</p>
        ) : null}
        {events.length === 0 ? (
          <p className="ws-muted">没有后台守护事件。</p>
        ) : (
          <ul className="ws-guardian">
            {events.map((event) => (
              <li key={event.id} className="ws-guardian__item">
                <span className="ws-guardian__what">
                  <UntrustedText value={event.text} />
                </span>
                <span className="ws-guardian__when mono">
                  {formatClock(event.created_at)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </section>

      {project ? (
        <p className="ws-muted ws-inspector__repo mono">
          {cleanText(project.repo, true)}
        </p>
      ) : null}
    </div>
  );
}

function NeedItemCard({
  item,
  data,
  isViewer,
  onChoose,
  onDismiss,
  onReply,
  onSelect,
  onOpenApproval,
}: {
  item: InboxItem;
  data: ProjectData;
  isViewer: boolean;
  onChoose: (itemId: string, optionId: string) => void;
  onDismiss: (itemId: string) => void;
  onReply: () => void;
  onSelect: (selection: Selection) => void;
  onOpenApproval: (approvalId: string) => void;
}): ReactNode {
  const finding = inboxFinding(item);
  const missing = inboxMissing(item);
  const source = inboxSource(item);
  return (
    <div className="ws-need-card">
      <div className="ws-need-card__head">
        <span>! 需要你 · {inboxKindLabel(item.kind)}</span>
      </div>
      <p className="ws-need-card__summary">
        <UntrustedText value={item.summary} />
      </p>
      {finding ? (
        <p className="ws-need-card__detail">
          <span className="ws-muted">
            {cleanText(finding.severity, true)} · {cleanText(finding.category, true)}：
          </span>
          <UntrustedText value={finding.title} />
        </p>
      ) : null}
      {missing.length > 0 ? (
        <ul className="ws-need-card__missing">
          {missing.map((entry) => (
            <li key={`${entry.aspect}:${entry.question}`}>
              <span className="ws-muted">{cleanText(entry.aspect)}：</span>
              {cleanText(entry.question)}
            </li>
          ))}
        </ul>
      ) : null}
      {source ? (
        <p className="ws-muted mono">{sourceLabel(source, data)}</p>
      ) : null}
      <div className="ws-need-card__actions">
        <InboxActions
          item={item}
          data={data}
          isViewer={isViewer}
          onChoose={onChoose}
          onDismiss={onDismiss}
          onReply={onReply}
          onSelect={onSelect}
          onOpenApproval={onOpenApproval}
        />
      </div>
    </div>
  );
}

function sourceLabel(ref: Ref, data: ProjectData): string {
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
      return delivery ? cleanText(delivery.title) : "交付";
    }
  }
}

function BaseInspector({
  project,
  data,
}: {
  project: Project | null;
  data: ProjectData;
}): ReactNode {
  const branch = project?.default_branch ?? "main";
  const displayBranch = cleanText(branch, true);
  const worktrees = data.worktrees.filter(
    (worktree) => worktree.base_ref === branch,
  );
  return (
    <div className="ws-inspector">
      <header className="ws-inspector__head">
        <p className="ws-muted">基线分支</p>
        <h2 className="ws-inspector__title mono">{displayBranch}</h2>
        <dl className="ws-kv">
          <dt>分支保护</dt>
          <dd>服务端未提供</dd>
          <dt>最近 CI</dt>
          <dd>服务端未提供</dd>
          <dt>工作树</dt>
          <dd>{worktrees.length} 个基于 {displayBranch}</dd>
        </dl>
        <p className="ws-muted">
          分支 Ref 尚未在契约中定义，“从分支开始任务…”暂不可用。
        </p>
      </header>
    </div>
  );
}

function InspectorEmpty({ children }: { children: ReactNode }): ReactNode {
  return <div className="ws-inspector ws-inspector--empty">{children}</div>;
}

function inspectorNumber(selection: Selection, delivery: Delivery): string {
  switch (selection.kind) {
    case "issue":
      return `#${selection.number}`;
    case "pr":
      return `PR #${selection.number}`;
    case "worktree":
      return dataLabel(delivery);
    case "base":
      return "main";
  }
}

function inspectorKind(selection: Selection): string {
  switch (selection.kind) {
    case "issue":
      return "Issue";
    case "pr":
      return "Pull Request";
    case "worktree":
      return "工作树";
    case "base":
      return "基线";
  }
}

function inspectorTitle(
  selection: Selection,
  delivery: Delivery,
  data: ProjectData,
): string {
  if (selection.kind === "issue") {
    const issue = data.deliveries
      .flatMap((item) => item.issues)
      .find(
        (item) =>
          item.repo === selection.repo && item.number === selection.number,
      );
    if (issue) return cleanText(issue.title.text);
  }
  if (selection.kind === "pr") {
    const pr = data.deliveries
      .map((item) => item.pr)
      .find(
        (item) =>
          item?.repo === selection.repo && item.number === selection.number,
      );
    if (pr) return cleanText(pr.title.text);
  }
  if (selection.kind === "worktree") {
    const worktree = data.worktrees.find((item) => item.id === selection.id);
    if (worktree) return cleanText(worktree.branch, true);
  }
  return cleanText(delivery.title);
}

function WorkflowEntry({
  controller,
  delivery,
  workflow,
  onOpenWorkflow,
}: {
  controller: DesktopController;
  delivery: Delivery;
  workflow: Workflow | null;
  onOpenWorkflow: (workflowId: string, deliveryId?: string) => void;
}): ReactNode {
  useEffect(() => {
    if (workflow) return;
    void controller
      .ensureWorkflowForDelivery(delivery.id)
      .catch(() => {
        // 入口保持禁用与说明；加载失败不阻塞其他操作。
      });
  }, [controller, delivery.id, workflow]);

  return (
    <div className="ws-inspector__actions">
      <button
        type="button"
        className="btn btn-sm"
        disabled={!workflow}
        title={workflow ? undefined : "正在读取当前阶段的图…"}
        onClick={() => {
          if (workflow) onOpenWorkflow(workflow.id, delivery.id);
        }}
      >
        {workflow ? "查看执行图" : "正在读取执行图…"}
      </button>
    </div>
  );
}

function dataLabel(delivery: Delivery): string {
  return delivery.worktree_id ? "工作树" : "交付";
}

function abandonDescription(delivery: Delivery, data: ProjectData): string {
  const clauses: string[] = [];
  if (delivery.pr) {
    clauses.push(`关闭 PR #${delivery.pr.number}（不合并）`);
  }
  if (delivery.worktree_id) {
    const worktree = data.worktrees.find(
      (item) => item.id === delivery.worktree_id,
    );
    if (worktree) clauses.push(`保留分支 ${worktree.branch}`);
  }
  if (delivery.issues.length > 0) clauses.push("关联 Issue 保持打开");
  const detail =
    clauses.length > 0 ? clauses.join("；") : "不会关闭任何 PR";
  return `放弃这个交付：${detail}。此操作只经 controller 调服务端，不在本地伪造结果。`;
}
