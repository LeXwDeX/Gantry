import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createElement, createRef, useSyncExternalStore } from "react";
import type { ApiResult, ConnectionState } from "@/api/shell";
import type { IdentityStatus, StreamFrame } from "@/api/shell";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { DesktopController } from "@/store/controller";
import { Composer } from "@/features/workspace/Composer";
import { PairingView } from "@/features/overlays/PairingView";
import { event, sleep, startController, waitForResync } from "./helpers";

function LivePairing({ controller }: { controller: DesktopController }) {
  const state = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  );
  return createElement(PairingView, { controller, state });
}

function LiveComposer({ controller }: { controller: DesktopController }) {
  const state = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  );
  const projectId = state.currentProjectId;
  if (!projectId) return null;
  return createElement(Composer, {
    projectId,
    controller,
    data: state.projectData[projectId],
    ui: state.ui[projectId],
    isViewer: false,
    platform: null,
    connectionLive: state.connection?.phase === "live",
    focusRequest: 0,
    composerRef: createRef<HTMLTextAreaElement>(),
    onUpdate: (patch) => controller.updateUi(patch, projectId),
  });
}

class HeldUiStateAdapter extends ScenarioIpcAdapter {
  hold = false;
  started = false;
  private releaseRead: (() => void) | null = null;

  override async uiStateList(): Promise<
    Record<string, import("@/api/shell").ProjectUiState>
  > {
    if (this.hold) {
      this.hold = false;
      this.started = true;
      await new Promise<void>((resolve) => {
        this.releaseRead = resolve;
      });
    }
    return super.uiStateList();
  }

  release(): void {
    this.releaseRead?.();
  }
}

class HeldProjectLoadAdapter extends ScenarioIpcAdapter {
  armed = false;
  started = false;
  private releaseRead: (() => void) | null = null;

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    if (
      route === "deliveries.list" &&
      this.armed &&
      (args as RouteArgs["deliveries.list"]).projectId === "prj_gantry"
    ) {
      this.armed = false;
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

class HeldSummaryAdapter extends ScenarioIpcAdapter {
  armed = false;
  started = false;
  private releaseRead: (() => void) | null = null;

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    if (route === "inbox.summary" && this.armed) {
      this.armed = false;
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

class HeldResetAdapter extends ScenarioIpcAdapter {
  holdReset = false;
  failReset = false;
  failConnectionState = false;
  started = false;
  private releaseRead: (() => void) | null = null;

  override async connectionState(): Promise<ConnectionState> {
    if (this.failConnectionState) {
      this.failConnectionState = false;
      throw new Error("模拟连接读取失败");
    }
    return super.connectionState();
  }

  override async identityReset(): Promise<IdentityStatus> {
    if (this.failReset) {
      this.failReset = false;
      throw new Error("模拟重置失败");
    }
    const result = await super.identityReset();
    if (this.holdReset) {
      this.holdReset = false;
      this.started = true;
      await new Promise<void>((resolve) => {
        this.releaseRead = resolve;
      });
    }
    return result;
  }

  release(): void {
    this.releaseRead?.();
  }
}

async function pairAfterResetWithStaleOutbox(
  adapter: ScenarioIpcAdapter,
  controller: DesktopController,
): Promise<void> {
  controller.updateUi({ outbox: [queuedItem("旧身份队列")] }, "prj_gantry");
  adapter.setConnection("revoked");
  await waitFor(() => expect(controller.getSnapshot().status).toBe("unpaired"));
  await controller.resetIdentity();
  await adapter.pairingBegin({
    pairingString: "gantry+pair://example",
    deviceName: "测试 Mac",
  });
  await adapter.pairingConfirm({ matches: true });
  await controller.refresh();
  expect(controller.getSnapshot().status).toBe("ready");
  expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1);
  expect(controller.staleOutboxCount()).toBe(1);
}

function queuedItem(text: string) {
  return {
    local_id: `local_${text}`,
    idempotency_key: `idem_${text}`,
    text,
    refs: [],
    created_at: "2026-10-24T12:59:00.000Z",
    state: "queued" as const,
  };
}

describe("F1 身份重置与旧待发队列", () => {
  it("重置并重新配对后，旧身份的待发消息不会自动发送，直到用户丢弃", async () => {
    const { adapter, controller } = await startController("offline");
    try {
      controller.updateUi({ draft: "旧身份消息" });
      controller.send();
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1);

      adapter.setConnection("revoked");
      expect(controller.getSnapshot().status).toBe("unpaired");
      expect(controller.getSnapshot().ui.prj_gantry.outbox[0]?.state).toBe(
        "queued",
      );

      await controller.resetIdentity();
      expect(controller.getSnapshot().identity?.state).toBe("unpaired");
      expect(controller.staleOutboxCount()).toBe(1);

      const post = vi.spyOn(adapter, "api");
      await adapter.pairingBegin({
        pairingString: "gantry+pair://example",
        deviceName: "测试 Mac",
      });
      await adapter.pairingConfirm({ matches: true });
      await controller.refresh();
      await controller.flushOutbox("prj_gantry");
      await sleep(10);

      const posted = post.mock.calls.filter(([route]) => route === "messages.post");
      expect(posted).toHaveLength(0);
      expect(controller.getSnapshot().ui.prj_gantry.outbox[0]).toMatchObject({
        text: "旧身份消息",
        state: "queued",
      });
      expect(controller.staleOutboxCount()).toBe(1);

      expect(controller.discardStaleOutbox()).toBe(1);
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    } finally {
      await controller.stop();
    }
  });

  it("配对完成后先询问旧队列，丢弃前不刷新、丢弃后清空", async () => {
    const { adapter, controller } = await startController("default");
    try {
      controller.updateUi({ outbox: [queuedItem("旧队列")] });
      adapter.setConnection("revoked");
      expect(controller.staleOutboxCount()).toBe(1);
      const refresh = vi.spyOn(controller, "refresh");

      render(createElement(LivePairing, { controller }));
      fireEvent.change(
        await screen.findByLabelText("配对串"),
        { target: { value: "gantry+pair://example" } },
      );
      fireEvent.change(screen.getByLabelText("设备名"), {
        target: { value: "测试 Mac" },
      });
      fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
      expect(await screen.findByText("492 713")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "一致" }));

      expect(
        await screen.findByText(/旧身份还有 1 条消息未发送/),
      ).toBeInTheDocument();
      expect(
        screen.getByText(
          "保留的旧消息仅留在当前会话，不写入新身份的本机存储；关闭窗口前请编辑、重试或删除。",
        ),
      ).toBeInTheDocument();
      expect(refresh).not.toHaveBeenCalled();

      fireEvent.click(
        screen.getByRole("button", { name: "丢弃旧消息并继续" }),
      );
      await waitFor(() => expect(refresh).toHaveBeenCalledTimes(1));
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    } finally {
      await controller.stop();
    }
  });

  it("保留旧队列后，在工作区提供显式编辑、重试和删除操作", async () => {
    const { adapter, controller } = await startController("default");
    try {
      controller.updateUi({ outbox: [queuedItem("旧队列")] });
      adapter.setConnection("revoked");
      await controller.resetIdentity();
      await adapter.pairingBegin({
        pairingString: "gantry+pair://example",
        deviceName: "测试 Mac",
      });
      await adapter.pairingConfirm({ matches: true });
      await controller.refresh();

      const retry = vi.spyOn(controller, "retryOutbox");
      render(createElement(LiveComposer, { controller }));

      expect(await screen.findByText("旧身份待处理")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "删除" })).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "重试" }));
      expect(retry).toHaveBeenCalledWith("local_旧队列");
    } finally {
      await controller.stop();
    }
  });

  it("取消重置保留身份、连接与待发队列，且不抑制正常持久化", async () => {
    const { adapter, controller } = await startController("default");
    const reset = vi.spyOn(adapter, "identityReset");
    try {
      controller.updateUi({ outbox: [queuedItem("保留项")] }, "prj_gantry");
      adapter.setConnection("revoked");
      await waitFor(() =>
        expect(controller.getSnapshot().status).toBe("unpaired"),
      );
      const before = controller.getSnapshot().ui.prj_gantry.outbox;

      render(createElement(LivePairing, { controller }));
      fireEvent.click(
        await screen.findByRole("button", { name: "重置本机身份…" }),
      );
      fireEvent.click(screen.getByRole("button", { name: "取消" }));

      await sleep(0);
      expect(reset).not.toHaveBeenCalled();
      expect(controller.getSnapshot().connection?.phase).toBe("revoked");
      expect(controller.getSnapshot().identity?.state).toBe("paired");
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toEqual(before);

      controller.updateUi({ draft: "取消后草稿" }, "prj_gantry");
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.draft).toBe("取消后草稿");
        // 尚未重置的终态仍属原身份：原有待发队列照常持久化。
        expect(stored.prj_gantry?.outbox).toEqual(before);
      });
    } finally {
      await controller.stop();
    }
  });
});

