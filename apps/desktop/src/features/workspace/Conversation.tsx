import type { ReactNode, RefObject } from "react";
import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type {
  Approval,
  Card,
  Delivery,
  InboxItem,
  Message,
  Ref,
  Workflow,
} from "@/api/types";
import type { ProjectUiState, Selection } from "@/api/shell";
import type { ProjectData } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { cleanText, MessageText, UntrustedText } from "@/render";
import { approvalActionLabel } from "@/i18n";
import { eventTone } from "@/theme/tone";
import {
  agentKindTone,
  buildChatRows,
  approvalCardInboxItem,
  approvalTargetText,
  approvalView,
  formatClock,
  inboxKindText,
  messageContextLabel,
  messageNeedItem,
  nodeRoleLabel,
  nodeStatusLabel,
  nodeStatusTone,
  refLabelFor,
  selectionForMessage,
  selectionForRef,
  workflowForDelivery,
} from "./model";
import { Chevron, NeedPill, ToneDot } from "./parts";
import { Composer } from "./Composer";
import { InboxActions } from "./InboxActions";
import {
  SimulationSummaryList,
  SimulationTemporary,
  SimulationToolList,
  simulationFactsForMessages,
  useSimulationClient,
  useSimulationSnapshot,
  type SimulationMessageFacts,
} from "./SimulationActivity";
import { ScenarioControls } from "./ScenarioControls";

export interface ConversationProps {
  projectId: string | null;
  controller: DesktopController;
  data: ProjectData;
  ui: ProjectUiState;
  inbox: InboxItem[];
  approvals: Record<string, Approval>;
  workflows: Record<string, Workflow>;
  isViewer: boolean;
  platform: "windows" | "macos" | "ios" | null;
  connectionLive: boolean;
  focusRequest: number;
  scrollTarget: string | null;
  composerRef: RefObject<HTMLTextAreaElement | null>;
  onUpdate: (patch: Partial<ProjectUiState>) => void;
  onSelect: (selection: Selection) => void;
  onReply: (deliveryId: string) => void;
  onOpenApproval: (approvalId: string) => void;
  onOpenExternal: (url: string) => void;
  onScrolled: () => void;
  /** 打开执行图；未接线时计划卡按钮禁用并说明。 */
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
}

