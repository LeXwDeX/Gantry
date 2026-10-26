import { afterEach, describe, expect, it, vi } from "vitest";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { startController } from "./helpers";

function plain<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

async function modelValue(adapter: ScenarioIpcAdapter): Promise<Record<string, unknown>> {
  const result = await adapter.api("settings.get", { resource: "models" });
  if (!result.ok || !result.body) throw new Error("models missing");
  return plain(result.body.value) as Record<string, unknown>;
}

afterEach(() => {
  // 每个用例使用独立适配器；没有需要保留的共享计时器。
});

describe("模拟设置写入", () => {
  it("校验、ETag、内存回读，并清除测试凭据值", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const before = await adapter.api("settings.get", { resource: "models" });
    expect(before.ok).toBe(true);
    if (!before.ok || !before.body) return;
    const value = plain(before.body.value) as Record<string, unknown>;
    const providers = value.providers as Record<string, Record<string, unknown>>;
    const credential = providers.anthropic.credential as Record<string, unknown>;
    credential.value = "sk-test-placeholder-only";
    const worker = value.worker as Record<string, unknown>;
    worker.model = "claude-opus-4-1";

    const preview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value,
      ifMatch: before.headers.etag ?? '"18235"',
    });
    expect(preview.valid).toBe(true);
    expect(preview.can_submit).toEqual({ ok: true });
    expect(preview.requires_signature).toBe(true);
    expect(JSON.stringify(preview)).not.toContain("sk-test-placeholder-only");

    const submitted = await adapter.guardedWriteSubmit({
      pendingId: preview.pending_id,
    });
    expect(submitted.ok).toBe(true);
    if (!submitted.ok) return;
    expect(submitted.headers.etag).toBe('"18236"');
    expect(JSON.stringify(submitted.body)).not.toContain("sk-test-placeholder-only");
    const after = await adapter.api("settings.get", { resource: "models" });
    if (!after.ok || !after.body) throw new Error("models missing");
    const readCredential = (
      (after.body.value as Record<string, unknown>).providers as Record<
        string,
        Record<string, unknown>
      >
    ).anthropic.credential as Record<string, unknown>;
    expect(readCredential.value).toBeUndefined();
    expect(readCredential.present).toBe(true);
    const status = await adapter.api("settings.status", {});
    if (status.ok && status.body) {
      expect(status.body.latest_rev).toBe(18236);
      expect(status.body.modules.models?.applied_rev).toBe(18236);
    }

    const stalePreview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value,
      ifMatch: before.headers.etag ?? '"18235"',
    });
    expect(stalePreview.valid).toBe(false);
    expect(stalePreview.errors[0]?.code).toBe("precondition_failed");
  });

  it("模拟冲突在提交时返回 412，字段校验拒绝无工具模型", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const before = await adapter.api("settings.get", { resource: "models" });
    if (!before.ok || !before.body) return;
    const value = plain(before.body.value) as Record<string, unknown>;
    const preview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value,
      ifMatch: before.headers.etag ?? '"18235"',
    });
    adapter.simulation.injectError("settings_conflict");
    const conflict = await adapter.guardedWriteSubmit({ pendingId: preview.pending_id });
    expect(conflict.ok).toBe(false);
    if (!conflict.ok && "status" in conflict) expect(conflict.status).toBe(412);

    const invalid = plain(value);
    const worker = invalid.worker as Record<string, unknown>;
    worker.model = "claude-haiku-4-5";
    const nextPreview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: invalid,
      ifMatch: '"18236"',
    });
    expect(nextPreview.valid).toBe(false);
    expect(nextPreview.errors.some((entry) => entry.code === "worker_model_no_tools")).toBe(true);
  });

  it("项目覆盖读取、收紧校验、删除回退，以及 not_overridable", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const current = await adapter.api("project_settings.get", {
      projectId: "prj_gantry",
      resource: "models",
    });
    expect(current.ok).toBe(true);
    if (!current.ok || !current.body) return;
    const preview = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "project",
      projectId: "prj_gantry",
      resource: "models",
      value: { worker: { provider: "anthropic", model: "claude-opus-4-1" } },
      ifMatch: current.headers.etag ?? '"7"',
    });
    expect(preview.valid).toBe(true);
    const submitted = await adapter.guardedWriteSubmit({ pendingId: preview.pending_id });
    expect(submitted.ok).toBe(true);
    if (!submitted.ok) return;
    const effective = await adapter.api("project_settings.get", {
      projectId: "prj_gantry",
      resource: "models",
      view: "effective",
    });
    expect(effective.ok).toBe(true);
    if (effective.ok && effective.body) {
      const worker = (effective.body.value as Record<string, unknown>).worker as Record<
        string,
        unknown
      >;
      expect(worker.model).toBe("claude-opus-4-1");
    }

    const deleted = await adapter.guardedWritePrepare({
      kind: "settings_delete",
      projectId: "prj_gantry",
      resource: "models",
      ifMatch: submitted.headers.etag ?? '"8"',
    });
    expect(deleted.valid).toBe(true);
    const deleteResult = await adapter.guardedWriteSubmit({ pendingId: deleted.pending_id });
    expect(deleteResult.ok).toBe(true);
    if (deleteResult.ok) {
      expect((deleteResult.body as { value: unknown }).value).toBeNull();
      expect(deleteResult.headers.etag).toBe('"0"');
    }
    const fallback = await adapter.api("project_settings.get", {
      projectId: "prj_gantry",
      resource: "models",
      view: "effective",
    });
    if (fallback.ok && fallback.body) {
      const worker = (fallback.body.value as Record<string, unknown>).worker as Record<
        string,
        unknown
      >;
      expect(worker.model).toBe("claude-sonnet-5");
    }

    const egress = await adapter.api("project_settings.get", {
      projectId: "prj_gantry",
      resource: "egress",
    });
    if (!egress.ok || !egress.body) return;
    const loose = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "project",
      projectId: "prj_gantry",
      resource: "egress",
      value: { profiles: { default: { groups: ["model_api", "github"] } } },
      ifMatch: egress.headers.etag ?? '"2"',
    });
    expect(loose.valid).toBe(false);
    expect(loose.errors.some((entry) => entry.code === "override_loosens_global")).toBe(true);

    const notify = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "project",
      projectId: "prj_gantry",
      resource: "notify",
      value: {},
      ifMatch: '"0"',
    });
    expect(notify.valid).toBe(false);
    expect(notify.errors.some((entry) => entry.code === "not_overridable")).toBe(true);
  });

  it("六类项目覆盖可继承全局 effective，档位默认值按项目覆盖", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const levels = await adapter.api("project_settings.get", {
      projectId: "prj_specgit",
      resource: "levels",
      view: "effective",
    });
    expect(levels.ok).toBe(true);
    if (levels.ok && levels.body) {
      expect((levels.body.value as Record<string, unknown>).default_level).toBe("High");
      expect((levels.body.value as Record<string, unknown>).high).toBeTruthy();
    }
    for (const resource of ["policies", "guardian", "sandbox", "egress"]) {
      const override = await adapter.api("project_settings.get", {
        projectId: "prj_gantry",
        resource,
      });
      expect(override.ok).toBe(true);
      if (override.ok && override.body?.value !== null) {
        const effective = await adapter.api("project_settings.get", {
          projectId: "prj_gantry",
          resource,
          view: "effective",
        });
        expect(effective.ok).toBe(true);
      }
    }
  });

  it("设备角色与吊销受审批设备与自我保护限制", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const rolePreview = await adapter.guardedWritePrepare({
      kind: "device_role",
      deviceId: "dev_scenario_second",
      role: "viewer",
    });
    expect(rolePreview.can_submit).toEqual({ ok: true });
    expect(rolePreview.requires_signature).toBe(true);
    await adapter.guardedWriteSubmit({ pendingId: rolePreview.pending_id });
    const devices = await adapter.api("devices.list", { limit: 100 });
    if (devices.ok && devices.body) {
      expect(
        devices.body.items.find((device) => device.id === "dev_scenario_second")?.role,
      ).toBe("viewer");
    }

    const self = await adapter.guardedWritePrepare({
      kind: "device_role",
      deviceId: "dev_scenario_main",
      role: "viewer",
    });
    expect(self.valid).toBe(false);
    expect(self.errors[0]?.code).toBe("self_role_change");

    const revoke = await adapter.guardedWritePrepare({
      kind: "device_revoke",
      deviceId: "dev_scenario_second",
    });
    expect(revoke.can_submit).toEqual({ ok: true });
    await adapter.guardedWriteSubmit({ pendingId: revoke.pending_id });
    const after = await adapter.api("devices.list", { limit: 100 });
    if (after.ok && after.body) {
      expect(
        after.body.items.find((device) => device.id === "dev_scenario_second")?.status,
      ).toBe("revoked");
    }
  });
});

