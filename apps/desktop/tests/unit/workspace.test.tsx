import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { useSyncExternalStore } from "react";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { Workspace } from "@/features/workspace";
import { startController } from "./helpers";

const controllers: DesktopController[] = [];

afterEach(async () => {
  await Promise.all(controllers.splice(0).map((controller) => controller.stop()));
});

function renderWorkspace(controller: DesktopController) {
  const onSettings = vi.fn();
  const onApproval = vi.fn();
  const onInbox = vi.fn();

  function Harness() {
    const state: DesktopState = useSyncExternalStore(
      controller.subscribe,
      controller.getSnapshot,
    );
    return (
      <Workspace
        controller={controller}
        state={state}
        onSettings={onSettings}
        onApproval={onApproval}
        onInbox={onInbox}
      />
    );
  }

  render(<Harness />);
  return { onSettings, onApproval, onInbox };
}

async function mount(scenario: "default" | "viewer" = "default") {
  const { controller } = await startController(scenario, {
    prefetchWorkflows: true,
  });
  controllers.push(controller);
  const mocks = renderWorkspace(controller);
  await screen.findByRole("navigation", { name: "项目" });
  return { controller, ...mocks };
}

function leftPane(): HTMLElement {
  return screen.getByRole("complementary", { name: "项目信息" });
}

function inspector(): HTMLElement {
  return screen.getByRole("complementary", { name: "检视器" });
}

function mainPane(): HTMLElement {
  return screen.getByRole("main");
}

