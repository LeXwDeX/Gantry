import { afterEach, describe, expect, it } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { Delivery } from "@/api/types";
import type { MockIpcAdapter } from "@/ipc/mock/adapter";
import { mountMode, type Mounted } from "./modes";

const DELIVERY = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const WEB = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";

let mounted: Mounted | null = null;

afterEach(async () => {
  await mounted?.stop();
  mounted = null;
});

async function openSheet(): Promise<HTMLElement> {
  fireEvent.click(screen.getByRole("button", { name: /^需要你 · \d+$/ }));
  const inbox = await screen.findByRole("dialog");
  fireEvent.click((await within(inbox).findAllByRole("button", { name: /审阅审批/ }))[0]);
  const sheet = await screen.findByRole("dialog");
  await within(sheet).findByText(/^[a-f0-9]{40}$/);
  return sheet;
}

/** 在 Mock 侧满足 PR 闸门，可选推进 synced_at，并注入 forge.synced。 */
async function satisfyGates(m: Mounted, bumpSyncedAt: boolean): Promise<void> {
  const delivery = (await m.read("deliveries.get", { deliveryId: DELIVERY })) as Delivery;
  if (!delivery.pr) throw new Error("夹具交付缺少 PR");
  const synced = bumpSyncedAt
    ? new Date(Date.parse(delivery.pr.synced_at) + 60_000).toISOString()
    : delivery.pr.synced_at;
  await m.mock?.mutate("deliveries", DELIVERY, {
    pr: {
      ...delivery.pr,
      checks: { ...delivery.pr.checks, required_ok: true, failed: 0, pending: 0 },
      mergeable_state: "clean",
      review_state: "approved",
      synced_at: synced,
    },
  });
  await m.mock?.inject("forge.synced", DELIVERY, { delivery_id: DELIVERY }, WEB);
}

describe("审批单随 GitHub 同步与连接恢复更新可签状态", () => {
  it("E2E-07：forge.synced 带来更新的 PR 快照后，签名无需重新载入即可启用", async () => {
    const m = (mounted = await mountMode("mock"));
    const sheet = await openSheet();
    const sign = within(sheet).getByRole("button", { name: /签名批准/ });
    expect(sign).toBeDisabled();
    expect(sign).toHaveAttribute("title", expect.stringMatching(/必需 checks 未通过/));
    await satisfyGates(m, true);
    await waitFor(() => expect(within(sheet).getByRole("button", { name: /签名批准/ })).toBeEnabled(), {
      timeout: 10_000,
    });
  });

  it("E2E-04：事件流重连期间打开的审批单在连接恢复后自动重新准备，签名可用", async () => {
    const m = (mounted = await mountMode("mock"));
    await satisfyGates(m, true);
    await waitFor(
      () => {
        const pr = m.controller
          .getSnapshot()
          .projectData[WEB]?.deliveries.find((d) => d.id === DELIVERY)?.pr;
        expect(pr?.checks.required_ok).toBe(true);
        expect(m.controller.getSnapshot().connection?.phase).toBe("live");
      },
      { timeout: 10_000 },
    );
    const adapter = m.adapter as MockIpcAdapter;
    adapter.interruptStream();
    const sheet = await openSheet();
    const sign = within(sheet).getByRole("button", { name: /签名批准/ });
    expect(sign).toBeDisabled();
    expect(sign).toHaveAttribute("title", "连接中断，暂不能签名。");
    const before = adapter.calls.filter((c) => c.command === "approval_prepare").length;
    adapter.reconnect();
    await waitFor(() => expect(within(sheet).getByRole("button", { name: /签名批准/ })).toBeEnabled(), {
      timeout: 10_000,
    });
    expect(adapter.calls.filter((c) => c.command === "approval_prepare").length).toBe(before + 1);
  });

  it("交付缓存的同步时间不比快照新时，仍按审批单快照保守禁用", async () => {
    const m = (mounted = await mountMode("mock"));
    const sheet = await openSheet();
    await satisfyGates(m, false);
    await waitFor(
      () => {
        const pr = m.controller
          .getSnapshot()
          .projectData[WEB]?.deliveries.find((d) => d.id === DELIVERY)?.pr;
        expect(pr?.checks.required_ok).toBe(true);
      },
      { timeout: 10_000 },
    );
    expect(within(sheet).getByRole("button", { name: /签名批准/ })).toBeDisabled();
  });
});
