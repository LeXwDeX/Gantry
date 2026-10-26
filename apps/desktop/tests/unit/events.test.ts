import { describe, expect, it, vi } from "vitest";
import { waitFor } from "@testing-library/react";
import { isBelowWatermark, isSnapshotStale, minEventSeq } from "@/sync/watermark";
import { resourcesForEvent } from "@/sync/events";
import { needsYou } from "@/store/needsYou";
import { buildScenarioData } from "@/ipc/scenario/data";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import type { DesktopState } from "@/store/state";
import { sleep, startController, event, waitForResync } from "./helpers";

class HeldCloseAdapter extends ScenarioIpcAdapter {
  closeStarted = false;
  private release: (() => void) | null = null;
  private holdClose = true;

  override async eventsClose(): Promise<void> {
    if (!this.holdClose) return;
    this.holdClose = false;
    this.closeStarted = true;
    await new Promise<void>((resolve) => {
      this.release = resolve;
    });
  }

  releaseClose(): void {
    this.release?.();
  }
}

const lookup = {
  currentProjectId: "prj_gantry",
  deviceId: "dev_scenario_main",
  projectForDelivery: (id: string) =>
    id.startsWith("dlv_gantry") ? "prj_gantry" : "prj_specgit",
  deliveryForCorrelation: (id: string | null) =>
    id && id.startsWith("dlv_") ? id : null,
};

describe("水位线", () => {
  it("取快照最小值，忽略空值", () => {
    expect(minEventSeq([null, 12, 9, undefined])).toBe(9);
    expect(minEventSeq([])).toBeNull();
  });

  it("较旧的快照不得覆盖较新的缓存", () => {
    expect(isSnapshotStale(9, 12)).toBe(true);
    expect(isSnapshotStale(12, 12)).toBe(false);
    expect(isSnapshotStale(null, 12)).toBe(false);
    expect(isBelowWatermark(11, 12)).toBe(true);
    expect(isBelowWatermark(13, 12)).toBe(false);
  });
});

describe("事件到资源的映射", () => {
  it("消息、交付、收件箱与审批只作刷新提示", () => {
    expect(
      resourcesForEvent(event(1, "message.posted", "msg_1"), lookup),
    ).toEqual(["messages:prj_gantry"]);
    expect(
      resourcesForEvent(
        event(2, "delivery.stage_changed", "dlv_gantry_5"),
        lookup,
      ),
    ).toEqual(["deliveries:prj_gantry", "workflow:dlv_gantry_5"]);
    expect(
      resourcesForEvent(event(3, "inbox.raised", "inb_x"), lookup),
    ).toEqual(["inbox", "summary"]);
    expect(
      resourcesForEvent(event(4, "approval.granted", "apr_gantry_9"), lookup),
    ).toEqual(["inbox", "summary", "approvals", "approval:apr_gantry_9"]);
  });

  it("未知事件不刷新任何资源", () => {
    expect(
      resourcesForEvent(event(5, "future.unknown", "x"), lookup),
    ).toEqual([]);
  });
});

describe("needsYou 单一投影", () => {
  it("按项目与交付聚合，只数 open", () => {
    const data = buildScenarioData("default");
    const state = { inbox: data.inbox } as Pick<DesktopState, "inbox">;
    const projection = needsYou(state);
    expect(projection.total).toBe(4);
    expect(projection.byProject.get("prj_gantry")).toHaveLength(3);
    expect(projection.byProject.get("prj_specgit")).toHaveLength(1);
    expect(projection.byProject.get("prj_graphagent")).toBeUndefined();
    expect(projection.byDelivery.get("dlv_gantry_9")).toHaveLength(1);
    const oneClosed = data.inbox.map((item) =>
      item.id === "inb_esc6" ? { ...item, status: "resolved" as const } : item,
    );
    expect(needsYou({ inbox: oneClosed }).total).toBe(3);
  });
});