export function Conversation({
  projectId,
  controller,
  data,
  ui,
  inbox,
  approvals,
  workflows,
  isViewer,
  platform,
  connectionLive,
  focusRequest,
  scrollTarget,
  composerRef,
  onUpdate,
  onSelect,
  onReply,
  onOpenApproval,
  onOpenExternal,
  onScrolled,
  onOpenWorkflow,
}: ConversationProps): ReactNode {
  const listRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const previousProject = useRef<string>("");
  const previousCount = useRef(0);
  const pendingAnchor = useRef<{ height: number; top: number } | null>(null);
  const [newBelow, setNewBelow] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [localFocus, setLocalFocus] = useState(0);

  const rows = buildChatRows(data.messages);
  const activeDeliveries = data.deliveries.filter(
    (delivery) => delivery.stage !== "closed" && delivery.stage !== "abandoned",
  );
  const emptyChat = data.messages.length === 0;
  const simulationClient = useSimulationClient(controller);
  const simulation = useSimulationSnapshot(simulationClient);
  const simulationFacts = useMemo(
    () => simulationFactsForMessages(simulation, projectId, data.messages),
    [simulation, projectId, data.messages],
  );

  useLayoutEffect(() => {
    const element = listRef.current;
    if (!element) return;
    const projectKey = data.conversation?.project_id ?? "";
    if (previousProject.current !== projectKey) {
      previousProject.current = projectKey;
      previousCount.current = data.messages.length;
      pendingAnchor.current = null;
      stickToBottom.current = true;
      element.scrollTop = element.scrollHeight;
      setNewBelow(false);
      return;
    }
    if (data.messages.length > previousCount.current && !pendingAnchor.current) {
      if (stickToBottom.current) {
        element.scrollTop = element.scrollHeight;
      } else {
        setNewBelow(true);
      }
    }
    previousCount.current = data.messages.length;
  }, [data.conversation?.project_id, data.messages.length]);

  useLayoutEffect(() => {
    if (!pendingAnchor.current || !listRef.current) return;
    const anchor = pendingAnchor.current;
    pendingAnchor.current = null;
    const element = listRef.current;
    const delta = element.scrollHeight - anchor.height;
    if (delta > 0) element.scrollTop = anchor.top + delta;
  }, [data.messages.length]);

  useLayoutEffect(() => {
    if (!scrollTarget) return;
    const element = listRef.current;
    if (!element) return;
    const target = Array.from(
      element.querySelectorAll<HTMLElement>("[data-message-id]"),
    ).find((node) => node.getAttribute("data-message-id") === scrollTarget);
    if (!target) return;
    target.scrollIntoView?.({ block: "center" });
    onScrolled();
  }, [scrollTarget, data.messages.length, ui.expandedGroups, onScrolled]);

  function loadOlder(): void {
    if (loadingOlder || !data.messageCursor) return;
    const element = listRef.current;
    if (element) {
      pendingAnchor.current = {
        height: element.scrollHeight,
        top: element.scrollTop,
      };
    }
    const before = data.messages.length;
    setLoadingOlder(true);
    void controller
      .loadOlder()
      .catch(() => undefined)
      .finally(() => {
        setLoadingOlder(false);
        if (controller.currentData.messages.length === before) {
          pendingAnchor.current = null;
        }
      });
  }

  function handleScroll(): void {
    const element = listRef.current;
    if (!element) return;
    const atBottom =
      element.scrollHeight - element.scrollTop - element.clientHeight < 40;
    stickToBottom.current = atBottom;
    if (atBottom) setNewBelow(false);
    if (element.scrollTop < 24 && data.messageCursor && !loadingOlder) {
      loadOlder();
    }
  }

  function toggleGroup(key: string): void {
    const open = ui.expandedGroups.includes(key);
    onUpdate({
      expandedGroups: open
        ? ui.expandedGroups.filter((item) => item !== key)
        : [...ui.expandedGroups, key],
    });
  }

  const composerFocus = focusRequest + localFocus;

  return (
    <div className="ws-chat">
      {import.meta.env.MODE !== "production" && simulationClient ? (
        <ScenarioControls controller={controller} />
      ) : null}
      <div
        ref={listRef}
        className="ws-chat__scroll"
        onScroll={handleScroll}
        aria-label="对话"
      >
        <div className="ws-chat__inner">
          {emptyChat ? (
            activeDeliveries.length === 0 ? (
              <EmptyProjectCard
                onDescribe={() => setLocalFocus((value) => value + 1)}
                onStartIssue={() => setLocalFocus((value) => value + 1)}
                onNewIssue={() => {
                  onUpdate({ draft: "新建 Issue：" });
                  setLocalFocus((value) => value + 1);
                }}
              />
            ) : (
              <p className="ws-empty">暂无对话消息。</p>
            )
          ) : null}

          {data.messageCursor ? (
            <button
              type="button"
              className="ws-load-older"
              onClick={loadOlder}
              disabled={loadingOlder}
            >
              {loadingOlder ? "正在加载更早的消息…" : "加载更早的消息"}
            </button>
          ) : null}

          {rows.map((row) =>
            row.type === "events" && row.messages ? (
              row.messages.length === 1 ? (
                <EventRow
                  key={row.key}
                  message={row.messages[0]}
                  data={data}
                  onSelect={onSelect}
                />
              ) : (
                <EventGroup
                  key={row.key}
                  messages={row.messages}
                  data={data}
                  expanded={ui.expandedGroups.includes(row.key)}
                  onToggle={() => toggleGroup(row.key)}
                  onSelect={onSelect}
                />
              )
            ) : row.message ? (
              <MessageRow
                key={row.message.id}
                message={row.message}
                data={data}
                inbox={inbox}
                approvals={approvals}
                workflows={workflows}
                isViewer={isViewer}
                onSelect={onSelect}
                onReply={onReply}
                onOpenApproval={onOpenApproval}
                onOpenExternal={onOpenExternal}
                onOpenWorkflow={onOpenWorkflow}
                facts={simulationFacts.get(row.message.id)}
                onChoose={(itemId, optionId) => void controller.choose(itemId, optionId)}
                onDismiss={(itemId) => void controller.dismiss(itemId)}
              />
            ) : null,
          )}

          <SimulationTemporary
            snapshot={simulation}
            projectId={projectId}
            messages={data.messages}
          />
        </div>
      </div>

      {newBelow ? (
        <button
          type="button"
          className="ws-new-below"
          onClick={() => {
            const element = listRef.current;
            if (element) element.scrollTop = element.scrollHeight;
            stickToBottom.current = true;
            setNewBelow(false);
          }}
        >
          新消息 ↓
        </button>
      ) : null}

      <Composer
        controller={controller}
        data={data}
        ui={ui}
        projectId={projectId}
        isViewer={isViewer}
        platform={platform}
        connectionLive={connectionLive}
        focusRequest={composerFocus}
        composerRef={composerRef}
        onUpdate={onUpdate}
      />
    </div>
  );
}