describe("F2 刷新与项目选择交错", () => {
  it("迟到的 uiStateList 响应不改写用户后来选中的项目与数据", async () => {
    const adapter = new HeldUiStateAdapter("default");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      expect(controller.getSnapshot().currentProjectId).toBe("prj_gantry");
      adapter.hold = true;
      const refresh = controller.refresh();
      await waitFor(() => expect(adapter.started).toBe(true));

      controller.selectProject("prj_specgit");
      await waitFor(() =>
        expect(
          controller.getSnapshot().projectData.prj_specgit?.messages.length,
        ).toBe(1),
      );
      const before = JSON.stringify(
        controller.getSnapshot().projectData.prj_specgit?.messages,
      );

      adapter.release();
      await refresh;
      await sleep(0);

      expect(controller.getSnapshot().currentProjectId).toBe("prj_specgit");
      expect(
        JSON.stringify(controller.getSnapshot().projectData.prj_specgit?.messages),
      ).toBe(before);
    } finally {
      await controller.stop();
    }
  });

  it("刷新后半程的项目加载不覆盖之后选中的项目数据", async () => {
    const adapter = new HeldProjectLoadAdapter("default");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      adapter.armed = true;
      const refresh = controller.refresh();
      await waitFor(() => expect(adapter.started).toBe(true));

      controller.selectProject("prj_specgit");
      await waitFor(() =>
        expect(
          controller.getSnapshot().projectData.prj_specgit?.messages.length,
        ).toBe(1),
      );
      const before = JSON.stringify(
        controller.getSnapshot().projectData.prj_specgit?.messages,
      );

      adapter.release();
      await refresh;
      await sleep(0);

      expect(controller.getSnapshot().currentProjectId).toBe("prj_specgit");
      expect(
        JSON.stringify(controller.getSnapshot().projectData.prj_specgit?.messages),
      ).toBe(before);
    } finally {
      await controller.stop();
    }
  });

  it("刷新等待收件箱摘要时发送成功的消息不被旧快照覆盖", async () => {
    const adapter = new HeldSummaryAdapter("default");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      const before =
        controller.getSnapshot().projectData.prj_gantry?.messages.length ?? 0;
      const conversation = controller.getSnapshot().projectData.prj_gantry
        ?.conversation;
      expect(conversation).toBeTruthy();

      adapter.armed = true;
      const refresh = controller.refresh();
      await waitFor(() => expect(adapter.started).toBe(true));

      controller.updateUi({ draft: "刷新期间发送" });
      controller.send();
      await waitFor(() => {
        expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
        expect(
          controller.getSnapshot().projectData.prj_gantry?.messages.length,
        ).toBe(before + 1);
      });

      adapter.release();
      await refresh;

      const messages =
        controller.getSnapshot().projectData.prj_gantry?.messages ?? [];
      expect(messages).toHaveLength(before + 1);
      expect(
        messages.filter((message) => message.text === "刷新期间发送"),
      ).toHaveLength(1);
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);

      const stored = await adapter.api("messages.list", {
        conversationId: conversation!.id,
        limit: 50,
        order: "desc",
      });
      expect(
        stored.ok &&
          stored.body?.items.filter((message) => message.text === "刷新期间发送"),
      ).toHaveLength(1);

      await controller.refresh();
      expect(
        controller.getSnapshot().projectData.prj_gantry?.messages.filter(
          (message) => message.text === "刷新期间发送",
        ),
      ).toHaveLength(1);
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    } finally {
      await controller.stop();
    }
  });
});

