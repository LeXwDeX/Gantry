import { describe, expect, it, vi } from "vitest";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { DesktopController } from "@/store/controller";
import {
  SCENARIO_PR9_SHA,
  buildScenarioData,
  summarizeInbox,
} from "@/ipc/scenario/data";
import { SCENARIO_PROJECT_COUNTS } from "@/ipc/scenario/adapter";
import type { ApiResult } from "@/api/shell";

describe("开发场景数据", () => {
  it("三个项目与 3 / 1 / 0 的 open 计数", () => {
    const data = buildScenarioData("default");
    expect(data.projects.map((project) => project.id)).toEqual([
      "prj_gantry",
      "prj_specgit",
      "prj_graphagent",
    ]);
    const summary = summarizeInbox(data.inbox);
    expect(summary.total_open).toBe(4);
    expect(summary.by_project).toEqual([
      { project_id: "prj_gantry", open: 3, max_severity: "blocker" },
      { project_id: "prj_specgit", open: 1, max_severity: "warn" },
    ]);
    expect(SCENARIO_PROJECT_COUNTS).toEqual({
      prj_gantry: 3,
      prj_specgit: 1,
      prj_graphagent: 0,
    });
  });

  it("choice 卡引用真实存在的 open 上报项", () => {
    const data = buildScenarioData("default");
    const message = data.messages.prj_gantry.find((item) =>
      item.cards.some((card) => card.kind === "choice"),
    );
    const card = message?.cards.find((entry) => entry.kind === "choice");
    if (!card || card.kind !== "choice") throw new Error("choice card missing");
    const item = data.inbox.find((entry) => entry.id === card.inbox_id);
    expect(item?.status).toBe("open");
    expect(item?.kind).toBe("decision_needed");
  });

  it("unsafe 场景保留计数但注入不可信文本", () => {
    const data = buildScenarioData("unsafe");
    expect(summarizeInbox(data.inbox).total_open).toBe(4);
    expect(
      data.messages.prj_gantry.some((message) => message.text.includes("<script>")),
    ).toBe(true);
    const pr = data.deliveries.find((delivery) => delivery.pr)?.pr;
    expect(pr?.title.text).toContain("onerror");
  });

  it("stale 场景的 PR head 与审批摘要不同", () => {
    const data = buildScenarioData("stale");
    const approval = data.approvals[0];
    const delivery = data.deliveries.find(
      (item) => item.id === approval.delivery_id,
    );
    expect(approval.subject_digest).toBe(SCENARIO_PR9_SHA);
    expect(delivery?.pr?.head_sha).not.toBe(approval.subject_digest);
  });
});

