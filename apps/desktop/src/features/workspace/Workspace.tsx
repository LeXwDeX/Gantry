import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from "react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import type { Message, Project, Ref } from "@/api/types";
import type {
  Prefs,
  ProjectUiState,
  ProjectView,
  Selection,
} from "@/api/shell";
import type { DesktopState, ProjectData } from "@/store/state";
import { emptyProjectData } from "@/store/state";
import { defaultProjectUi } from "@/store/ui";
import { needsYou } from "@/store/needsYou";
import type { DesktopController } from "@/store/controller";
import { cleanText } from "@/render";
import { phaseLabel, roleLabel } from "@/i18n";
import type { Tone } from "@/theme/tone";
import {
  buildWorkRows,
  deliveryState,
  doneRows,
  inboxCountForProject,
  projectName,
  selectionForMessage,
  sameSelection,
  type DoneRow,
  type WorkRow,
} from "./model";
import { Conversation } from "./Conversation";
import { Inspector } from "./Inspector";
import { MapView } from "./MapView";
import { WorktreeTable } from "./WorktreeTable";
import { GantryMark, GearIcon, NeedPill, ToneDot } from "./parts";
import "./workspace.css";

export interface WorkspaceProps {
  controller: DesktopController;
  state: DesktopState;
  prefs: Prefs;
  onSettings: (page?: string) => void;
  onApproval: (id: string) => void;
  onInbox: () => void;
  onOpenWorkflow: (workflowId: string, deliveryId?: string) => void;
}

const VIEWS: Array<{ id: ProjectView; label: string }> = [
  { id: "chat", label: "对话" },
  { id: "map", label: "关系地图" },
  { id: "worktrees", label: "工作树" },
];

/**
 * 桌面工作区。
 *
 * 只消费 DesktopController / DesktopState，所有写操作经 controller；
 * 审批单、设置与收件箱由父级打开（onApproval / onSettings / onInbox）。
 */