describe("审计收尾：身份重置同步连接状态", () => {
  it.each(["revoked", "cert_expired"] as const)(
    "重置后（%s）配对页显示未配对，不再显示旧的终态提示",
    async (phase) => {
      const { adapter, controller } = await startController("default");
      try {
        adapter.setConnection(phase);
        await waitFor(() =>
          expect(controller.getSnapshot().status).toBe("unpaired"),
        );

        render(createElement(LivePairing, { controller }));
        fireEvent.click(
          await screen.findByRole("button", { name: "重置本机身份…" }),
        );
        fireEvent.click(screen.getByRole("button", { name: "确认重置" }));

        await waitFor(() =>
          expect(controller.getSnapshot().identity?.state).toBe("unpaired"),
        );
        expect(controller.getSnapshot().connection?.phase).toBe("unpaired");
        expect(
          await screen.findByText("此设备尚未配对。配对完成后才能载入工作区。"),
        ).toBeInTheDocument();
        expect(screen.queryByText(/此设备已被吊销/)).not.toBeInTheDocument();
        expect(screen.queryByText(/证书已过期/)).not.toBeInTheDocument();
      } finally {
        await controller.stop();
      }
    },
  );

  it("重置后重新配对进入工作区，旧身份的队列保留且不自动发送", async () => {
    const { adapter, controller } = await startController("default");
    try {
      controller.updateUi({ outbox: [queuedItem("跨重置旧队列")] });
      adapter.setConnection("revoked");
      await waitFor(() =>
        expect(controller.getSnapshot().status).toBe("unpaired"),
      );
      expect(controller.getSnapshot().connection?.phase).toBe("revoked");

      await controller.resetIdentity();
      expect(controller.getSnapshot().connection?.phase).toBe("unpaired");

      const post = vi.spyOn(adapter, "api");
      await adapter.pairingBegin({
        pairingString: "gantry+pair://example",
        deviceName: "测试 Mac",
      });
      await adapter.pairingConfirm({ matches: true });
      await controller.refresh();

      expect(controller.getSnapshot().status).toBe("ready");
      expect(controller.getSnapshot().identity?.state).toBe("paired");
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1);
      expect(controller.staleOutboxCount()).toBe(1);

      await controller.flushOutbox("prj_gantry");
      await sleep(10);
      const posted = post.mock.calls.filter(
        ([route]) => route === "messages.post",
      );
      expect(posted).toHaveLength(0);
      expect(controller.getSnapshot().ui.prj_gantry.outbox[0]).toMatchObject({
        text: "跨重置旧队列",
        state: "queued",
      });
    } finally {
      await controller.stop();
    }
  });

  it("重置后停止旧控制器不回写界面状态，新控制器不恢复旧草稿", async () => {
    const { adapter, controller } = await startController("default");
    const post = vi.spyOn(adapter, "api");
    const options = {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    };
    let fresh: DesktopController | null = null;
    try {
      controller.updateUi({
        draft: "旧身份草稿",
        outbox: [queuedItem("旧身份队列")],
      });
      await waitFor(async () => {
        expect(Object.keys(await adapter.uiStateList())).toContain("prj_gantry");
      });

      adapter.setConnection("revoked");
      await waitFor(() =>
        expect(controller.getSnapshot().status).toBe("unpaired"),
      );
      await controller.resetIdentity();
      expect(await adapter.uiStateList()).toEqual({});

      await controller.stop();
      expect(await adapter.uiStateList()).toEqual({});

      fresh = new DesktopController(adapter, options);
      await fresh.start();
      expect(fresh.getSnapshot().status).toBe("unpaired");
      expect(fresh.getSnapshot().ui.prj_gantry?.draft ?? "").not.toBe(
        "旧身份草稿",
      );

      await adapter.pairingBegin({
        pairingString: "gantry+pair://example",
        deviceName: "测试 Mac",
      });
      await adapter.pairingConfirm({ matches: true });
      await fresh.refresh();

      expect(fresh.getSnapshot().status).toBe("ready");
      expect(fresh.getSnapshot().ui.prj_gantry.draft).not.toBe("旧身份草稿");
      expect(fresh.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
      expect(
        post.mock.calls.filter(([route]) => route === "messages.post"),
      ).toHaveLength(0);
    } finally {
      if (fresh) await fresh.stop();
      else await controller.stop();
    }
  });

  it("重新配对后保留的旧队列不写入新身份持久化，新控制器不自动发送", async () => {
    const { adapter, controller } = await startController("default");
    const post = vi.spyOn(adapter, "api");
    const options = {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    };
    let fresh: DesktopController | null = null;
    try {
      await pairAfterResetWithStaleOutbox(adapter, controller);

      controller.updateUi({ draft: "配对后草稿" }, "prj_gantry");
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.draft).toBe("配对后草稿");
      });
      await controller.stop();

      fresh = new DesktopController(adapter, options);
      await fresh.start();
      await waitFor(() => expect(fresh?.getSnapshot().status).toBe("ready"));
      await sleep(20);
      expect(
        post.mock.calls.filter(([route]) => route === "messages.post"),
      ).toHaveLength(0);
      expect(fresh.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);

      const stored = await adapter.uiStateList();
      expect(stored.prj_gantry?.draft).toBe("配对后草稿");
      expect(stored.prj_gantry?.outbox).toEqual([]);
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1);
    } finally {
      if (fresh) await fresh.stop();
      else await controller.stop();
    }
  });

  it("新身份的新排队项可恢复并发送，旧身份项只留在当前会话", async () => {
    const { adapter, controller } = await startController("default");
    const post = vi.spyOn(adapter, "api");
    const options = {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    };
    let fresh: DesktopController | null = null;
    try {
      await pairAfterResetWithStaleOutbox(adapter, controller);
      controller.updateUi(
        {
          outbox: [
            ...controller.getSnapshot().ui.prj_gantry.outbox,
            queuedItem("新身份队列"),
          ],
        },
        "prj_gantry",
      );
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.outbox.map((item) => item.text)).toEqual([
          "新身份队列",
        ]);
      });
      await controller.stop();

      fresh = new DesktopController(adapter, options);
      await fresh.start();
      await waitFor(() => expect(fresh?.getSnapshot().status).toBe("ready"));
      await waitFor(() =>
        expect(fresh?.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0),
      );
      const posted = post.mock.calls.filter(
        ([route]) => route === "messages.post",
      );
      expect(posted).toHaveLength(1);
      expect(
        (posted[0][1] as RouteArgs["messages.post"]).body?.text,
      ).toBe("新身份队列");
    } finally {
      if (fresh) await fresh.stop();
      else await controller.stop();
    }
  });

  it("保留的旧队列仍可在当前会话显式重试并恰好发送一次", async () => {
    const { adapter, controller } = await startController("default");
    try {
      await pairAfterResetWithStaleOutbox(adapter, controller);
      const post = vi.spyOn(adapter, "api");
      controller.retryOutbox("local_旧身份队列");
      await waitFor(() =>
        expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0),
      );
      const posted = post.mock.calls.filter(
        ([route]) => route === "messages.post",
      );
      expect(posted).toHaveLength(1);
      expect(
        (posted[0][1] as RouteArgs["messages.post"]).body?.text,
      ).toBe("旧身份队列");
    } finally {
      await controller.stop();
    }
  });

  it("普通离线队列仍持久化", async () => {
    const { adapter, controller } = await startController("default");
    try {
      adapter.setConnection("reconnecting");
      controller.updateUi({ outbox: [queuedItem("离线队列")] }, "prj_gantry");
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.outbox[0]?.text).toBe("离线队列");
      });
    } finally {
      await controller.stop();
    }
  });

  it("重置返回挂起期间不写回旧草稿", async () => {
    const adapter = new HeldResetAdapter("default");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      await waitFor(() => expect(controller.getSnapshot().status).toBe("ready"));
      controller.updateUi({ draft: "重置前草稿" }, "prj_gantry");
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.draft).toBe("重置前草稿");
      });

      adapter.holdReset = true;
      const reset = controller.resetIdentity();
      await waitFor(() => expect(adapter.started).toBe(true));
      expect(await adapter.uiStateList()).toEqual({});

      controller.updateUi({ draft: "重置期间草稿" }, "prj_gantry");
      await sleep(20);
      expect(await adapter.uiStateList()).toEqual({});

      adapter.release();
      await reset;
      expect(controller.getSnapshot().identity?.state).toBe("unpaired");
      expect(controller.getSnapshot().connection?.phase).toBe("unpaired");
      expect(await adapter.uiStateList()).toEqual({});
    } finally {
      await controller.stop();
    }
  });

  it("重置失败后原身份状态与持久化继续工作", async () => {
    const adapter = new HeldResetAdapter("default");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      await waitFor(() => expect(controller.getSnapshot().status).toBe("ready"));
      adapter.failReset = true;
      await expect(controller.resetIdentity()).rejects.toThrow("模拟重置失败");
      expect(controller.getSnapshot().identity?.state).toBe("paired");

      controller.updateUi({ draft: "失败后草稿" }, "prj_gantry");
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.draft).toBe("失败后草稿");
      });
    } finally {
      await controller.stop();
    }
  });

  it.each([
    { action: "none", outbox: [], kept: 0 },
    { action: "keep", outbox: ["旧身份队列"], kept: 1 },
    { action: "discard", outbox: ["旧身份队列"], kept: 0 },
  ] as const)(
    "重置后重新配对不编辑草稿即停止：只保留内存旧队列（$action），新控制器不恢复旧界面",
    async ({ action, outbox, kept }) => {
      const { adapter, controller } = await startController("default");
      const post = vi.spyOn(adapter, "api");
      const options = {
        refreshDelayMs: 0,
        persistDelayMs: 0,
        prefetchWorkflows: false,
      };
      let fresh: DesktopController | null = null;
      try {
        controller.updateUi(
          {
            draft: "旧身份草稿",
            view: "map",
            ctx: { kind: "pr", repo: "acme/web", number: 1 },
            chatScroll: {
              atBottom: false,
              anchor_message_id: "msg_old",
              offset: 120,
            },
            outbox: outbox.map((text) => queuedItem(text)),
          },
          "prj_gantry",
        );
        adapter.setConnection("revoked");
        await waitFor(() =>
          expect(controller.getSnapshot().status).toBe("unpaired"),
        );
        await controller.resetIdentity();
        expect(await adapter.uiStateList()).toEqual({});

        const reset = controller.getSnapshot().ui.prj_gantry;
        expect(reset.draft).toBe("");
        expect(reset.view).toBe("chat");
        expect(reset.ctx).toBeNull();
        expect(reset.chatScroll).toEqual({
          atBottom: true,
          anchor_message_id: null,
          offset: 0,
        });
        expect(reset.outbox).toHaveLength(outbox.length);

        await adapter.pairingBegin({
          pairingString: "gantry+pair://example",
          deviceName: "测试 Mac",
        });
        await adapter.pairingConfirm({ matches: true });
        await controller.refresh();
        expect(controller.getSnapshot().status).toBe("ready");
        if (action === "discard") {
          expect(controller.discardStaleOutbox()).toBe(outbox.length);
        }
        expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(kept);

        await controller.stop();
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.draft).toBe("");
        expect(stored.prj_gantry?.view).toBe("chat");
        expect(stored.prj_gantry?.ctx).toBeNull();
        expect(stored.prj_gantry?.outbox).toEqual([]);

        fresh = new DesktopController(adapter, options);
        await fresh.start();
        await waitFor(() => expect(fresh?.getSnapshot().status).toBe("ready"));
        await sleep(20);
        expect(
          post.mock.calls.filter(([route]) => route === "messages.post"),
        ).toHaveLength(0);
        const restored = fresh.getSnapshot().ui.prj_gantry;
        expect(restored.draft).toBe("");
        expect(restored.view).toBe("chat");
        expect(restored.ctx).toBeNull();
        expect(restored.outbox).toHaveLength(0);
      } finally {
        if (fresh) await fresh.stop();
        else await controller.stop();
      }
    },
  );

  it("connectionState 回读失败时，已成功的重置仍清空界面且不回写", async () => {
    const adapter = new HeldResetAdapter("default");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    await controller.start();
    try {
      await waitFor(() => expect(controller.getSnapshot().status).toBe("ready"));
      controller.updateUi(
        { draft: "旧草稿", outbox: [queuedItem("旧队列")] },
        "prj_gantry",
      );
      await waitFor(async () => {
        const stored = await adapter.uiStateList();
        expect(stored.prj_gantry?.draft).toBe("旧草稿");
      });

      adapter.failConnectionState = true;
      await expect(controller.resetIdentity()).rejects.toThrow(
        "模拟连接读取失败",
      );
      expect(controller.getSnapshot().identity?.state).toBe("unpaired");
      expect(controller.getSnapshot().ui.prj_gantry.draft).toBe("");
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1);
      expect(await adapter.uiStateList()).toEqual({});
    } finally {
      await controller.stop();
    }
  });
});
describe("F4 live 恢复冲刷后台项目队列", () => {
  it("重同步后恢复连接：先加载对话，再无需访问 B 恰好发送一次", async () => {
    const { adapter, controller } = await startController("default");
    try {
      controller.selectProject("prj_specgit");
      await waitFor(() =>
        expect(
          controller.getSnapshot().projectData.prj_specgit?.messages.length,
        ).toBe(1),
      );

      adapter.setConnection("reconnecting");
      controller.updateUi({ draft: "后台恢复" }, "prj_specgit");
      controller.send();
      expect(controller.getSnapshot().ui.prj_specgit.outbox).toHaveLength(1);

      controller.selectProject("prj_gantry");
      await waitFor(() =>
        expect(controller.getSnapshot().currentProjectId).toBe("prj_gantry"),
      );

      adapter.emitFrame({ t: "resync_required", reason: "cursor_ahead" });
      await waitForResync(controller);
      expect(controller.getSnapshot().projectData.prj_specgit).toBeUndefined();

      const post = vi.spyOn(adapter, "api");
      adapter.setConnection("live");
      await waitFor(
        () =>
          expect(controller.getSnapshot().ui.prj_specgit.outbox).toHaveLength(0),
        { timeout: 3000 },
      );

      const posted = post.mock.calls.filter(([route]) => route === "messages.post");
      expect(posted).toHaveLength(1);
      expect(controller.getSnapshot().currentProjectId).toBe("prj_gantry");
      expect(
        controller.getSnapshot().projectData.prj_specgit?.messages.some(
          (message) => message.text === "后台恢复",
        ),
      ).toBe(true);
    } finally {
      await controller.stop();
    }
  });

  it("项目不存在时队列有明确失败结果，不静默卡住", async () => {
    const { adapter, controller } = await startController("default");
    try {
      controller.updateUi({ outbox: [queuedItem("已删除项目")] }, "prj_missing");
      await controller.flushOutbox("prj_missing");

      const item = controller.getSnapshot().ui.prj_missing.outbox[0];
      expect(item).toMatchObject({
        text: "已删除项目",
        state: "failed",
        error_code: "not_found",
      });
      expect(controller.getSnapshot().notice).toContain("资源不存在");
      const posted = vi
        .spyOn(adapter, "api")
        .mock.calls.filter(([route]) => route === "messages.post");
      expect(posted).toHaveLength(0);
    } finally {
      await controller.stop();
    }
  });
});

