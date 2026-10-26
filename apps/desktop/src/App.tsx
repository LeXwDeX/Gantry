import {
  Component,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { Workspace } from "@/features/workspace";
import {
  ApprovalSheet,
  InboxOverlay,
  PairingView,
  SettingsOverlay,
} from "@/features/overlays/Overlays";
import { ExecutionGraph } from "@/features/workspace/ExecutionGraph";
import { ScenarioControls } from "@/features/workspace/ScenarioControls";
import {
  simulationFor,
  useSimulationSnapshot,
} from "@/features/workspace/SimulationActivity";
import type { Prefs } from "@/api/shell";
import { isScenarioAdapter } from "@/ipc";
import type { DesktopController } from "@/store/controller";
import "./app.css";

/** 偏好读取失败时的布局；不是示例数据，不进入任何业务视图。 */
const FALLBACK_PREFS: Prefs = {
  theme: "dark",
  density: "compact",
  language: "zh-CN",
  systemNotifications: true,
  layout: {
    leftWidth: 240,
    inspectorWidth: 300,
    leftCollapsed: false,
    inspectorCollapsed: false,
  },
};

export interface AppProps {
  controller: DesktopController;
}

type Overlay =
  | { kind: "settings"; page?: string }
  | { kind: "inbox" }
  | { kind: "approval"; approvalId: string }
  | { kind: "graph"; workflowId: string; deliveryId?: string };

/**
 * 应用入口：唯一的状态订阅与浮层编排点。
 * 工作区在浮层打开期间保持挂载；任何时刻最多一个原生模态。
 */
export function App({ controller }: AppProps): ReactNode {
  const state = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  );
  const [overlay, setOverlay] = useState<Overlay | null>(null);
  const [overlayEpoch, setOverlayEpoch] = useState(0);
  const [retrying, setRetrying] = useState(false);
  const [prefs, setPrefs] = useState<Prefs>(FALLBACK_PREFS);
  const overlayRef = useRef<Overlay | null>(null);
  overlayRef.current = overlay;
  const returnFocusRef = useRef<HTMLElement | null>(null);
  const scenario = isScenarioAdapter(controller.ipc);
  const simulation = useMemo(() => simulationFor(controller), [controller]);
  const simulationSnapshot = useSimulationSnapshot(simulation);

  useEffect(() => {
    // 控制器内部已处理启动错误；这里只负责与组件生命周期配对。
    void controller.start().catch(() => undefined);
    return () => {
      void controller.stop();
    };
  }, [controller]);

  useEffect(() => {
    let alive = true;
    void controller.ipc.prefsGet().then(
      (value) => {
        if (alive) setPrefs(value);
      },
      () => {
        // 偏好不可用时保持默认布局；不回退到任何示例数据。
      },
    );
    return () => {
      alive = false;
    };
  }, [controller]);

  const openOverlay = useCallback((next: Overlay) => {
    if (overlayRef.current === null) {
      returnFocusRef.current =
        document.activeElement instanceof HTMLElement
          ? document.activeElement
          : null;
    }
    overlayRef.current = next;
    setOverlay(next);
    setOverlayEpoch((value) => value + 1);
  }, []);

  const closeOverlay = useCallback(
    (match: (current: Overlay) => boolean) => {
      setOverlay((current) => (current && match(current) ? null : current));
    },
    [],
  );

  useEffect(() => {
    if (state.status === "ready") return;
    if (overlayRef.current === null) return;
    overlayRef.current = null;
    returnFocusRef.current = null;
    setOverlay(null);
  }, [state.status]);

  useEffect(() => {
    if (overlay !== null) return;
    const target = returnFocusRef.current;
    if (!target) return;
    returnFocusRef.current = null;
    // 等原生 dialog 的关闭清理完成后再还给触发点，避免焦点被抢回 body。
    const timer = window.setTimeout(() => {
      if (target.isConnected) target.focus();
    }, 0);
    return () => window.clearTimeout(timer);
  }, [overlay]);

  const graphProgress = useMemo(() => {
    if (!simulation || overlay?.kind !== "graph") return null;
    const workflow = state.workflows[overlay.workflowId];
    const deliveryId = overlay.deliveryId ?? workflow?.delivery_id ?? null;
    if (!deliveryId) return null;
    return {
      tools: simulationSnapshot.activities.filter(
        (activity) => activity.delivery_id === deliveryId,
      ),
      summaries: simulationSnapshot.summaries.filter(
        (summary) => summary.delivery_id === deliveryId,
      ),
    };
  }, [overlay, simulation, simulationSnapshot, state.workflows]);

  async function retry(): Promise<void> {
    setRetrying(true);
    try {
      await controller.refresh();
    } finally {
      setRetrying(false);
    }
  }

  if (state.status === "unpaired") {
    return (
      <div className="app-pairing-shell">
        <PairingView controller={controller} state={state} />
        <ScenarioControls controller={controller} />
      </div>
    );
  }

  if (state.status === "error") {
    const unavailable =
      "unavailableReason" in controller.ipc;
    return (
      <main className="app-blocking" aria-label="无法载入工作区">
        <section className="app-blocking__card">
          <span className="app-blocking__brand">Gantry</span>
          <h1 className="app-blocking__title">
            {unavailable ? "桌面外壳未连接" : "无法载入工作区"}
          </h1>
          <p role="alert" className="app-blocking__error">
            {state.error ?? "载入失败，请重试。"}
          </p>
          <p className="app-blocking__hint">
            {unavailable
              ? "当前是正式前端构建，需要 Tauri 外壳提供身份、连接与受保护操作。本地演示请运行 npm run dev（显式示例模式）。"
              : "界面没有回退到示例数据。请检查外壳与服务端连接后重试。"}
          </p>
          <button
            type="button"
            className="btn btn-sm"
            disabled={retrying}
            onClick={() => void retry()}
          >
            {retrying ? "正在重试…" : "重试连接"}
          </button>
          {scenario ? <ScenarioBadge /> : null}
          <ScenarioControls controller={controller} />
        </section>
      </main>
    );
  }

  if (state.status !== "ready") {
    return (
      <main className="app-blocking" aria-label="正在启动 Gantry">
        <section className="app-blocking__card">
          <span className="app-blocking__brand">Gantry</span>
          <h1 className="app-blocking__title">Gantry</h1>
          <p role="status" className="app-blocking__status">
            正在载入工作区…
          </p>
          {scenario ? <ScenarioBadge /> : null}
        </section>
      </main>
    );
  }

  return (
    <div className="app-root">
      <AppErrorBoundary label="工作区">
        <Workspace
          controller={controller}
          state={state}
          prefs={prefs}
          onSettings={(page) => openOverlay({ kind: "settings", page })}
          onApproval={(approvalId) =>
            openOverlay({ kind: "approval", approvalId })
          }
          onInbox={() => openOverlay({ kind: "inbox" })}
          onOpenWorkflow={(workflowId, deliveryId) =>
            openOverlay({ kind: "graph", workflowId, deliveryId })
          }
        />
      </AppErrorBoundary>

      {overlay?.kind === "settings" ? (
        <AppErrorBoundary
          key={`settings:${overlay.page ?? ""}:${overlayEpoch}`}
          label="设置"
          onDismiss={() =>
            closeOverlay((current) => current.kind === "settings")
          }
        >
          <SettingsOverlay
            controller={controller}
            state={state}
            initialPage={overlay.page}
            onPrefsChanged={setPrefs}
            onClose={() =>
              closeOverlay((current) => current.kind === "settings")
            }
          />
        </AppErrorBoundary>
      ) : null}

      {overlay?.kind === "inbox" ? (
        <AppErrorBoundary
          key={`inbox:${overlayEpoch}`}
          label="收件箱"
          onDismiss={() => closeOverlay((current) => current.kind === "inbox")}
        >
          <InboxOverlay
            controller={controller}
            state={state}
            onApproval={(approvalId) =>
              openOverlay({ kind: "approval", approvalId })
            }
            onClose={() => closeOverlay((current) => current.kind === "inbox")}
          />
        </AppErrorBoundary>
      ) : null}

      {overlay?.kind === "approval" ? (
        <AppErrorBoundary
          key={`approval:${overlay.approvalId}:${overlayEpoch}`}
          label="审批单"
          onDismiss={() =>
            closeOverlay((current) => current.kind === "approval")
          }
        >
          <ApprovalSheet
            controller={controller}
            state={state}
            approvalId={overlay.approvalId}
            onClose={() =>
              closeOverlay(
                (current) =>
                  current.kind === "approval" &&
                  current.approvalId === overlay.approvalId,
              )
            }
          />
        </AppErrorBoundary>
      ) : null}

      {overlay?.kind === "graph" ? (
        <AppErrorBoundary
          key={`graph:${overlay.workflowId}:${overlayEpoch}`}
          label="执行图"
          onDismiss={() => closeOverlay((current) => current.kind === "graph")}
        >
          <ExecutionGraph
            controller={controller}
            state={state}
            workflowId={overlay.workflowId}
            deliveryId={overlay.deliveryId}
            progress={graphProgress}
            onClose={() =>
              closeOverlay((current) => current.kind === "graph")
            }
          />
        </AppErrorBoundary>
      ) : null}
    </div>
  );
}

