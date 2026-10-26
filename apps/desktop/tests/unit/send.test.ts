import { describe, expect, it } from "vitest";
import { waitFor } from "@testing-library/react";
import type { ApiResult } from "@/api/shell";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import { buildSend } from "@/store/send";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { event, sleep, startController } from "./helpers";

class FlakyPostAdapter extends ScenarioIpcAdapter {
  keys: string[] = [];
  failed = false;

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    if (route === "messages.post") {
      const post = args as RouteArgs["messages.post"];
      this.keys.push(post.idempotencyKey);
      if (!this.failed) {
        this.failed = true;
        return { ok: false, network: "timeout" };
      }
    }
    return super.api(route, args);
  }
}

class HeldPostAdapter extends ScenarioIpcAdapter {
  postStarted = false;
  completed = false;
  posts = 0;
  private releasePost: (() => void) | null = null;

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    if (route === "messages.post") {
      this.posts += 1;
      this.postStarted = true;
      await new Promise<void>((resolve) => {
        this.releasePost = resolve;
      });
    }
    const result = await super.api(route, args);
    if (route === "messages.post") this.completed = true;
    return result;
  }

  release(): void {
    this.releasePost?.();
  }
}

class HeldDeliveriesAdapter extends ScenarioIpcAdapter {
  hold = false;
  started = false;
  private releaseRead: (() => void) | null = null;

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    if (route === "deliveries.list" && this.hold) {
      this.hold = false;
      this.started = true;
      await new Promise<void>((resolve) => {
        this.releaseRead = resolve;
      });
    }
    return super.api(route, args);
  }

  release(): void {
    this.releaseRead?.();
  }
}

class ValidationFailAdapter extends ScenarioIpcAdapter {
  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    if (route === "messages.post") {
      return {
        ok: false,
        status: 422,
        problem: {
          code: "validation_failed",
          title: "Validation failed.",
          request_id: "req_test",
        },
      };
    }
    return super.api(route, args);
  }
}

describe("buildSend", () => {
  it("只发 text / refs，未显式改上限时省略 level_cap", () => {
    const body = buildSend({ text: "你好", ctx: null, levelOverride: null });
    expect(body).toEqual({ text: "你好", refs: [] });
    expect("level_cap" in body).toBe(false);
  });

  it("上下文只来自 ctx，显式上限才出现", () => {
    const body = buildSend({
      text: "继续",
      ctx: { kind: "issue", repo: "LeXwDeX/gantry", number: 5 },
      levelOverride: "High",
    });
    expect(body).toEqual({
      text: "继续",
      refs: [{ kind: "issue", repo: "LeXwDeX/gantry", number: 5 }],
      level_cap: "High",
    });
  });
});

