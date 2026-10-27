import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useSyncExternalStore } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { NodeStatus, Workflow, WorkflowNode } from "@/api/types";
import type { ProjectUiState } from "@/api/shell";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { initialState } from "@/store/state";
import { defaultProjectUi } from "@/store/ui";
import { Inspector } from "@/features/workspace/Inspector";
import { ExecutionGraph } from "@/features/workspace/ExecutionGraph";
import type { SimulationSummary, SimulationToolActivity } from "@/demo/types";
import { startController } from "./helpers";

const controllers: DesktopController[] = [];

afterEach(async () => {
  await Promise.all(controllers.splice(0).map((controller) => controller.stop()));
});

function GraphHarness({
  controller,
  workflowId,
  deliveryId,
  progress,
  onClose,
}: {
  controller: DesktopController;
  workflowId: string;
  deliveryId?: string;
  progress?: {
    tools: readonly SimulationToolActivity[];
    summaries: readonly SimulationSummary[];
  };
  onClose: () => void;
}): React.ReactNode {
  const state = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  );
  return (
    <ExecutionGraph
      controller={controller}
      state={state}
      workflowId={workflowId}
      deliveryId={deliveryId}
      progress={progress}
      onClose={onClose}
    />
  );
}

function fakeController(
  overrides: Partial<Record<keyof DesktopController, unknown>> = {},
): DesktopController {
  return {
    can: () => true,
    ensureWorkflow: vi.fn(async () => undefined),
    ensureAttempts: vi.fn(async () => undefined),
    ensureBudget: vi.fn(async () => undefined),
    retryNode: vi.fn(async () => undefined),
    ...overrides,
  } as unknown as DesktopController;
}

function makeNode(
  id: string,
  status: NodeStatus,
  options: {
    role?: WorkflowNode["role"];
    depends_on?: string[];
    superseded_by?: string | null;
    verdict?: WorkflowNode["verdict"];
  } = {},
): WorkflowNode {
  return {
    id,
    created_at: "2026-10-24T12:00:00.000Z",
    updated_at: "2026-10-24T12:00:00.000Z",
    workflow_id: "wfl_fixture",
    role: options.role ?? "build",
    tier: "worker",
    exec: "sandbox",
    depends_on: options.depends_on ?? [],
    required: true,
    status,
    verdict: options.verdict ?? null,
    superseded_by: options.superseded_by ?? null,
  };
}

function nodeButton(graph: HTMLElement, id: string): HTMLButtonElement {
  const button = graph.querySelector<HTMLButtonElement>(`[data-node-id="${id}"]`);
  if (!button) throw new Error(`node not found: ${id}`);
  return button;
}

async function graphDialog(): Promise<HTMLElement> {
  return screen.findByLabelText("执行图");
}

async function mountGraph(
  workflowId: string,
  deliveryId?: string,
  progress?: {
    tools: readonly SimulationToolActivity[];
    summaries: readonly SimulationSummary[];
  },
) {
  const { controller } = await startController("default");
  controllers.push(controller);
  const onClose = vi.fn();
  render(
    <GraphHarness
      controller={controller}
      workflowId={workflowId}
      deliveryId={deliveryId}
      progress={progress}
      onClose={onClose}
    />,
  );
  return { controller, onClose };
}

describe("执行图 Inspector 入口", () => {
  it("从检视器打开同一工作流与交付", async () => {
    const { controller } = await startController("default");
    controllers.push(controller);
    await controller.ensureWorkflow("wfl_gantry_7_review");
    const onOpenWorkflow = vi.fn();

    function Harness(): React.ReactNode {
      const state = useSyncExternalStore(
        controller.subscribe,
        controller.getSnapshot,
      );
      const project =
        state.projects.find((item) => item.id === state.currentProjectId) ??
        null;
      const data = state.projectData[state.currentProjectId ?? ""];
      const ui: ProjectUiState = {
        ...defaultProjectUi(),
        sel: { kind: "issue", repo: "LeXwDeX/gantry", number: 2 },
      };
      if (!data) return null;
      return (
        <Inspector
          controller={controller}
          state={state}
          project={project}
          data={data}
          ui={ui}
          inbox={state.inbox}
          needsByDelivery={new Map()}
          isViewer={false}
          onCite={vi.fn()}
          onReplyDelivery={vi.fn()}
          onSelect={vi.fn()}
          onOpenApproval={vi.fn()}
          onOpenWorkflow={onOpenWorkflow}
        />
      );
    }

    render(<Harness />);
    fireEvent.click(
      await screen.findByRole("button", { name: "查看执行图" }),
    );
    expect(onOpenWorkflow).toHaveBeenCalledWith(
      "wfl_gantry_7_review",
      "dlv_gantry_7",
    );
  });
});

