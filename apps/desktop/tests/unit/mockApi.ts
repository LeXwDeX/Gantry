/**
 * Mock API 场景控制的测试客户端（实例由 scripts/mock-harness.mjs 管理，globalSetup 启动；
 * 控制动作转发给 tools/mock-api 自带的 /__mock/*）。每次 startMockApi() 新建一个独立实例。
 */
import { inject } from "vitest";
import type { EventEnvelope } from "@/api/types";
import type { InjectedEvent } from "./mockEvents";

export interface MockRequest {
  method: string;
  path: string;
  operationId: string;
}

export interface RunningMockApi {
  baseUrl: string;
  /** 追加一条事件并实时推送给已建立的事件流；data 由 Mock 按 schemas/events 校验。 */
  emit(event: InjectedEvent): Promise<EventEnvelope>;
  /** 断开已建立的事件流（客户端按游标重连并拿到回放）。 */
  dropStreams(): Promise<void>;
  /** 合并 patch 到 state[collection].items 中 id 匹配的项（不产生事件）。 */
  mutate(collection: string, id: string, patch: Record<string, unknown>): Promise<void>;
  push(collection: string, item: Record<string, unknown>): Promise<void>;
  set(key: string, value: unknown): Promise<void>;
  clock(advanceSeconds: number): Promise<void>;
  failNext(operationId: string, status: number, code: string): Promise<void>;
  requests(): Promise<MockRequest[]>;
  operations(): Promise<string[]>;
  close(): Promise<void>;
}

async function control<T>(method: string, path: string, body?: unknown): Promise<T> {
  const response = await globalThis.fetch(`${inject("mockControlUrl")}${path}`, {
    method,
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`mock-harness ${method} ${path} → ${response.status} ${text}`);
  return (text ? JSON.parse(text) : undefined) as T;
}

export async function startMockApi(): Promise<RunningMockApi> {
  const { id, baseUrl } = await control<{ id: string; baseUrl: string }>("POST", "/instances");
  const base = `/instances/${id}`;
  return {
    baseUrl,
    emit: (event) => control("POST", `${base}/events`, event),
    dropStreams: () => control("POST", `${base}/drop-streams`),
    mutate: (collection, itemId, patch) =>
      control("POST", `${base}/mutate`, { collection, id: itemId, patch }),
    push: (collection, item) => control("POST", `${base}/mutate`, { collection, push: item }),
    set: (key, value) => control("POST", `${base}/mutate`, { set: key, value }),
    clock: (advanceSeconds) => control("POST", `${base}/clock`, { advance_s: advanceSeconds }),
    failNext: (operationId, status, code) =>
      control("POST", `${base}/fail-next`, { operation_id: operationId, status, code }),
    requests: () => control("GET", `${base}/requests`),
    operations: async () =>
      (await control<MockRequest[]>("GET", `${base}/requests`)).map((r) => r.operationId),
    close: () => control("DELETE", base),
  };
}
