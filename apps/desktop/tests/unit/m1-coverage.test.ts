import { afterEach, describe, expect, it, vi } from "vitest";
import type { Delivery } from "@/api/types";
import type { DesktopController } from "@/store/controller";
import { deliveryChain } from "@/features/workspace/model";
import { TONE_LABELS, eventTone } from "@/theme/tone";
import { event, sleep, startController } from "./helpers";

const controllers: DesktopController[] = [];

afterEach(async () => {
  await Promise.all(controllers.splice(0).map((controller) => controller.stop()));
});

describe("事件风暴下合并刷新（desktop-ui §7.2、§12）", () => {
  it("500 条同一交付的事件只触发与突发窗口数相当的重新拉取", async () => {
    const { adapter, controller } = await startController("default", { refreshDelayMs: 100 });
    controllers.push(controller);
    const api = vi.spyOn(adapter, "api");
    const delivery = controller.getSnapshot().projectData.prj_gantry?.deliveries[0];
    if (!delivery) throw new Error("scenario delivery missing");

    // 5 个突发窗口，每个窗口 100 条（约 500 条 / s）。
    let seq = 1_000_000;
    for (let burst = 0; burst < 5; burst += 1) {
      for (let index = 0; index < 100; index += 1) {
        seq += 1;
        adapter.emitFrame({
          t: "event",
          envelope: event(seq, "delivery.stage_changed", delivery.id, "prj_gantry", {
            from: "implement",
            to: "review",
            stage_status: "running",
          }),
        });
      }
      await sleep(200);
    }
    await sleep(300);

    const lists = api.mock.calls.filter(([route]) => route === "deliveries.list").length;
    // 每次刷新读“进行中”与“最近完成”两页；5 个窗口最多 10 次，远少于 500。
    expect(lists).toBeGreaterThan(0);
    expect(lists).toBeLessThanOrEqual(5 * 2);
    // 事件仍逐条 ack，游标推进到最后一条。
    expect(adapter.ackedSeq).toBe(seq);
  });
});

describe("关联链按交付计算", () => {
  it("只列本交付的 Issue → PR → 工作树，不混入属于其他交付的 PR", async () => {
    const { controller } = await startController();
    controllers.push(controller);
    const data = controller.getSnapshot().projectData.prj_gantry;
    if (!data) throw new Error("project data missing");
    const [first, second] = data.deliveries.filter((d) => d.pr !== null);
    if (!first || !second) throw new Error("need two deliveries with PRs");
    const chain = deliveryChain(first, data);
    expect(chain).toContain(`PR #${first.pr?.number}`);
    expect(chain).not.toContain(`PR #${second.pr?.number}`);
    for (const issue of first.issues) expect(chain).toContain(`#${issue.number}`);

    // 没有 PR、也没有工作树的交付只显示 Issue。
    const bare: Delivery = { ...first, pr: null, worktree_id: null };
    expect(deliveryChain(bare, data)).toBe(first.issues.map((issue) => `#${issue.number}`).join(" → "));
  });
});

describe("Tone 映射（desktop-ui §7.1）", () => {
  it("事件行 tone 映射与每个 Tone 的文字标签保持不变", () => {
    expect(
      Object.fromEntries(["info", "ok", "run", "warn", "unknown", null].map((tone) => [String(tone), eventTone(tone)])),
    ).toMatchInlineSnapshot(`
      {
        "info": "heal",
        "null": "heal",
        "ok": "ok",
        "run": "run",
        "unknown": "heal",
        "warn": "warn",
      }
    `);
    expect(TONE_LABELS).toMatchInlineSnapshot(`
      {
        "heal": "自愈中",
        "idle": "空闲",
        "need": "需要你",
        "ok": "通过",
        "run": "运行中",
        "warn": "异常",
      }
    `);
  });
});