describe("工作区 顶栏与左栏", () => {
  it("显示项目徽标、总需要你数、开发场景连接文案与禁用的添加项目", async () => {
    const { onInbox } = await mount();
    const nav = screen.getByRole("navigation", { name: "项目" });
    expect(within(nav).getByRole("button", { name: /gantry/ })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(screen.getByLabelText("需要你 3 项")).toBeInTheDocument();
    expect(screen.getByLabelText("需要你 1 项")).toBeInTheDocument();
    expect(
      screen.getByText("开发场景 · 未连接真实服务端"),
    ).toBeInTheDocument();
    expect(screen.getByText("示例数据")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "添加项目" })).toBeDisabled();
    expect(
      within(leftPane()).getByText("私有 CA 与 mTLS 配对"),
    ).toBeInTheDocument();
    expect(
      within(leftPane()).queryByText("#4 私有 CA 与 mTLS 配对"),
    ).toBeNull();
    expect(
      within(leftPane()).getByText("feat: 配对 token 与 CSR 签发"),
    ).toBeInTheDocument();
    expect(
      within(leftPane()).getAllByText("feat/sandbox-driver"),
    ).toHaveLength(1);

    fireEvent.click(screen.getByRole("button", { name: "需要你 · 4" }));
    expect(onInbox).toHaveBeenCalledTimes(1);
  });

  it("选择左栏 Issue 只改变 sel，不改变发送上下文；引用 chip 同样只改 sel", async () => {
    const { controller } = await mount();
    const pane = leftPane();
    fireEvent.click(within(pane).getByRole("button", { name: /#5 沙盒驱动/ }));

    expect(controller.getSnapshot().ui.prj_gantry.sel).toEqual({
      kind: "issue",
      repo: "LeXwDeX/gantry",
      number: 5,
    });
    expect(controller.getSnapshot().ui.prj_gantry.ctx).toBeNull();
    expect(
      within(mainPane()).getByRole("button", { name: /@ 全项目/ }),
    ).toBeInTheDocument();

    const chips = within(mainPane()).getAllByRole("button", { name: "#5" });
    fireEvent.click(chips[0]);
    expect(controller.getSnapshot().ui.prj_gantry.sel).toEqual({
      kind: "issue",
      repo: "LeXwDeX/gantry",
      number: 5,
    });
    expect(controller.getSnapshot().ui.prj_gantry.ctx).toBeNull();
    expect(
      within(inspector()).getByRole("button", { name: "在对话中引用" }),
    ).toBeInTheDocument();
  });

  it("连续后台事件折叠，展开后逐条显示", async () => {
    await mount();
    const main = mainPane();
    const group = within(main).getByRole("button", {
      name: /后台处理了 3 件事/,
    });
    expect(group).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(group);
    expect(group).toHaveAttribute("aria-expanded", "true");
    expect(
      within(main).getByText("PR #8 · lint nit × 2 → 攒到下次推送"),
    ).toBeInTheDocument();
  });

  it("回复把输入上下文显式设为所属交付", async () => {
    const { controller } = await mount();
    const replies = within(mainPane()).getAllByRole("button", { name: "回复" });
    fireEvent.click(replies[0]);
    expect(controller.getSnapshot().ui.prj_gantry.ctx).toEqual({
      kind: "delivery",
      id: "dlv_gantry_5",
    });
    expect(
      within(mainPane()).getByRole("button", { name: /@ PR #8/ }),
    ).toBeInTheDocument();
  });
});

describe("工作区 输入区", () => {
  it("档位选择器用方向键移动，并写回显式 levelOverride", async () => {
    const { controller } = await mount();
    const group = screen.getByRole("radiogroup", { name: "档位上限" });
    expect(screen.getByRole("radio", { name: "Medium" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    fireEvent.keyDown(group, { key: "ArrowRight" });
    expect(controller.getSnapshot().ui.prj_gantry.levelOverride).toBe("High");
    expect(screen.getByRole("radio", { name: "High" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    fireEvent.keyDown(group, { key: "ArrowRight" });
    expect(controller.getSnapshot().ui.prj_gantry.levelOverride).toBe("Max");
  });

  it("上下文选择器打开后聚焦当前项，方向键移动，Esc 回到触发按钮", async () => {
    await mount();
    const trigger = within(mainPane()).getByRole("button", {
      name: /@ 全项目/,
    });
    fireEvent.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");

    const options = screen.getAllByRole("option");
    expect(document.activeElement).toBe(options[0]);
    fireEvent.keyDown(document.activeElement as HTMLElement, {
      key: "ArrowDown",
    });
    expect(document.activeElement).toBe(options[1]);
    fireEvent.keyDown(document.activeElement as HTMLElement, {
      key: "Escape",
    });
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(document.activeElement).toBe(trigger);
  });

  it("macOS 用 ⌘↵ 发送；IME 组合中的回车不发送", async () => {
    const { controller } = await mount();
    const textarea = screen.getByLabelText("给 agent 发消息");
    fireEvent.change(textarea, { target: { value: "补一条验收标准" } });
    expect(textarea).toHaveValue("补一条验收标准");

    fireEvent.keyDown(textarea, {
      key: "Enter",
      metaKey: true,
      keyCode: 229,
    });
    expect(controller.getSnapshot().ui.prj_gantry.draft).toBe("补一条验收标准");

    fireEvent.keyDown(textarea, { key: "Enter", metaKey: true });
    expect(controller.getSnapshot().ui.prj_gantry.draft).toBe("");
  });
});

describe("工作区 视图与检视器", () => {
  it("关系地图显示连线图例与服务端缺口；工作树表标明沙盒未知和后台守护", async () => {
    await mount();
    fireEvent.click(screen.getByRole("tab", { name: "关系地图" }));
    const main = mainPane();
    expect(within(main).getByText("尚未开 PR")).toBeInTheDocument();
    expect(within(main).getByText("沙盒编号：服务端未提供")).toBeInTheDocument();
    expect(within(main).getByText("缺规格")).toBeInTheDocument();
    expect(
      within(main).getByRole("button", { name: /PR #9/ }),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("tab", { name: "工作树" }));
    expect(
      within(main).getByRole("region", { name: "工作树列表" }),
    ).toHaveAttribute("tabindex", "0");
    const table = within(main).getByRole("table");
    expect(within(table).getByRole("button", { name: "feat/dag-engine" })).toBeInTheDocument();
    expect(within(table).getAllByText("服务端未提供").length).toBeGreaterThan(0);
    expect(within(table).getByText("后台守护")).toBeInTheDocument();
  });

  it("Low 交付阶段条显示跳过；合并阶段显示合并闸门", async () => {
    await mount();
    const pane = leftPane();
    fireEvent.click(within(pane).getByRole("button", { name: /#6 桌面端控制台原型/ }));
    expect(within(inspector()).getByText("跳过")).toBeInTheDocument();
    expect(within(inspector()).queryByText("合并闸门")).toBeNull();

    fireEvent.click(within(pane).getByRole("button", { name: /PR #9/ }));
    expect(within(inspector()).getByText("合并闸门")).toBeInTheDocument();
    expect(within(inspector()).getByText(/必需 checks @/)).toBeInTheDocument();
  });

  it("审批入口只调用 onApproval，不在工作区直接签名", async () => {
    const { onApproval } = await mount();
    const buttons = within(mainPane()).getAllByRole("button", {
      name: "审阅并签名…",
    });
    expect(buttons.length).toBeGreaterThan(0);
    fireEvent.click(buttons[0]);
    expect(onApproval).toHaveBeenCalledWith("apr_gantry_9");
  });
});

describe("工作区 viewer", () => {
  it("隐藏设置入口、禁用输入与上报操作", async () => {
    await mount("viewer");
    expect(screen.queryByRole("button", { name: "设置" })).toBeNull();
    expect(screen.getByLabelText("给 agent 发消息")).toBeDisabled();

    fireEvent.click(
      within(leftPane()).getByRole("button", { name: /PR #9/ }),
    );
    const approvalButtons = within(mainPane()).getAllByRole("button", {
      name: "审阅并签名…",
    });
    for (const button of approvalButtons) expect(button).toBeDisabled();
    expect(
      within(inspector()).getAllByRole("button", { name: "审阅并签名…" })[0],
    ).toBeDisabled();
  });
});
