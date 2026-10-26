/**
 * 生产外壳适配器：**唯一**允许引用 `@tauri-apps/api` 的模块。
 * 参数由 camelCase 映射为外壳命令的 snake_case；签名/受保护写入只传句柄。
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  ApiResult,
  ApprovalSheetModel,
  ConnectionState,
  GuardedWriteArgs,
  GuardedWritePreview,
  IdentityStatus,
  IpcAdapter,
  OpenGithubArgs,
  Prefs,
  ProjectUiState,
  StreamFrame,
} from "@/api/shell";
import type { Approval, Device } from "@/api/types";
import { ROUTES, type RouteId } from "@/api/routes";
import { fromUnknownError } from "./errors";
import { httpBody } from "./routeBody";

type InvokeArgs = Record<string, unknown>;

export class TauriIpcAdapter implements IpcAdapter {
  async identityStatus(): Promise<IdentityStatus> {
    return this.call<IdentityStatus>("identity_status");
  }

  async pairingBegin(a: {
    pairingString: string;
    deviceName: string;
  }): Promise<{ confirmationCode: string; device: Device }> {
    const raw = await this.call<{
      confirmation_code: string;
      device: Device;
    }>("pairing_begin", {
      pairing_string: a.pairingString,
      device_name: a.deviceName,
    });
    return { confirmationCode: raw.confirmation_code, device: raw.device };
  }

  async pairingConfirm(a: { matches: boolean }): Promise<IdentityStatus> {
    return this.call<IdentityStatus>("pairing_confirm", { matches: a.matches });
  }

  async identityReset(): Promise<IdentityStatus> {
    return this.call<IdentityStatus>("identity_reset");
  }

  async connectionState(): Promise<ConnectionState> {
    return this.call<ConnectionState>("connection_state");
  }

  async api<R extends RouteId>(
    route: R,
    args: import("@/api/routes").RouteArgs[R],
  ): Promise<ApiResult<import("@/api/routes").RouteBody[R]>> {
    const meta = ROUTES[route];
    const record = args as unknown as {
      body?: unknown;
      idempotencyKey?: string;
    };
    const query = meta.query
      ? cleanQuery(meta.query(args as never))
      : undefined;
    const payload: InvokeArgs = {
      route,
      path: meta.path(args as never),
    };
    if (query && Object.keys(query).length > 0) payload.query = query;
    const body = httpBody(route, args, record.body);
    if (body !== undefined) payload.body = body;
    if (record.idempotencyKey !== undefined) {
      payload.idempotency_key = record.idempotencyKey;
    }
    return this.call<ApiResult<import("@/api/routes").RouteBody[R]>>(
      "api_request",
      payload,
    );
  }

  async eventsOpen(
    a: { afterSeq?: number },
    onFrame: (f: StreamFrame) => void,
  ): Promise<void> {
    const channel = new Channel<StreamFrame>();
    channel.onmessage = onFrame;
    const payload: InvokeArgs = { on_frame: channel };
    if (a.afterSeq !== undefined) payload.after_seq = a.afterSeq;
    await this.call<void>("events_open", payload);
  }

  async eventsAck(seq: number): Promise<void> {
    await this.call<void>("events_ack", { seq });
  }

  async eventsClose(): Promise<void> {
    await this.call<void>("events_close");
  }

  async approvalPrepare(a: { approvalId: string }): Promise<ApprovalSheetModel> {
    return this.call<ApprovalSheetModel>("approval_prepare", {
      approval_id: a.approvalId,
    });
  }

  async approvalSign(a: { preparedId: string }): Promise<ApiResult<Approval>> {
    return this.call<ApiResult<Approval>>("approval_sign", {
      prepared_id: a.preparedId,
    });
  }

  async approvalDiscard(a: { preparedId: string }): Promise<void> {
    await this.call<void>("approval_discard", { prepared_id: a.preparedId });
  }

  async guardedWritePrepare(a: GuardedWriteArgs): Promise<GuardedWritePreview> {
    return this.call<GuardedWritePreview>("guarded_write_prepare", toGuardedArgs(a));
  }

  async guardedWriteSubmit(a: { pendingId: string }): Promise<ApiResult<unknown>> {
    return this.call<ApiResult<unknown>>("guarded_write_submit", {
      pending_id: a.pendingId,
    });
  }

  async guardedWriteDiscard(a: { pendingId: string }): Promise<void> {
    await this.call<void>("guarded_write_discard", { pending_id: a.pendingId });
  }

  async prefsGet(): Promise<Prefs> {
    return this.call<Prefs>("prefs_get");
  }

  async prefsSet(a: { patch: Partial<Prefs> }): Promise<Prefs> {
    return this.call<Prefs>("prefs_set", { patch: a.patch });
  }

  async uiStateGet(a: { projectId: string }): Promise<ProjectUiState | null> {
    return this.call<ProjectUiState | null>("ui_state_get", {
      project_id: a.projectId,
    });
  }

  async uiStateSet(a: { projectId: string; state: ProjectUiState }): Promise<void> {
    await this.call<void>("ui_state_set", {
      project_id: a.projectId,
      state: a.state,
    });
  }

  async uiStateList(): Promise<Record<string, ProjectUiState>> {
    return this.call<Record<string, ProjectUiState>>("ui_state_list");
  }

  async openExternal(a: { url: string }): Promise<void> {
    await this.call<void>("open_external", { url: a.url });
  }

  async openGithub(a: OpenGithubArgs): Promise<void> {
    const payload: InvokeArgs = { kind: a.kind, repo: a.repo };
    if (a.number !== undefined) payload.number = a.number;
    if (a.sha !== undefined) payload.sha = a.sha;
    await this.call<void>("open_github", payload);
  }

  private async call<T>(command: string, args?: InvokeArgs): Promise<T> {
    try {
      return await invoke<T>(command, args);
    } catch (error) {
      throw fromUnknownError(error);
    }
  }
}


function cleanQuery(query: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(query)) {
    if (value !== undefined) result[key] = value;
  }
  return result;
}

function toGuardedArgs(a: GuardedWriteArgs): InvokeArgs {
  switch (a.kind) {
    case "settings_put": {
      const payload: InvokeArgs = {
        kind: a.kind,
        scope: a.scope,
        resource: a.resource,
        value: a.value,
        if_match: a.ifMatch,
      };
      if (a.projectId !== undefined) payload.project_id = a.projectId;
      return payload;
    }
    case "settings_delete":
      return {
        kind: a.kind,
        project_id: a.projectId,
        resource: a.resource,
        if_match: a.ifMatch,
      };
    case "device_role":
      return { kind: a.kind, device_id: a.deviceId, role: a.role };
    case "device_revoke":
      return { kind: a.kind, device_id: a.deviceId };
  }
}
