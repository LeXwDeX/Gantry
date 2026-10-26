import { describe, expect, it, vi } from "vitest";
import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { useSyncExternalStore, type ReactNode } from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import type { Device } from "@/api/types";
import { SettingsOverlay } from "@/features/overlays/Overlays";
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

async function openDevices(
  scenario: Parameters<typeof startController>[0] = "default",
  beforeRender?: (adapter: Awaited<ReturnType<typeof startController>>["adapter"]) => void,
): Promise<{
  controller: DesktopController;
  adapter: Awaited<ReturnType<typeof startController>>["adapter"];
  onClose: ReturnType<typeof vi.fn>;
}> {
  const { controller, adapter } = await startController(scenario);
  beforeRender?.(adapter);
  const onClose = vi.fn();
  render(
    <Live controller={controller}>
      {(state) => (
        <SettingsOverlay
          controller={controller}
          state={state}
          initialPage="devices"
          onClose={onClose}
        />
      )}
    </Live>,
  );
  await screen.findByRole("heading", { name: "设备与证书" });
  await screen.findByText("书房 Mac");
  return { controller, adapter, onClose };
}

async function readDevices(
  adapter: Awaited<ReturnType<typeof startController>>["adapter"],
): Promise<Device[]> {
  const result = await adapter.api("devices.list", { limit: 100 });
  expect(result.ok).toBe(true);
  if (!result.ok || !result.body) throw new Error("no devices");
  return result.body.items;
}

function deviceCard(name: string): HTMLElement {
  return screen.getByRole("article", { name });
}

async function applyDevicePreview(): Promise<void> {
  const apply = await screen.findByRole("button", {
    name: "模拟应用设备操作",
  });
  fireEvent.click(apply);
}