export function Workspace({
  controller,
  state,
  prefs,
  onSettings,
  onApproval,
  onInbox,
  onOpenWorkflow,
}: WorkspaceProps): ReactNode {
  const project =
    state.projects.find((item) => item.id === state.currentProjectId) ?? null;
  const projectId = state.currentProjectId;
  const data: ProjectData = projectId
    ? state.projectData[projectId] ?? emptyProjectData()
    : emptyProjectData();
  const ui: ProjectUiState = projectId
    ? state.ui[projectId] ?? defaultProjectUi()
    : defaultProjectUi();
  const projection = needsYou(state);
  const totalNeeds = projection.total;
  const isViewer = state.me?.device.role === "viewer";
  const isScenario =
    (controller.ipc as { isScenario?: boolean }).isScenario === true;
  const composerRef = useRef<HTMLTextAreaElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const tabRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const [focusRequest, setFocusRequest] = useState(0);
  const [scrollTarget, setScrollTarget] = useState<string | null>(null);

  // lint 禁止 JSX 内联样式；布局宽度只在根节点上更新 CSS 变量。
  useLayoutEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    root.style.setProperty("--left-w", `${prefs.layout.leftWidth}px`);
    root.style.setProperty("--inspector-w", `${prefs.layout.inspectorWidth}px`);
    root.classList.toggle("ws-root--left-collapsed", prefs.layout.leftCollapsed);
    root.classList.toggle(
      "ws-root--inspector-collapsed",
      prefs.layout.inspectorCollapsed,
    );
  }, [prefs]);

  const requestFocus = useCallback(() => {
    setFocusRequest((value) => value + 1);
  }, []);

  const clearScrollTarget = useCallback(() => {
    setScrollTarget(null);
  }, []);

  const openExternal = useCallback(
    (url: string) => {
      void controller.ipc.openExternal({ url }).catch(() => undefined);
    },
    [controller],
  );

  const select = useCallback(
    (selection: Selection) => {
      controller.updateUi({ sel: selection });
    },
    [controller],
  );

  const update = useCallback(
    (patch: Partial<ProjectUiState>) => {
      controller.updateUi(patch);
    },
    [controller],
  );

  const cite = useCallback(
    (ref: Ref) => {
      controller.updateUi({
        view: "chat",
        ctx: ref,
        levelOverride: null,
      });
      requestFocus();
    },
    [controller, requestFocus],
  );

  const reply = useCallback(
    (deliveryId: string) => {
      controller.updateUi({
        view: "chat",
        ctx: { kind: "delivery", id: deliveryId },
        levelOverride: null,
      });
      requestFocus();
    },
    [controller, requestFocus],
  );

  useEffect(() => {
    setScrollTarget(null);
  }, [projectId]);

  const connection = connectionView(state, isScenario);
  const platform =
    state.identity?.device?.platform ?? state.me?.device.platform ?? null;
  const connectionLive = state.connection?.phase === "live";
  const allWorkRows = buildWorkRows(data, projection.byDelivery);

  const lastEvent = [...data.messages]
    .reverse()
    .find((message) => message.role === "event");

  function openLastEvent(message: Message): void {
    const index = data.messages.findIndex((item) => item.id === message.id);
    let first = index;
    while (first > 0 && data.messages[first - 1].role === "event") {
      first -= 1;
    }
    const groupKey =
      first >= 0 && data.messages[first]?.role === "event"
        ? data.messages[first].id
        : message.id;
    const expandedGroups = ui.expandedGroups.includes(groupKey)
      ? ui.expandedGroups
      : [...ui.expandedGroups, groupKey];
    const target = selectionForMessage(message, data);
    controller.updateUi({
      view: "chat",
      expandedGroups,
      ...(target ? { sel: target } : {}),
    });
    setScrollTarget(message.id);
  }

  function selectProject(next: Project): void {
    if (next.id === projectId) return;
    controller.selectProject(next.id);
    setScrollTarget(null);
  }

  function onTabKeyDown(
    event: ReactKeyboardEvent<HTMLDivElement>,
  ): void {
    const current = VIEWS.findIndex((view) => view.id === ui.view);
    if (current < 0) return;
    let next = current;
    if (event.key === "ArrowRight") next = (current + 1) % VIEWS.length;
    else if (event.key === "ArrowLeft") {
      next = (current - 1 + VIEWS.length) % VIEWS.length;
    } else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = VIEWS.length - 1;
    else if (event.key === "Escape") {
      event.preventDefault();
      tabRefs.current[current]?.focus();
      return;
    } else return;
    event.preventDefault();
    const view = VIEWS[next];
    update({ view: view.id });
    window.setTimeout(() => tabRefs.current[next]?.focus(), 0);
  }

  return (
    <div
      className="ws-root"
      ref={rootRef}
      data-left-width={prefs.layout.leftWidth}
      data-inspector-width={prefs.layout.inspectorWidth}
      data-left-collapsed={prefs.layout.leftCollapsed ? "true" : "false"}
      data-inspector-collapsed={
        prefs.layout.inspectorCollapsed ? "true" : "false"
      }
    >
      <a className="skip-link" href="#composer">
        跳到输入框
      </a>

      <header className="ws-topbar">
        <div className="ws-brand">
          <GantryMark />
          <span>Gantry</span>
        </div>

        <nav aria-label="项目" className="ws-project-tabs">
          {state.projects.map((item) => {
            const count = inboxCountForProject(
              state.inboxSummary,
              projection.byProject.get(item.id)?.length ?? 0,
              item.id,
            );
            const active = item.id === projectId;
            return (
              <button
                key={item.id}
                type="button"
                className={`ws-project-tab${active ? " is-active" : ""}`}
                aria-current={active ? "page" : undefined}
                onClick={() => selectProject(item)}
              >
                <span className="mono">{projectName(item)}</span>
                {count > 0 ? (
                  <span
                    className="ws-project-tab__badge"
                    aria-label={`需要你 ${count} 项`}
                  >
                    {count}
                  </span>
                ) : null}
              </button>
            );
          })}
          <button
            type="button"
            className="ws-project-add"
            aria-label="添加项目"
            title="项目在服务端绑定，界面暂不支持添加"
            disabled
          >
            +
          </button>
        </nav>

        <div className="ws-topbar__right">
          {isScenario ? (
            <span
              className="ws-demo-badge"
              title="开发场景：示例数据，未连接真实服务端；模拟审批、配对与保存只改内存，不生成真实签名或证书。"
            >
              示例数据
            </span>
          ) : null}
          <span className="ws-role" title="当前设备角色">
            本机 · {roleLabel(state.me?.device.role)}
          </span>
          <span
            className={`ws-connection tone-text-${connection.tone}`}
            title={connection.title}
          >
            <ToneDot tone={connection.tone} />
            <span className="mono">{connection.label}</span>
          </span>
          <button
            type="button"
            className="ws-needs-button"
            onClick={onInbox}
          >
            需要你 · {totalNeeds}
          </button>
          {controller.can("readSettings") ? (
            <button
              type="button"
              className="ws-icon-button"
              aria-label="设置"
              title="设置"
              onClick={() => onSettings()}
            >
              <GearIcon />
            </button>
          ) : null}
        </div>
      </header>

      {state.connection && state.connection.phase !== "live" && !isScenario ? (
        <div className="notice notice-warn" role="status">
          连接中断，界面可能不是最新；签名已禁用；新消息会在恢复后发送。
        </div>
      ) : null}

      {state.resyncing ? (
        <div className="notice notice-info" role="status">
          {state.lastResyncReason === "log_epoch_changed"
            ? "服务端已从备份恢复，正在重新同步…"
            : "界面正在重新同步…"}
        </div>
      ) : null}

      {state.notice ? (
        <div className="notice notice-info ws-notice" role="status">
          <span className="grow">{state.notice}</span>
          <button
            type="button"
            className="btn btn-sm"
            onClick={() => controller.notify(null)}
          >
            关闭提示
          </button>
        </div>
      ) : null}

      <h1 className="visually-hidden">Gantry 工作区</h1>

      <div className="ws-body">
        <aside className="ws-left" aria-label="项目信息">
          <ProjectInfo
            project={project}
            data={data}
            canReadSettings={controller.can("readSettings")}
            isViewer={isViewer}
            onOpenProjectSettings={() => {
              if (project) onSettings(`project:${project.id}`);
            }}
          />
          <WorkSection
            title="进行中 Issue"
            rows={allWorkRows.filter((row) => row.kind === "issue")}
            selection={ui.sel}
            showAll={ui.showAll.issues}
            onSelect={select}
            onShowAll={() =>
              update({
                showAll: { ...ui.showAll, issues: true },
              })
            }
          />
          <WorkSection
            title="进行中 PR"
            rows={allWorkRows.filter((row) => row.kind === "pr")}
            selection={ui.sel}
            showAll={ui.showAll.prs}
            onSelect={select}
            onShowAll={() =>
              update({
                showAll: { ...ui.showAll, prs: true },
              })
            }
          />
          <WorkSection
            title="工作树"
            rows={allWorkRows.filter((row) => row.kind === "worktree")}
            selection={ui.sel}
            showAll={ui.showAll.worktrees}
            onSelect={select}
            onShowAll={() =>
              update({
                showAll: { ...ui.showAll, worktrees: true },
              })
            }
            mainRow={
              <button
                type="button"
                className={`ws-work-row ws-work-row--main${
                  ui.sel?.kind === "base" ? " is-selected" : ""
                }`}
                aria-current={ui.sel?.kind === "base" ? "true" : undefined}
                onClick={() => select({ kind: "base" })}
              >
                <span className="mono ws-work-row__label">main</span>
                <span className="ws-muted">基线</span>
              </button>
            }
            footer={
              <button
                type="button"
                className="ws-start-branch"
                disabled
                title="分支 Ref 尚未在契约中定义，“从分支开始任务…”暂不可用"
              >
                从分支开始任务…
              </button>
            }
          />
          <DoneSection
            rows={doneRows(data)}
            selection={ui.sel}
            onSelect={select}
          />
        </aside>

        <main className="ws-main">
          <div className="ws-viewbar">
            <div
              role="tablist"
              aria-label="视图"
              className="ws-viewtabs"
              onKeyDown={onTabKeyDown}
            >
              {VIEWS.map((view, index) => {
                const selected = ui.view === view.id;
                return (
                  <button
                    key={view.id}
                    ref={(element) => {
                      tabRefs.current[index] = element;
                    }}
                    type="button"
                    role="tab"
                    id={`ws-tab-${view.id}`}
                    aria-controls={`ws-panel-${view.id}`}
                    aria-selected={selected}
                    tabIndex={selected ? 0 : -1}
                    className={`ws-viewtab${selected ? " is-active" : ""}`}
                    onClick={() => update({ view: view.id })}
                  >
                    {view.label}
                  </button>
                );
              })}
            </div>

            {ui.view === "chat" ? (
              <>
                <span className="ws-viewbar__note">
                  与 <strong>{projectName(project)}</strong> 的 agent 对话
                </span>
                <div className="ws-agent-legend" aria-label="气泡边框图例">
                  <span>气泡边框 = agent 种类</span>
                  <span>
                    <span className="ws-agent-swatch ws-agent-swatch--planner" aria-hidden="true" />
                    大脑
                  </span>
                  <span>
                    <span className="ws-agent-swatch ws-agent-swatch--worker" aria-hidden="true" />
                    干活
                  </span>
                  <span>
                    <span className="ws-agent-swatch ws-agent-swatch--utility" aria-hidden="true" />
                    打杂
                  </span>
                </div>
              </>
            ) : null}
          </div>

          <div
            id="ws-panel-chat"
            role="tabpanel"
            aria-labelledby="ws-tab-chat"
            className="ws-panel"
            hidden={ui.view !== "chat"}
          >
            {project ? (
              <Conversation
                projectId={project.id}
                controller={controller}
                data={data}
                ui={ui}
                inbox={state.inbox}
                approvals={state.approvals}
                workflows={state.workflows}
                isViewer={isViewer}
                focusRequest={focusRequest}
                scrollTarget={scrollTarget}
                composerRef={composerRef}
                platform={platform}
                connectionLive={connectionLive}
                onUpdate={update}
                onSelect={select}
                onReply={reply}
                onOpenApproval={onApproval}
                onOpenExternal={openExternal}
                onScrolled={clearScrollTarget}
                onOpenWorkflow={onOpenWorkflow}
              />
            ) : (
              <p className="ws-empty">尚未载入项目。</p>
            )}
          </div>

          <div
            id="ws-panel-map"
            role="tabpanel"
            aria-labelledby="ws-tab-map"
            className="ws-panel"
            hidden={ui.view !== "map"}
          >
            {project ? (
              <MapView
                data={data}
                selection={ui.sel}
                needsOnly={ui.needsOnly}
                needsByDelivery={projection.byDelivery}
                onSelect={select}
                onToggleNeedsOnly={() =>
                  update({ needsOnly: !ui.needsOnly })
                }
                onClearSelection={() => update({ sel: null })}
              />
            ) : (
              <p className="ws-empty">尚未载入项目。</p>
            )}
          </div>

          <div
            id="ws-panel-worktrees"
            role="tabpanel"
            aria-labelledby="ws-tab-worktrees"
            className="ws-panel"
            hidden={ui.view !== "worktrees"}
          >
            {project ? (
              <WorktreeTable
                data={data}
                selection={ui.sel}
                needsByDelivery={projection.byDelivery}
                onSelect={select}
              />
            ) : (
              <p className="ws-empty">尚未载入项目。</p>
            )}
          </div>
        </main>

        <aside className="ws-right" aria-label="检视器">
          <Inspector
            key={`inspector:${project?.id ?? "none"}`}
            controller={controller}
            state={state}
            project={project}
            data={data}
            ui={ui}
            inbox={state.inbox}
            needsByDelivery={projection.byDelivery}
            isViewer={isViewer}
            onCite={cite}
            onReplyDelivery={reply}
            onSelect={select}
            onOpenApproval={onApproval}
            onOpenWorkflow={onOpenWorkflow}
          />
        </aside>
      </div>

      <footer className="ws-statusbar">
        <span className="ws-statusbar__item">
          <ToneDot tone={connection.tone} />
          <span className="mono">{connection.short}</span>
        </span>
        {lastEvent ? (
          <button
            type="button"
            className="ws-statusbar__event mono"
            title="在对话中查看"
            onClick={() => openLastEvent(lastEvent)}
          >
            {lastEvent.created_at
              ? `${formatEventClock(lastEvent.created_at)}  `
              : ""}
            {cleanText(lastEvent.text)}
          </button>
        ) : (
          <span className="ws-statusbar__item">无事件</span>
        )}
      </footer>
    </div>
  );
}

