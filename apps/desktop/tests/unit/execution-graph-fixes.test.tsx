import { readFileSync } from "node:fs";
import path from "node:path";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Attempt, NodeStatus, Workflow, WorkflowNode } from "@/api/types";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { initialState } from "@/store/state";
import { ExecutionGraph } from "@/features/workspace/ExecutionGraph";

const ROOT = path.resolve(process.cwd());

function css(file: string): string {
  return readFileSync(path.join(ROOT, "src", file), "utf8");
}

function tokens(): Map<string, string> {
  const map = new Map<string, string>();
  for (const match of css("theme/tokens.css").matchAll(
    /--([\w-]+):\s*(#[0-9a-fA-F]{6})\s*;/g,
  )) {
    map.set(match[1], match[2]);
  }
  return map;
}

function luminance(hex: string): number {
  const value = Number.parseInt(hex.slice(1), 16);
  const channels = [(value >> 16) & 255, (value >> 8) & 255, value & 255].map(
    (channel) => {
      const ratio = channel / 255;
      return ratio <= 0.03928
        ? ratio / 12.92
        : ((ratio + 0.055) / 1.055) ** 2.4;
    },
  );
  return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
}

function contrast(foreground: string, background: string): number {
  const first = luminance(foreground);
  const second = luminance(background);
  const [light, dark] = first > second ? [first, second] : [second, first];
  return (light + 0.05) / (dark + 0.05);
}

function makeNode(
  id: string,
  status: NodeStatus,
  options: {
    role?: WorkflowNode["role"];
    superseded_by?: string | null;
  } = {},
): WorkflowNode {
  return {
    id,
    created_at: "2026-10-24T12:00:00.000Z",
    updated_at: "2026-10-24T12:00:00.000Z",
    workflow_id: "wfl_contrast",
    role: options.role ?? "build",
    tier: "worker",
    exec: "sandbox",
    depends_on: [],
    required: true,
    status,
    verdict: null,
    superseded_by: options.superseded_by ?? null,
  };
}

function makeAttempt(
  id: string,
  nodeId: string,
  status: Attempt["status"],
): Attempt {
  return {
    id,
    created_at: "2026-10-24T12:00:00.000Z",
    updated_at: "2026-10-24T12:00:00.000Z",
    node_id: nodeId,
    n: 1,
    sandbox_id: null,
    status,
    started_at: "2026-10-24T11:20:00.000Z",
    ended_at: "2026-10-24T11:21:00.000Z",
    result_ref: "sha256:fixture",
    failure_class: null,
    failure_detail: null,
    usage: {
      input_tokens: 1,
      output_tokens: 1,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      requests: 1,
    },
  };
}

function fakeController(): DesktopController {
  return {
    can: () => true,
    ensureWorkflow: vi.fn(async () => undefined),
    ensureAttempts: vi.fn(async () => undefined),
    ensureBudget: vi.fn(async () => undefined),
    retryNode: vi.fn(async () => undefined),
  } as unknown as DesktopController;
}

function makeState(
  status: Workflow["status"],
  nodes: WorkflowNode[],
  attemptsByNode: Record<string, Attempt[]> = {},
): DesktopState {
  const workflow: Workflow = {
    id: "wfl_contrast",
    created_at: "2026-10-24T12:00:00.000Z",
    updated_at: "2026-10-24T12:00:00.000Z",
    delivery_id: null,
    conversation_id: null,
    stage: "implement",
    graph_rev: 1,
    status,
    template: "contrast.graph",
    nodes,
  };
  return {
    ...initialState(),
    status: "ready",
    workflows: { wfl_contrast: workflow },
    attemptsByNode,
  };
}

function mountGraph(state: DesktopState, onClose = vi.fn()): void {
  render(
    <ExecutionGraph
      controller={fakeController()}
      state={state}
      workflowId="wfl_contrast"
      onClose={onClose}
    />,
  );
}

describe("执行图终态文字对比度", () => {
  it("完成态工作流状态使用通过令牌，而非低对比度闲置色", async () => {
    mountGraph(makeState("succeeded", [makeNode("nod_done", "succeeded")]));
    const graph = await screen.findByLabelText("执行图");
    const head = graph.querySelector<HTMLElement>(".eg__graph-head");
    if (!head) throw new Error("missing graph head");
    const status = within(head).getByText("完成");
    expect(status).toHaveClass("tone-text-ok");
    expect(status).not.toHaveClass("tone-text-idle");
  });

  it("失败态工作流状态使用异常令牌", async () => {
    mountGraph(makeState("failed", [makeNode("nod_failed", "failed")]));
    const graph = await screen.findByLabelText("执行图");
    const head = graph.querySelector<HTMLElement>(".eg__graph-head");
    if (!head) throw new Error("missing graph head");
    const status = within(head).getByText("失败");
    expect(status).toHaveClass("tone-text-warn");
    expect(status).not.toHaveClass("tone-text-idle");
  });

  it("取消、替换等中性状态文字改用可访问的中性文字类", async () => {
    mountGraph(
      makeState("cancelled", [
        makeNode("nod_cancelled", "cancelled"),
        makeNode("nod_superseded", "superseded", {
          superseded_by: "nod_cancelled",
        }),
      ]),
    );
    const graph = await screen.findByLabelText("执行图");
    const head = graph.querySelector<HTMLElement>(".eg__graph-head");
    if (!head) throw new Error("missing graph head");
    expect(within(head).getByText("已取消")).toHaveClass(
      "eg-status-text--neutral",
    );
    expect(within(head).getByText("已取消")).not.toHaveClass("tone-text-idle");

    fireEvent.click(
      graph.querySelector<HTMLButtonElement>('[data-node-id="nod_cancelled"]')!,
    );
    const detail = await within(graph).findByRole("complementary", {
      name: "节点详情",
    });
    const detailStatus = detail.querySelector<HTMLElement>(
      ".eg-detail__status",
    );
    expect(detailStatus).toHaveClass("eg-status-text--neutral");
    expect(detailStatus).not.toHaveClass("tone-text-idle");
    expect(detailStatus).toHaveTextContent("取消");
  });

  it("取消的尝试状态同样不使用低对比度闲置色", async () => {
    mountGraph(
      makeState("cancelled", [makeNode("nod_cancelled", "cancelled")], {
        nod_cancelled: [makeAttempt("att_1", "nod_cancelled", "cancelled")],
      }),
    );
    const graph = await screen.findByLabelText("执行图");
    fireEvent.click(
      graph.querySelector<HTMLButtonElement>('[data-node-id="nod_cancelled"]')!,
    );
    await waitFor(() => {
      expect(graph.querySelector(".eg-attempt__head")).not.toBeNull();
    });
    const attemptHead = graph.querySelector<HTMLElement>(".eg-attempt__head");
    const status = within(attemptHead!).getByText("取消");
    expect(status).toHaveClass("eg-status-text--neutral");
    expect(status).not.toHaveClass("tone-text-idle");
  });

  it("中性状态文字令牌对图内背景达到 AA 4.5:1", () => {
    const colors = tokens();
    const rule = css("features/workspace/execution-graph.css").match(
      /\.eg-status-text--neutral\s*\{([^}]*)\}/,
    );
    expect(rule?.[1]).toMatch(/color:\s*var\(--text-muted\)/);

    const neutral = colors.get("text-muted");
    expect(neutral).toBeDefined();
    for (const background of ["bg", "panel", "card", "deep"]) {
      const color = colors.get(background);
      expect(color).toBeDefined();
      expect(contrast(neutral!, color!)).toBeGreaterThanOrEqual(4.5);
    }

    for (const tone of ["run", "ok", "warn"]) {
      const color = colors.get(tone);
      expect(color).toBeDefined();
      expect(contrast(color!, colors.get("bg")!)).toBeGreaterThanOrEqual(4.5);
    }
  });

  it("保留节点键盘移动与返回按钮关闭", async () => {
    const onClose = vi.fn();
    mountGraph(
      makeState("succeeded", [
        makeNode("nod_a", "succeeded"),
        makeNode("nod_b", "failed"),
      ]),
      onClose,
    );
    const graph = await screen.findByLabelText("执行图");
    const first = graph.querySelector<HTMLButtonElement>('[data-node-id="nod_a"]');
    const second = graph.querySelector<HTMLButtonElement>('[data-node-id="nod_b"]');
    first!.focus();
    fireEvent.keyDown(first!, { key: "ArrowRight" });
    expect(second).toHaveFocus();
    fireEvent.click(within(graph).getByRole("button", { name: "← 返回" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