describe("模拟数据读取", () => {
  it("attempts 与 budget 返回真实模拟集合，而不是空值或 404", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const attempts = await adapter.api("attempts.list", {
      nodeId: "nod_gantry_5_build",
      order: "asc",
      limit: 100,
    });
    expect(attempts.ok).toBe(true);
    if (attempts.ok && attempts.body) {
      expect(attempts.body.items.map((entry) => entry.n)).toEqual([1, 2]);
      expect(attempts.body.items[0]?.status).toBe("failed");
      expect(attempts.body.items[1]?.status).toBe("running");
    }
    const one = await adapter.api("attempts.get", { attemptId: "att_gantry_7_sec_1" });
    expect(one.ok).toBe(true);

    const budget = await adapter.api("deliveries.budget", { deliveryId: "dlv_gantry_5" });
    expect(budget.ok).toBe(true);
    if (budget.ok && budget.body) {
      expect(budget.body.limits).toMatchObject({
        tokens_by_tier: expect.objectContaining({ worker: expect.any(Number) }),
        nodes: expect.any(Number),
        wall_time: expect.any(Number),
      });
      expect(budget.body.used).toMatchObject({
        tokens_by_tier: expect.objectContaining({ worker: expect.any(Number) }),
        attempts: expect.any(Number),
      });
    }
    const superseded = await adapter.api("workflows.get", {
      workflowId: "wfl_gantry_7_review",
      includeSuperseded: true,
    });
    if (superseded.ok && superseded.body) {
      expect(
        superseded.body.nodes?.some((node) => node.status === "superseded"),
      ).toBe(true);
    }
    const current = await adapter.api("workflows.get", {
      workflowId: "wfl_gantry_7_review",
    });
    if (current.ok && current.body) {
      expect(
        current.body.nodes?.some((node) => node.status === "superseded"),
      ).toBe(false);
    }
  });

  it("设置 catalog 含完整动作表、锁定分流类与出网建议", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const catalog = await adapter.api("settings.catalog", {});
    expect(catalog.ok).toBe(true);
    if (catalog.ok && catalog.body) {
      expect(Object.keys(catalog.body.policies?.actions ?? {})).toHaveLength(20);
      expect(catalog.body.guardian?.triage?.security_finding?.locked).toBe(true);
      expect(catalog.body.guardian?.triage?.security_finding?.value).toBe("escalate");
      expect(catalog.body.levels?.graph_templates?.high).toContain("high.multi_review");
      expect(
        (catalog.body.egress?.suggested_groups as Record<string, unknown>)?.model_api,
      ).toBeTruthy();
      expect(catalog.body.notify?.channels?.mobile_push?.available).toBe(false);
    }
  });

  it("messages.post 使用幂等键，重复请求不重复追加", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const args = {
      conversationId: "cnv_gantry",
      body: { text: "幂等测试消息", refs: [] },
      idempotencyKey: "idem_scenario_same_key",
    };
    const first = await adapter.api("messages.post", args);
    const second = await adapter.api("messages.post", args);
    expect(first.ok).toBe(true);
    expect(second.ok).toBe(true);
    if (first.ok && second.ok) {
      expect(first.body?.id).toBe(second.body?.id);
      expect(second.headers.idempotent_replayed).toBe(true);
    }
    const list = await adapter.api("messages.list", {
      conversationId: "cnv_gantry",
      limit: 200,
      order: "asc",
    });
    if (list.ok && list.body) {
      expect(list.body.items.filter((message) => message.text === "幂等测试消息")).toHaveLength(1);
    }
  });
});