describe("执行图节点、尝试与预算", () => {
  it("显示修订、并行节点、已替换节点与真实模拟尝试", async () => {
    await mountGraph("wfl_gantry_7_review");
    const graph = await graphDialog();
    await waitFor(() => {
      expect(nodeButton(graph, "nod_gantry_7_sec")).toBeTruthy();
    });

    expect(within(graph).getByText(/修订 3/)).toBeInTheDocument();
    expect(
      within(graph).getByText(/不是完整历史快照/),
    ).toBeInTheDocument();
    expect(
      nodeButton(graph, "nod_gantry_7_sec").textContent,
    ).toContain("评审 · security");
    expect(
      nodeButton(graph, "nod_gantry_7_perf_old").textContent,
    ).toContain("已被替换");
    expect(nodeButton(graph, "nod_gantry_7_sec").style.left).toBe(
      nodeButton(graph, "nod_gantry_7_perf").style.left,
    );
    expect(nodeButton(graph, "nod_gantry_7_arb").style.left).not.toBe(
      nodeButton(graph, "nod_gantry_7_sec").style.left,
    );

    fireEvent.click(nodeButton(graph, "nod_gantry_7_perf"));
    expect(await within(graph).findByText("尝试 #1")).toBeInTheDocument();
    expect(
      within(graph).getByText(
        "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9",
      ),
    ).toBeInTheDocument();
    expect(within(graph).getByText("输入 44,800")).toBeInTheDocument();
    expect(within(graph).getByText("输出 5,900")).toBeInTheDocument();
    expect(within(graph).getAllByText("完成 · REJECT").length).toBeGreaterThan(0);

    fireEvent.click(nodeButton(graph, "nod_gantry_7_arb"));
    const detail = within(graph).getByRole("complementary", {
      name: "节点详情",
    });
    expect(within(detail).getByText("评审 · security")).toBeInTheDocument();
    expect(within(detail).getByText("评审 · performance")).toBeInTheDocument();
  });

  it("预算只展示已定义维度、上限与消耗，不提供追加", async () => {
    await mountGraph("wfl_gantry_7_review", "dlv_gantry_7");
    const graph = await graphDialog();

    expect(await within(graph).findByText("max.adversarial")).toBeInTheDocument();
    expect(within(graph).getByText("20,000,000")).toBeInTheDocument();
    expect(within(graph).getByText("611,000")).toBeInTheDocument();
    expect(within(graph).getByText("6 小时")).toBeInTheDocument();
    expect(within(graph).getByText("17 分 30 秒")).toBeInTheDocument();
    expect(within(graph).getAllByText("tokens").length).toBeGreaterThan(0);
    expect(within(graph).getAllByText("秒").length).toBeGreaterThan(0);

    const concurrencyRow = within(graph).getByText("并发").closest("tr");
    expect(concurrencyRow).not.toBeNull();
    expect(within(concurrencyRow as HTMLElement).getByText("4")).toBeInTheDocument();
    expect(
      within(concurrencyRow as HTMLElement).getAllByText("—").length,
    ).toBeGreaterThan(0);
    expect(
      within(graph).queryByRole("button", { name: /追加|增加预算|提高预算/ }),
    ).toBeNull();
  });

  it("失败节点经既有 nodes.retry 追加新尝试，旧尝试保留", async () => {
    const { controller } = await mountGraph("wfl_specgit_31_spec");
    const graph = await graphDialog();
    await waitFor(() => {
      expect(nodeButton(graph, "nod_specgit_31_admission")).toBeTruthy();
    });
    fireEvent.click(nodeButton(graph, "nod_specgit_31_admission"));
    expect(await within(graph).findByText("尝试 #1")).toBeInTheDocument();
    expect(within(graph).getByText("verdict_fail")).toBeInTheDocument();
    expect(
      within(graph).getByText("missing acceptance criteria"),
    ).toBeInTheDocument();

    fireEvent.click(
      within(graph).getByRole("button", { name: "重试失败节点" }),
    );
    expect(await within(graph).findByText("尝试 #2", {}, { timeout: 5000 })).toBeInTheDocument();
    expect(within(graph).getByText("尝试 #1")).toBeInTheDocument();
    expect(within(graph).getByText("missing acceptance criteria")).toBeInTheDocument();
    expect(
      controller.getSnapshot().attemptsByNode["nod_specgit_31_admission"],
    ).toHaveLength(2);
    await waitFor(() => {
      expect(
        within(graph).queryByRole("button", { name: "重试失败节点" }),
      ).toBeNull();
    });
  });
});

