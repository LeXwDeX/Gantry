import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { useRef, useSyncExternalStore } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Message } from "@/api/types";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import type { ApiResult } from "@/api/shell";
import { Conversation } from "@/features/workspace/Conversation";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { UnavailableIpcAdapter } from "@/ipc/unavailable";
import { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { emptyProjectData } from "@/store/state";
import { defaultProjectUi } from "@/store/ui";
import type { SimulationScriptId } from "@/demo/types";
import { startController } from "./helpers";

class UnknownCardAdapter extends ScenarioIpcAdapter {
  override async api<R extends RouteId>(
    route: R,
    args: RouteArgs[R],
  ): Promise<ApiResult<RouteBody[R]>> {
    const result = await super.api(route, args);
    if (route !== "messages.list" || !result.ok || !result.body) return result;
    const page = result.body as RouteBody["messages.list"];
    return {
      ...result,
      body: {
        ...page,
        items: page.items.map((message: Message) =>
          message.id === "msg_gantry_6"
            ? {
                ...message,
                cards: [{ kind: "future_card", detail: "客户端未知" }],
              }
            : message,
        ),
      },
    } as ApiResult<RouteBody[R]>;
  }
}

const controllers: DesktopController[] = [];

afterEach(async () => {
  await Promise.all(controllers.splice(0).map((controller) => controller.stop()));
});

function Harness({
  controller,
  onOpenWorkflow,
}: {
  controller: DesktopController;
  onOpenWorkflow?: (workflowId: string, deliveryId?: string) => void;
}) {
  const state: DesktopState = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  );
  const projectId = state.currentProjectId;
  const data = projectId
    ? state.projectData[projectId] ?? emptyProjectData()
    : emptyProjectData();
  const ui = projectId
    ? state.ui[projectId] ?? defaultProjectUi()
    : defaultProjectUi();
  const composerRef = useRef<HTMLTextAreaElement>(null);
  return (
    <Conversation
      projectId={projectId}
      controller={controller}
      data={data}
      ui={ui}
      inbox={state.inbox}
      approvals={state.approvals}
      workflows={state.workflows}
      isViewer={state.me?.device.role === "viewer"}
      platform="macos"
      connectionLive={state.connection?.phase === "live"}
      focusRequest={0}
      scrollTarget={null}
      composerRef={composerRef}
      onUpdate={(patch) => controller.updateUi(patch)}
      onSelect={() => undefined}
      onReply={() => undefined}
      onOpenApproval={() => undefined}
      onOpenExternal={() => undefined}
      onScrolled={() => undefined}
      onOpenWorkflow={onOpenWorkflow}
    />
  );
}

async function mount() {
  const { adapter, controller } = await startController();
  controllers.push(controller);
  const onOpenWorkflow = vi.fn();
  const view = render(
    <Harness controller={controller} onOpenWorkflow={onOpenWorkflow} />,
  );
  return { adapter, controller, onOpenWorkflow, ...view };
}

function selectScript(
  adapter: Awaited<ReturnType<typeof mount>>["adapter"],
  controller: DesktopController,
  id: SimulationScriptId,
): void {
  const projectId = controller.getSnapshot().currentProjectId;
  expect(projectId).toBeTruthy();
  const result = adapter.simulation.select(id, { projectId: projectId! });
  expect(result.ok).toBe(true);
}

async function send(controller: DesktopController, text: string): Promise<void> {
  act(() => controller.updateUi({ draft: text }));
  await act(async () => {
    controller.send();
    await Promise.resolve();
  });
}

function chatText(): string {
  return screen.getByLabelText("对话").textContent ?? "";
}

function expandScenarioControls(): void {
  fireEvent.click(screen.getByRole("button", { name: /开发场景/ }));
}

