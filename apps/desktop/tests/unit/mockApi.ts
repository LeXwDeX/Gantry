/**
 * Mock API 场景控制的测试客户端（服务端见 scripts/mock-harness.mjs，由 globalSetup 启动）。
 * 每次 startMockApi() 新建一个独立的 Mock 实例，测试之间不共享状态。
 */
import { inject } from "vitest";
import type { EventEnvelope } from "@/api/types";

export interface MockRequest {
  method: string;
  path: string;
  operationId: string;
}

export interface RunningMockApi {
  baseUrl: string;
  /** 追加一条事件（seq = head + 1），并断开现有事件流促使客户端按游标续流。 */
  inject(
    type: string,
    subject: string,
    data?: unknown,
    projectId?: string | null,
  ): Promise<EventEnvelope>;
  dropStreams(): Promise<void>;
  /** 合并 patch 到 state[collection].items 中 id 匹配的项。 */
  mutate(collection: string, id: string, patch: Record<string, unknown>): Promise<void>;
  push(collection: string, item: Record<string, unknown>): Promise<void>;
  set(key: string, value: unknown): Promise<void>;
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
  if (!response.ok) throw new Error(`mock-harness ${method} ${path} → ${response.status}`);
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

export async function startMockApi(): Promise<RunningMockApi> {
  const { id, baseUrl } = await control<{ id: string; baseUrl: string }>("POST", "/instances");
  const base = `/instances/${id}`;
  return {
    baseUrl,
    inject: (type, subject, data = {}, projectId) =>
      control("POST", `${base}/inject`, { type, subject, data, project_id: projectId }),
    dropStreams: () => control("POST", `${base}/drop-streams`),
    mutate: (collection, itemId, patch) =>
      control("POST", `${base}/mutate`, { collection, id: itemId, patch }),
    push: (collection, item) => control("POST", `${base}/mutate`, { collection, push: item }),
    set: (key, value) => control("POST", `${base}/mutate`, { set: key, value }),
    requests: () => control("GET", `${base}/requests`),
    operations: async () =>
      (await control<MockRequest[]>("GET", `${base}/requests`)).map((r) => r.operationId),
    close: () => control("DELETE", base),
  };
}