describe("执行图状态、边界与只读", () => {
  it("覆盖全部节点状态、未知状态与依赖连线", async () => {
    const statuses: Array<NodeStatus | string> = [
      "pending",
      "queued",
      "running",
      "succeeded",
      "failed",
      "skipped",
      "cancelled",
      "superseded",
      "weird",
    ];
    const nodes = statuses.map((status, index) =>
      makeNode(`nod_${status}`, status as NodeStatus, {
        role: index === 2 ? "verify" : "build",
        depends_on: index > 2 ? ["nod_build"] : [],
        superseded_by: status === "superseded" ? "nod_succeeded" : null,
      }),
    );
    // 用真实 id 连接若干依赖，保证图形有可断言的两条边。
    nodes.push(
      makeNode("nod_build", "succeeded", { role: "build" }),
      makeNode("nod_review", "running", {
        role: "review:security",
        depends_on: ["nod_build", "nod_running"],
      }),
    );
    const workflow: Workflow = {
      id: "wfl_fixture",
      created_at: "2026-10-24T12:00:00.000Z",
      updated_at: "2026-10-24T12:00:00.000Z",
      delivery_id: "dlv_fixture",
      conversation_id: null,
      stage: "implement",
      graph_rev: 9,
      status: "running",
      template: "fixture.graph",
      nodes,
    };
    const state: DesktopState = {
      ...initialState(),
      status: "ready",
      workflows: { wfl_fixture: workflow },
    };
    render(
      <ExecutionGraph
        controller={fakeController()}
        state={state}
        workflowId="wfl_fixture"
        deliveryId="dlv_fixture"
        onClose={vi.fn()}
      />,
    );
    const graph = await graphDialog();
    await waitFor(() => {
      expect(nodeButton(graph, "nod_weird")).toBeTruthy();
    });

    expect(nodeButton(graph, "nod_pending").textContent).toContain("等待");
    expect(nodeButton(graph, "nod_queued").textContent).toContain("排队");
    expect(nodeButton(graph, "nod_running").textContent).toContain("运行中");
    expect(nodeButton(graph, "nod_succeeded").textContent).toContain("完成");
    expect(nodeButton(graph, "nod_failed").textContent).toContain("失败");
    expect(nodeButton(graph, "nod_skipped").textContent).toContain("跳过");
    expect(nodeButton(graph, "nod_cancelled").textContent).toContain("取消");
    expect(nodeButton(graph, "nod_superseded").textContent).toContain("已被替换");
    expect(nodeButton(graph, "nod_weird").textContent).toContain(
      "未知状态 · weird",
    );
    expect(graph.querySelectorAll("svg path.eg-edge")).toHaveLength(8);

    fireEvent.click(nodeButton(graph, "nod_weird"));
    expect(
      await within(graph).findByText("尝试记录未载入。"),
    ).toBeInTheDocument();
  });

  it("加载后无工作流时显示错误与重试", async () => {
    let resolveFirst: (() => void) | null = null;
    const ensureWorkflow = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveFirst = resolve;
        }),
    );
    const controller = fakeController({ ensureWorkflow });
    render(
      <ExecutionGraph
        controller={controller}
        state={{ ...initialState(), status: "ready" }}
        workflowId="wfl_missing"
        onClose={vi.fn()}
      />,
    );
    expect(screen.getByText("正在读取执行图…")).toBeInTheDocument();
    await act(async () => {
      resolveFirst?.();
      await Promise.resolve();
    });
    expect(await screen.findByText("执行图未载入。")).toBeInTheDocument();

    const retry = screen.getByRole("button", { name: "重新读取执行图" });
    ensureWorkflow.mockImplementation(async () => undefined);
    fireEvent.click(retry);
    await waitFor(() => expect(ensureWorkflow).toHaveBeenCalledTimes(2));
  });

  it("工作流没有节点时显示空态", async () => {
    const workflow: Workflow = {
      id: "wfl_empty",
      created_at: "2026-10-24T12:00:00.000Z",
      updated_at: "2026-10-24T12:00:00.000Z",
      delivery_id: null,
      conversation_id: null,
      stage: null,
      graph_rev: 1,
      status: "succeeded",
      template: "empty.graph",
      nodes: [],
    };
    const state: DesktopState = {
      ...initialState(),
      status: "ready",
      workflows: { wfl_empty: workflow },
    };
    render(
      <ExecutionGraph
        controller={fakeController()}
        state={state}
        workflowId="wfl_empty"
        onClose={vi.fn()}
      />,
    );
    expect(await screen.findByText("这个工作流没有节点。")).toBeInTheDocument();
    expect(
      await screen.findByText("该工作流没有交付预算台账。"),
    ).toBeInTheDocument();
  });

  it("只读角色不能重试节点，也没有编辑控件", async () => {
    const { controller } = await startController("viewer");
    controllers.push(controller);
    const onClose = vi.fn();
    render(
      <GraphHarness
        controller={controller}
        workflowId="wfl_specgit_31_spec"
        onClose={onClose}
      />,
    );
    const graph = await graphDialog();
    await waitFor(() => {
      expect(nodeButton(graph, "nod_specgit_31_admission")).toBeTruthy();
    });
    fireEvent.click(nodeButton(graph, "nod_specgit_31_admission"));
    expect(await within(graph).findByText("尝试 #1")).toBeInTheDocument();
    expect(controller.can("retry")).toBe(false);
    expect(
      within(graph).queryByRole("button", { name: "重试失败节点" }),
    ).toBeNull();
    expect(within(graph).queryByRole("textbox")).toBeNull();
  });

  it("键盘移动节点，返回按钮关闭执行图", async () => {
    const { onClose } = await mountGraph("wfl_gantry_7_review");
    const graph = await graphDialog();
    await waitFor(() => {
      expect(nodeButton(graph, "nod_gantry_7_sec")).toBeTruthy();
    });
    const security = nodeButton(graph, "nod_gantry_7_sec");
    security.focus();
    fireEvent.keyDown(security, { key: "ArrowRight" });
    expect(nodeButton(graph, "nod_gantry_7_perf")).toHaveFocus();
    expect(
      nodeButton(graph, "nod_gantry_7_perf").getAttribute("aria-pressed"),
    ).toBe("true");

    fireEvent.click(within(graph).getByRole("button", { name: "← 返回" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("关联演示工具与公开执行摘要，并明确标注模拟", async () => {
    const at = "2026-10-24T11:20:00.000Z";
    const tool: SimulationToolActivity = {
      call_id: "call_fixture",
      name: "npm test",
      target: "npm test -- execution-graph",
      status: "failed",
      started_at: at,
      ended_at: "2026-10-24T11:20:01.200Z",
      elapsed_ms: 1_200,
      collapsed_detail: "模拟摘要：命令退出码 1（完整输出未提供）。",
      simulated: true,
      delivery_id: "dlv_specgit_31",
      node_id: "nod_specgit_31_admission",
      attempt_id: "att_specgit_admission_1",
      message_id: "msg_fixture",
    };
    const summary: SimulationSummary = {
      summary_id: "sum_fixture",
      status: "done",
      text: "检查验收标准。",
      simulated: true,
      delivery_id: "dlv_specgit_31",
      node_id: "nod_specgit_31_admission",
      attempt_id: "att_specgit_admission_1",
      message_id: "msg_fixture",
    };
    await mountGraph("wfl_specgit_31_spec", undefined, {
      tools: [tool],
      summaries: [summary],
    });
    const graph = await graphDialog();
    await waitFor(() => {
      expect(nodeButton(graph, "nod_specgit_31_admission")).toBeTruthy();
    });
    fireEvent.click(nodeButton(graph, "nod_specgit_31_admission"));
    expect(await within(graph).findByText("模拟工具（演示）")).toBeInTheDocument();
    expect(within(graph).getByText("npm test")).toBeInTheDocument();
    expect(
      within(graph).getByText("npm test -- execution-graph"),
    ).toBeInTheDocument();
    expect(
      within(graph).getByText("模拟执行摘要（演示）"),
    ).toBeInTheDocument();
    expect(within(graph).getByText("检查验收标准。")).toBeInTheDocument();
  });

  it("尝试为空时不伪造记录", async () => {
    const workflow: Workflow = {
      id: "wfl_empty_attempts",
      created_at: "2026-10-24T12:00:00.000Z",
      updated_at: "2026-10-24T12:00:00.000Z",
      delivery_id: "dlv_empty",
      conversation_id: null,
      stage: "implement",
      graph_rev: 1,
      status: "running",
      template: "empty.graph",
      nodes: [makeNode("nod_empty", "pending")],
    };
    const state: DesktopState = {
      ...initialState(),
      status: "ready",
      workflows: { wfl_empty_attempts: workflow },
      attemptsByNode: { nod_empty: [] },
    };
    render(
      <ExecutionGraph
        controller={fakeController()}
        state={state}
        workflowId="wfl_empty_attempts"
        onClose={vi.fn()}
      />,
    );
    const graph = await graphDialog();
    fireEvent.click(nodeButton(graph, "nod_empty"));
    expect(await within(graph).findByText("没有尝试记录。")).toBeInTheDocument();
  });
});