class HeldProjectSettingsAdapter extends ScenarioIpcAdapter {
  armed = false;
  started = false;
  private releaseRead: (() => void) | null = null;

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    const result = await super.api(route, args);
    if (route === "project_settings.get" && this.armed) {
      this.armed = false;
      this.started = true;
      await new Promise<void>((resolve) => {
        this.releaseRead = resolve;
      });
    }
    return result;
  }

  release(): void {
    this.releaseRead?.();
  }
}

class HeldMessagesAdapter extends ScenarioIpcAdapter {
  holdNext = 0;
  started = 0;
  private readonly releases: Array<() => void> = [];

  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    const result = await super.api(route, args);
    if (route === "messages.list" && this.holdNext > 0) {
      this.holdNext -= 1;
      this.started += 1;
      await new Promise<void>((resolve) => {
        this.releases.push(resolve);
      });
    }
    return result;
  }

  releaseNext(): void {
    this.releases.shift()?.();
  }

  releaseAll(): void {
    for (const release of this.releases.splice(0)) release();
  }
}

const controllerOptions = {
  refreshDelayMs: 0,
  persistDelayMs: 0,
  prefetchWorkflows: false,
};

describe("R1 快照提交保留读取期间确认的消息", () => {
  it("项目设置读取挂起时发送成功的消息不被较早快照覆盖", async () => {
    const adapter = new HeldProjectSettingsAdapter("default");
    const controller = new DesktopController(adapter, controllerOptions);
    await controller.start();
    try {
      const data = controller.getSnapshot().projectData.prj_gantry;
      const conversation = data?.conversation;
      expect(conversation).toBeTruthy();
      const before = data?.messages.length ?? 0;
      const text = "设置读取期间发送";
      const post = vi.spyOn(adapter, "api");

      adapter.armed = true;
      const refresh = controller.refresh();
      await waitFor(() => expect(adapter.started).toBe(true));

      controller.updateUi({ draft: text });
      controller.send();
      await waitFor(() => {
        expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
        expect(
          controller
            .getSnapshot()
            .projectData.prj_gantry.messages.filter(
              (message) => message.text === text,
            ),
        ).toHaveLength(1);
      });
      expect(controller.getSnapshot().projectData.prj_gantry.messages).toHaveLength(
        before + 1,
      );

      adapter.release();
      await refresh;
      await sleep(0);

      const visible = () =>
        controller
          .getSnapshot()
          .projectData.prj_gantry.messages.filter(
            (message) => message.text === text,
          );
      expect(visible()).toHaveLength(1);
      expect(controller.getSnapshot().projectData.prj_gantry.messages).toHaveLength(
        before + 1,
      );
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);

      const posted = post.mock.calls.filter(
        ([route]) => route === "messages.post",
      );
      expect(posted).toHaveLength(1);

      const stored = await adapter.api("messages.list", {
        conversationId: conversation!.id,
        limit: 50,
        order: "desc",
      });
      expect(
        stored.ok &&
          stored.body?.items.filter((message) => message.text === text),
      ).toHaveLength(1);

      await controller.refresh();
      expect(visible()).toHaveLength(1);
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    } finally {
      adapter.release();
      await controller.stop();
    }
  });

  it("事件刷新读取期间的发送结果不被较早列表覆盖", async () => {
    const adapter = new HeldMessagesAdapter("default");
    const controller = new DesktopController(adapter, controllerOptions);
    await controller.start();
    try {
      const before =
        controller.getSnapshot().projectData.prj_gantry.messages.length;
      const text = "事件刷新期间发送";
      adapter.holdNext = 2;
      adapter.emitFrame({
        t: "event",
        envelope: event(5000, "message.posted", "msg_refresh_race"),
      });
      await waitFor(() => expect(adapter.started).toBe(1));

      controller.updateUi({ draft: text });
      controller.send();
      await waitFor(() => {
        expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
        expect(
          controller
            .getSnapshot()
            .projectData.prj_gantry.messages.filter(
              (message) => message.text === text,
            ),
        ).toHaveLength(1);
      });

      adapter.releaseNext();
      await waitFor(() => expect(adapter.started).toBe(2));

      const visible = () =>
        controller
          .getSnapshot()
          .projectData.prj_gantry.messages.filter(
            (message) => message.text === text,
          );
      expect(visible()).toHaveLength(1);
      expect(controller.getSnapshot().projectData.prj_gantry.messages).toHaveLength(
        before + 1,
      );

      adapter.releaseNext();
      await controller.flushRefreshes();
      await sleep(0);
      expect(visible()).toHaveLength(1);
      expect(controller.getSnapshot().projectData.prj_gantry.messages).toHaveLength(
        before + 1,
      );
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0);
    } finally {
      adapter.releaseAll();
      await controller.stop();
    }
  });
});