describe("DesktopController 发送", () => {
  it("选中项不改变发送上下文", async () => {
    const { adapter, controller } = await startController("offline");
    controller.updateUi({
      sel: { kind: "pr", repo: "LeXwDeX/gantry", number: 8 },
      draft: "只发全项目",
    });
    controller.send();
    const ui = controller.getSnapshot().ui.prj_gantry;
    expect(controller.getSnapshot().ui.prj_gantry.sel).toEqual({
      kind: "pr",
      repo: "LeXwDeX/gantry",
      number: 8,
    });
    expect(ui.ctx).toBeNull();
    expect(ui.outbox).toHaveLength(1);
    expect(ui.outbox[0].refs).toEqual([]);
    expect(ui.outbox[0].state).toBe("queued");
    expect(ui.draft).toBe("");
    await controller.stop();
    void adapter;
  });

  it("显式上下文与上限入队，发送后复位", async () => {
    const { controller } = await startController("offline");
    controller.updateUi({
      ctx: { kind: "issue", repo: "LeXwDeX/gantry", number: 5 },
      levelOverride: "High",
      draft: "带上下文",
    });
    controller.send();
    const ui = controller.getSnapshot().ui.prj_gantry;
    expect(ui.outbox[0].refs).toEqual([
      { kind: "issue", repo: "LeXwDeX/gantry", number: 5 },
    ]);
    expect(ui.outbox[0].level_cap).toBe("High");
    expect(ui.ctx).toBeNull();
    expect(ui.levelOverride).toBeNull();
    await controller.stop();
  });

  it("未显式改上限时不发送 level_cap", async () => {
    const { controller } = await startController("offline");
    controller.updateUi({ draft: "默认上限" });
    controller.send();
    const item = controller.getSnapshot().ui.prj_gantry.outbox[0];
    expect(item.level_cap).toBeUndefined();
    await controller.stop();
  });

  it("viewer 不能入队发送", async () => {
    const { controller } = await startController("viewer");
    controller.updateUi({ draft: "只读" });
    controller.send();
    expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    expect(controller.getSnapshot().notice).toContain("只读");
    await controller.stop();
  });

  it("网络失败后沿用同一 Idempotency-Key 重试", async () => {
    const adapter = new FlakyPostAdapter("default");
    const { DesktopController } = await import("@/store/controller");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    controller.updateUi({ draft: "幂等消息" });
    controller.send();
    await controller.flushOutbox();
    expect(controller.getSnapshot().ui.prj_gantry.outbox[0]?.state).toBe(
      "queued",
    );
    await controller.flushOutbox();
    await waitFor(() => {
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    });
    expect(adapter.keys).toHaveLength(2);
    expect(adapter.keys[0]).toBe(adapter.keys[1]);
    await controller.stop();
  });

  it("离线队列按 FIFO 恢复后发送", async () => {
    const { adapter, controller } = await startController("offline");
    controller.updateUi({ draft: "第一条" });
    controller.send();
    controller.updateUi({ draft: "第二条" });
    controller.send();
    expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(2);
    adapter.setConnection("live");
    await controller.flushOutbox();
    await waitFor(() => {
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    });
    const messages = controller.getSnapshot().projectData.prj_gantry.messages;
    const texts = messages.slice(-2).map((message) => message.text);
    expect(texts).toEqual(["第一条", "第二条"]);
    await controller.stop();
  });

  it("吊销后在途发送的结果不回填缓存，待发项保留", async () => {
    const adapter = new HeldPostAdapter("default");
    const { DesktopController } = await import("@/store/controller");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      controller.updateUi({ draft: "在途消息" });
      controller.send();
      await waitFor(() => expect(adapter.postStarted).toBe(true));

      adapter.setConnection("revoked");
      expect(controller.getSnapshot().ui.prj_gantry.outbox[0]?.state).toBe(
        "queued",
      );

      adapter.release();
      await waitFor(() => expect(adapter.completed).toBe(true));
      await sleep(0);

      const snapshot = controller.getSnapshot();
      expect(snapshot.status).toBe("unpaired");
      expect(snapshot.projectData).toEqual({});
      expect(snapshot.ui.prj_gantry.outbox).toHaveLength(1);
      expect(snapshot.ui.prj_gantry.outbox[0]).toMatchObject({
        text: "在途消息",
        state: "queued",
      });
      await controller.flushOutbox();
      expect(adapter.posts).toBe(1);
    } finally {
      await controller.stop();
    }
  });

  it("吊销后在途刷新不回填缓存，停留在阻塞视图", async () => {
    const adapter = new HeldDeliveriesAdapter("default");
    const { DesktopController } = await import("@/store/controller");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      adapter.hold = true;
      adapter.emitFrame({
        t: "event",
        envelope: event(6000, "delivery.stage_changed", "dlv_gantry_9"),
      });
      const flushing = controller.flushRefreshes();
      await waitFor(() => expect(adapter.started).toBe(true));

      adapter.setConnection("cert_expired");
      adapter.release();
      await flushing;
      await sleep(0);

      const snapshot = controller.getSnapshot();
      expect(snapshot.status).toBe("unpaired");
      expect(snapshot.projectData).toEqual({});
    } finally {
      await controller.stop();
    }
  });

  it("非重试错误保留失败项与原文，编辑后回到草稿", async () => {
    const adapter = new ValidationFailAdapter("default");
    const { DesktopController } = await import("@/store/controller");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    controller.updateUi({
      ctx: { kind: "issue", repo: "LeXwDeX/gantry", number: 5 },
      levelOverride: "High",
      draft: "无效内容",
    });
    controller.send();
    await controller.flushOutbox();
    const item = controller.getSnapshot().ui.prj_gantry.outbox[0];
    expect(item.state).toBe("failed");
    expect(item.error_code).toBe("validation_failed");
    expect(item.text).toBe("无效内容");
    controller.editOutbox(item.local_id);
    expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    expect(controller.getSnapshot().ui.prj_gantry.draft).toBe("无效内容");
    expect(controller.getSnapshot().ui.prj_gantry.ctx).toEqual({
      kind: "issue",
      repo: "LeXwDeX/gantry",
      number: 5,
    });
    expect(controller.getSnapshot().ui.prj_gantry.levelOverride).toBe("High");
    await controller.stop();
  });

  it("角色降级为 viewer 后阻止新的排队发送", async () => {
    const { adapter, controller } = await startController("default");
    adapter.setRole("viewer");
    await waitFor(() => {
      expect(controller.getSnapshot().me?.device.role).toBe("viewer");
    });
    controller.updateUi({ draft: "降级后" });
    controller.send();
    expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    expect(controller.getSnapshot().notice).toContain("只读");
    await controller.stop();
  });

  it("角色降级时已有队列以 forbidden_role 失败", async () => {
    const { adapter, controller } = await startController("offline");
    controller.updateUi({ draft: "后来降级" });
    controller.send();
    adapter.setRole("viewer");
    await waitFor(() => {
      expect(controller.getSnapshot().me?.device.role).toBe("viewer");
    });
    await controller.flushOutbox();
    const item = controller.getSnapshot().ui.prj_gantry.outbox[0];
    expect(item.state).toBe("failed");
    expect(item.error_code).toBe("forbidden_role");
    await controller.stop();
  });
});