function ProjectInfo({
  project,
  data,
  canReadSettings,
  isViewer,
  onOpenProjectSettings,
}: {
  project: Project | null;
  data: ProjectData;
  canReadSettings: boolean;
  isViewer: boolean;
  onOpenProjectSettings: () => void;
}): ReactNode {
  return (
    <section className="ws-project-info" aria-label="项目">
      <span className="ws-project-info__repo mono">
        {project ? cleanText(project.repo, true) : "尚未载入项目"}
      </span>
      <span className="ws-project-info__name">
        {projectName(project)}
      </span>
      <div className="ws-project-info__badges">
        <span className="ws-chip ws-chip--static">
          {canReadSettings
            ? `默认上限 ${data.defaultLevel ?? "未配置"}`
            : "默认上限 —"}
        </span>
        <span className="ws-chip ws-chip--static mono">
          {project?.default_branch ?? "main"}
        </span>
        {!isViewer && project ? (
          <button
            type="button"
            className="ws-link-button"
            onClick={onOpenProjectSettings}
          >
            项目设置
          </button>
        ) : null}
      </div>
    </section>
  );
}

function WorkSection({
  title,
  rows,
  selection,
  showAll,
  onSelect,
  onShowAll,
  mainRow,
  footer,
}: {
  title: string;
  rows: WorkRow[];
  selection: Selection | null;
  showAll: boolean;
  onSelect: (selection: Selection) => void;
  onShowAll: () => void;
  mainRow?: ReactNode;
  footer?: ReactNode;
}): ReactNode {
  const [collapsed, setCollapsed] = useState(false);
  const visible = showAll ? rows : rows.slice(0, 5);
  return (
    <section className="ws-work-section" aria-label={title}>
      <button
        type="button"
        className="ws-section-head ws-section-head--button"
        aria-expanded={!collapsed}
        onClick={() => setCollapsed((value) => !value)}
      >
        <span>{title}</span>
        <span className="mono">{rows.length}</span>
      </button>
      <div hidden={collapsed}>
      {mainRow}
      {visible.map((row) => {
        const selected = sameSelection(selection, row.selection);
        const stateLabel = row.hasNeed
          ? "需要你"
          : deliveryState(row.delivery, false).label;
        const accessibleName = [
          row.label,
          row.kind === "worktree" ? null : row.titleText,
          stateLabel,
          row.hasNeed ? null : row.ci?.label,
        ]
          .filter(Boolean)
          .join(" ");
        return (
          <button
            key={row.key}
            type="button"
            className={`ws-work-row${selected ? " is-selected" : ""}`}
            aria-current={selected ? "true" : undefined}
            aria-label={accessibleName}
            title={row.titleText}
            onClick={() => onSelect(row.selection)}
          >
            <ToneDot tone={deliveryRowTone(row)} />
            <span className="mono ws-work-row__label">{row.label}</span>
            {row.kind !== "worktree" ? (
              <span className="ws-work-row__title">{row.titleText}</span>
            ) : null}
            {row.hasNeed ? (
              <NeedPill />
            ) : (
              <span className="ws-work-row__state">{stateLabel}</span>
            )}
            {!row.hasNeed && row.ci ? (
              <span className={`ws-work-row__ci tone-text-${row.ci.tone}`}>
                {row.ci.label}
              </span>
            ) : null}
          </button>
        );
      })}
      {rows.length > 5 && !showAll ? (
        <button type="button" className="ws-more" onClick={onShowAll}>
          显示全部（{rows.length}）
        </button>
      ) : null}
      {rows.length === 0 ? <span className="ws-empty ws-empty--inline">暂无</span> : null}
      {footer}
      </div>
    </section>
  );
}