async function prepareSecondTerminal(
  adapter: ScenarioIpcAdapter,
  controller: DesktopController,
  phase: "revoked" | "cert_expired",
): Promise<{ oldText: string; newText: string; oldId: string }> {
  const oldText = `旧身份真实离线-${phase}`;
  adapter.setConnection("reconnecting");
  controller.updateUi({ draft: oldText });
  controller.send();
  await waitFor(() =>
    expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1),
  );
  const oldId = controller.getSnapshot().ui.prj_gantry.outbox[0].local_id;

  adapter.setConnection("revoked");
  await waitFor(() =>
    expect(controller.getSnapshot().status).toBe("unpaired"),
  );
  await controller.resetIdentity();
  await adapter.pairingBegin({
    pairingString: "gantry+pair://example",
    deviceName: `测试 Mac ${phase}`,
  });
  await adapter.pairingConfirm({ matches: true });
  await controller.refresh();
  expect(controller.getSnapshot().status).toBe("ready");
  expect(controller.staleOutboxCount()).toBe(1);

  const newText = `新身份真实离线-${phase}`;
  adapter.setConnection("reconnecting");
  controller.updateUi({ draft: newText });
  controller.send();
  await waitFor(() =>
    expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(2),
  );
  await waitFor(async () => {
    const stored = await adapter.uiStateList();
    expect(stored.prj_gantry?.outbox.map((item) => item.text)).toEqual([
      newText,
    ]);
  });

  adapter.setConnection(phase);
  await waitFor(() =>
    expect(controller.getSnapshot().status).toBe("unpaired"),
  );
  expect(
    controller.getSnapshot().ui.prj_gantry.outbox.map((item) => item.text),
  ).toEqual([oldText, newText]);
  return { oldText, newText, oldId };
}

