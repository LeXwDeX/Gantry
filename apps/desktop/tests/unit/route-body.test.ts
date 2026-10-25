import { describe, expect, it } from "vitest";
import { httpBody } from "@/ipc/routeBody";

describe("api_request 请求体映射", () => {
  it("把扁平 UI 参数映射为协议 JSON", () => {
    expect(
      httpBody("deliveries.patch", {
        deliveryId: "dlv_x",
        levelCap: "High",
      }),
    ).toEqual({ level_cap: "High" });
    expect(
      httpBody("inbox.choose", {
        inboxId: "inb_x",
        optionId: "upgrade",
        note: "继续",
        idempotencyKey: "key",
      }),
    ).toEqual({ option_id: "upgrade", note: "继续" });
    expect(
      httpBody("worktrees.lock_put", {
        worktreeId: "wtr_x",
        leaseSeconds: 1800,
      }),
    ).toEqual({ lease_seconds: 1800 });
    expect(
      httpBody("devices.rename", { deviceId: "dev_x", name: "书房 Mac" }),
    ).toEqual({ name: "书房 Mac" });
  });

  it("无请求体的路由保持空；消息体原样传递", () => {
    expect(httpBody("deliveries.pause", { deliveryId: "dlv_x" })).toEqual({});
    expect(
      httpBody("messages.post", {
        conversationId: "cnv_x",
        idempotencyKey: "key",
        body: { text: "hi", refs: [] },
      }),
    ).toEqual({ text: "hi", refs: [] });
    expect(httpBody("me.get", {})).toBeUndefined();
  });
});
