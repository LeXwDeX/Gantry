import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { waitFor } from "@testing-library/react";
import { DesktopController } from "@/store/controller";
import { MockIpcAdapter } from "@/ipc/mock/adapter";
import { startMockApi, type RunningMockApi } from "./mockApi";
import { APPROVAL_INBOX, events } from "./mockEvents";

let mock: RunningMockApi;
let adapter: MockIpcAdapter;
let controller: DesktopController | null = null;

beforeEach(async () => {
  mock = await startMockApi();
  adapter = new MockIpcAdapter({ baseUrl: mock.baseUrl, reconnectDelayMs: 20 });
});

afterEach(async () => {
  await controller?.stop();
  controller = null;
  await mock.close();
});

async function start(): Promise<DesktopController> {
  controller = new DesktopController(adapter, {
    refreshDelayMs: 0,
    persistDelayMs: 0,
    prefetchWorkflows: false,
  });
  await controller.start();
  await waitFor(
    () => {
      expect(controller?.getSnapshot().status).toBe("ready");
      expect(controller?.getSnapshot().connection?.phase).toBe("live");
    },
    { timeout: 5000 },
  );
  return controller;
}

describe("DesktopController × Mock API", () => {
  it("按启动序列水合：me → projects → 当前项目资源与 inbox.summary → 开流", async () => {
    await start();
    const ops = await mock.operations();
    expect(ops[0]).toBe("getMe");
    expect(ops.indexOf("listProjects")).toBeGreaterThan(0);
    for (const op of ["getInboxSummary", "listInbox", "listConversations", "listDeliveries", "listWorktrees"]) {
      expect(ops).toContain(op);
    }
    const stream = (await mock.requests()).find((r) => r.operationId === "streamEvents");
    expect(stream).toBeTruthy();
    // 开流在全部水合请求之后（api.md §5.3：取最小 X-Event-Seq 再续流）。
    expect(ops.lastIndexOf("streamEvents")).toBeGreaterThan(ops.indexOf("listConversations"));
    expect(adapter.calls.find((c) => c.command === "events_open")?.args).toEqual({ after_seq: 18240 });
  });

  it("注入事件后按资源重新拉取，ack 单调推进", async () => {
    await start();
    const before = (await mock.operations()).filter((op) => op === "getInboxSummary").length;
    const item = await adapter.api("inbox.get", { inboxId: APPROVAL_INBOX });
    if (!item.ok || !item.body) throw new Error("inbox.get failed");
    await mock.emit(events.inboxRaised(item.body as unknown as Record<string, unknown> & { id: string }));
    await waitFor(() => expect(adapter.ackedSeq).toBe(18241), { timeout: 5000 });
    await waitFor(
      async () => {
        const after = (await mock.operations()).filter((op) => op === "getInboxSummary").length;
        expect(after).toBeGreaterThan(before);
      },
      { timeout: 5000 },
    );
  });
});