describe("R2 再次终态不重新持久化已隔离的旧队列", () => {
  it.each(["revoked", "cert_expired"] as const)(
    "再次 %s 时只持久化新身份队列，新控制器不恢复也不发送旧项",
    async (phase) => {
      const adapter = new ScenarioIpcAdapter("default");
      const controller = new DesktopController(adapter, controllerOptions);
      let fresh: DesktopController | null = null;
      await controller.start();
      try {
        const { oldText, newText } = await prepareSecondTerminal(
          adapter,
          controller,
          phase,
        );
        const post = vi.spyOn(adapter, "api");
        await controller.stop();

        const persisted = (await adapter.uiStateList()).prj_gantry.outbox;
        expect(persisted.map((item) => item.text)).toEqual([newText]);
        expect(persisted.some((item) => item.text === oldText)).toBe(false);

        fresh = new DesktopController(adapter, controllerOptions);
        await fresh.start();
        await adapter.pairingBegin({
          pairingString: "gantry+pair://example",
          deviceName: `测试 Mac 重启 ${phase}`,
        });
        await adapter.pairingConfirm({ matches: true });
        await fresh.refresh();
        await waitFor(
          () =>
            expect(
              fresh!.getSnapshot().ui.prj_gantry.outbox,
            ).toHaveLength(0),
          { timeout: 3000 },
        );

        const posted = post.mock.calls
          .filter(([route]) => route === "messages.post")
          .map(
            ([, args]) =>
              (args as RouteArgs["messages.post"]).body?.text,
          );
        expect(posted).toEqual([newText]);
        expect(
          fresh.getSnapshot().projectData.prj_gantry.messages.filter(
            (message) => message.text === oldText,
          ),
        ).toHaveLength(0);
      } finally {
        if (fresh) await fresh.stop();
        else await controller.stop();
      }
    },
  );

  it.each(["revoked", "cert_expired"] as const)(
    "再次 %s 后内存旧项仍可显式重试且只发送一次",
    async (phase) => {
      const adapter = new ScenarioIpcAdapter("default");
      const controller = new DesktopController(adapter, controllerOptions);
      await controller.start();
      try {
        const { oldText, newText, oldId } = await prepareSecondTerminal(
          adapter,
          controller,
          phase,
        );
        const post = vi.spyOn(adapter, "api");

        adapter.setConnection("live");
        await controller.refresh();
        await waitFor(() =>
          expect(
            controller.getSnapshot().ui.prj_gantry.outbox.map(
              (item) => item.text,
            ),
          ).toEqual([oldText, newText]),
        );

        controller.retryOutbox(oldId);
        await waitFor(() =>
          expect(
            controller.getSnapshot().ui.prj_gantry.outbox.map(
              (item) => item.text,
            ),
          ).toEqual([newText]),
        );

        const oldPosts = post.mock.calls
          .filter(([route]) => route === "messages.post")
          .filter(
            ([, args]) =>
              (args as RouteArgs["messages.post"]).body?.text === oldText,
          );
        expect(oldPosts).toHaveLength(1);
        expect(
          controller
            .getSnapshot()
            .projectData.prj_gantry.messages.filter(
              (message) => message.text === oldText,
            ),
        ).toHaveLength(1);
      } finally {
        await controller.stop();
      }
    },
  );
});

