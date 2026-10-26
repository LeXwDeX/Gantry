import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useSyncExternalStore, type ReactNode } from "react";
import { ApprovalSheet } from "@/features/overlays/Overlays";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { buildScenarioData } from "@/ipc/scenario/data";
import { approvalStatusLabel } from "@/store/sign";
import { approvalView, doneRows } from "@/features/workspace/model";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { startController } from "./helpers";

function Live({
  controller,
  children,
}: {
  controller: DesktopController;
  children: (state: DesktopState) => ReactNode;
}) {
  const state = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
  return <>{children(state)}</>;
}

afterEach(() => {
  vi.restoreAllMocks();
});

async function simulationDeliveries(
  adapter: ScenarioIpcAdapter,
  projectId: string,
): Promise<string[]> {
  const result = await adapter.api("deliveries.list", { projectId, limit: 200 });
  if (!result.ok || !result.body) return [];
  return result.body.items
    .filter((item) => item.id.startsWith("dlv_sim_"))
    .map((item) => item.id);
}

function explanationInProject(adapter: ScenarioIpcAdapter, projectId: string) {
  return adapter.api("messages.list", {
    conversationId: `cnv_${projectId.replace("prj_", "")}`,
    limit: 200,
    order: "asc",
  });
}

describe("F5 模拟运行冲突", () => {
  it("A 运行中在 B 发送只记录消息，A 继续完成且 B 不遗留新交付", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("task", { projectId: "prj_gantry" });
    expect(adapter.simulation.play().ok).toBe(true);
    adapter.simulation.advance(600);
    const running = adapter.simulation.getSnapshot();
    expect(running.status).toBe("running");
    expect(running.project_id).toBe("prj_gantry");
    const deliveryId = running.drafts[0]?.delivery_id;
    expect(deliveryId).toBeTruthy();

    const sent = await adapter.api("messages.post", {
      conversationId: "cnv_specgit",
      body: { text: "B 项目的新任务", refs: [] },
      idempotencyKey: "idem_sim_conflict_running",
    });
    expect(sent.ok).toBe(true);

    const afterPost = adapter.simulation.getSnapshot();
    expect(afterPost.project_id).toBe("prj_gantry");
    expect(afterPost.status).toBe("running");
    expect(afterPost.drafts[0]?.delivery_id).toBe(deliveryId);

    const bMessages = await explanationInProject(adapter, "prj_specgit");
    expect(
      bMessages.ok &&
        bMessages.body?.items.some((message) =>
          message.text.includes("未启动新的模拟运行"),
        ),
    ).toBe(true);
    expect(await simulationDeliveries(adapter, "prj_specgit")).toEqual([]);

    expect(adapter.simulation.advance(3_000).ok).toBe(true);
    const completed = adapter.simulation.getSnapshot();
    expect(completed.status).toBe("completed");
    expect(completed.project_id).toBe("prj_gantry");
    expect(completed.activities.every((entry) => entry.ended_at !== null)).toBe(true);
    expect(
      completed.activities.every((entry) => entry.delivery_id === deliveryId),
    ).toBe(true);

    const delivery = await adapter.api("deliveries.get", {
      deliveryId: deliveryId!,
    });
    expect(delivery.ok && delivery.body?.stage).toBe("closed");
    const workflows = await adapter.api("workflows.list", {
      deliveryId: deliveryId!,
    });
    expect(
      workflows.ok && workflows.body?.items[0]?.status,
    ).toBe("succeeded");
    const budget = await adapter.api("deliveries.budget", {
      deliveryId: deliveryId!,
    });
    const used = budget.ok
      ? (budget.body?.used as { tokens_by_tier?: { worker?: number } } | undefined)
      : undefined;
    expect(used?.tokens_by_tier?.worker).toBeGreaterThan(0);
    expect(await simulationDeliveries(adapter, "prj_specgit")).toEqual([]);
    expect(await simulationDeliveries(adapter, "prj_gantry")).toEqual([
      deliveryId,
    ]);
    adapter.simulation.reset();
  });

  it("A 同项目重新发送时先结算旧交付，再启动新运行", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("task", { projectId: "prj_gantry" });
    adapter.simulation.play();
    adapter.simulation.advance(1_000);
    const first = adapter.simulation.getSnapshot();
    expect(first.status).toBe("running");
    const oldDeliveryId = first.drafts[0]?.delivery_id;
    expect(oldDeliveryId).toBeTruthy();
    const oldNodeId = first.drafts[0]?.node_id;
    expect(oldNodeId).toBeTruthy();

    await adapter.api("messages.post", {
      conversationId: "cnv_gantry",
      body: { text: "同项目的新任务", refs: [] },
      idempotencyKey: "idem_sim_replaced_same_project",
    });
    const replaced = adapter.simulation.getSnapshot();
    expect(replaced.status).toBe("running");
    expect(replaced.drafts[0]?.delivery_id).not.toBe(oldDeliveryId);
    expect(
      replaced.activities.every((entry) => entry.delivery_id !== oldDeliveryId),
    ).toBe(true);

    const oldDelivery = await adapter.api("deliveries.get", {
      deliveryId: oldDeliveryId!,
    });
    expect(oldDelivery.ok && oldDelivery.body?.stage).toBe("abandoned");
    const oldWorkflow = await adapter.api("workflows.list", {
      deliveryId: oldDeliveryId!,
    });
    expect(oldWorkflow.ok && oldWorkflow.body?.items[0]?.status).toBe(
      "cancelled",
    );
    const oldAttempts = await adapter.api("attempts.list", {
      nodeId: oldNodeId!,
      order: "asc",
      limit: 100,
    });
    expect(
      oldAttempts.ok &&
        oldAttempts.body?.items.every((entry) => entry.status === "cancelled"),
    ).toBe(true);

    adapter.simulation.advance(3_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
    const ids = await simulationDeliveries(adapter, "prj_gantry");
    expect(ids).toHaveLength(2);
    const newDeliveryId = ids.find((id) => id !== oldDeliveryId)!;
    const newDelivery = await adapter.api("deliveries.get", {
      deliveryId: newDeliveryId,
    });
    expect(newDelivery.ok && newDelivery.body?.stage).toBe("closed");
    adapter.simulation.reset();
  });

  it("取消运行中的任务会结算交付、图、尝试和工具，不留迟到写入", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("task", { projectId: "prj_gantry" });
    adapter.simulation.play();
    adapter.simulation.advance(1_000);
    const running = adapter.simulation.getSnapshot();
    const deliveryId = running.drafts[0]?.delivery_id;
    const nodeId = running.drafts[0]?.node_id;
    expect(deliveryId).toBeTruthy();
    expect(nodeId).toBeTruthy();
    expect(running.activities.some((entry) => entry.status === "running")).toBe(
      true,
    );

    expect(adapter.simulation.cancel().ok).toBe(true);
    const cancelled = adapter.simulation.getSnapshot();
    expect(cancelled.status).toBe("cancelled");
    expect(
      cancelled.activities.every(
        (entry) => entry.status === "failed" && entry.ended_at !== null,
      ),
    ).toBe(true);

    const delivery = await adapter.api("deliveries.get", {
      deliveryId: deliveryId!,
    });
    expect(delivery.ok && delivery.body?.stage).toBe("abandoned");
    const workflow = await adapter.api("workflows.list", {
      deliveryId: deliveryId!,
    });
    expect(workflow.ok && workflow.body?.items[0]?.status).toBe("cancelled");
    const attempts = await adapter.api("attempts.list", {
      nodeId: nodeId!,
      order: "asc",
      limit: 100,
    });
    expect(
      attempts.ok &&
        attempts.body?.items.every((entry) => entry.status === "cancelled"),
    ).toBe(true);

    expect(adapter.simulation.advance(3_000).ok).toBe(false);
    expect(adapter.simulation.getSnapshot().status).toBe("cancelled");
    adapter.simulation.reset();
  });

  it("A 暂停时拒绝 B 的新运行，继续后完成同一交付", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("task", { projectId: "prj_gantry" });
    adapter.simulation.play();
    adapter.simulation.advance(300);
    adapter.simulation.pause();
    const deliveryId = adapter.simulation.getSnapshot().drafts[0]?.delivery_id;
    expect(adapter.simulation.getSnapshot().status).toBe("paused");

    await adapter.api("messages.post", {
      conversationId: "cnv_specgit",
      body: { text: "暂停期间的 B 消息", refs: [] },
      idempotencyKey: "idem_sim_conflict_paused",
    });
    expect(adapter.simulation.getSnapshot().status).toBe("paused");
    expect(await simulationDeliveries(adapter, "prj_specgit")).toEqual([]);

    adapter.simulation.resume();
    adapter.simulation.advance(3_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
    const delivery = await adapter.api("deliveries.get", {
      deliveryId: deliveryId!,
    });
    expect(delivery.ok && delivery.body?.stage).toBe("closed");
    adapter.simulation.reset();
  });

  it("A 等待重试时拒绝 B 的新运行，重试后完成且旧尝试不变", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("failure_retry", { projectId: "prj_gantry" });
    adapter.simulation.play();
    adapter.simulation.advance(2_000);
    const waiting = adapter.simulation.getSnapshot();
    expect(waiting.status).toBe("paused");
    expect(waiting.awaiting?.kind).toBe("retry");
    if (waiting.awaiting?.kind !== "retry") return;
    const nodeId = waiting.awaiting.node_id;
    const deliveryId = waiting.awaiting.delivery_id;

    await adapter.api("messages.post", {
      conversationId: "cnv_specgit",
      body: { text: "等待重试期间的 B 消息", refs: [] },
      idempotencyKey: "idem_sim_conflict_retry",
    });
    expect(adapter.simulation.getSnapshot().awaiting?.kind).toBe("retry");
    expect(await simulationDeliveries(adapter, "prj_specgit")).toEqual([]);

    const before = await adapter.api("attempts.list", {
      nodeId,
      order: "asc",
      limit: 100,
    });
    const failedId = before.ok ? before.body?.items[0]?.id : undefined;
    const retried = await adapter.api("nodes.retry", {
      nodeId,
      idempotencyKey: "idem_sim_conflict_retry_node",
    });
    expect(retried.ok).toBe(true);
    adapter.simulation.advance(3_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");

    const after = await adapter.api("attempts.list", {
      nodeId,
      order: "asc",
      limit: 100,
    });
    expect(after.ok && after.body?.items[0]?.id).toBe(failedId);
    expect(after.ok && after.body?.items[0]?.status).toBe("failed");
    const delivery = await adapter.api("deliveries.get", { deliveryId });
    expect(delivery.ok && delivery.body?.stage).toBe("closed");
    adapter.simulation.reset();
  });

  it("A 等待选择时拒绝 B 的新运行，选择与批准后完成", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("choice_approval", { projectId: "prj_gantry" });
    adapter.simulation.play();
    adapter.simulation.advance(1_000);
    const waiting = adapter.simulation.getSnapshot();
    expect(waiting.awaiting?.kind).toBe("choice");
    if (waiting.awaiting?.kind !== "choice") return;
    const choiceId = waiting.awaiting.inbox_id;
    const deliveryIds = await simulationDeliveries(adapter, "prj_gantry");

    await adapter.api("messages.post", {
      conversationId: "cnv_specgit",
      body: { text: "等待选择期间的 B 消息", refs: [] },
      idempotencyKey: "idem_sim_conflict_choice",
    });
    expect(adapter.simulation.getSnapshot().awaiting?.kind).toBe("choice");
    expect(await simulationDeliveries(adapter, "prj_gantry")).toEqual(deliveryIds);
    expect(await simulationDeliveries(adapter, "prj_specgit")).toEqual([]);

    const chosen = await adapter.api("inbox.choose", {
      inboxId: choiceId,
      optionId: "fix",
      idempotencyKey: "idem_sim_conflict_choice_fix",
    });
    expect(chosen.ok).toBe(true);
    const awaitingApproval = adapter.simulation.getSnapshot();
    expect(awaitingApproval.status).toBe("paused");
    expect(awaitingApproval.phase).toBe("await_approval");
    expect(awaitingApproval.awaiting?.kind).toBe("approval");
    expect(awaitingApproval.pending_approval_id).toBeTruthy();
    const approvals = await adapter.api("approvals.list", { status: "pending" });
    const approval = approvals.ok
      ? approvals.body?.items.find((entry) => entry.id.startsWith("apr_sim_"))
      : null;
    expect(approval).toBeTruthy();
    expect(adapter.simulation.simulateApproval(approval!.id).ok).toBe(true);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
    const delivery = await adapter.api("deliveries.get", {
      deliveryId: deliveryIds[0],
    });
    expect(delivery.ok && delivery.body?.stage).toBe("closed");
    adapter.simulation.reset();
  });

  it("选择 keep 停止本次演示：不进入审批，不遗留待审批入口与 open 收件箱", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("choice_approval", { projectId: "prj_gantry" });
    expect(adapter.simulation.play().ok).toBe(true);
    adapter.simulation.advance(1_000);
    const waiting = adapter.simulation.getSnapshot();
    expect(waiting.awaiting?.kind).toBe("choice");
    if (waiting.awaiting?.kind !== "choice") return;
    const choiceId = waiting.awaiting.inbox_id;

    const pendingBefore = await adapter.api("approvals.list", {
      status: "pending",
      limit: 200,
    });
    const pendingApproval = pendingBefore.ok
      ? pendingBefore.body?.items.find((item) => item.id.startsWith("apr_sim_"))
      : undefined;
    expect(pendingApproval).toBeTruthy();
    const deliveryId = pendingApproval!.delivery_id!;
    const workflowsBefore = await adapter.api("workflows.list", { deliveryId });
    expect(workflowsBefore.ok).toBe(true);
    const workflowId = workflowsBefore.ok
      ? workflowsBefore.body?.items[0]?.id
      : undefined;
    expect(workflowId).toBeTruthy();
    const graphBefore = await adapter.api("workflows.get", {
      workflowId: workflowId!,
    });
    const nodeIds = graphBefore.ok
      ? (graphBefore.body?.nodes ?? []).map((node) => node.id)
      : [];
    expect(nodeIds.length).toBeGreaterThan(0);

    const chosen = await adapter.api("inbox.choose", {
      inboxId: choiceId,
      optionId: "keep",
      idempotencyKey: "idem_sim_choice_keep_stop",
    });
    expect(chosen.ok).toBe(true);

    const stopped = adapter.simulation.getSnapshot();
    expect(stopped.status).toBe("cancelled");
    expect(stopped.phase).toBe("cancelled");
    expect(stopped.awaiting).toBeNull();
    expect(stopped.pending_approval_id).toBeNull();
    expect(stopped.activities.every((entry) => entry.ended_at !== null)).toBe(
      true,
    );

    const openInbox = await adapter.api("inbox.list", { status: "open", limit: 200 });
    expect(
      openInbox.ok && openInbox.body?.items.some((item) => item.id === choiceId),
    ).toBe(false);
    expect(
      openInbox.ok &&
        openInbox.body?.items.some(
          (item) => item.delivery_id === deliveryId && item.kind === "approval_needed",
        ),
    ).toBe(false);
    const resolved = await adapter.api("inbox.get", { inboxId: choiceId });
    expect(resolved.ok && resolved.body?.status).toBe("resolved");

    const pendingAfter = await adapter.api("approvals.list", {
      status: "pending",
      limit: 200,
    });
    expect(
      pendingAfter.ok &&
        pendingAfter.body?.items.some((item) => item.id === pendingApproval!.id),
    ).toBe(false);
    const gone = await adapter.api("approvals.get", {
      approvalId: pendingApproval!.id,
    });
    expect(gone.ok).toBe(false);

    const delivery = await adapter.api("deliveries.get", { deliveryId });
    expect(delivery.ok && delivery.body?.stage).toBe("abandoned");
    const workflows = await adapter.api("workflows.list", { deliveryId });
    expect(workflows.ok && workflows.body?.items[0]?.status).toBe("cancelled");
    const graphAfter = await adapter.api("workflows.get", {
      workflowId: workflowId!,
    });
    expect(
      graphAfter.ok &&
        (graphAfter.body?.nodes ?? []).every(
          (node) => node.status === "succeeded" || node.status === "cancelled",
        ),
    ).toBe(true);
    const budget = await adapter.api("deliveries.budget", { deliveryId });
    expect(budget.ok && budget.body?.used.nodes).toBe(nodeIds.length);
    const attempts = await adapter.api("attempts.list", {
      nodeId: nodeIds[0],
      order: "asc",
      limit: 100,
    });
    expect(
      attempts.ok && attempts.body?.items.every((entry) => entry.status === "cancelled"),
    ).toBe(true);

    const messages = await explanationInProject(adapter, "prj_gantry");
    expect(
      messages.ok &&
        messages.body?.items.some(
          (message) =>
            message.role === "agent" &&
            message.text.includes("停止演示") &&
            message.text.includes("已取消"),
        ),
    ).toBe(true);

    // 迟到推进与旧审批回调都不能让已停止的演示继续。
    expect(adapter.simulation.advance(3_000).ok).toBe(false);
    expect(adapter.simulation.simulateApproval(pendingApproval!.id).ok).toBe(false);
    expect(adapter.simulation.getSnapshot()).toMatchObject({
      status: "cancelled",
      phase: "cancelled",
      awaiting: null,
      pending_approval_id: null,
    });
    const lateApprovals = await adapter.api("approvals.list", {
      status: "pending",
      limit: 200,
    });
    expect(
      lateApprovals.ok &&
        lateApprovals.body?.items.some((item) => item.delivery_id === deliveryId),
    ).toBe(false);
    const lateInbox = await adapter.api("inbox.list", { status: "open", limit: 200 });
    expect(
      lateInbox.ok &&
        lateInbox.body?.items.some(
          (item) => item.delivery_id === deliveryId && item.kind === "approval_needed",
        ),
    ).toBe(false);
    adapter.simulation.reset();
  });

  it("keep 停止后控制器不缓存预创建审批，也没有待审批入口", async () => {
    const { controller, adapter } = await startController();
    try {
      adapter.simulation.select("choice_approval", { projectId: "prj_gantry" });
      expect(adapter.simulation.play().ok).toBe(true);
      adapter.simulation.advance(1_000);
      const waiting = adapter.simulation.getSnapshot();
      expect(waiting.awaiting?.kind).toBe("choice");
      if (waiting.awaiting?.kind !== "choice") return;
      const pending = await adapter.api("approvals.list", {
        status: "pending",
        limit: 200,
      });
      const approval = pending.ok
        ? pending.body?.items.find((item) => item.id.startsWith("apr_sim_"))
        : undefined;
      expect(approval).toBeTruthy();
      const deliveryId = approval!.delivery_id!;

      // 现实刷新不会拉取没有 approval.requested 的预创建审批。
      await controller.refresh();
      expect(controller.getSnapshot().approvals[approval!.id]).toBeUndefined();

      const chosen = await adapter.api("inbox.choose", {
        inboxId: waiting.awaiting.inbox_id,
        optionId: "keep",
        idempotencyKey: "idem_sim_keep_controller_cache",
      });
      expect(chosen.ok).toBe(true);
      await waitFor(() => {
        expect(
          controller.getSnapshot().inbox.some(
            (item) =>
              item.delivery_id === deliveryId && item.kind === "approval_needed",
          ),
        ).toBe(false);
      });
      expect(controller.getSnapshot().approvals[approval!.id]).toBeUndefined();
    } finally {
      await controller.stop();
    }
  });
});

