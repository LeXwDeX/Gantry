import {
  IpcError,
  type ApiResult,
  type ApprovalSheetModel,
  type ConnectionState,
  type GuardedWriteArgs,
  type GuardedWritePreview,
  type IdentityStatus,
  type IpcAdapter,
  type OpenGithubArgs,
  type Prefs,
  type ProjectUiState,
  type StreamFrame,
} from "@/api/shell";
import type { Approval, Device } from "@/api/types";

const MESSAGE =
  "桌面外壳未连接：生产构建必须运行在 Tauri 应用中。开发时请使用 npm run dev（显式示例模式）。";

/**
 * 非 Tauri 环境的生产兜底：每个调用都明确失败，绝不回落到示例数据。
 */
export class UnavailableIpcAdapter implements IpcAdapter {
  readonly unavailableReason = MESSAGE;

  identityStatus(): Promise<IdentityStatus> {
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  pairingBegin(_a: {
    pairingString: string;
    deviceName: string;
  }): Promise<{ confirmationCode: string; device: Device }> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  pairingConfirm(_a: { matches: boolean }): Promise<IdentityStatus> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  identityReset(): Promise<IdentityStatus> {
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  connectionState(): Promise<ConnectionState> {
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  api<R extends import("@/api/routes").RouteId>(
    _r: R,
    _a: import("@/api/routes").RouteArgs[R],
  ): Promise<ApiResult<import("@/api/routes").RouteBody[R]>> {
    void _r;
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  eventsOpen(
    _a: { afterSeq?: number },
    _onFrame: (f: StreamFrame) => void,
  ): Promise<void> {
    void _a;
    void _onFrame;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  eventsAck(_seq: number): Promise<void> {
    void _seq;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  eventsClose(): Promise<void> {
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  approvalPrepare(_a: { approvalId: string }): Promise<ApprovalSheetModel> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  approvalSign(_a: { preparedId: string }): Promise<ApiResult<Approval>> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  approvalDiscard(_a: { preparedId: string }): Promise<void> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  guardedWritePrepare(_a: GuardedWriteArgs): Promise<GuardedWritePreview> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  guardedWriteSubmit(_a: { pendingId: string }): Promise<ApiResult<unknown>> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  guardedWriteDiscard(_a: { pendingId: string }): Promise<void> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  prefsGet(): Promise<Prefs> {
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  prefsSet(_a: { patch: Partial<Prefs> }): Promise<Prefs> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  uiStateGet(_a: { projectId: string }): Promise<ProjectUiState | null> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  uiStateSet(_a: {
    projectId: string;
    state: ProjectUiState;
  }): Promise<void> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  uiStateList(): Promise<Record<string, ProjectUiState>> {
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  openExternal(_a: { url: string }): Promise<void> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }

  openGithub(_a: OpenGithubArgs): Promise<void> {
    void _a;
    return Promise.reject(new IpcError("shell.unavailable", MESSAGE));
  }
}
