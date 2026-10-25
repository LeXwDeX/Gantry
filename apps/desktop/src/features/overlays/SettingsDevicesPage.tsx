import {
  useCallback,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import type { Device } from "@/api/types";
import { roleLabel } from "@/i18n";
import { useGuardedWrite } from "./useGuardedWrite";
import {
  formatDateTime,
  ipcErrorText,
  issueText,
  parseApiFailure,
} from "./settingsData";

export interface SettingsDevicesPageProps {
  controller: DesktopController;
  state: DesktopState;
  onDirtyChange: (dirty: boolean) => void;
}

type DeviceRole = Device["role"];

interface ConfirmState {
  device: Device;
  kind: "role" | "revoke";
  role?: DeviceRole;
}

function deviceIssueText(code: string): string {
  switch (code) {
    case "self_role_change":
      return "不能修改自己的设备角色。";
    case "mobile_cannot_approve":
      return "手机档案不能成为审批设备。";
    case "not_connected":
      return "外壳未连接。示例模式不会提交或保存。";
    case "not_approver":
      return "本机不是审批设备。";
    default:
      return "预览未通过校验。";
  }
}

export function SettingsDevicesPage({
  controller,
  state,
  onDirtyChange,
}: SettingsDevicesPageProps): ReactNode {
  const [devices, setDevices] = useState<Device[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [renameId, setRenameId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [renameBusy, setRenameBusy] = useState(false);
  const [renameError, setRenameError] = useState<string | null>(null);
  const [roleDrafts, setRoleDrafts] = useState<Record<string, DeviceRole>>({});
  const [confirm, setConfirm] = useState<ConfirmState | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const write = useGuardedWrite(controller.ipc);

  const load = useCallback(async (): Promise<void> => {
    setStatus("loading");
    setError(null);
    try {
      const result = await controller.ipc.api("devices.list", { limit: 100 });
      if (!result.ok) {
        setStatus("error");
        setError(parseApiFailure(result));
        return;
      }
      const items = result.body?.items ?? [];
      setDevices(items);
      setRoleDrafts(
        Object.fromEntries(items.map((device) => [device.id, device.role])),
      );
      setStatus("ready");
    } catch (failure) {
      setStatus("error");
      setError(ipcErrorText(failure));
    }
  }, [controller]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    onDirtyChange(false);
  }, [onDirtyChange]);

  const ownId = state.me?.device.id ?? null;
  const canRename = controller.can("renameDevice");
  const canChangeRole = controller.can("changeRole");
  const canRevoke = controller.can("revoke");

  const startRename = (device: Device): void => {
    setRenameId(device.id);
    setRenameValue(device.name);
    setRenameError(null);
    setDone(null);
  };

  const submitRename = async (device: Device): Promise<void> => {
    const name = renameValue.trim();
    if (!name) {
      setRenameError("设备名不能为空。");
      return;
    }
    setRenameBusy(true);
    setRenameError(null);
    try {
      const result = await controller.ipc.api("devices.rename", {
        deviceId: device.id,
        name,
      });
      if (!result.ok) {
        setRenameError(parseApiFailure(result));
        return;
      }
      setRenameId(null);
      setDone("设备名已更新。");
      await load();
    } catch (failure) {
      setRenameError(ipcErrorText(failure));
    } finally {
      setRenameBusy(false);
    }
  };

  const requestRole = (device: Device): void => {
    const role = roleDrafts[device.id] ?? device.role;
    if (role === device.role) {
      setError("请选择与当前角色不同的角色。");
      return;
    }
    setError(null);
    setDone(null);
    setConfirm({ kind: "role", device, role });
  };

  const requestRevoke = (device: Device): void => {
    setError(null);
    setDone(null);
    setConfirm({ kind: "revoke", device });
  };

  const prepareConfirm = async (): Promise<void> => {
    if (!confirm) return;
    if (confirm.kind === "role") {
      await write.prepare({
        kind: "device_role",
        deviceId: confirm.device.id,
        role: confirm.role ?? "viewer",
      });
    } else {
      await write.prepare({
        kind: "device_revoke",
        deviceId: confirm.device.id,
      });
    }
  };

  const submitConfirm = async (): Promise<void> => {
    const result = await write.submit();
    if (!result) return;
    if (result.ok) {
      setDone(
        confirm?.kind === "revoke" ? "吊销请求已提交。" : "设备角色已更新。",
      );
      setConfirm(null);
      await load();
    }
  };

  const cancelConfirm = (): void => {
    write.clear();
    setConfirm(null);
  };

  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">设备与证书</h1>
        <p className="ov-settings__muted">
          设备列表含已吊销设备。改名需要操作设备权限；改角色与吊销需要审批设备签名，且不能修改自己的角色。
        </p>
      </div>

      {error ? (
        <div role="alert" className="ov-alert">
          {error}
        </div>
      ) : null}
      {renameError ? (
        <div role="alert" className="ov-alert">
          {renameError}
        </div>
      ) : null}
      {done ? (
        <div role="status" className="ov-notice ov-notice--ok">
          {done}
        </div>
      ) : null}

      <section className="ov-settings__section" aria-label="本机证书">
        <h2 className="ov-settings__h2">本机证书（公开信息）</h2>
        {state.identity?.cert ? (
          <dl className="ov-kv">
            <dt>证书序列号</dt>
            <dd className="ov-mono">{state.identity.cert.serial}</dd>
            <dt>到期时间</dt>
            <dd>{formatDateTime(state.identity.cert.not_after)}</dd>
            <dt>下次自动续期</dt>
            <dd>{formatDateTime(state.identity.cert.renew_after)}</dd>
            <dt>本机公钥指纹</dt>
            <dd className="ov-mono ov-break">{state.me?.device.pubkey_fpr ?? "未提供"}</dd>
            <dt>审批公钥指纹</dt>
            <dd className="ov-mono ov-break">
              {state.me?.device.approval_pubkey_fpr ??
                state.identity.device?.approval_pubkey_fpr ??
                "未提供"}
            </dd>
          </dl>
        ) : (
          <p className="ov-settings__muted">未取得本机证书；请先完成配对。</p>
        )}
      </section>

      <section className="ov-settings__section" aria-label="设备列表">
        <div className="ov-settings__section-head">
          <h2 className="ov-settings__h2">设备</h2>
          <button type="button" className="ov-btn ov-btn--sm" onClick={() => void load()}>
            重新载入
          </button>
        </div>

        {status === "loading" ? (
          <p role="status" className="ov-settings__muted">
            正在载入设备…
          </p>
        ) : null}

        {status === "error" ? (
          <div role="alert" className="ov-alert">
            {error ?? "设备列表不可用。"}
            <button type="button" className="ov-btn ov-btn--sm" onClick={() => void load()}>
              重试
            </button>
          </div>
        ) : null}

        {status === "ready" ? (
          <div className="ov-devices">
            {devices.map((device) => {
              const own = device.id === ownId;
              const revoked = device.status === "revoked";
              const roleDraft = roleDrafts[device.id] ?? device.role;
              return (
                <article className="ov-device" key={device.id} aria-label={device.name}>
                  <div className="ov-device__head">
                    <span className="ov-device__name">{device.name}</span>
                    {own ? <span className="ov-badge">本机</span> : null}
                    <span className={`ov-badge ${revoked ? "ov-badge--warn" : ""}`}>
                      {revoked ? "已吊销" : "有效"}
                    </span>
                    <span className="ov-settings__meta">{device.platform}</span>
                  </div>

                  <dl className="ov-kv ov-kv--compact">
                    <dt>角色</dt>
                    <dd>{roleLabel(device.role)}</dd>
                    <dt>证书序列号</dt>
                    <dd className="ov-mono">{device.cert_serial}</dd>
                    <dt>公钥指纹</dt>
                    <dd className="ov-mono ov-break">{device.pubkey_fpr}</dd>
                    <dt>审批公钥指纹</dt>
                    <dd className="ov-mono ov-break">
                      {device.approval_pubkey_fpr ?? "未提供"}
                    </dd>
                    <dt>最后在线</dt>
                    <dd>{formatDateTime(device.last_seen)}</dd>
                  </dl>

                  {renameId === device.id ? (
                    <div className="ov-device__row">
                      <label className="ov-field ov-field--inline">
                        <span>新设备名</span>
                        <input
                          className="ov-input"
                          value={renameValue}
                          aria-label={`${device.name} 新设备名`}
                          onChange={(event) => setRenameValue(event.target.value)}
                        />
                      </label>
                      <button
                        type="button"
                        className="ov-btn ov-btn--primary ov-btn--sm"
                        disabled={renameBusy || !canRename}
                        onClick={() => void submitRename(device)}
                      >
                        保存名称
                      </button>
                      <button
                        type="button"
                        className="ov-btn ov-btn--sm"
                        onClick={() => setRenameId(null)}
                      >
                        取消
                      </button>
                    </div>
                  ) : (
                    <div className="ov-device__actions">
                      <button
                        type="button"
                        className="ov-btn ov-btn--sm"
                        disabled={!canRename}
                        title={!canRename ? "此设备为只读" : undefined}
                        onClick={() => startRename(device)}
                      >
                        改名
                      </button>
                      <label className="ov-field ov-field--inline">
                        <span>角色</span>
                        <select
                          className="ov-select ov-select--sm"
                          aria-label={`${device.name} 角色`}
                          value={roleDraft}
                          disabled={own || revoked || !canChangeRole}
                          onChange={(event) =>
                            setRoleDrafts((prev) => ({
                              ...prev,
                              [device.id]: event.target.value as DeviceRole,
                            }))
                          }
                        >
                          <option value="viewer">只读设备</option>
                          <option value="operator">操作设备</option>
                          <option value="approver" disabled={device.platform === "ios"}>
                            审批设备{device.platform === "ios" ? "（手机不可）" : ""}
                          </option>
                        </select>
                      </label>
                      <button
                        type="button"
                        className="ov-btn ov-btn--sm"
                        disabled={
                          own ||
                          revoked ||
                          !canChangeRole ||
                          roleDraft === device.role
                        }
                        title={own ? "不能修改自己的设备角色" : undefined}
                        onClick={() => requestRole(device)}
                      >
                        应用角色
                      </button>
                      <button
                        type="button"
                        className="ov-btn ov-btn--sm ov-btn--danger-ghost"
                        disabled={own || revoked || !canRevoke}
                        title={
                          own
                            ? "不能在当前设备上吊销自己"
                            : !canRevoke
                              ? "需要审批设备"
                              : undefined
                        }
                        onClick={() => requestRevoke(device)}
                      >
                        吊销…
                      </button>
                    </div>
                  )}

                  {own ? (
                    <span className="ov-settings__meta">
                      不能修改自己的设备角色。请在其他审批设备上操作。
                    </span>
                  ) : null}
                  {device.platform === "ios" ? (
                    <span className="ov-settings__meta">
                      手机档案不能成为审批设备。
                    </span>
                  ) : null}
                </article>
              );
            })}
          </div>
        ) : null}
      </section>

      {confirm ? (
        <section className="ov-confirm" aria-label="确认设备操作">
          <h3 className="ov-settings__h3">
            {confirm.kind === "revoke" ? "吊销设备" : "修改设备角色"}
          </h3>
          <p>
            {confirm.kind === "revoke"
              ? `确认吊销 ${confirm.device.name}？已建立的连接将失效。`
              : `确认将 ${confirm.device.name} 的角色改为 ${roleLabel(
                  confirm.role ?? "viewer",
                )}？`}
          </p>
          {write.error ? (
            <div role="alert" className="ov-alert">
              {write.error}
            </div>
          ) : null}
          {!write.preview ? (
            <div className="ov-save__actions">
              <button
                type="button"
                className="ov-btn ov-btn--primary"
                disabled={write.phase !== "idle"}
                onClick={() => void prepareConfirm()}
              >
                确认并预检
              </button>
              <button
                type="button"
                className="ov-btn"
                onClick={cancelConfirm}
                disabled={write.phase !== "idle"}
              >
                取消
              </button>
            </div>
          ) : (
            <div className="ov-write-preview">
              {write.preview.errors.map((issue) => (
                <div
                  role="alert"
                  className="ov-alert"
                  key={`${issue.pointer}-${issue.code}`}
                >
                  {issue.pointer ? `${issue.pointer}：` : ""}
                  {deviceIssueText(issue.code)}
                </div>
              ))}
              {write.preview.warnings.map((issue) => (
                <div
                  role="status"
                  className="ov-notice ov-notice--warn"
                  key={`${issue.pointer}-${issue.code}`}
                >
                  {issueText(issue)}
                </div>
              ))}
              {write.preview.requires_signature ? (
                <div role="status" className="ov-notice ov-notice--info">
                  此操作需要审批设备签名。
                </div>
              ) : null}
              {!write.preview.can_submit.ok ? (
                <div role="status" className="ov-notice ov-notice--warn">
                  无法提交：
                  {write.preview.can_submit.reason === "not_connected"
                    ? "外壳未连接。示例模式不会提交或保存。"
                    : write.preview.can_submit.reason === "not_approver"
                      ? "本机不是审批设备。"
                      : "预览未通过校验。"}
                </div>
              ) : null}
              <div className="ov-save__actions">
                <button
                  type="button"
                  className="ov-btn ov-btn--primary"
                  disabled={
                    !write.preview.can_submit.ok || write.phase !== "idle"
                  }
                  onClick={() => void submitConfirm()}
                >
                  提交
                </button>
                <button
                  type="button"
                  className="ov-btn"
                  onClick={cancelConfirm}
                  disabled={write.phase !== "idle"}
                >
                  取消
                </button>
              </div>
            </div>
          )}
        </section>
      ) : null}

      {devices.length === 0 && status === "ready" ? (
        <p className="ov-settings__muted">服务端没有返回设备。</p>
      ) : null}
      <p className="ov-settings__meta">
        证书与公钥指纹属于公开信息；本页不显示任何私钥或密钥材料。
      </p>
    </section>
  );
}
