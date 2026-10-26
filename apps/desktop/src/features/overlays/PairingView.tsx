import { useState, type ReactNode } from "react";
import type { Device } from "@/api/types";
import type { ConnectionState, IdentityStatus } from "@/api/shell";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { simulationFor } from "@/features/workspace/SimulationActivity";
import { phaseLabel } from "@/i18n";
import { ScenarioMark } from "./ScenarioMark";
import { ipcErrorText } from "./settingsData";

export interface PairingViewProps {
  controller: DesktopController;
  state: DesktopState;
}

type Stage = "form" | "confirm" | "done";

export function PairingView({
  controller,
  state,
}: PairingViewProps): ReactNode {
  const identity = state.identity;
  const simulation = simulationFor(controller);
  const [pairingNotice, setPairingNotice] = useState<string | null>(null);
  const [pairingString, setPairingString] = useState("");
  const [deviceName, setDeviceName] = useState(identity?.device?.name ?? "");
  const [stage, setStage] = useState<Stage>("form");
  const [confirmationCode, setConfirmationCode] = useState<string | null>(null);
  const [pendingDevice, setPendingDevice] = useState<Device | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [resetConfirm, setResetConfirm] = useState(false);
  const [leftover, setLeftover] = useState<number | null>(null);

  const begin = async (): Promise<void> => {
    const trimmedPairing = pairingString.trim();
    const trimmedName = deviceName.trim();
    if (!trimmedPairing) {
      setError("请粘贴服务端生成的配对串再继续。");
      return;
    }
    if (!trimmedName) {
      setError("请填写设备名再继续。");
      return;
    }
    if (trimmedName.length > 64) {
      setError("设备名必须为 1–64 字符。");
      return;
    }
    setBusy(true);
    setError(null);
    setDone(null);
    setPairingNotice(null);
    // 配对串不留在组件状态中；失败后也必须重新粘贴。
    setPairingString("");
    try {
      const result = await controller.ipc.pairingBegin({
        pairingString: trimmedPairing,
        deviceName: trimmedName,
      });
      setConfirmationCode(result.confirmationCode);
      setPendingDevice(result.device);
      setStage("confirm");
    } catch (failure) {
      setError(ipcErrorText(failure));
      setStage("form");
    } finally {
      setBusy(false);
    }
  };

  const confirm = async (matches: boolean): Promise<void> => {
    setBusy(true);
    setError(null);
    setPairingNotice(null);
    try {
      const result = await controller.ipc.pairingConfirm({ matches });
      if (!matches || result.state !== "paired") {
        setConfirmationCode(null);
        setPendingDevice(null);
        setStage("form");
        if (!matches) {
          setError(
            "已选择不一致；等待外壳丢弃待定证书。请在服务端移除该设备后重新配对。",
          );
        } else {
          setError("外壳尚未报告配对完成，请按提示重试。");
        }
        return;
      }
      setStage("done");
      const stale = controller.staleOutboxCount();
      if (stale > 0) {
        // 旧身份的待发消息保留在队列里，必须先由用户明确处理。
        setLeftover(stale);
        setDone("配对已确认；旧身份还有未发送消息，请先处理。");
        return;
      }
      setDone("配对已确认，正在载入工作区…");
      try {
        await controller.refresh();
      } catch (failure) {
        setError(`配对已确认，但工作区载入失败：${ipcErrorText(failure)}`);
      }
    } catch (failure) {
      setError(ipcErrorText(failure));
    } finally {
      setBusy(false);
    }
  };

  const finishLeftover = async (discard: boolean): Promise<void> => {
    setBusy(true);
    setError(null);
    try {
      if (discard) controller.discardStaleOutbox();
      setLeftover(null);
      setDone("配对已确认，正在载入工作区…");
      await controller.refresh();
    } catch (failure) {
      setError(`配对已确认，但工作区载入失败：${ipcErrorText(failure)}`);
    } finally {
      setBusy(false);
    }
  };

  const resetIdentity = async (): Promise<void> => {
    setBusy(true);
    setError(null);
    setDone(null);
    try {
      await controller.resetIdentity();
      setStage("form");
      setConfirmationCode(null);
      setPendingDevice(null);
      setPairingString("");
      setResetConfirm(false);
      setLeftover(null);
      setDone("外壳已接受重置请求；请重新配对。");
    } catch (failure) {
      setError(ipcErrorText(failure));
    } finally {
      setBusy(false);
    }
  };

  const terminal =
    identity?.state === "revoked" ||
    identity?.state === "cert_expired" ||
    state.connection?.phase === "revoked" ||
    state.connection?.phase === "cert_expired";
  const stateText = blockedStateText(identity, state.connection);

  return (
    <main className="ov-pairing" aria-label="配对设备">
      <div className="ov-pairing__card">
        <header className="ov-pairing__head">
          <h1 className="ov-settings__h1">配对设备</h1>
          <ScenarioMark ipc={controller.ipc} />
          <span className="ov-pairing__phase">
            {phaseLabel(state.connection?.phase)}
          </span>
        </header>
        <p className="ov-settings__muted">{stateText}</p>
        {simulation ? (
          <p className="ov-notice ov-notice--info" role="status">
            模拟配对：只在本会话内存中核对设备名与确认码；不生成密钥、证书，不调用系统认证或真实服务端。确认码是固定测试值。
          </p>
        ) : null}

        {identity ? (
          <dl className="ov-kv ov-kv--compact">
            <dt>服务端</dt>
            <dd className="ov-mono">
              {identity.server_host ?? "由外壳在配对时提供"}
            </dd>
            <dt>离线根 CA 指纹</dt>
            <dd className="ov-mono ov-break">
              {identity.ca_fingerprint ?? "由外壳在配对时固定"}
            </dd>
          </dl>
        ) : null}

        {error ? (
          <div role="alert" className="ov-alert">
            {error}
          </div>
        ) : null}
        {pairingNotice ? (
          <div role="status" className="ov-notice ov-notice--info">
            {pairingNotice}
          </div>
        ) : null}
        {done ? (
          <div role="status" className="ov-notice ov-notice--ok">
            {done}
          </div>
        ) : null}
        {leftover !== null ? (
          <section
            className="ov-confirm"
            role="group"
            aria-label="处理旧身份的待发消息"
          >
            <p>
              旧身份还有 {leftover} 条消息未发送；它们不会使用新身份自动发送。
            </p>
            <p className="ov-settings__meta">
              保留的旧消息仅留在当前会话，不写入新身份的本机存储；关闭窗口前请编辑、重试或删除。
            </p>
            <div className="ov-save__actions">
              <button
                type="button"
                className="ov-btn ov-btn--danger"
                disabled={busy}
                onClick={() => void finishLeftover(true)}
              >
                丢弃旧消息并继续
              </button>
              <button
                type="button"
                className="ov-btn"
                disabled={busy}
                onClick={() => void finishLeftover(false)}
              >
                保留并继续
              </button>
            </div>
          </section>
        ) : null}
        {busy ? (
          <p role="status" className="ov-settings__muted">
            正在等待外壳…
          </p>
        ) : null}

        {stage === "form" ? (
          <>
            <label className="ov-field">
              <span>配对串</span>
              <textarea
                className="ov-textarea"
                value={pairingString}
                autoComplete="off"
                spellCheck={false}
                placeholder="粘贴服务端 `gantry pair` 生成的配对串"
                onChange={(event) => setPairingString(event.target.value)}
              />
            </label>
            <label className="ov-field">
              <span>设备名</span>
              <input
                className="ov-input"
                value={deviceName}
                maxLength={64}
                autoComplete="off"
                onChange={(event) => setDeviceName(event.target.value)}
              />
            </label>
            <p className="ov-settings__meta">
              {simulation
                ? "配对串只在本输入框内存在，模拟配对开始后立即清空；不写入界面状态存储或日志。确认码是开发场景固定测试值，不来自真实外壳。"
                : "配对串只在本输入框内存在，配对开始后立即清空；不写入界面状态存储或日志。配对码由桌面外壳在完成密钥与证书流程后提供，本界面不会自行生成。"}
            </p>
            <div className="ov-save__actions">
              <button
                type="button"
                className="ov-btn ov-btn--primary"
                disabled={busy}
                onClick={() => void begin()}
              >
                开始配对
              </button>
              {simulation ? (
                <button
                  type="button"
                  className="ov-btn"
                  disabled={busy}
                  onClick={() => {
                    const result = simulation.failNextPairing();
                    setPairingNotice(result.message ?? "已安排一次模拟失败。");
                  }}
                >
                  模拟下次配对失败
                </button>
              ) : null}
              {terminal ? (
                <button
                  type="button"
                  className="ov-btn"
                  disabled={busy}
                  onClick={() => setResetConfirm(true)}
                >
                  重置本机身份…
                </button>
              ) : null}
            </div>
            {resetConfirm ? (
              <div className="ov-confirm" role="alertdialog" aria-label="确认重置本机身份">
                <p>重置会删除本机密钥与配对状态；已建立的连接会断开。</p>
                <div className="ov-save__actions">
                  <button
                    type="button"
                    className="ov-btn ov-btn--danger"
                    disabled={busy}
                    onClick={() => void resetIdentity()}
                  >
                    确认重置
                  </button>
                  <button
                    type="button"
                    className="ov-btn"
                    onClick={() => setResetConfirm(false)}
                  >
                    取消
                  </button>
                </div>
              </div>
            ) : null}
          </>
        ) : null}

        {stage === "confirm" ? (
          <section className="ov-pairing__confirm" aria-label="核对待确认配对">
            <h2 className="ov-settings__h2">核对确认码</h2>
            <p className="ov-settings__muted">
              {simulation
                ? "开发场景核对固定测试确认码；选择一致只把测试设备写入内存，不生成证书或系统认证。"
                : "对照服务端控制台显示的 6 位确认码。确认码来自 IPC 响应，本界面不生成、不猜测。"}
            </p>
            <span className="ov-pairing__code" aria-label="6 位确认码">
              {confirmationCode ?? "未提供"}
            </span>
            {pendingDevice ? (
              <dl className="ov-kv ov-kv--compact">
                <dt>待配对设备</dt>
                <dd>{pendingDevice.name}</dd>
                <dt>平台</dt>
                <dd>{pendingDevice.platform}</dd>
              </dl>
            ) : null}
            <p className="ov-settings__meta">
              {simulation
                ? "模拟配对一致只改开发场景内存；生产配对仍需服务端与外壳确认，且本界面不生成密钥。"
                : "只有服务端与外壳都确认一致后才会写入身份；本界面不伪造配对成功。"}
            </p>
            <div className="ov-save__actions">
              <button
                type="button"
                className="ov-btn ov-btn--primary"
                disabled={busy}
                onClick={() => void confirm(true)}
              >
                一致
              </button>
              <button
                type="button"
                className="ov-btn ov-btn--danger"
                disabled={busy}
                onClick={() => void confirm(false)}
              >
                不一致
              </button>
              <button
                type="button"
                className="ov-btn"
                disabled={busy}
                onClick={() => {
                  setStage("form");
                  setConfirmationCode(null);
                  setPendingDevice(null);
                }}
              >
                返回
              </button>
            </div>
          </section>
        ) : null}

        <p className="ov-settings__meta">
          当前状态：{stateText}
        </p>
      </div>
    </main>
  );
}

/**
 * 身份状态与连接状态可能不一致：外壳吊销后 identity 仍可能是 paired。
 * 终态原因以连接为准，避免配对页显示"设备已配对"。
 */
function blockedStateText(
  identity: IdentityStatus | null,
  connection: ConnectionState | null,
): string {
  if (
    connection?.phase === "revoked" ||
    identity?.state === "revoked"
  ) {
    return "此设备已被吊销，需要重新配对。";
  }
  if (
    connection?.phase === "cert_expired" ||
    identity?.state === "cert_expired"
  ) {
    return "证书已过期，需要重新配对。";
  }
  return identityStateText(identity);
}

function identityStateText(identity: IdentityStatus | null): string {
  if (!identity) return "尚未取得身份状态。";
  switch (identity.state) {
    case "unpaired":
      return "此设备尚未配对。配对完成后才能载入工作区。";
    case "pending_confirm":
      return "已有待确认的配对请求；请按外壳提示核对确认码。";
    case "revoked":
      return "此设备已被吊销，需要重新配对。";
    case "cert_expired":
      return "证书已过期，需要重新配对。";
    case "paired":
      return "设备已配对。";
    default:
      return "未知身份状态。";
  }
}