function DoneSection({
  rows,
  selection,
  onSelect,
}: {
  rows: DoneRow[];
  selection: Selection | null;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  const [collapsed, setCollapsed] = useState(false);
  if (rows.length === 0) return null;
  return (
    <section className="ws-work-section" aria-label="最近完成">
      <button
        type="button"
        className="ws-section-head ws-section-head--button"
        aria-expanded={!collapsed}
        onClick={() => setCollapsed((value) => !value)}
      >
        <span>最近完成</span>
        <span className="mono">{rows.length}</span>
      </button>
      <div hidden={collapsed}>
      {rows.map((row) => {
        const selected = sameSelection(selection, row.selection);
        return (
          <button
            key={row.key}
            type="button"
            className={`ws-work-row${selected ? " is-selected" : ""}`}
            aria-current={selected ? "true" : undefined}
            title={row.titleText}
            onClick={() => onSelect(row.selection)}
          >
            <ToneDot tone="ok" />
            <span className="mono ws-work-row__label">{row.label}</span>
            <span className="ws-work-row__title ws-muted">{row.stateLabel}</span>
          </button>
        );
      })}
      </div>
    </section>
  );
}

function deliveryRowTone(row: WorkRow): Tone {
  const delivery = row.delivery;
  if (delivery.stage_status === "running") return "run";
  if (delivery.stage_status === "waiting_human") return "need";
  if (delivery.stage_status === "blocked") return "warn";
  if (delivery.stage_status === "paused") return "idle";
  if (delivery.stage_status === "done") return "ok";
  return "idle";
}

function connectionView(
  state: DesktopState,
  isScenario: boolean,
): { label: string; short: string; tone: Tone; title: string } {
  if (isScenario) {
    return {
      label: "开发场景 · 未连接真实服务端",
      short: "开发场景",
      tone: "idle",
      title:
        "这是开发场景替身，不是真实 mTLS 连接；示例数据与交互结果不代表服务端状态。",
    };
  }
  const phase = state.connection?.phase ?? null;
  const host = state.identity?.server_host ?? "服务端";
  if (phase === "live") {
    if (state.connection?.cert_expiring) {
      return {
        label: "证书将过期",
        short: "证书将过期",
        tone: "warn",
        title: "外壳自动续期失败后才会出现此提示。",
      };
    }
    return {
      label: `${host} · 已连接`,
      short: "已连接",
      tone: "ok",
      title: "mTLS 与事件流连接正常。",
    };
  }
  if (phase === "connecting" || phase === "reconnecting") {
    return {
      label: "连接中断 · 重连中",
      short: "重连中",
      tone: "idle",
      title: "连接中断，界面可能不是最新；签名已禁用。",
    };
  }
  if (phase === "handshake_failed") {
    return {
      label: "无法建立安全连接",
      short: "不安全",
      tone: "warn",
      title: "mTLS 握手失败；需要检查证书或重新配对。",
    };
  }
  if (phase === "revoked") {
    return {
      label: "此设备已被吊销",
      short: "已吊销",
      tone: "warn",
      title: "设备证书已被吊销，需要重新配对。",
    };
  }
  if (phase === "cert_expired") {
    return {
      label: "证书已过期",
      short: "证书过期",
      tone: "warn",
      title: "设备证书已过期，需要重新配对。",
    };
  }
  return {
    label: phase ? phaseLabel(phase) : "未知连接状态",
    short: phase ? phaseLabel(phase) : "未知",
    tone: "idle",
    title: "尚未取得连接状态。",
  };
}

function formatEventClock(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}