describe("R1/R2 边界回归", () => {
  it("普通刷新采纳同 id 的服务端新表示，不补回返回列表缺失的基线旧项", async () => {
    const { adapter, controller } = await startController("default");
    try {
      const data = controller.getSnapshot().projectData.prj_gantry;
      const [first, second] = data.messages;
      expect(second).toBeTruthy();
      const updated = { ...first, text: "服务端新表示" };
      const original = adapter.api.bind(adapter);
      vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
        if (route === "messages.list") {
          return {
            ok: true,
            status: 200,
            headers: {},
            body: { items: [updated], next_cursor: "cursor_boundary" },
          } as never;
        }
        return original(route, args as never);
      });

      await controller.refresh();

      const next = controller.getSnapshot().projectData.prj_gantry;
      expect(next.messages.map((message) => message.id)).toEqual([first.id]);
      expect(next.messages[0].text).toBe("服务端新表示");
      expect(next.messages.some((message) => message.id === second.id)).toBe(
        false,
      );
      expect(next.messageCursor).toBe("cursor_boundary");
    } finally {
      await controller.stop();
    }
  });

  it("新 conversation 快照不混入读取期间旧 conversation 的发送结果", async () => {
    const adapter = new HeldProjectSettingsAdapter("default");
    const controller = new DesktopController(adapter, controllerOptions);
    await controller.start();
    try {
      const data = controller.getSnapshot().projectData.prj_gantry;
      const oldConversation = data.conversation!;
      const seed = data.messages[0];
      const newConversation = { ...oldConversation, id: "cnv_boundary_new" };
      const first = {
        ...seed,
        id: "msg_boundary_new_1",
        conversation_id: newConversation.id,
        text: "新对话消息 1",
      };
      const second = {
        ...seed,
        id: "msg_boundary_new_2",
        conversation_id: newConversation.id,
        text: "新对话消息 2",
      };
      const original = adapter.api.bind(adapter);
      const post = vi.spyOn(adapter, "api").mockImplementation(
        async (route, args) => {
          if (route === "conversations.list") {
            return {
              ok: true,
              status: 200,
              headers: {},
              body: { items: [newConversation] },
            } as never;
          }
          if (
            route === "messages.list" &&
            (args as RouteArgs["messages.list"]).conversationId ===
              newConversation.id
          ) {
            return {
              ok: true,
              status: 200,
              headers: {},
              body: { items: [second, first], next_cursor: "cursor_new" },
            } as never;
          }
          return original(route, args as never);
        },
      );

      adapter.armed = true;
      const refresh = controller.refresh();
      await waitFor(() => expect(adapter.started).toBe(true));

      const text = "旧对话读取期间发送";
      controller.updateUi({ draft: text });
      controller.send();
      await waitFor(() =>
        expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(0),
      );
      expect(
        controller
          .getSnapshot()
          .projectData.prj_gantry.messages.some(
            (message) => message.text === text,
          ),
      ).toBe(true);

      adapter.release();
      await refresh;
      await sleep(0);

      const next = controller.getSnapshot().projectData.prj_gantry;
      expect(next.conversation?.id).toBe(newConversation.id);
      expect(next.messages.map((message) => message.id)).toEqual([
        first.id,
        second.id,
      ]);
      expect(next.messageCursor).toBe("cursor_new");
      expect(next.messages.some((message) => message.text === text)).toBe(false);
      const posted = post.mock.calls.filter(
        ([route]) => route === "messages.post",
      );
      expect(posted).toHaveLength(1);
      expect(
        (posted[0][1] as RouteArgs["messages.post"]).conversationId,
      ).toBe(oldConversation.id);
    } finally {
      adapter.release();
      await controller.stop();
    }
  });

  it("重同步清空缓存后不复活旧 log_epoch 的消息", async () => {
    const { adapter, controller } = await startController("default");
    try {
      const seed = controller.getSnapshot().projectData.prj_gantry.messages[0];
      const marker = {
        ...seed,
        id: "msg_boundary_old_epoch",
        text: "旧 log_epoch 消息",
      };
      const original = adapter.api.bind(adapter);
      const api = vi.spyOn(adapter, "api").mockImplementation(
        async (route, args) => {
          if (route === "messages.list") {
            const result = await original(route, args as never);
            if (result.ok && result.body) {
              return {
                ...result,
                headers: { ...result.headers, x_event_seq: 7000 },
                body: {
                  ...result.body,
                  items: [marker, ...result.body.items],
                },
              } as never;
            }
          }
          return original(route, args as never);
        },
      );

      await controller.refresh();
      expect(
        controller
          .getSnapshot()
          .projectData.prj_gantry.messages.some(
            (message) => message.id === marker.id,
          ),
      ).toBe(true);
      expect(controller.watermarkFor("messages:prj_gantry")).toBe(7000);
      api.mockRestore();

      adapter.emitFrame({
        t: "resync_required",
        reason: "log_epoch_changed",
      });
      await waitForResync(controller);

      const next = controller.getSnapshot();
      expect(next.lastResyncReason).toBe("log_epoch_changed");
      expect(
        next.projectData.prj_gantry.messages.some(
          (message) => message.id === marker.id,
        ),
      ).toBe(false);
      expect(controller.watermarkFor("messages:prj_gantry")).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it.each(["revoked", "cert_expired"] as const)(
    "刷新期间当前流推送 %s 终态：立即未配对，重置配对后旧快照被丢弃",
    async (phase) => {
      const adapter = new HeldProjectSettingsAdapter("default");
      const controller = new DesktopController(adapter, controllerOptions);
      const listeners: Array<(frame: StreamFrame) => void> = [];
      const open = adapter.eventsOpen.bind(adapter);
      vi.spyOn(adapter, "eventsOpen").mockImplementation(
        async (args, listener) => {
          listeners.push(listener);
          return open(args, listener);
        },
      );
      await controller.start();
      try {
        const oldText = `旧身份离线-${phase}`;
        adapter.setConnection("reconnecting");
        controller.updateUi({ draft: oldText });
        controller.send();
        await waitFor(() =>
          expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1),
        );
        expect(listeners).toHaveLength(1);

        adapter.armed = true;
        const staleRefresh = controller.refresh();
        await waitFor(() => expect(adapter.started).toBe(true));

        const api = vi.spyOn(adapter, "api");
        adapter.setConnection(phase);
        await waitFor(() =>
          expect(controller.getSnapshot().status).toBe("unpaired"),
        );
        expect(controller.getSnapshot().projects).toEqual([]);
        expect(controller.getSnapshot().projectData).toEqual({});
        expect(controller.getSnapshot().ui.prj_gantry.outbox[0]).toMatchObject(
          { text: oldText },
        );
        controller.send();
        await sleep(0);
        expect(
          api.mock.calls.filter(([route]) => route === "messages.post"),
        ).toHaveLength(0);

        await controller.resetIdentity();
        expect(controller.getSnapshot().identity?.state).toBe("unpaired");
        expect(await adapter.uiStateList()).toEqual({});

        await adapter.pairingBegin({
          pairingString: "gantry+pair://example",
          deviceName: `测试 Mac ${phase}`,
        });
        await adapter.pairingConfirm({ matches: true });
        await controller.refresh();
        expect(controller.getSnapshot().status).toBe("ready");
        expect(controller.staleOutboxCount()).toBe(1);

        const newText = `新身份确认-${phase}`;
        controller.updateUi({ draft: newText });
        controller.send();
        await waitFor(() =>
          expect(
            controller
              .getSnapshot()
              .projectData.prj_gantry.messages.filter(
                (message) => message.text === newText,
              ),
          ).toHaveLength(1),
        );
        expect(
          controller.getSnapshot().ui.prj_gantry.outbox.map(
            (item) => item.text,
          ),
        ).toEqual([oldText]);
        await waitFor(async () => {
          const stored = await adapter.uiStateList();
          expect(stored.prj_gantry?.outbox ?? []).toHaveLength(0);
        });
        const before = JSON.stringify(
          controller.getSnapshot().projectData.prj_gantry,
        );

        adapter.release();
        await staleRefresh;
        await sleep(0);
        expect(
          JSON.stringify(controller.getSnapshot().projectData.prj_gantry),
        ).toBe(before);
        expect(controller.getSnapshot().status).toBe("ready");
        expect(
          controller.getSnapshot().ui.prj_gantry.outbox.map(
            (item) => item.text,
          ),
        ).toEqual([oldText]);

        listeners[0]({
          t: "state",
          state: { ...(await adapter.connectionState()), phase },
        });
        await sleep(0);
        expect(controller.getSnapshot().status).toBe("ready");
        expect(controller.getSnapshot().connection?.phase).toBe("live");
        expect(
          JSON.stringify(controller.getSnapshot().projectData.prj_gantry),
        ).toBe(before);

        const posted = api.mock.calls
          .filter(([route]) => route === "messages.post")
          .map(
            ([, args]) =>
              (args as RouteArgs["messages.post"]).body?.text,
          );
        expect(posted).toEqual([newText]);
      } finally {
        adapter.release();
        await controller.stop();
      }
    },
  );
});