describe("设备与证书：模拟闭环", () => {
  it("角色变更与吊销都先确认，再模拟预检并回读列表", async () => {
    const { controller, adapter } = await openDevices();
    try {
      const card = deviceCard("书房 Mac");
      fireEvent.change(within(card).getByLabelText("书房 Mac 角色"), {
        target: { value: "approver" },
      });
      fireEvent.click(within(card).getByRole("button", { name: "模拟角色变更" }));
      expect(
        screen.getByText(/确认将 书房 Mac 的角色改为 审批设备/),
      ).toBeInTheDocument();
      expect(
        screen.getByText(/开发场景只模拟内存角色/),
      ).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "确认并模拟预检" }));
      await applyDevicePreview();
      expect(
        await screen.findByText(/模拟设备角色已更新/),
      ).toBeInTheDocument();
      let devices = await readDevices(adapter);
      expect(devices.find((device) => device.name === "书房 Mac")?.role).toBe(
        "approver",
      );

      const updated = deviceCard("书房 Mac");
      fireEvent.click(within(updated).getByRole("button", { name: "模拟吊销…" }));
      expect(screen.getByText(/确认吊销 书房 Mac/)).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "取消" }));
      expect(within(deviceCard("书房 Mac")).getByText("有效")).toBeInTheDocument();
      devices = await readDevices(adapter);
      expect(devices.find((device) => device.name === "书房 Mac")?.status).toBe(
        "active",
      );

      fireEvent.click(
        within(deviceCard("书房 Mac")).getByRole("button", {
          name: "模拟吊销…",
        }),
      );
      fireEvent.click(screen.getByRole("button", { name: "确认并模拟预检" }));
      await applyDevicePreview();
      expect(await screen.findByText(/模拟吊销已完成/)).toBeInTheDocument();
      devices = await readDevices(adapter);
      expect(devices.find((device) => device.name === "书房 Mac")?.status).toBe(
        "revoked",
      );
      expect(within(deviceCard("书房 Mac")).getByText("已吊销")).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("改名正常保存并回读，未保存的改名会拦截离开设置", async () => {
    const { controller, adapter, onClose } = await openDevices();
    try {
      fireEvent.click(
        within(deviceCard("书房 Mac")).getByRole("button", {
          name: "改名（模拟）",
        }),
      );
      fireEvent.change(screen.getByLabelText("书房 Mac 新设备名"), {
        target: { value: "实验室 Mac" },
      });
      expect(await screen.findByText("有未保存的改动")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "返回工作区" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      expect(onClose).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "继续编辑" }));

      fireEvent.click(screen.getByRole("button", { name: "模拟保存名称" }));
      expect(await screen.findByText(/模拟改名已保存/)).toBeInTheDocument();
      const devices = await readDevices(adapter);
      expect(devices.some((device) => device.name === "实验室 Mac")).toBe(true);
      expect(screen.getByRole("article", { name: "实验室 Mac" })).toBeInTheDocument();
      await waitFor(() =>
        expect(screen.queryByText("有未保存的改动")).toBeNull(),
      );
    } finally {
      await controller.stop();
    }
  });

  it("空设备名显示本地校验错误，不提交", async () => {
    const { controller, adapter } = await openDevices();
    const rename = vi.spyOn(adapter, "api");
    try {
      fireEvent.click(
        within(deviceCard("书房 Mac")).getByRole("button", {
          name: "改名（模拟）",
        }),
      );
      fireEvent.change(screen.getByLabelText("书房 Mac 新设备名"), {
        target: { value: "   " },
      });
      fireEvent.click(screen.getByRole("button", { name: "模拟保存名称" }));
      expect(await screen.findByText("设备名不能为空。")).toBeInTheDocument();
      expect(
        rename.mock.calls.some(([route]) => route === "devices.rename"),
      ).toBe(false);
      expect(within(deviceCard("书房 Mac")).getByText("书房 Mac")).toBeInTheDocument();
    } finally {
      rename.mockRestore();
      await controller.stop();
    }
  });

  it("不能改自己的角色或吊销自己，且不显示假系统认证", async () => {
    const { controller } = await openDevices();
    try {
      const own = deviceCard("Susan 的 MacBook");
      expect(within(own).getByLabelText("Susan 的 MacBook 角色")).toBeDisabled();
      expect(
        within(own).getByRole("button", { name: "模拟吊销…" }),
      ).toBeDisabled();
      expect(
        within(own).getByText(/不能修改自己的设备角色/),
      ).toBeInTheDocument();
      expect(screen.getByText(/不调用\s*Touch ID/)).toBeInTheDocument();
      expect(document.body.textContent).not.toMatch(/已通过系统认证/);
    } finally {
      await controller.stop();
    }
  });

  it("只读角色不能改名、改角色或吊销", async () => {
    const { controller } = await openDevices("viewer");
    try {
      const second = deviceCard("书房 Mac");
      expect(within(second).getByRole("button", { name: "改名（模拟）" })).toBeDisabled();
      expect(within(second).getByLabelText("书房 Mac 角色")).toBeDisabled();
      expect(
        within(second).getByRole("button", { name: "模拟角色变更" }),
      ).toBeDisabled();
      expect(
        within(second).getByRole("button", { name: "模拟吊销…" }),
      ).toBeDisabled();
      expect(screen.getByText(/当前角色只读/)).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("iOS 设备不能选审批角色，本地守卫阻止提交", async () => {
    const { controller } = await openDevices("default", (adapter) => {
      const original = adapter.api.bind(adapter);
      const ios: Device = {
        id: "dev_scenario_ios",
        name: "测试 iPhone",
        platform: "ios",
        cert_serial: "test-ios-cert",
        pubkey_fpr: "sha256:test-ios-pubkey",
        approval_pubkey_fpr: null,
        role: "operator",
        status: "active",
        last_seen: null,
      };
      vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
        if (route === "devices.list") {
          const result = await original(route, args as never);
          if (result.ok && result.body) {
            return {
              ...result,
              body: { ...result.body, items: [...result.body.items, ios] },
            } as never;
          }
        }
        return original(route, args as never);
      });
    });
    try {
      const card = deviceCard("测试 iPhone");
      const select = within(card).getByLabelText("测试 iPhone 角色");
      expect(
        within(select).getByRole("option", { name: /审批设备/ }),
      ).toBeDisabled();
      expect(within(card).getByText(/手机档案不能成为审批设备/)).toBeInTheDocument();
      fireEvent.change(select, { target: { value: "approver" } });
      fireEvent.click(within(card).getByRole("button", { name: "模拟角色变更" }));
      expect(
        (await screen.findAllByText("手机档案不能成为审批设备。")).length,
      ).toBeGreaterThan(0);
      expect(screen.queryByText(/确认将 测试 iPhone/)).toBeNull();
    } finally {
      await controller.stop();
    }
  });
});