describe("对话模拟边界", () => {
  it("普通问答分段生成后结算为正式消息，摘要可展开且不重复临时文本", async () => {
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "qa");
    await send(controller, "解释模拟边界");

    const live = await screen.findByLabelText(
      "模拟生成",
      {},
      { timeout: 2500 },
    );
    expect(live.textContent).toContain("模拟回答");
    expect(live.textContent).toContain("生成中");
    expect(live.querySelectorAll("[data-segment]").length).toBeGreaterThan(0);

    await waitFor(
      () => {
        expect(screen.queryByLabelText("模拟生成")).toBeNull();
      },
      { timeout: 3000 },
    );

    expect(chatText()).toContain("场景只使用内存数据");
    expect(chatText()).not.toContain("第 1/3 步");
    const summary = screen.getByRole("button", { name: "模拟执行摘要" });
    expect(summary).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(summary);
    expect(summary).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/公开摘要：读取场景上下文/)).toBeInTheDocument();
  });

  it("多节点任务工具按 call id 匹配，折叠状态在进度更新中保持，计划卡可上抛执行图", async () => {
    const { adapter, controller, onOpenWorkflow } = await mount();
    selectScript(adapter, controller, "task");
    await send(controller, "实现模拟任务");

    await waitFor(
      () => {
        expect(document.querySelectorAll("[data-call-id]").length).toBe(2);
      },
      { timeout: 4000 },
    );

    const tool = document.querySelector("[data-call-id]") as HTMLElement;
    const callId = tool.getAttribute("data-call-id")!;
    expect(tool.getAttribute("data-status")).toBe("running");
    expect(tool.textContent).toContain("npm test -- --runInBand");
    expect(tool.textContent).toContain("运行中");
    fireEvent.click(within(tool).getByRole("button", { name: "详情" }));
    expect(within(tool).getByRole("button", { name: "收起" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );

    const summary = screen.getByRole("button", { name: "模拟执行摘要" });
    fireEvent.click(summary);
    expect(summary).toHaveAttribute("aria-expanded", "true");

    await waitFor(
      () => {
        const current = document.querySelector(`[data-call-id="${callId}"]`);
        expect(current?.getAttribute("data-status")).toBe("succeeded");
      },
      { timeout: 5000 },
    );

    const finished = document.querySelector(
      `[data-call-id="${callId}"]`,
    ) as HTMLElement;
    expect(finished.textContent).toContain("成功");
    expect(finished.textContent).toMatch(/\d+ ms/);
    expect(within(finished).getByRole("button", { name: "收起" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );
    expect(finished.textContent).toContain("退出码 0");
    expect(screen.getByRole("button", { name: "模拟执行摘要" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );

    await waitFor(
      () => {
        expect(chatText()).toContain("模拟任务完成");
      },
      { timeout: 3000 },
    );

    const opens = screen.getAllByRole("button", { name: "查看执行图" });
    const open = opens[opens.length - 1];
    expect(open).toBeEnabled();
    fireEvent.click(open);
    expect(onOpenWorkflow).toHaveBeenCalledWith(
      expect.stringMatching(/^wfl_sim_/),
      expect.stringMatching(/^dlv_sim_/),
    );
  });

  it("失败后重试用新 call id 关联新尝试，旧失败调用保持不变", async () => {
    // 失败脚本与重试共约 2.5s；给测试留出真实计时余量。
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "failure_retry");
    await send(controller, "触发失败");

    await waitFor(
      () => {
        expect(
          document.querySelector('[data-call-id][data-status="failed"]'),
        ).not.toBeNull();
      },
      { timeout: 3500 },
    );
    const failed = document.querySelector(
      '[data-call-id][data-status="failed"]',
    ) as HTMLElement;
    const callId = failed.getAttribute("data-call-id")!;
    expect(failed.textContent).toContain("失败");
    expect(chatText()).toContain("模拟构建失败");

    const failedActivity = adapter.simulation
      .getSnapshot()
      .activities.find((item) => item.status === "failed");
    const nodeId = failedActivity?.node_id;
    const firstAttempt = failedActivity?.attempt_id;
    expect(nodeId).toBeTruthy();
    await act(async () => {
      await controller.retryNode(nodeId!);
      await controller.ensureAttempts(nodeId!);
    });

    await waitFor(
      () => {
        expect(
          adapter.simulation.getSnapshot().activities.length,
        ).toBeGreaterThan(2);
      },
      { timeout: 4000 },
    );
    const newActivities = adapter.simulation
      .getSnapshot()
      .activities.filter((activity) => activity.attempt_id !== firstAttempt);
    expect(newActivities.length).toBeGreaterThan(0);
    expect(newActivities.every((activity) => activity.call_id !== callId)).toBe(
      true,
    );
    await waitFor(
      () => {
        const attempts = controller.getSnapshot().attemptsByNode[nodeId!] ?? [];
        expect(
          attempts.some((attempt) => attempt.n > 1 && attempt.status === "succeeded"),
        ).toBe(true);
      },
      { timeout: 4000 },
    );
    await waitFor(
      () => {
        expect(chatText()).toContain("重试成功");
      },
      { timeout: 3000 },
    );
    // 场景驱动重试时没有新增工具调用；旧调用必须保持失败的公开事实，不伪造成功。
    const after = document.querySelector(
      `[data-call-id="${callId}"]`,
    ) as HTMLElement;
    expect(after.getAttribute("data-status")).toBe("failed");
    expect(chatText()).toContain("新尝试已完成");
  }, 10_000);

  it("取消生成后无迟到回复，重置清理模拟事实", async () => {
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "task");
    await send(controller, "中断任务");

    await waitFor(
      () => {
        expect(document.querySelectorAll("[data-call-id]").length).toBe(2);
      },
      { timeout: 4000 },
    );
    expandScenarioControls();
    const messageCount = controller.currentData.messages.length;
    fireEvent.click(screen.getByRole("button", { name: "取消生成" }));

    await waitFor(
      () => {
        expect(chatText()).toContain("已中断");
      },
      { timeout: 1000 },
    );
    await new Promise((resolve) => setTimeout(resolve, 900));
    expect(controller.currentData.messages.length).toBe(messageCount);
    expect(chatText()).not.toContain("模拟任务完成");

    fireEvent.click(screen.getByRole("button", { name: "重置场景" }));
    await waitFor(
      () => {
        expect(controller.currentData.messages.length).toBeLessThan(messageCount);
      },
      { timeout: 2000 },
    );
    expect(document.querySelector("[data-call-id]")).toBeNull();
  });

  it("切换项目时不显示其他项目的模拟生成，消息仍写回原项目", async () => {
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "qa");
    const origin = controller.getSnapshot().currentProjectId!;
    const other = controller.getSnapshot().projects.find(
      (project) => project.id !== origin,
    )!;
    await send(controller, "项目隔离问答");
    expect(
      await screen.findByLabelText("模拟生成", {}, { timeout: 2000 }),
    ).toBeInTheDocument();

    act(() => controller.selectProject(other.id));
    expect(screen.queryByLabelText("模拟生成")).toBeNull();
    expect(chatText()).not.toContain("场景只使用内存数据");

    await waitFor(
      () => {
        expect(adapter.simulation.getSnapshot().status).toBe("completed");
      },
      { timeout: 3000 },
    );

    act(() => controller.selectProject(origin));
    await waitFor(
      () => {
        expect(chatText()).toContain("场景只使用内存数据");
      },
      { timeout: 2000 },
    );
  });

  it("不可信与长内容只按文本显示", async () => {
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "qa");
    await send(controller, `${"长".repeat(5000)}<img src=x onerror="window.__xss=1">`);
    await waitFor(
      () => {
        expect(chatText()).toContain("场景只使用内存数据");
      },
      { timeout: 3000 },
    );
    const chat = screen.getByLabelText("对话");
    expect(chat.querySelector("img")).toBeNull();
    expect(chat.textContent).toContain("<img src=x");
    expect(chat.textContent).toContain("长".repeat(100));
    expect(chat.textContent!.length).toBeGreaterThan(5000);
    expect((window as { __xss?: unknown }).__xss).toBeUndefined();
  });

  it("未知卡片安全降级为文字提示", async () => {
    const adapter = new UnknownCardAdapter();
    const controller = new DesktopController(adapter, {
      refreshDelayMs: 0,
      persistDelayMs: 0,
      prefetchWorkflows: false,
      now: () => Date.parse("2026-10-24T12:59:00.000Z"),
    });
    controllers.push(controller);
    render(<Harness controller={controller} />);
    await act(async () => {
      await controller.start();
    });
    await waitFor(() => {
      expect(controller.getSnapshot().status).toBe("ready");
    });
    expect(
      await screen.findByText("此卡片需要更新客户端后才能显示。"),
    ).toBeInTheDocument();
  });

  it("场景工具默认折叠，可选择脚本、单步暂停回放并再次折叠", async () => {
    const { adapter } = await mount();
    expect(screen.queryByLabelText("演示脚本")).toBeNull();
    expandScenarioControls();
    const toggle = screen.getByRole("button", { name: /开发场景/ });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    fireEvent.change(screen.getByLabelText("演示脚本"), {
      target: { value: "qa" },
    });
    expect(adapter.simulation.getSnapshot().script_id).toBe("qa");

    fireEvent.click(screen.getByRole("button", { name: "单步" }));
    expect(adapter.simulation.getSnapshot().drafts).toHaveLength(1);
    expect(screen.getByLabelText("模拟生成")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "暂停" }));
    expect(adapter.simulation.getSnapshot().status).toBe("paused");
    fireEvent.click(screen.getByRole("button", { name: "回放" }));
    await waitFor(
      () => {
        expect(adapter.simulation.getSnapshot().status).toBe("completed");
      },
      { timeout: 3000 },
    );

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByLabelText("演示脚本")).toBeNull();
  });

  it("断线后恢复继续同一任务，并可切换只读角色", async () => {
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "offline_switch");
    await send(controller, "断线恢复任务");

    await waitFor(
      () => {
        expect(adapter.simulation.getSnapshot().awaiting?.kind).toBe("connection");
      },
      { timeout: 2000 },
    );
    expect(chatText()).toContain("连接中断，恢复后继续同一任务");
    expandScenarioControls();
    const restore = await screen.findByRole("button", { name: "恢复连接" });
    fireEvent.click(restore);

    await waitFor(
      () => {
        expect(adapter.simulation.getSnapshot().status).toBe("completed");
      },
      { timeout: 3000 },
    );
    await waitFor(
      () => {
        expect(chatText()).toContain("断线恢复演示完成");
      },
      { timeout: 2000 },
    );
    expect(chatText()).toContain("连接已恢复");

    const readonly = screen.getByRole("button", { name: "切换为只读" });
    fireEvent.click(readonly);
    await waitFor(
      () => {
        expect(controller.getSnapshot().me?.device.role).toBe("viewer");
      },
      { timeout: 1000 },
    );
    expect(screen.getByLabelText("给 agent 发消息")).toBeDisabled();
  });

  it("用输入框发送后不夺走焦点", async () => {
    const { adapter, controller } = await mount();
    selectScript(adapter, controller, "qa");
    const textarea = screen.getByLabelText("给 agent 发消息");
    fireEvent.change(textarea, { target: { value: "焦点保持" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(document.activeElement).toBe(textarea);

    await waitFor(
      () => {
        expect(chatText()).toContain("场景只使用内存数据");
      },
      { timeout: 3000 },
    );
    expect(document.activeElement).toBe(textarea);
    expect(screen.getByLabelText("给 agent 发消息")).toHaveValue("");
  });

  it("非场景适配器不渲染场景工具或模拟生成事实", () => {
    const controller = new DesktopController(new UnavailableIpcAdapter());
    controllers.push(controller);
    render(<Harness controller={controller} />);
    expect(screen.queryByRole("button", { name: "开发场景" })).toBeNull();
    expect(screen.queryByLabelText("模拟生成")).toBeNull();
  });
});