function EventGroup({
  messages,
  data,
  expanded,
  onToggle,
  onSelect,
}: {
  messages: Message[];
  data: ProjectData;
  expanded: boolean;
  onToggle: () => void;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  return (
    <div className="ws-event-group">
      <button
        type="button"
        className="ws-event-group__toggle"
        aria-expanded={expanded}
        onClick={onToggle}
      >
        <Chevron open={expanded} />
        <span>后台处理了 {messages.length} 件事</span>
      </button>
      {expanded ? (
        <div className="ws-event-group__items">
          {messages.map((message) => (
            <EventRow
              key={message.id}
              message={message}
              data={data}
              onSelect={onSelect}
            />
          ))}
        </div>
      ) : null}
    </div>
  );
}

function EventRow({
  message,
  data,
  onSelect,
}: {
  message: Message;
  data: ProjectData;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  const target = selectionForMessage(message, data);
  const tone = eventTone(message.tone);
  const content = (
    <>
      <ToneDot tone={tone} />
      <span className="mono ws-event__time">{formatClock(message.created_at)}</span>
      <span className="ws-event__text">
        <UntrustedText value={message.text} />
      </span>
    </>
  );
  if (!target) {
    return (
      <span
        className="ws-event ws-event--static"
        data-message-id={message.id}
      >
        {content}
      </span>
    );
  }
  return (
    <button
      type="button"
      className="ws-event"
      data-message-id={message.id}
      onClick={() => onSelect(target)}
      title="在检视器中查看"
    >
      {content}
    </button>
  );
}

function MessageRow({
  message,
  data,
  inbox,
  approvals,
  workflows,
  isViewer,
  onSelect,
  onReply,
  onOpenApproval,
  onOpenExternal,
  onOpenWorkflow,
  facts,
  onChoose,
  onDismiss,
}: {
  message: Message;
  data: ProjectData;
  inbox: InboxItem[];
  approvals: Record<string, Approval>;
  workflows: Record<string, Workflow>;
  isViewer: boolean;
  onSelect: (selection: Selection) => void;
  onReply: (deliveryId: string) => void;
  onOpenApproval: (approvalId: string) => void;
  onOpenExternal: (url: string) => void;
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
  facts?: SimulationMessageFacts;
  onChoose: (itemId: string, optionId: string) => void;
  onDismiss: (itemId: string) => void;
}): ReactNode {
  if (message.role === "user") {
    const meta = [
      `@${messageContextLabel(message, data)}`,
      `上限 ${message.level_cap ?? "项目默认"}`,
      formatClock(message.created_at),
    ].filter(Boolean);
    return (
      <div
        className="ws-msg ws-msg--user"
        data-message-id={message.id}
      >
        <div className="ws-bubble ws-bubble--user">
          <MessageText
            role="user"
            text={message.text}
            onOpenExternal={onOpenExternal}
          />
        </div>
        <span className="ws-msg__time">{meta.join(" · ")}</span>
      </div>
    );
  }

  const needItem = messageNeedItem(message, inbox);
  const agent = agentKindTone(message.agent_kind);
  const delivery = message.delivery_id
    ? data.deliveries.find((item) => item.id === message.delivery_id) ?? null
    : null;

  return (
    <div
      className="ws-msg ws-msg--agent"
      data-message-id={message.id}
    >
      <div className={`ws-bubble ws-bubble--agent ws-agent--${agent.className}`}>
        <div className="ws-bubble__head">
          <span className="ws-agent__label">{agent.label}</span>
          {needItem ? (
            <NeedPill kind={inboxKindText(needItem.kind)} />
          ) : null}
        </div>
        <MessageText
          role="agent"
          text={message.text}
          onOpenExternal={onOpenExternal}
        />
        {message.cards.map((card, index) => (
          <MessageCard
            key={`${message.id}:card:${index}`}
            card={card}
            delivery={delivery}
            data={data}
            inbox={inbox}
            approvals={approvals}
            workflows={workflows}
            isViewer={isViewer}
            onSelect={onSelect}
            onReply={onReply}
            onOpenApproval={onOpenApproval}
            onOpenWorkflow={onOpenWorkflow}
            onChoose={onChoose}
            onDismiss={onDismiss}
          />
        ))}
        {facts?.activities.length ? (
          <SimulationToolList activities={facts.activities} />
        ) : null}
        {facts?.summaries.length ? (
          <SimulationSummaryList summaries={facts.summaries} />
        ) : null}
        {message.refs.length > 0 ? (
          <div className="ws-refs">
            {message.refs.map((ref, index) => (
              <RefChip
                key={`${message.id}:ref:${index}`}
                refValue={ref}
                data={data}
                onSelect={onSelect}
              />
            ))}
          </div>
        ) : null}
        <div className="ws-bubble__foot">
          {delivery && !isViewer ? (
            <button
              type="button"
              className="ws-reply"
              onClick={() => onReply(delivery.id)}
            >
              回复
            </button>
          ) : null}
        </div>
      </div>
      <span className="ws-msg__time">{formatClock(message.created_at)}</span>
    </div>
  );
}

function RefChip({
  refValue,
  data,
  onSelect,
}: {
  refValue: Ref;
  data: ProjectData;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  const label = refLabelFor(refValue, data);
  const target = selectionForRef(refValue, data);
  if (!target) {
    return <span className="ws-chip ws-chip--static">{label}</span>;
  }
  return (
    <button
      type="button"
      className="ws-chip"
      title="在检视器中查看"
      onClick={() => onSelect(target)}
    >
      {label}
    </button>
  );
}

function MessageCard({
  card,
  delivery,
  data,
  inbox,
  approvals,
  workflows,
  isViewer,
  onSelect,
  onReply,
  onOpenApproval,
  onOpenWorkflow,
  onChoose,
  onDismiss,
}: {
  card: Card;
  delivery: Delivery | null;
  data: ProjectData;
  inbox: InboxItem[];
  approvals: Record<string, Approval>;
  workflows: Record<string, Workflow>;
  isViewer: boolean;
  onSelect: (selection: Selection) => void;
  onReply: (deliveryId: string) => void;
  onOpenApproval: (approvalId: string) => void;
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
  onChoose: (itemId: string, optionId: string) => void;
  onDismiss: (itemId: string) => void;
}): ReactNode {
  switch (card.kind) {
    case "plan":
      return (
        <PlanCard
          card={card as Extract<Card, { kind: "plan" }>}
          delivery={delivery}
          workflows={workflows}
          onOpenWorkflow={onOpenWorkflow}
        />
      );
    case "summary":
      return (
        <SummaryCard
          card={card as Extract<Card, { kind: "summary" }>}
          delivery={delivery}
          data={data}
          onSelect={onSelect}
        />
      );
    case "approval":
      return (
        <ApprovalCard
          card={card as Extract<Card, { kind: "approval" }>}
          delivery={delivery}
          inbox={inbox}
          approvals={approvals}
          isViewer={isViewer}
          onOpenApproval={onOpenApproval}
        />
      );
    case "escalation":
    case "choice":
      return (
        <DecisionCard
          card={card as Extract<Card, { kind: "escalation" | "choice" }>}
          data={data}
          inbox={inbox}
          isViewer={isViewer}
          onSelect={onSelect}
          onReply={onReply}
          onChoose={onChoose}
          onDismiss={onDismiss}
        />
      );
    default:
      return (
        <p className="ws-card ws-card--unknown">
          此卡片需要更新客户端后才能显示。
        </p>
      );
  }
}

function PlanCard({
  card,
  delivery,
  workflows,
  onOpenWorkflow,
}: {
  card: Extract<Card, { kind: "plan" }>;
  delivery: Delivery | null;
  workflows: Record<string, Workflow>;
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
}): ReactNode {
  const workflow =
    workflows[card.workflow_id] ??
    (delivery ? workflowForDelivery(workflows, delivery) : null);
  const live = workflow?.status === "running";
  return (
    <section className="ws-card">
      <div className="ws-card__head">
        <span>执行计划</span>
        <span className={live ? "ws-card__live" : "ws-muted"}>
          {live ? "● 实时" : workflow ? "已结束" : "详情未载入"}
        </span>
      </div>
      <p className="ws-card__summary">
        <UntrustedText value={card.summary} />
      </p>
      {workflow?.nodes && workflow.nodes.length > 0 ? (
        <ol className="ws-plan">
          {workflow.nodes.map((node) => (
            <li key={node.id} className="ws-plan__row">
              <ToneDot tone={nodeStatusTone(node)} />
              <span className="ws-plan__name">{nodeRoleLabel(node.role)}</span>
              <span className="ws-plan__state">{nodeStatusLabel(node)}</span>
            </li>
          ))}
        </ol>
      ) : (
        <p className="ws-muted">工作流详情未载入。</p>
      )}
      <div className="ws-card__actions">
        <button
          type="button"
          className="btn btn-sm ws-card__action"
          disabled={!onOpenWorkflow}
          title={onOpenWorkflow ? undefined : "执行图视图尚未接线"}
          onClick={() =>
            onOpenWorkflow?.(card.workflow_id, card.delivery_id)
          }
        >
          查看执行图
        </button>
      </div>
    </section>
  );
}

function SummaryCard({
  card,
  delivery,
  data,
  onSelect,
}: {
  card: Extract<Card, { kind: "summary" }>;
  delivery: Delivery | null;
  data: ProjectData;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  const items = card.items ?? [];
  if (items.length === 0) return null;
  return (
    <section className="ws-card ws-card--summary">
      <div className="ws-card__head">
        <span>打杂 · 离开期间摘要</span>
        {delivery ? (
          <span className="ws-muted">{cleanText(delivery.title)}</span>
        ) : null}
      </div>
      <ul className="ws-summary-list">
        {items.map((item, index) => {
          const target = item.ref ? selectionForRef(item.ref, data) : null;
          return (
            <li key={index}>
              {target ? (
                <button
                  type="button"
                  className="ws-summary-link"
                  onClick={() => onSelect(target)}
                >
                  <UntrustedText value={item.text} />
                </button>
              ) : (
                <span>
                  <UntrustedText value={item.text} />
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function ApprovalCard({
  card,
  delivery,
  inbox,
  approvals,
  isViewer,
  onOpenApproval,
}: {
  card: Extract<Card, { kind: "approval" }>;
  delivery: Delivery | null;
  inbox: InboxItem[];
  approvals: Record<string, Approval>;
  isViewer: boolean;
  onOpenApproval: (approvalId: string) => void;
}): ReactNode {
  const approval = approvals[card.approval_id] ?? null;
  const item =
    approvalCardInboxItem(card, inbox) ??
    (delivery
      ? inbox.find(
          (entry) =>
            entry.status === "open" &&
            entry.kind === "approval_needed" &&
            entry.delivery_id === delivery.id,
        ) ?? null
      : null);
  const view = approvalView(approvals, inbox, card.approval_id, delivery);
  const action = approval ? approvalActionLabel(approval.action) : "审批";
  const target = approval ? approvalTargetText(approval) : item?.summary ?? "";
  return (
    <section className="ws-card ws-card--approval">
      <div className="ws-card__head ws-card__head--wrap">
        <span>
          {action} <span className="mono">{target}</span>
        </span>
        <span className={`ws-card__state tone-text-${view.tone}`}>
          {view.label}
        </span>
      </div>
      {item ? (
        <p className="ws-muted">
          <UntrustedText value={item.summary} />
        </p>
      ) : null}
      {view.pending && view.id ? (
        <button
          type="button"
          className="btn btn-primary btn-sm ws-card__action"
          disabled={isViewer}
          title={isViewer ? "此设备为只读" : undefined}
          onClick={() => {
            if (view.id) onOpenApproval(view.id);
          }}
        >
          审阅审批…
        </button>
      ) : null}
      {!approval && !item ? (
        <p className="ws-muted">审批状态未载入。</p>
      ) : null}
    </section>
  );
}

function DecisionCard({
  card,
  data,
  inbox,
  isViewer,
  onSelect,
  onReply,
  onChoose,
  onDismiss,
}: {
  card: Extract<Card, { kind: "escalation" | "choice" }>;
  data: ProjectData;
  inbox: InboxItem[];
  isViewer: boolean;
  onSelect: (selection: Selection) => void;
  onReply: (deliveryId: string) => void;
  onChoose: (itemId: string, optionId: string) => void;
  onDismiss: (itemId: string) => void;
}): ReactNode {
  const item = inbox.find(
    (entry) => entry.status === "open" && entry.id === card.inbox_id,
  );
  if (!item) {
    return (
      <section className="ws-card">
        <p className="ws-muted">已处理。</p>
      </section>
    );
  }
  return (
    <section className="ws-card">
      <p className="ws-card__summary">
        <UntrustedText value={item.summary} />
      </p>
      <div className="ws-card__actions">
        <InboxActions
          item={item}
          data={data}
          isViewer={isViewer}
          onChoose={onChoose}
          onDismiss={onDismiss}
          onReply={(entry) => {
            if (entry.delivery_id) onReply(entry.delivery_id);
          }}
          onSelect={onSelect}
          onOpenApproval={() => undefined}
        />
      </div>
    </section>
  );
}

function EmptyProjectCard({
  onDescribe,
  onStartIssue,
  onNewIssue,
}: {
  onDescribe: () => void;
  onStartIssue: () => void;
  onNewIssue: () => void;
}): ReactNode {
  return (
    <section className="ws-empty-card">
      <h2>这个项目没有进行中的工作</h2>
      <p>在下面直接描述一个任务，或者从已有 Issue 开始。</p>
      <div className="ws-empty-card__actions">
        <button type="button" className="btn btn-primary btn-sm" onClick={onDescribe}>
          描述一个任务
        </button>
        <button type="button" className="btn btn-sm" onClick={onStartIssue}>
          从已有 Issue 开始
        </button>
        <button type="button" className="btn btn-sm" onClick={onNewIssue}>
          新建 Issue
        </button>
      </div>
    </section>
  );
}