describe("演示脚本与控制器", () => {
  it("多节点脚本完成同一交付，controller 缓存尝试与预算", async () => {
    const { adapter, controller } = await startController();
    try {
      adapter.simulation.select("task");
      const play = adapter.simulation.play();
      expect(play.ok).toBe(true);
      const advanced = adapter.simulation.advance(2_000);
      expect(advanced.ok).toBe(true);
      expect(adapter.simulation.getSnapshot().status).toBe("completed");

      await controller.flushRefreshes();
      const data = controller.getSnapshot();
      const delivery = data.projectData.prj_gantry.deliveries.find((entry) =>
        entry.id.startsWith("dlv_sim_"),
      );
      expect(delivery).toBeTruthy();
      if (!delivery) return;
      const workflow = controller.workflowForDelivery(delivery.id);
      expect(workflow).toBeTruthy();
      const build = workflow?.nodes?.find((node) => node.role === "build");
      expect(build).toBeTruthy();
      await controller.ensureAttempts(build!.id);
      expect(controller.getSnapshot().attemptsByNode[build!.id]?.length).toBeGreaterThan(0);
      await controller.ensureBudget(delivery.id);
      expect(controller.getSnapshot().budgetsByDelivery[delivery.id]?.used).toBeTruthy();
      const messages = controller.getSnapshot().projectData.prj_gantry.messages;
      expect(messages.some((message) => message.text.includes("模拟任务完成"))).toBe(true);
    } finally {
      await controller.stop();
    }
  });

  it("失败脚本等待 nodes.retry，并追加新尝试而不改旧尝试", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("failure_retry");
    adapter.simulation.play();
    adapter.simulation.advance(2_000);
    const waiting = adapter.simulation.getSnapshot();
    expect(waiting.status).toBe("paused");
    expect(waiting.awaiting?.kind).toBe("retry");
    if (waiting.awaiting?.kind !== "retry") return;
    const before = await adapter.api("attempts.list", {
      nodeId: waiting.awaiting.node_id,
      order: "asc",
    });
    expect(before.ok).toBe(true);
    if (!before.ok || !before.body) return;
    expect(before.body.items).toHaveLength(1);
    const failedId = before.body.items[0].id;

    const retried = await adapter.api("nodes.retry", {
      nodeId: waiting.awaiting.node_id,
      idempotencyKey: "idem_scenario_retry",
    });
    expect(retried.ok).toBe(true);
    adapter.simulation.advance(2_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
    const after = await adapter.api("attempts.list", {
      nodeId: waiting.awaiting.node_id,
      order: "asc",
    });
    if (after.ok && after.body) {
      expect(after.body.items).toHaveLength(2);
      expect(after.body.items[0].id).toBe(failedId);
      expect(after.body.items[0].status).toBe("failed");
      expect(after.body.items[1].status).toBe("succeeded");
    }
  });

  it("选择与模拟批准完成，签名保持 null；拒绝、过期与 head 变化可分别模拟", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("choice_approval");
    adapter.simulation.play();
    adapter.simulation.advance(1_000);
    const snapshot = adapter.simulation.getSnapshot();
    expect(snapshot.awaiting?.kind).toBe("choice");
    const approvals = await adapter.api("approvals.list", { status: "pending" });
    if (!approvals.ok || !approvals.body) return;
    const approval = approvals.body.items.find((entry) =>
      entry.id.startsWith("apr_sim_"),
    );
    expect(approval).toBeTruthy();
    if (!approval) return;
    const result = adapter.simulation.simulateApproval(approval.id);
    expect(result.ok).toBe(true);
    const read = await adapter.api("approvals.get", { approvalId: approval.id });
    expect(read.ok).toBe(true);
    if (read.ok && read.body) {
      expect(read.body.status).toBe("granted");
      expect(read.body.signature).toBeNull();
    }
    expect(adapter.simulation.getSnapshot().status).toBe("completed");

    const denyAdapter = new ScenarioIpcAdapter("default");
    denyAdapter.simulation.select("choice_approval");
    denyAdapter.simulation.play();
    denyAdapter.simulation.advance(1_000);
    const pending = await denyAdapter.api("approvals.list", { status: "pending" });
    if (pending.ok && pending.body) {
      const target = pending.body.items.find((entry) => entry.id.startsWith("apr_sim_"));
      if (target) {
        denyAdapter.simulation.simulateDenial(target.id);
        const denied = await denyAdapter.api("approvals.get", { approvalId: target.id });
        if (denied.ok && denied.body) expect(denied.body.status).toBe("denied");
      }
    }
  });

  it("fake timer 驱动完整任务，工具调用按 id 匹配且不串项目", async () => {
    vi.useFakeTimers();
    const taskAdapter = new ScenarioIpcAdapter("default");
    const qaAdapter = new ScenarioIpcAdapter("default");
    try {
      taskAdapter.simulation.select("task", { projectId: "prj_specgit" });
      expect(taskAdapter.simulation.play().ok).toBe(true);
      await vi.advanceTimersByTimeAsync(3_000);
      const snapshot = taskAdapter.simulation.getSnapshot();
      expect(snapshot.status).toBe("completed");
      expect(snapshot.project_id).toBe("prj_specgit");
      expect(snapshot.activities).toHaveLength(2);
      expect(new Set(snapshot.activities.map((entry) => entry.call_id)).size).toBe(2);
      expect(snapshot.activities.every((entry) => entry.status === "succeeded")).toBe(true);
      expect(snapshot.activities.every((entry) => entry.ended_at !== null)).toBe(true);
      expect(snapshot.drafts[0]?.attempt_id).toBeTruthy();
      expect(
        snapshot.activities.every(
          (entry) => entry.attempt_id === snapshot.drafts[0]?.attempt_id,
        ),
      ).toBe(true);

      const specgit = await taskAdapter.api("deliveries.list", {
        projectId: "prj_specgit",
        limit: 200,
      });
      if (specgit.ok && specgit.body) {
        expect(specgit.body.items.some((entry) => entry.id.startsWith("dlv_sim_"))).toBe(true);
      }
      const gantry = await taskAdapter.api("deliveries.list", {
        projectId: "prj_gantry",
        limit: 200,
      });
      if (gantry.ok && gantry.body) {
        expect(gantry.body.items.some((entry) => entry.id.startsWith("dlv_sim_"))).toBe(false);
      }

      qaAdapter.simulation.select("qa");
      const sent = await qaAdapter.api("messages.post", {
        conversationId: "cnv_gantry",
        body: { text: "普通问答 fake timer", refs: [] },
        idempotencyKey: "idem_fake_timer_qa",
      });
      expect(sent.ok).toBe(true);
      await vi.advanceTimersByTimeAsync(2_000);
      expect(qaAdapter.simulation.getSnapshot().status).toBe("completed");
      const qaMessages = await qaAdapter.api("messages.list", {
        conversationId: "cnv_gantry",
        limit: 200,
        order: "asc",
      });
      if (qaMessages.ok && qaMessages.body) {
        expect(qaMessages.body.items.some((entry) => entry.text.includes("模拟回答"))).toBe(true);
      }
    } finally {
      taskAdapter.simulation.cancel();
      qaAdapter.simulation.cancel();
      vi.useRealTimers();
    }
  });

  it("fake timer 取消后无迟到更新，过期与 head 变化使审批失效", async () => {
    vi.useFakeTimers();
    const adapter = new ScenarioIpcAdapter("default");
    try {
      adapter.simulation.select("qa");
      await adapter.api("messages.post", {
        conversationId: "cnv_gantry",
        body: { text: "取消定时器", refs: [] },
        idempotencyKey: "idem_fake_timer_cancel",
      });
      await vi.advanceTimersByTimeAsync(300);
      const listBefore = await adapter.api("messages.list", {
        conversationId: "cnv_gantry",
        limit: 200,
        order: "asc",
      });
      const count = listBefore.ok ? listBefore.body?.items.length ?? 0 : 0;
      adapter.simulation.cancel();
      await vi.advanceTimersByTimeAsync(2_000);
      const after = await adapter.api("messages.list", {
        conversationId: "cnv_gantry",
        limit: 200,
        order: "asc",
      });
      const afterCount = after.ok ? after.body?.items.length ?? 0 : 0;
      expect(afterCount).toBe(count);
      expect(adapter.simulation.getSnapshot().drafts[0]?.status).toBe("cancelled");
    } finally {
      adapter.simulation.cancel();
      vi.useRealTimers();
    }

    const expireAdapter = new ScenarioIpcAdapter("default");
    expireAdapter.simulation.select("choice_approval");
    expireAdapter.simulation.play();
    expireAdapter.simulation.advance(1_000);
    const pending = await expireAdapter.api("approvals.list", { status: "pending" });
    const target = pending.ok
      ? pending.body?.items.find((entry) => entry.id.startsWith("apr_sim_"))
      : null;
    if (target) {
      expect(expireAdapter.simulation.expireApproval(target.id).ok).toBe(true);
      const expired = await expireAdapter.api("approvals.get", { approvalId: target.id });
      if (expired.ok && expired.body) expect(expired.body.status).toBe("expired");
      expect(expireAdapter.simulation.getSnapshot().status).toBe("failed");
    }

    const headAdapter = new ScenarioIpcAdapter("default");
    headAdapter.simulation.select("choice_approval");
    headAdapter.simulation.play();
    headAdapter.simulation.advance(1_000);
    const headPending = await headAdapter.api("approvals.list", { status: "pending" });
    const headTarget = headPending.ok
      ? headPending.body?.items.find((entry) => entry.id.startsWith("apr_sim_"))
      : null;
    if (headTarget) {
      expect(headAdapter.simulation.setHeadChanged(headTarget.id).ok).toBe(true);
      const invalidated = await headAdapter.api("approvals.get", {
        approvalId: headTarget.id,
      });
      if (invalidated.ok && invalidated.body) {
        expect(invalidated.body.status).toBe("invalidated");
      }
    }
  });

  it("交付暂停阻止脚本产生新回复，继续后完成同一任务", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("task");
    adapter.simulation.play();
    adapter.simulation.advance(300);
    const running = adapter.simulation.getSnapshot();
    expect(running.status).toBe("running");
    const deliveryId = running.drafts[0]?.delivery_id;
    expect(deliveryId).toBeTruthy();
    if (!deliveryId) return;
    const before = await adapter.api("messages.list", {
      conversationId: "cnv_gantry",
      limit: 200,
      order: "asc",
    });
    const count = before.ok ? before.body?.items.length ?? 0 : 0;
    const paused = await adapter.api("deliveries.pause", {
      deliveryId,
      idempotencyKey: "idem_pause_sim",
    });
    expect(paused.ok).toBe(true);
    expect(adapter.simulation.getSnapshot().status).toBe("paused");
    expect(adapter.simulation.advance(2_000).ok).toBe(false);
    const afterPause = await adapter.api("messages.list", {
      conversationId: "cnv_gantry",
      limit: 200,
      order: "asc",
    });
    expect(afterPause.ok ? afterPause.body?.items.length ?? 0 : 0).toBe(count);

    const resumed = await adapter.api("deliveries.resume", {
      deliveryId,
      idempotencyKey: "idem_resume_sim",
    });
    expect(resumed.ok).toBe(true);
    adapter.simulation.advance(2_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
  });

  it("拒绝内联凭据与非测试占位值，模拟断线恢复后继续同一交付", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const models = await adapter.api("settings.get", { resource: "models" });
    if (models.ok && models.body) {
      const value = plain(models.body.value) as Record<string, unknown>;
      const providers = value.providers as Record<string, Record<string, unknown>>;
      (providers.anthropic.credential as Record<string, unknown>).value = "real-secret-value-not-for-demo";
      const preview = await adapter.guardedWritePrepare({
        kind: "settings_put",
        scope: "global",
        resource: "models",
        value,
        ifMatch: models.headers.etag ?? '"18235"',
      });
      expect(preview.valid).toBe(false);
      expect(preview.errors.some((entry) => entry.code === "placeholder_required")).toBe(true);
    }

    adapter.simulation.select("offline_switch");
    adapter.simulation.play();
    adapter.simulation.advance(400);
    expect(adapter.simulation.getSnapshot().awaiting?.kind).toBe("connection");
    adapter.simulation.setConnection("live");
    adapter.simulation.advance(1_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
  });

  it("只读角色不能读取或写入设置", async () => {
    const adapter = new ScenarioIpcAdapter("viewer");
    const read = await adapter.api("settings.get", { resource: "models" });
    expect(read.ok).toBe(false);
    if (!read.ok && "status" in read) expect(read.status).toBe(403);
    const write = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: {},
      ifMatch: '"18235"',
    });
    expect(write.can_submit.ok).toBe(false);
    const submitted = await adapter.guardedWriteSubmit({ pendingId: write.pending_id });
    expect(submitted.ok).toBe(false);
  });

  it("取消后不再产生迟到消息；选择新脚本清理旧演示实体", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    adapter.simulation.select("qa");
    const sent = await adapter.api("messages.post", {
      conversationId: "cnv_gantry",
      body: { text: "取消测试", refs: [] },
      idempotencyKey: "idem_cancel",
    });
    expect(sent.ok).toBe(true);
    adapter.simulation.advance(300);
    const beforeCancel = adapter.simulation.getSnapshot();
    expect(beforeCancel.drafts.length).toBe(1);
    adapter.simulation.cancel();
    const listBefore = await adapter.api("messages.list", {
      conversationId: "cnv_gantry",
      limit: 200,
      order: "asc",
    });
    const countBefore = listBefore.ok ? listBefore.body?.items.length ?? 0 : 0;
    adapter.simulation.advance(2_000);
    expect(adapter.simulation.getSnapshot().drafts[0]?.status).toBe("cancelled");
    const listAfter = await adapter.api("messages.list", {
      conversationId: "cnv_gantry",
      limit: 200,
      order: "asc",
    });
    expect(listAfter.ok ? listAfter.body?.items.length ?? 0 : 0).toBe(countBefore);

    adapter.simulation.select("task");
    adapter.simulation.play();
    adapter.simulation.advance(2_000);
    expect(adapter.simulation.getSnapshot().status).toBe("completed");
    const dynamic = await adapter.api("deliveries.list", {
      projectId: "prj_gantry",
      limit: 200,
    });
    const created = dynamic.ok
      ? dynamic.body?.items.filter((entry) => entry.id.startsWith("dlv_sim_"))
      : [];
    expect(created).toHaveLength(1);
    adapter.simulation.reset();
    const afterReset = await adapter.api("deliveries.list", {
      projectId: "prj_gantry",
      limit: 200,
    });
    expect(
      afterReset.ok
        ? afterReset.body?.items.filter((entry) => entry.id.startsWith("dlv_sim_"))
        : [],
    ).toHaveLength(0);
  });
});
