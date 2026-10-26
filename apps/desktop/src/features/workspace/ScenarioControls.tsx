import type { ReactNode } from "react";
import { useState, useSyncExternalStore } from "react";
import type {
  SimulationAction,
  SimulationErrorKind,
  SimulationScriptId,
} from "@/demo/types";
import type { DesktopController } from "@/store/controller";
import {
  useSimulationClient,
  useSimulationSnapshot,
} from "./SimulationActivity";
import "./conversation-simulation.css";

const ERROR_BUTTONS: Array<{
  kind: SimulationErrorKind;
  label: string;
}> = [
  { kind: "http_500", label: "注入 500" },
  { kind: "settings_conflict", label: "注入设置冲突" },
  { kind: "late_event", label: "注入旧事件" },
];

/**
 * 可见开发场景工具区。只在场景构建中渲染，所有操作都调用适配层；
 * 组件不直接修改 seed。审批批准只存在于审批单，不放在这里。
 */
export function ScenarioControls({
  controller,
}: {
  controller: DesktopController;
}): ReactNode {
  const simulation = useSimulationClient(controller);
  const snapshot = useSimulationSnapshot(simulation);
  const state = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  );
  const [open, setOpen] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);

  if (import.meta.env.MODE === "production" || !simulation) return null;

  const sim = simulation;
  const scripts = sim.scripts();
  const projectId = state.currentProjectId;
  const pendingApproval = snapshot.pending_approval_id;
  const phase = state.connection?.phase ?? null;
  const connectionLive = phase === "live";
  const connectionRecoverable =
    phase === "reconnecting" || phase === "connecting";
  const currentScript = scripts.find(
    (script) => script.id === snapshot.script_id,
  );
  const statusText =
    snapshot.status === "running"
      ? "生成中"
      : snapshot.status === "paused"
        ? "已暂停"
        : snapshot.status === "completed"
          ? "已完成"
          : snapshot.status === "failed"
            ? "失败"
            : snapshot.status === "cancelled"
              ? "已中断"
              : "未开始";
  const viewer = state.me?.device.role === "viewer";
  const running = snapshot.status === "running";
  const paused = snapshot.status === "paused";

  function apply(action: SimulationAction): void {
    setFeedback(
      action.ok
        ? action.message ?? "已执行。"
        : action.message ?? "适配层拒绝了该操作。",
    );
  }

  function selectScript(id: SimulationScriptId): void {
    if (!id) return;
    const action = sim.select(
      id,
      projectId ? { projectId } : undefined,
    );
    apply(action);
    if (action.ok && snapshot.run_id > 0) {
      void controller.refresh().catch(() => undefined);
    }
  }

  function reset(): void {
    apply(sim.reset());
    controller.updateUi({ sel: null, view: "chat" });
    void controller.refresh().catch(() => undefined);
  }

  return (
    <section className="cs-scenario" aria-label="开发场景工具">
      <button
        type="button"
        className="cs-scenario__toggle"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span>开发场景</span>
        <span className="cs-scenario__badge">模拟</span>
        <span className="ws-muted">
          {snapshot.script_id
            ? scripts.find((script) => script.id === snapshot.script_id)?.label ??
              snapshot.script_id
            : "未选择脚本"}
        </span>
        <span className="cs-scenario__status mono">
          {snapshot.script_id
            ? `第 ${snapshot.turn}/${snapshot.total_turns} 步`
            : "内存数据"}
        </span>
      </button>

      {open ? (
        <div className="cs-scenario__panel">
          <p className="cs-scenario__hint">
            开发场景只使用内存数据，不调用真实模型、网络、密钥或系统认证。
            选择脚本后在下方输入并发送消息即可回放，也可用单步手动推进。
          </p>
          {currentScript ? (
            <p className="cs-scenario__hint">{currentScript.description}</p>
          ) : null}

          <div className="cs-scenario__row">
            <label className="cs-scenario__label" htmlFor="cs-script">
              演示脚本
            </label>
            <select
              id="cs-script"
              value={snapshot.script_id ?? ""}
              onChange={(event) =>
                selectScript(event.target.value as SimulationScriptId)
              }
            >
              <option value="" disabled>
                选择脚本
              </option>
              {scripts.map((script) => (
                <option key={script.id} value={script.id}>
                  {script.label}
                </option>
              ))}
            </select>
            <button
              type="button"
              className="btn btn-sm"
              disabled={running || snapshot.awaiting !== null}
              onClick={() => apply(simulation.play())}
            >
              回放
            </button>
            <button
              type="button"
              className="btn btn-sm"
              disabled={snapshot.awaiting !== null}
              onClick={() => apply(simulation.step())}
            >
              单步
            </button>
            {running ? (
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => apply(simulation.pause())}
              >
                暂停
              </button>
            ) : null}
            {paused ? (
              <button
                type="button"
                className="btn btn-sm"
                disabled={snapshot.awaiting !== null}
                onClick={() => apply(simulation.resume())}
              >
                继续
              </button>
            ) : null}
            <button
              type="button"
              className="btn btn-sm"
              disabled={snapshot.status === "idle" && snapshot.turn === 0}
              onClick={() => apply(simulation.cancel())}
            >
              取消生成
            </button>
            <button type="button" className="btn btn-sm" onClick={reset}>
              重置场景
            </button>
          </div>

          <div className="cs-scenario__row">
            <span className="cs-scenario__label">连接</span>
            {connectionLive ? (
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => apply(simulation.setConnection("reconnecting"))}
              >
                模拟断线
              </button>
            ) : connectionRecoverable ? (
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => apply(simulation.setConnection("live"))}
              >
                恢复连接
              </button>
            ) : (
              <span className="cs-scenario__hint">
                当前连接状态需重新配对或由原生外壳处理。
              </span>
            )}
            <span className="cs-scenario__label">设备角色</span>
            <button
              type="button"
              className="btn btn-sm"
              onClick={() => apply(simulation.setRole(viewer ? "operator" : "viewer"))}
            >
              {viewer ? "切换为可操作" : "切换为只读"}
            </button>
            <span className="cs-scenario__label">异常</span>
            {ERROR_BUTTONS.map((entry) => (
              <button
                key={entry.kind}
                type="button"
                className="btn btn-sm"
                onClick={() => apply(simulation.injectError(entry.kind))}
              >
                {entry.label}
              </button>
            ))}
          </div>

          <div className="cs-scenario__row">
            <span className="cs-scenario__label">审批失效</span>
            {pendingApproval ? (
              <>
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={() => apply(simulation.expireApproval(pendingApproval))}
                >
                  模拟审批过期
                </button>
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={() => apply(simulation.setHeadChanged(pendingApproval))}
                >
                  模拟 head 变化
                </button>
              </>
            ) : (
              <span className="cs-scenario__hint">
                待审批项出现后可用；模拟批准在审批单中进行。
              </span>
            )}
            <span className="cs-scenario__status mono">
              {statusText} · {snapshot.phase || "未开始"}
            </span>
          </div>

          {feedback ? (
            <p className="cs-scenario__feedback" role="status">
              {feedback}
            </p>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}