describe("F6 审批结果文案与事实一致", () => {
  it("granted 不再仅凭状态断言已签名或排队 merge", () => {
    const data = buildScenarioData("default");
    const approval = data.approvals[0];
    const unsigned = approvalStatusLabel({ ...approval, status: "granted" });
    expect(unsigned.label).toContain("已批准");
    expect(unsigned.label).not.toContain("已签名");
    expect(unsigned.label).not.toContain("排队");

    const signed = { ...approval, status: "granted" as const, signature: "sig" };
    const waiting = approvalStatusLabel(signed, {
      deliveryStage: "merge",
      prState: "open",
    });
    expect(waiting.label).toBe("已签名 · 等待合并");
    expect(waiting.label).not.toContain("排队");

    const closed = approvalStatusLabel(signed, { deliveryStage: "closed" });
    expect(closed.label).toContain("交付已关闭");
    expect(closed.label).not.toContain("排队");

    const abandoned = approvalStatusLabel(signed, {
      deliveryStage: "abandoned",
    });
    expect(abandoned.label).toContain("交付已放弃");
    expect(abandoned.label).not.toContain("已签名");
  });

  it("denied 在交付已放弃时不再声称回到评审", () => {
    const data = buildScenarioData("default");
    const approval = data.approvals[0];
    const denied = approvalStatusLabel(
      { ...approval, status: "denied" },
      { deliveryStage: "abandoned" },
    );
    expect(denied.label).toContain("已拒绝");
    expect(denied.label).toContain("已放弃");
    expect(denied.label).not.toContain("回到评审");

    const reviewing = approvalStatusLabel(
      { ...approval, status: "denied" },
      { deliveryStage: "merge" },
    );
    expect(reviewing.label).toContain("回到评审");
  });

  it("模拟批准后审批单、检视器与最近完成区不再出现已签名或排队矛盾", async () => {
    const { controller, adapter } = await startController();
    const sign = vi.spyOn(adapter, "approvalSign");
    try {
      render(
        <Live controller={controller}>
          {(state) => (
            <ApprovalSheet
              controller={controller}
              state={state}
              approvalId="apr_gantry_9"
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      fireEvent.click(
        await screen.findByRole("button", { name: "模拟批准（不生成签名）" }),
      );
      expect(
        await screen.findByText(/模拟批准已应用：未生成 signature/),
      ).toBeInTheDocument();
      expect(
        (await screen.findAllByText(/已批准 · 交付已关闭/)).length,
      ).toBeGreaterThan(0);
      const sheetText = document.body.textContent ?? "";
      expect(sheetText).not.toContain("已签名 · 排队 merge");
      expect(sheetText).not.toContain("排队 merge");
      expect(sheetText).not.toContain("签名句柄已使用");
      expect(sheetText).not.toContain("已签名信息");
      expect(sheetText).not.toContain("技术细节（已签名）");

      await waitFor(() => {
        const delivery = controller
          .getSnapshot()
          .projectData.prj_gantry.deliveries.find(
            (item) => item.id === "dlv_gantry_9",
          );
        expect(delivery?.stage).toBe("closed");
      });
      const state = controller.getSnapshot();
      expect(state.approvals.apr_gantry_9.signature).toBeNull();
      const delivery = state.projectData.prj_gantry.deliveries.find(
        (item) => item.id === "dlv_gantry_9",
      )!;
      const view = approvalView(
        state.approvals,
        state.inbox,
        "apr_gantry_9",
        delivery,
      );
      expect(view.label).toContain("已批准");
      expect(view.label).not.toContain("已签名");
      expect(view.label).not.toContain("排队");
      const row = doneRows(state.projectData.prj_gantry).find(
        (entry) => entry.delivery.id === "dlv_gantry_9",
      );
      expect(row?.stateLabel).toBe("已关闭");
      expect(sign).not.toHaveBeenCalled();

      fireEvent.click(screen.getByRole("button", { name: "重新载入" }));
      await waitFor(() => {
        expect(screen.getByText("待签名载荷（模拟，未生成签名）")).toBeInTheDocument();
      });
      const reloaded = document.body.textContent ?? "";
      expect(reloaded).not.toContain("已签名信息");
      expect(reloaded).not.toContain("技术细节（已签名）");
      expect(reloaded).not.toContain("已签名 · 排队 merge");
      expect(reloaded).not.toContain("签名句柄已使用");
    } finally {
      await controller.stop();
    }
  });

  it("模拟拒绝后审批单与交付终态一致，不出现回到评审", async () => {
    const { controller, adapter } = await startController();
    const sign = vi.spyOn(adapter, "approvalSign");
    try {
      render(
        <Live controller={controller}>
          {(state) => (
            <ApprovalSheet
              controller={controller}
              state={state}
              approvalId="apr_gantry_9"
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      fireEvent.click(await screen.findByRole("button", { name: "模拟拒绝" }));
      expect(
        await screen.findByText(/模拟拒绝已应用：未生成 signature/),
      ).toBeInTheDocument();
      expect(
        (await screen.findAllByText(/已拒绝 · 交付已放弃/)).length,
      ).toBeGreaterThan(0);
      const text = document.body.textContent ?? "";
      expect(text).not.toContain("已拒绝，回到评审");
      expect(text).not.toContain("已签名 · 排队 merge");
      expect(text).not.toContain("签名句柄已使用");
      expect(text).not.toContain("已签名信息");

      await waitFor(() => {
        const delivery = controller
          .getSnapshot()
          .projectData.prj_gantry.deliveries.find(
            (item) => item.id === "dlv_gantry_9",
          );
        expect(delivery?.stage).toBe("abandoned");
      });
      const state = controller.getSnapshot();
      expect(state.approvals.apr_gantry_9.signature).toBeNull();
      const delivery = state.projectData.prj_gantry.deliveries.find(
        (item) => item.id === "dlv_gantry_9",
      )!;
      const view = approvalView(
        state.approvals,
        state.inbox,
        "apr_gantry_9",
        delivery,
      );
      expect(view.label).toContain("已拒绝");
      expect(view.label).not.toContain("回到评审");
      const row = doneRows(state.projectData.prj_gantry).find(
        (entry) => entry.delivery.id === "dlv_gantry_9",
      );
      expect(row?.stateLabel).toBe("已放弃");
      expect(sign).not.toHaveBeenCalled();
    } finally {
      await controller.stop();
    }
  });
});
