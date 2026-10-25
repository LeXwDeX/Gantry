import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App, AppErrorBoundary } from "@/App";
import { DesktopController } from "@/store/controller";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { UnavailableIpcAdapter } from "@/ipc/unavailable";
import { startController, TEST_NOW } from "./helpers";

const controllers: DesktopController[] = [];

afterEach(async () => {
  await Promise.all(controllers.splice(0).map((controller) => controller.stop()));
});

function track(controller: DesktopController): DesktopController {
  controllers.push(controller);
  return controller;
}

function scenarioController(scenario: "default" | "revoked" = "default") {
  return track(
    new DesktopController(new ScenarioIpcAdapter(scenario), {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
      now: () => TEST_NOW,
    }),
  );
}

describe("App 入口编排", () => {
  it("挂载时启动控制器，卸载时停止并关闭事件流", async () => {
    const adapter = new ScenarioIpcAdapter("default");
    const controller = track(
      new DesktopController(adapter, {
        refreshDelayMs: 0,
        persistDelayMs: 0,
        prefetchWorkflows: false,
        now: () => TEST_NOW,
      }),
    );
    const closeStream = vi.spyOn(adapter, "eventsClose");
    const { unmount } = render(<App controller={controller} />);

    expect(await screen.findByRole("navigation", { name: "项目" })).toBeInTheDocument();
    expect(screen.getByText("示例数据")).toBeInTheDocument();

    unmount();
    await waitFor(() => expect(closeStream).toHaveBeenCalled());
  });

  it("非 Tauri 生产产物明确显示外壳不可用，且不回退示例", async () => {
    const controller = track(
      new DesktopController(new UnavailableIpcAdapter(), {
        now: () => TEST_NOW,
        prefetchWorkflows: false,
      }),
    );
    render(<App controller={controller} />);

    expect(screen.getByText("正在载入工作区…")).toBeInTheDocument();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(/桌面外壳未连接/);
    expect(screen.queryByRole("navigation", { name: "项目" })).not.toBeInTheDocument();
    expect(screen.queryByText("示例数据")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "设置" })).not.toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "重试连接" })).toBeInTheDocument();
  });

  it("吊销的设备被配对界面阻塞，工作区不渲染", async () => {
    const controller = scenarioController("revoked");
    render(<App controller={controller} />);

    expect(
      await screen.findByRole("main", { name: "配对设备" }),
    ).toBeInTheDocument();
    expect(screen.getByText("此设备已被吊销，需要重新配对。")).toBeInTheDocument();
    expect(screen.getByText("示例数据")).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "项目" })).not.toBeInTheDocument();
    expect(document.querySelector("#composer")).toBeNull();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("设置浮层覆盖时工作区保持挂载，关闭后焦点回到触发点", async () => {
    const { controller } = await startController("default");
    track(controller);
    const user = userEvent.setup();
    render(<App controller={controller} />);

    const trigger = await screen.findByRole("button", { name: "设置" });
    await user.click(trigger);
    const dialog = await screen.findByRole("dialog", { name: "设置" });
    await within(dialog).findByRole("heading", { name: "模型" });
    expect(screen.getByRole("navigation", { name: "项目" })).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "返回工作区" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(trigger).toHaveFocus());
    expect(screen.getByRole("navigation", { name: "项目" })).toBeInTheDocument();
  });

  it("收件箱进入审批单后只保留一个模态，Esc 关闭回到原入口", async () => {
    const { controller } = await startController("default");
    track(controller);
    const user = userEvent.setup();
    render(<App controller={controller} />);

    const trigger = await screen.findByRole("button", { name: "需要你 · 4" });
    await user.click(trigger);
    const inbox = await screen.findByRole("dialog", { name: "需要你处理的事项" });
    const review = within(inbox).getAllByRole("button", { name: /审阅并签名/ })[0];
    if (!review) throw new Error("缺少审批入口");
    await user.click(review);

    const sheet = await screen.findByRole("dialog", { name: "审批单" });
    expect(screen.queryByRole("dialog", { name: "需要你处理的事项" })).not.toBeInTheDocument();
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    expect(await within(sheet).findByText("未签名信息，仅供参考")).toBeInTheDocument();

    fireEvent(sheet, new Event("cancel", { cancelable: true }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(trigger).toHaveFocus());
  });

  it("场景替身出现异常时由错误边界接住，重试可恢复", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    let fail = true;
    function Flaky() {
      if (fail) throw new Error("测试注入渲染失败");
      return <p>内容已恢复</p>;
    }
    const user = userEvent.setup();
    try {
      render(
        <AppErrorBoundary label="测试区">
          <Flaky />
        </AppErrorBoundary>,
      );
      const alert = screen.getByRole("alert");
      expect(alert).toHaveTextContent("测试区");
      expect(alert).toHaveTextContent("此内容无法显示");

      fail = false;
      await user.click(screen.getByRole("button", { name: "重试" }));
      expect(await screen.findByText("内容已恢复")).toBeInTheDocument();
    } finally {
      consoleError.mockRestore();
    }
  });
});