function ScenarioBadge(): ReactNode {
  return (
    <span
      className="app-scenario-badge"
      title="开发场景：示例数据，未连接真实服务端；模拟审批、配对与保存只改内存，不生成真实签名或证书。"
    >
      示例数据
    </span>
  );
}

export interface AppErrorBoundaryProps {
  label: string;
  children: ReactNode;
  onDismiss?: () => void;
}

interface AppErrorBoundaryState {
  failed: boolean;
}

/** 局部错误边界：只替换出错区域，不把整页打成白屏。 */
export class AppErrorBoundary extends Component<
  AppErrorBoundaryProps,
  AppErrorBoundaryState
> {
  override state: AppErrorBoundaryState = { failed: false };

  static getDerivedStateFromError(): AppErrorBoundaryState {
    return { failed: true };
  }

  override componentDidCatch(): void {
    // 不记录错误详情或 props：其中可能含凭据与不可信文本。
  }

  private reset = (): void => {
    this.setState({ failed: false });
  };

  private dismiss = (): void => {
    this.setState({ failed: false });
    this.props.onDismiss?.();
  };

  override render(): ReactNode {
    if (!this.state.failed) return this.props.children;
    return (
      <section className="app-boundary">
        <p role="alert" className="app-boundary__message">
          {this.props.label}渲染失败，此内容无法显示。其余界面仍可继续使用。
        </p>
        <div className="app-boundary__actions">
          <button type="button" className="btn btn-sm" onClick={this.reset}>
            重试
          </button>
          {this.props.onDismiss ? (
            <button type="button" className="btn btn-sm" onClick={this.dismiss}>
              关闭
            </button>
          ) : null}
        </div>
      </section>
    );
  }
}