describe("事件应用、ack 与 resync", () => {
  it("stop 后不再打开事件流", async () => {
    const adapter = new HeldCloseAdapter("default");
    const { DesktopController } = await import("@/store/controller");
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    const open = vi.spyOn(adapter, "eventsOpen");
    const starting = controller.start();
    await waitFor(() => expect(adapter.closeStarted).toBe(true));
    const stopping = controller.stop();
    adapter.releaseClose();
    await Promise.all([starting, stopping]);
    expect(open).not.toHaveBeenCalled();
  });

  it("事件顺序推进游标，未知事件安全 ack", async () => {
    const { adapter, controller } = await startController();
    adapter.emitFrame({
      t: "event",
      envelope: event(5000, "message.posted", "msg_test"),
    });
    expect(controller.appliedEventSeq).toBe(5000);
    await waitFor(() => expect(adapter.ackedSeq).toBe(5000));

    adapter.emitFrame({
      t: "event",
      envelope: event(5001, "future.unknown", "future_1"),
    });
    expect(controller.appliedEventSeq).toBe(5001);
    await waitFor(() => expect(adapter.ackedSeq).toBe(5001));

    adapter.emitFrame({
      t: "event",
      envelope: event(4999, "message.posted", "msg_old"),
    });
    expect(controller.appliedEventSeq).toBe(5001);
    await controller.stop();
  });

  it("settings.updated 递增可观察计数", async () => {
    const { adapter, controller } = await startController();
    adapter.emitFrame({
      t: "event",
      envelope: event(5100, "settings.updated", "settings", null, {
        resource: "levels",
      }),
    });
    expect(controller.getSnapshot().settingsRevision).toBe(1);
    await controller.flushRefreshes();
    await controller.stop();
  });

  it("resync 清缓存但保留 UI 与待发送队列", async () => {
    const { adapter, controller } = await startController("offline");
    controller.updateUi({
      draft: "保留草稿",
      sel: { kind: "pr", repo: "LeXwDeX/gantry", number: 9 },
    });
    // 直接入队一条待发送项，模拟离线队列。
    const ui = controller.getSnapshot().ui.prj_gantry;
    controller.updateUi({
      draft: "",
      outbox: [
        ...ui.outbox,
        {
          local_id: "local_test",
          idempotency_key: "idem_test_1234567890",
          text: "待发送",
          refs: [],
          created_at: "2026-10-24T12:59:00.000Z",
          state: "queued",
        },
      ],
    });
    adapter.emitFrame({ t: "resync_required", reason: "cursor_ahead" });
    await waitForResync(controller);
    const snapshot = controller.getSnapshot();
    expect(snapshot.projectData.prj_gantry.messages.length).toBeGreaterThan(0);
    expect(snapshot.ui.prj_gantry.draft).toBe("");
    expect(snapshot.ui.prj_gantry.sel).toEqual({
      kind: "pr",
      repo: "LeXwDeX/gantry",
      number: 9,
    });
    expect(snapshot.ui.prj_gantry.outbox).toHaveLength(1);
    expect(snapshot.lastResyncReason).toBe("cursor_ahead");
    await controller.stop();
  });

  it.each(["revoked", "cert_expired"] as const)(
    "live → %s：停流、清实体缓存，保留界面状态与待发队列",
    async (phase) => {
      const { adapter, controller } = await startController();
      const close = vi.spyOn(adapter, "eventsClose");
      const persist = vi.spyOn(adapter, "uiStateSet");
      controller.updateUi({
        draft: "保留草稿",
        sel: { kind: "pr", repo: "LeXwDeX/gantry", number: 9 },
      });
      controller.updateUi({
        outbox: [
          {
            local_id: "local_terminal",
            idempotency_key: "idem_terminal_1234567890",
            text: "待发送",
            refs: [],
            created_at: "2026-10-24T12:59:00.000Z",
            state: "sending",
          },
        ],
      });
      expect(controller.getSnapshot().projects.length).toBeGreaterThan(0);

      adapter.setConnection(phase);

      const snapshot = controller.getSnapshot();
      expect(snapshot.status).toBe("unpaired");
      expect(snapshot.identity?.state).toBe("paired");
      expect(snapshot.connection?.phase).toBe(phase);
      expect(snapshot.me).toBeNull();
      expect(snapshot.projects).toEqual([]);
      expect(snapshot.currentProjectId).toBeNull();
      expect(snapshot.projectData).toEqual({});
      expect(snapshot.inbox).toEqual([]);
      expect(snapshot.inboxSummary).toBeNull();
      expect(snapshot.approvals).toEqual({});
      expect(snapshot.workflows).toEqual({});
      expect(snapshot.ui.prj_gantry.draft).toBe("保留草稿");
      expect(snapshot.ui.prj_gantry.sel).toEqual({
        kind: "pr",
        repo: "LeXwDeX/gantry",
        number: 9,
      });
      expect(snapshot.ui.prj_gantry.outbox).toHaveLength(1);
      expect(snapshot.ui.prj_gantry.outbox[0]).toMatchObject({
        text: "待发送",
        state: "queued",
      });
      await waitFor(() => expect(close).toHaveBeenCalled());

      const api = vi.spyOn(adapter, "api");
      await controller.flushRefreshes();
      await sleep(10);
      expect(api).not.toHaveBeenCalled();
      expect(persist).not.toHaveBeenCalled();
      await controller.stop();
    },
  );

  it("log_epoch 变化触发全量重同步并提示备份恢复", async () => {
    const { adapter, controller } = await startController();
    adapter.bumpLogEpoch();
    await waitForResync(controller);
    const snapshot = controller.getSnapshot();
    expect(snapshot.lastResyncReason).toBe("log_epoch_changed");
    expect(snapshot.notice).toContain("服务端已从备份恢复");
    expect(snapshot.projectData.prj_gantry.messages.length).toBeGreaterThan(0);
    await controller.stop();
  });
});