describe("ScenarioIpcAdapter", () => {
  it("签名仍拒绝；受保护写入只在演示内存中保存并可回读", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const before = await adapter.api("approvals.get", {
      approvalId: "apr_gantry_9",
    });
    await expect(
      adapter.approvalSign({ preparedId: "prepared_default_apr_gantry_9" }),
    ).rejects.toMatchObject({ code: "shell.key_unavailable" });
    const after = await adapter.api("approvals.get", {
      approvalId: "apr_gantry_9",
    });
    expect(after.ok && after.body?.status).toBe("pending");
    expect(before.ok).toBe(true);

    const prepared = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: {},
      ifMatch: '"18235"',
    });
    expect(prepared.valid).toBe(true);
    expect(prepared.can_submit).toEqual({ ok: true });
    const submitted = await adapter.guardedWriteSubmit({ pendingId: prepared.pending_id });
    expect(submitted.ok).toBe(true);
    const settings = await adapter.api("settings.get", { resource: "models" });
    expect(settings.ok && settings.body?.rev).toBe(18236);
  });

  it("模拟配对不生成密钥或证书，确认后进入已配对", async () => {
    const adapter = new ScenarioIpcAdapter("unpaired");
    const begin = await adapter.pairingBegin({
      pairingString: "gantry+pair://test-placeholder",
      deviceName: "Mac",
    });
    expect(begin.confirmationCode).toMatch(/^\d{3} \d{3}$/);
    expect(begin.device.cert_serial).toContain("test");
    const identity = await adapter.pairingConfirm({ matches: true });
    expect(identity.state).toBe("paired");
    expect(identity.key_backend).toBe("test_software");
    await expect(
      adapter.approvalSign({ preparedId: "prepared_never_issued" }),
    ).rejects.toMatchObject({ code: "shell.key_unavailable" });
  });

  it("prefs 与 uiState 只存在会话内存", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const prefs = await adapter.prefsGet();
    const updated = await adapter.prefsSet({
      patch: { theme: "dark", layout: { ...prefs.layout, leftWidth: 300 } },
    });
    expect(updated.layout.leftWidth).toBe(300);
    expect((await adapter.prefsGet()).layout.leftWidth).toBe(300);

    await adapter.uiStateSet({
      projectId: "prj_gantry",
      state: {
        view: "map",
        sel: null,
        draft: "草稿",
        ctx: null,
        levelOverride: null,
        needsOnly: false,
        showAll: { issues: false, prs: false, worktrees: false },
        expandedGroups: [],
        chatScroll: { atBottom: true, anchor_message_id: null, offset: 0 },
        outbox: [],
      },
    });
    const read = await adapter.uiStateGet({ projectId: "prj_gantry" });
    expect(read?.view).toBe("map");
    expect(read?.draft).toBe("草稿");
    expect(await adapter.uiStateList()).toHaveProperty("prj_gantry");
  });

  it("identityReset 清除持久化 uiState 并保留偏好，旧队列保留且不自动发送", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const prefs = await adapter.prefsGet();
    await adapter.prefsSet({
      patch: { theme: "dark", layout: { ...prefs.layout, leftWidth: 321 } },
    });
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
    });
    let fresh: DesktopController | null = null;
    try {
      await controller.start();
      controller.updateUi({
        draft: "旧身份草稿",
        view: "map",
        ctx: { kind: "pr", repo: "acme/web", number: 1 },
        chatScroll: {
          atBottom: false,
          anchor_message_id: "msg_old",
          offset: 120,
        },
        outbox: [
          {
            local_id: "local_old_identity",
            idempotency_key: "idem_old_identity",
            text: "旧身份待发",
            refs: [],
            created_at: "2026-10-24T12:59:00.000Z",
            state: "queued",
          },
        ],
      });
      await new Promise<void>((resolve) => setTimeout(resolve, 10));
      expect((await adapter.uiStateList()).prj_gantry?.draft).toBe(
        "旧身份草稿",
      );

      const post = vi.spyOn(adapter, "api");
      await controller.resetIdentity();

      expect(await adapter.uiStateList()).toEqual({});
      const keptPrefs = await adapter.prefsGet();
      expect(keptPrefs.theme).toBe("dark");
      expect(keptPrefs.layout.leftWidth).toBe(321);

      // 旧界面按默认值清理，只保留内存队列；不会自动发送。
      expect(controller.getSnapshot().ui.prj_gantry.draft).toBe("");
      expect(controller.getSnapshot().ui.prj_gantry.view).toBe("chat");
      expect(controller.getSnapshot().ui.prj_gantry.ctx).toBeNull();
      expect(controller.getSnapshot().ui.prj_gantry.chatScroll).toEqual({
        atBottom: true,
        anchor_message_id: null,
        offset: 0,
      });
      expect(controller.getSnapshot().ui.prj_gantry.outbox).toHaveLength(1);
      expect(controller.staleOutboxCount()).toBe(1);
      await controller.flushOutbox("prj_gantry");
      await new Promise<void>((resolve) => setTimeout(resolve, 10));
      expect(
        post.mock.calls.filter(([route]) => route === "messages.post"),
      ).toHaveLength(0);
      expect(controller.getSnapshot().ui.prj_gantry.outbox[0]).toMatchObject({
        text: "旧身份待发",
        state: "queued",
      });

      await controller.stop();
      expect(await adapter.uiStateList()).toEqual({});
      await adapter.pairingBegin({
        pairingString: "gantry+pair://example",
        deviceName: "新身份 Mac",
      });
      await adapter.pairingConfirm({ matches: true });
      fresh = new DesktopController(adapter, {
        refreshDelayMs: 0,
        persistDelayMs: 0,
        prefetchWorkflows: false,
      });
      await fresh.start();
      expect(fresh.getSnapshot().status).toBe("ready");
      expect(fresh.getSnapshot().ui.prj_gantry?.draft ?? "").not.toBe(
        "旧身份草稿",
      );
      expect(fresh.getSnapshot().ui.prj_gantry?.draft ?? "").toBe("");
      expect(fresh.getSnapshot().ui.prj_gantry?.outbox ?? []).toHaveLength(0);
    } finally {
      if (fresh) await fresh.stop();
      await controller.stop();
    }
  });

  it("openExternal 只记录 https；openGithub 校验 repo", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    await expect(
      adapter.openExternal({ url: "http://example.com" }),
    ).rejects.toMatchObject({ code: "shell.bad_argument" });
    await adapter.openExternal({ url: "https://github.com/acme/web" });
    expect(adapter.externalCalls).toEqual(["https://github.com/acme/web"]);
    await expect(
      adapter.openGithub({ kind: "pr", repo: "invalid" }),
    ).rejects.toMatchObject({ code: "shell.bad_argument" });
    await adapter.openGithub({
      kind: "pr",
      repo: "LeXwDeX/gantry",
      number: 9,
    });
    expect(adapter.githubCalls).toHaveLength(1);
  });

  it("choose 处理真实的 open 项并同步 summary；审批项不可 choose", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const result = await adapter.api("inbox.choose", {
      inboxId: "inb_choice7",
      optionId: "keep",
      idempotencyKey: "idem_test_1234567890",
    });
    expect(result.ok && result.body?.status).toBe("resolved");
    const summary = await adapter.api("inbox.summary", {});
    expect(summary.ok && summary.body?.total_open).toBe(3);

    const approvalChoice = await adapter.api("inbox.choose", {
      inboxId: "inb_apr9",
      optionId: "x",
      idempotencyKey: "idem_test_0987654321",
    });
    expect(approvalChoice.ok).toBe(false);
    if (!approvalChoice.ok && "problem" in approvalChoice) {
      expect(approvalChoice.status).toBe(409);
    }
  });

  it("messages.post 先回执用户消息，演示脚本随后结算 agent 回复", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const response = await adapter.api("messages.post", {
      conversationId: "cnv_gantry",
      body: { text: "一条测试消息", refs: [] },
      idempotencyKey: "idem_test_abcdefghij",
    });
    expect(response.ok).toBe(true);
    if (response.ok) {
      expect(response.body?.role).toBe("user");
      expect(response.body?.text).toBe("一条测试消息");
      expect(response.body?.cards).toEqual([]);
    }
    adapter.simulation.advance(1_000);
    const list = await adapter.api("messages.list", {
      conversationId: "cnv_gantry",
      limit: 200,
      order: "asc",
    });
    expect(
      list.ok && list.body?.items.some((item) => item.text === "一条测试消息"),
    ).toBe(true);
    expect(
      list.ok &&
        list.body?.items.some(
          (item) => item.role === "agent" && item.text.includes("模拟回答"),
        ),
    ).toBe(true);
  });

  it("setConnection 与 bumpLogEpoch 发出 state 帧", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const frames: string[] = [];
    await adapter.eventsOpen({}, (frame) => {
      frames.push(frame.t);
    });
    adapter.setConnection("live");
    adapter.bumpLogEpoch();
    expect(frames).toEqual(["state", "state", "state"]);
  });

  it("scene=error 首次 me.get 返回 500，之后可恢复", async () => {
    const adapter = new ScenarioIpcAdapter("error");
    const first = await adapter.api("me.get", {});
    expect(first.ok).toBe(false);
    if (!first.ok && "problem" in first) {
      expect(first.problem.code).toBe("internal");
    }
    const second = await adapter.api("me.get", {});
    expect(second.ok).toBe(true);
  });

  it("approvalPrepare 的已签名区来自审批，未签名区明确标注", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const sheet = await adapter.approvalPrepare({ approvalId: "apr_gantry_9" });
    expect(sheet.signed.subject_digest).toBe(SCENARIO_PR9_SHA);
    expect(sheet.signed.digest_head12).toHaveLength(12);
    expect(sheet.unsigned.note).toContain("示例模式");
    expect(sheet.can_sign).toEqual({ ok: false, reason: "not_connected" });
  });

  it("审批相关时间按会话时间构造，challenge 保持有效且签名始终拒绝", async () => {
    const now = Date.now();
    const adapter = new ScenarioIpcAdapter("default");
    const sheet = await adapter.approvalPrepare({ approvalId: "apr_gantry_9" });
    expect(Date.parse(sheet.challenge_expires_at)).toBeGreaterThan(now);
    expect(Date.parse(sheet.signed.expires_at)).toBeGreaterThan(now);
    await expect(
      adapter.approvalSign({ preparedId: sheet.prepared_id }),
    ).rejects.toThrow(/审批签名密钥/);
  });

  it("api 结果形状符合契约", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const result: ApiResult<unknown> = await adapter.api("inbox.summary", {});
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.status).toBe(200);
      expect(result.headers).toEqual({});
    }
  });
});
