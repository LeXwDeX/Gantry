import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useSyncExternalStore, type ReactNode } from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { ApprovalSheet } from "@/features/overlays/Overlays";
import { SCENARIO_PR9_SHA } from "@/ipc/scenario/data";
import { startController, TEST_NOW } from "./helpers";

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

describe("ApprovalSheet", () => {
  it("只从 signed 区显示完整 SHA、技术细节与两个有效期，并标出未签名区", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
    const onClose = vi.fn();
    try {
      render(
        <Live controller={controller}>
          {(state) => (
            <ApprovalSheet
              controller={controller}
              state={state}
              approvalId="apr_gantry_9"
              onClose={onClose}
            />
          )}
        </Live>,
      );

      expect(await screen.findByText(SCENARIO_PR9_SHA)).toBeInTheDocument();
      expect(screen.getByText("LeXwDeX/gantry · PR #9")).toBeInTheDocument();
      expect(screen.getByText("审批有效期")).toBeInTheDocument();
      expect(screen.getByText("本次确认窗口")).toBeInTheDocument();
      expect(screen.getByText("未签名信息，仅供参考")).toBeInTheDocument();
      expect(screen.getByText(/服务端未提供改动明细/)).toBeInTheDocument();
      expect(screen.getAllByText(/非生成字段|示例模式/).length).toBeGreaterThan(0);
      expect(screen.getByText("示例数据")).toBeInTheDocument();
      expect(
        screen.getByText("示例模式：未连接真实外壳，签名始终被拒绝。"),
      ).toBeInTheDocument();
      expect(screen.queryByText(/已过期/)).toBeNull();
      expect(screen.queryByText(/squash/)).toBeNull();

      const details = screen.getByText("技术细节（已签名）");
      fireEvent.click(details);
      expect(screen.getByText("apr_gantry_9")).toBeInTheDocument();
      expect(screen.getByText("c2NlbmFyaW8tbm9uY2UtcGxhY2Vob2xkZXI")).toBeInTheDocument();

      const github = vi.spyOn(adapter, "openGithub").mockResolvedValue(undefined);
      fireEvent.click(
        screen.getByRole("button", {
          name: "在 GitHub 上查看此 SHA 的 diff ↗",
        }),
      );
      await waitFor(() =>
        expect(github).toHaveBeenCalledWith({
          kind: "commit",
          repo: "LeXwDeX/gantry",
          sha: SCENARIO_PR9_SHA,
        }),
      );
      expect(onClose).not.toHaveBeenCalled();
    } finally {
      await controller.stop();
    }
  });

  it("签名失败只按错误显示并丢弃句柄，不乐观更新", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
    const sign = vi.spyOn(adapter, "approvalSign");
    const discard = vi.spyOn(adapter, "approvalDiscard");
    const base = await adapter.approvalPrepare({ approvalId: "apr_gantry_9" });
    vi.spyOn(adapter, "approvalPrepare").mockResolvedValue({
      ...base,
      can_sign: { ok: true },
    });
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
      const button = await screen.findByRole("button", { name: /签名批准/ });
      fireEvent.click(button);
      await waitFor(() =>
        expect(sign).toHaveBeenCalledWith({
          preparedId: "prepared_default_apr_gantry_9",
        }),
      );
      expect(
        await screen.findByText(/审批签名密钥不可用/),
      ).toBeInTheDocument();
      expect(await screen.findByText(/签名句柄已丢弃/)).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /签名批准/ })).toBeDisabled();
      expect(discard).toHaveBeenCalledWith({
        preparedId: "prepared_default_apr_gantry_9",
      });
      const after = await adapter.api("approvals.get", {
        approvalId: "apr_gantry_9",
      });
      expect(after.ok && after.body?.status).toBe("pending");
    } finally {
      await controller.stop();
    }
  });

  it("非审批设备禁用签名与拒绝并说明原因", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller } = await startController("operator");
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
      const sign = await screen.findByRole(
        "button",
        { name: /签名批准/ },
        { timeout: 3000 },
      );
      expect(sign).toBeDisabled();
      expect(screen.getByText(/此设备不能审批/)).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "模拟拒绝" })).toBeDisabled();
      expect(screen.getByText(/只有审批设备可以拒绝/)).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("challenge 过期后禁用签名并提供重新载入", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
    const base = await adapter.approvalPrepare({ approvalId: "apr_gantry_9" });
    vi.spyOn(adapter, "approvalPrepare").mockResolvedValue({
      ...base,
      challenge_expires_at: "2026-10-24T12:58:00.000Z",
    });
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
      const sign = await screen.findByRole(
        "button",
        { name: /签名批准/ },
        { timeout: 3000 },
      );
      expect(sign).toBeDisabled();
      expect(screen.getAllByText(/已过期/).length).toBeGreaterThan(0);
      expect(
        screen.getByRole("button", { name: "重新载入" }),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("未签名 PR 快照门禁未满足时也禁用签名", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
    const base = await adapter.approvalPrepare({ approvalId: "apr_gantry_9" });
    if (!base.unsigned.pr) throw new Error("pr snapshot missing");
    vi.spyOn(adapter, "approvalPrepare").mockResolvedValue({
      ...base,
      unsigned: {
        ...base.unsigned,
        pr: {
          ...base.unsigned.pr,
          checks: {
            passed: 8,
            failed: 1,
            pending: 0,
            required_ok: false,
          },
        },
      },
    });
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
      const sign = await screen.findByRole(
        "button",
        { name: /签名批准/ },
        { timeout: 3000 },
      );
      expect(sign).toBeDisabled();
      expect(screen.getAllByText(/必需 checks 未通过/).length).toBeGreaterThan(0);
    } finally {
      await controller.stop();
    }
  });

  it("签名成功后只按返回状态更新且不弹审批变化横幅", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
    const before = await adapter.api("approvals.get", {
      approvalId: "apr_gantry_9",
    });
    if (!before.ok || !before.body) throw new Error("approval missing");
    const base = await adapter.approvalPrepare({ approvalId: "apr_gantry_9" });
    vi.spyOn(adapter, "approvalPrepare").mockResolvedValue({
      ...base,
      can_sign: { ok: true },
    });
    const granted = {
      ...before.body,
      status: "granted" as const,
      signature: "test-signature",
    };
    const sign = vi.spyOn(adapter, "approvalSign").mockResolvedValue({
      ok: true,
      status: 200,
      headers: {},
      body: granted,
    });
    // 服务端在签名后不会再把这个审批列为 pending；模拟列表刷新必须一致。
    const realApi = adapter.api.bind(adapter);
    vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
      if (route === "approvals.list") {
        return {
          ok: true,
          status: 200,
          headers: {},
          body: { items: [], next_cursor: null },
        };
      }
      return realApi(route, args);
    });
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
        await screen.findByRole("button", { name: /签名批准/ }),
      );
      await waitFor(() =>
        expect(sign).toHaveBeenCalledWith({
          preparedId: "prepared_default_apr_gantry_9",
        }),
      );
      expect(
        await screen.findByText("签名请求已返回：已签名 · 等待合并。以服务端状态为准。"),
      ).toBeInTheDocument();
      expect(screen.queryByText(/审批已变化/)).toBeNull();
      expect(screen.getByRole("button", { name: /签名批准/ })).toBeDisabled();
      expect(controller.getSnapshot().approvals.apr_gantry_9.status).toBe(
        "granted",
      );
    } finally {
      await controller.stop();
    }
  });

  it("stale 与离线分别禁用签名并说明原因", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const stale = await startController("stale");
    try {
      render(
        <Live controller={stale.controller}>
          {(state) => (
            <ApprovalSheet
              controller={stale.controller}
              state={state}
              approvalId="apr_gantry_9"
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      expect(
        await screen.findByRole(
          "button",
          { name: /签名批准/ },
          { timeout: 3000 },
        ),
      ).toBeDisabled();
      expect(screen.getAllByText(/代码已更新/).length).toBeGreaterThan(0);
      expect(
        screen.getByRole("button", { name: "模拟批准（不生成签名）" }),
      ).toBeDisabled();
      expect(
        screen.getAllByText(/PR head 已变化，审批载荷不再对应当前代码/).length,
      ).toBeGreaterThan(0);
    } finally {
      await stale.controller.stop();
    }
  });

  it("离线时签名禁用并写明连接中断", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
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
      await screen.findByText(SCENARIO_PR9_SHA, undefined, { timeout: 3000 });
      adapter.setConnection("reconnecting");
      await waitFor(() =>
        expect(screen.getByRole("button", { name: /签名批准/ })).toBeDisabled(),
      );
      expect(
        screen.getByText("连接中断，暂不能签名。"),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("审批状态变化时丢弃旧 prepared 句柄且不自动重新准备", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
    const discard = vi.spyOn(adapter, "approvalDiscard");
    const prepare = vi.spyOn(adapter, "approvalPrepare");
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
      await screen.findByText(SCENARIO_PR9_SHA);
      expect(prepare).toHaveBeenCalledTimes(1);

      const result = await controller.denyApproval("apr_gantry_9");
      expect(result.ok).toBe(true);

      expect(await screen.findByText(/审批已变化/)).toBeInTheDocument();
      expect(discard).toHaveBeenCalledWith({
        preparedId: "prepared_default_apr_gantry_9",
      });
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      expect(screen.getByText("已拒绝，回到评审")).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("拒绝按钮要求桌面审批设备且连接 live", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
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
      const deny = await screen.findByRole("button", { name: "模拟拒绝" });
      expect(deny).toBeEnabled();

      adapter.setConnection("reconnecting");
      await waitFor(() => expect(deny).toBeDisabled());
      expect(screen.getByText(/连接中断，暂不能拒绝/)).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("模拟批准只改内存、不调用生产签名且不生成 signature", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
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
      expect(
        await screen.findByText(/生产签名路径在示例模式始终拒绝/),
      ).toBeInTheDocument();
      const simulate = await screen.findByRole("button", {
        name: "模拟批准（不生成签名）",
      });
      expect(simulate).toBeEnabled();
      fireEvent.click(simulate);
      expect(
        await screen.findByText(/模拟批准已应用：未生成 signature/),
      ).toBeInTheDocument();
      expect(sign).not.toHaveBeenCalled();
      const approval = controller.getSnapshot().approvals.apr_gantry_9;
      expect(approval.status).toBe("granted");
      expect(approval.signature).toBeNull();
      expect(document.body.textContent).not.toContain("test-signature");
      await waitFor(() =>
        expect(
          controller.getSnapshot().projectData.prj_gantry.deliveries.find(
            (item) => item.id === "dlv_gantry_9",
          )?.stage,
        ).toBe("closed"),
      );
    } finally {
      await controller.stop();
    }
  });

  it("非审批设备与离线连接都不能模拟批准", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const viewer = await startController("viewer");
    let first: ReturnType<typeof render> | null = null;
    try {
      first = render(
        <Live controller={viewer.controller}>
          {(state) => (
            <ApprovalSheet
              controller={viewer.controller}
              state={state}
              approvalId="apr_gantry_9"
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      await screen.findByText(SCENARIO_PR9_SHA);
      const simulate = screen.getByRole("button", {
        name: "模拟批准（不生成签名）",
      });
      expect(simulate).toBeDisabled();
      expect(screen.getAllByText(/只有审批设备可以模拟批准/).length).toBeGreaterThan(0);
    } finally {
      first?.unmount();
      await viewer.controller.stop();
    }

    const offline = await startController();
    try {
      render(
        <Live controller={offline.controller}>
          {(state) => (
            <ApprovalSheet
              controller={offline.controller}
              state={state}
              approvalId="apr_gantry_9"
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      await screen.findByText(SCENARIO_PR9_SHA);
      offline.adapter.setConnection("reconnecting");
      await waitFor(() =>
        expect(
          screen.getByRole("button", { name: "模拟批准（不生成签名）" }),
        ).toBeDisabled(),
      );
      expect(
        screen.getByText(/连接无效，不能模拟批准/),
      ).toBeInTheDocument();
    } finally {
      await offline.controller.stop();
    }
  });

  it("模拟拒绝关闭交付，跨视图状态一致且无真实签名", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, adapter } = await startController();
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
      await screen.findByText(SCENARIO_PR9_SHA);
      fireEvent.click(
        await screen.findByRole("button", { name: "模拟拒绝" }),
      );
      expect(
        await screen.findByText(/模拟拒绝已应用：未生成 signature/),
      ).toBeInTheDocument();
      const state = controller.getSnapshot();
      expect(state.approvals.apr_gantry_9.status).toBe("denied");
      expect(state.approvals.apr_gantry_9.signature).toBeNull();
      expect(
        state.projectData.prj_gantry.deliveries.find(
          (item) => item.id === "dlv_gantry_9",
        )?.stage,
      ).toBe("abandoned");
      expect(
        state.projectData.prj_gantry.messages.some((message) =>
          message.text.includes("审批被拒绝"),
        ),
      ).toBe(true);
      expect(
        state.inbox.some(
          (item) => item.id === "inb_apr9" && item.status === "open",
        ),
      ).toBe(false);
      expect(document.body.textContent).not.toContain("test-signature");
      const direct = await adapter.api("approvals.get", {
        approvalId: "apr_gantry_9",
      });
      expect(direct.ok && direct.body?.signature).toBeFalsy();
    } finally {
      await controller.stop();
    }
  });
});
