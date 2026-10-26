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
import type { SettingsDocument } from "@/api/types";
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

type Started = Awaited<ReturnType<typeof startController>>;

async function openProjectPage(
  projectId = "prj_gantry",
  scenario: Parameters<typeof startController>[0] = "default",
): Promise<{
  controller: DesktopController;
  adapter: Started["adapter"];
  onClose: ReturnType<typeof vi.fn>;
}> {
  const { controller, adapter } = await startController(scenario);
  const onClose = vi.fn();
  render(
    <Live controller={controller}>
      {(state) => (
        <SettingsOverlay
          controller={controller}
          state={state}
          initialPage={`project:${projectId}`}
          onClose={onClose}
        />
      )}
    </Live>,
  );
  await screen.findByRole("button", { name: "模型覆盖" });
  return { controller, adapter, onClose };
}

async function readOverride(
  adapter: Started["adapter"],
  projectId: string,
  resource: string,
): Promise<SettingsDocument> {
  const result = await adapter.api("project_settings.get", {
    projectId,
    resource,
  });
  expect(result.ok).toBe(true);
  if (!result.ok || !result.body) throw new Error("no project settings doc");
  return result.body;
}

function openTab(name: string): void {
  fireEvent.click(screen.getByRole("button", { name: `${name}覆盖` }));
}

function fieldGroup(label: string): HTMLElement {
  return screen.getByRole("group", { name: `${label} 覆盖行` });
}

async function savePreview(name: string): Promise<void> {
  fireEvent.click(screen.getByRole("button", { name: `保存${name}覆盖` }));
  await screen.findByRole("button", { name: "模拟应用设置" });
}

async function applyPreview(): Promise<void> {
  const button = screen.getByRole("button", { name: "模拟应用设置" });
  await waitFor(() => expect(button).toBeEnabled());
  fireEvent.click(button);
  await screen.findByText(/模拟设置已应用/);
}

function expectSubmitDisabled(): void {
  expect(screen.getByRole("button", { name: "模拟应用设置" })).toBeDisabled();
}

describe("项目覆盖：六类资源", () => {
  it("六类资源入口与代表性可覆盖字段都出现，而不是只有默认档位和模型", async () => {
    const { controller } = await openProjectPage();
    try {
      expect(
        screen.getByLabelText("干活 降级模型 项目覆盖"),
      ).toBeInTheDocument();
      expect(screen.getByLabelText("干活 随档推理 low")).toBeInTheDocument();

      openTab("档位与预算");
      expect(screen.getByLabelText("high nodes 项目覆盖")).toBeInTheDocument();

      openTab("危险动作策略");
      expect(screen.getByLabelText("动作 merge_pr 项目覆盖")).toBeInTheDocument();
      expect(screen.getAllByText(/全局继承：/).length).toBeGreaterThan(0);

      openTab("后台守护");
      expect(screen.getByLabelText("分流 nit 项目覆盖")).toBeInTheDocument();

      openTab("沙盒");
      expect(screen.getByLabelText("限制 mem_mb 项目覆盖")).toBeInTheDocument();

      openTab("出网档案");
      expect(
        screen.getByLabelText("档案 default 的分组 项目覆盖"),
      ).toBeInTheDocument();
      expect(
        screen.getByLabelText("档案 model_only 的分组 项目覆盖"),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("模型：全局/覆盖/生效值同屏，正常收紧并回读一致", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      const group = fieldGroup("干活 模型");
      expect(screen.getByRole("note", { name: "干活 模型 全局值" })).toHaveTextContent(
        "claude-sonnet-5",
      );
      expect(within(group).getByTitle("生效值（只读）")).toHaveTextContent(
        "claude-sonnet-5",
      );
      fireEvent.change(screen.getByLabelText("干活 模型 项目覆盖"), {
        target: { value: "claude-opus-4-1" },
      });
      await savePreview("模型");
      expect(screen.getByRole("region", { name: "项目覆盖保存" })).toBeInTheDocument();
      expect(screen.getByLabelText("覆盖写入预览").textContent).toContain(
        "claude-opus-4-1",
      );
      await applyPreview();
      expect(screen.getByText(/回读一致/)).toBeInTheDocument();
      const doc = await readOverride(adapter, "prj_gantry", "models");
      const worker = (doc.value as { worker: { model: string } }).worker;
      expect(worker.model).toBe("claude-opus-4-1");
    } finally {
      await controller.stop();
    }
  });

  it("模型：降级模型与随档推理映射可编辑并回读", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      fireEvent.change(screen.getByLabelText("大脑 提供方 项目覆盖"), {
        target: { value: "anthropic" },
      });
      fireEvent.change(screen.getByLabelText("大脑 模型 项目覆盖"), {
        target: { value: "claude-sonnet-4-5" },
      });
      fireEvent.change(screen.getByLabelText("大脑 降级模型 项目覆盖"), {
        target: { value: "anthropic/claude-sonnet-4-5" },
      });
      fireEvent.change(screen.getByLabelText("干活 随档推理 medium"), {
        target: { value: "high" },
      });
      await savePreview("模型");
      expect(screen.getByLabelText("覆盖写入预览").textContent).toContain(
        "effort_by_level",
      );
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "models");
      const value = doc.value as {
        planner: { fallback: string[] };
        worker: { effort_by_level: { medium: { default: string } } };
      };
      expect(value.planner.fallback).toEqual([
        "anthropic/claude-sonnet-4-5",
      ]);
      expect(value.worker.effort_by_level.medium.default).toBe("high");
    } finally {
      await controller.stop();
    }
  });

  it("模型：新增全局不存在的提供方被拒绝，不能提交", async () => {
    const { controller } = await openProjectPage();
    try {
      fireEvent.change(screen.getByLabelText("干活 提供方 项目覆盖"), {
        target: { value: "other-provider" },
      });
      await savePreview("模型");
      expect(await screen.findByText(/项目不能新增提供方/)).toBeInTheDocument();
      expectSubmitDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("档位：收紧预算并回读，放宽 None 每日上限被拒绝", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      openTab("档位与预算");
      expect(
        screen.getByRole("note", { name: "high nodes 全局值" }),
      ).toHaveTextContent("40");
      fireEvent.change(screen.getByLabelText("high nodes 项目覆盖"), {
        target: { value: "30" },
      });
      await savePreview("档位与预算");
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "levels");
      expect(
        (doc.value as { high: { budget: { nodes: number } } }).high.budget.nodes,
      ).toBe(30);

      fireEvent.change(screen.getByLabelText("None 档每日 token 上限 项目覆盖"), {
        target: { value: "600000" },
      });
      await savePreview("档位与预算");
      expect(await screen.findByText(/只能收紧全局上限/)).toBeInTheDocument();
      expectSubmitDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("档位：本地范围校验阻止预览，不发出受保护写入", async () => {
    const { controller, adapter } = await openProjectPage();
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    try {
      openTab("档位与预算");
      fireEvent.change(screen.getByLabelText("high nodes 项目覆盖"), {
        target: { value: "0" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存档位与预算覆盖" }));
      expect(
        await screen.findByText(/请先修正标出的 1 项字段错误/),
      ).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();
      expect(prepare).not.toHaveBeenCalled();
    } finally {
      await controller.stop();
    }
  });

  it("档位：恢复继承后清空覆盖提交 DELETE，并回退到全局生效值", async () => {
    const { controller, adapter } = await openProjectPage("prj_specgit");
    try {
      openTab("档位与预算");
      const group = fieldGroup("默认档位上限");
      expect(within(group).getByLabelText("默认档位上限 项目覆盖")).toHaveValue(
        "High",
      );
      fireEvent.click(within(group).getByRole("button", { name: "恢复继承" }));
      await savePreview("档位与预算");
      expect(screen.getByText(/恢复继承（DELETE）/)).toBeInTheDocument();
      await applyPreview();
      const doc = await readOverride(adapter, "prj_specgit", "levels");
      expect(doc.value).toBeNull();
      expect(within(group).getByTitle("生效值（只读）")).toHaveTextContent(
        "Medium",
      );
    } finally {
      await controller.stop();
    }
  });

  it("策略：收紧补丁上限成功，放宽动作处置被拒绝", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      openTab("危险动作策略");
      fireEvent.change(screen.getByLabelText("补丁 max_files 项目覆盖"), {
        target: { value: "50" },
      });
      await savePreview("危险动作策略");
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "policies");
      expect(
        (doc.value as { patch: { max_files: number } }).patch.max_files,
      ).toBe(50);

      fireEvent.change(
        screen.getByLabelText("动作 resolve_review_thread_human 项目覆盖"),
        { target: { value: "approve" } },
      );
      await savePreview("危险动作策略");
      expect(await screen.findByText(/动作处置只能收紧/)).toBeInTheDocument();
      expectSubmitDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("守护：自由字段保存成功，分流放宽被拒绝", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      openTab("后台守护");
      fireEvent.change(screen.getByLabelText("状态清单最小更新间隔 项目覆盖"), {
        target: { value: "45" },
      });
      await savePreview("后台守护");
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "guardian");
      expect(
        (doc.value as { status_checklist: { min_update_interval_s: number } })
          .status_checklist.min_update_interval_s,
      ).toBe(45);

      fireEvent.change(screen.getByLabelText("无进展 same_class_repeats 项目覆盖"), {
        target: { value: "3" },
      });
      await savePreview("后台守护");
      expect(await screen.findByText(/阈值只能收紧/)).toBeInTheDocument();
      expectSubmitDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("沙盒：收紧内存成功，放宽内存被拒绝", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      openTab("沙盒");
      fireEvent.change(screen.getByLabelText("限制 mem_mb 项目覆盖"), {
        target: { value: "1024" },
      });
      await savePreview("沙盒");
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "sandbox");
      expect((doc.value as { limits: { mem_mb: number } }).limits.mem_mb).toBe(
        1024,
      );

      fireEvent.change(screen.getByLabelText("限制 mem_mb 项目覆盖"), {
        target: { value: "8192" },
      });
      await savePreview("沙盒");
      expect(await screen.findByText(/沙盒限制只能收紧/)).toBeInTheDocument();
      expectSubmitDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("出网：收窄分组成功，加入全局档案之外的分组被拒绝", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      openTab("出网档案");
      const input = screen.getByLabelText("档案 default 的分组 项目覆盖");
      expect(input).toHaveValue("model_api\nnpm");
      fireEvent.change(input, { target: { value: "model_api" } });
      await savePreview("出网档案");
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "egress");
      expect(
        (doc.value as { profiles: { default: { groups: string[] } } }).profiles
          .default.groups,
      ).toEqual(["model_api"]);

      fireEvent.change(input, { target: { value: "model_api\nprivate_npm" } });
      await savePreview("出网档案");
      expect(
        await screen.findByText(/出网分组必须是全局档案的子集/),
      ).toBeInTheDocument();
      expectSubmitDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("全局后来收紧时显示 clamped，但不改写已保存的项目覆盖", async () => {
    const { controller, adapter } = await startController();
    try {
      const global = await adapter.api("settings.get", { resource: "levels" });
      expect(global.ok).toBe(true);
      if (!global.ok || !global.body) throw new Error("no global levels");
      const prepared = await adapter.guardedWritePrepare({
        kind: "settings_put",
        scope: "global",
        resource: "levels",
        value: {
          ...(global.body.value as Record<string, unknown>),
          none: { daily_tokens: 300_000 },
        },
        ifMatch: `"${global.body.rev}"`,
      });
      expect(prepared.errors).toEqual([]);
      const applied = await adapter.guardedWriteSubmit({
        pendingId: prepared.pending_id,
      });
      expect(applied.ok).toBe(true);

      const onClose = vi.fn();
      render(
        <Live controller={controller}>
          {(state) => (
            <SettingsOverlay
              controller={controller}
              state={state}
              initialPage="project:prj_gantry"
              onClose={onClose}
            />
          )}
        </Live>,
      );
      await screen.findByRole("button", { name: "模型覆盖" });
      openTab("档位与预算");
      const group = fieldGroup("None 档每日 token 上限");
      expect(
        screen.getByRole("note", { name: "None 档每日 token 上限 全局值" }),
      ).toHaveTextContent("300000");
      expect(within(group).getByTitle("生效值（只读）")).toHaveTextContent(
        "300000",
      );
      expect(
        within(group).getByLabelText("None 档每日 token 上限 项目覆盖"),
      ).toHaveValue(400000);
      expect(screen.getByText("已按全局收紧")).toBeInTheDocument();
      expect(screen.getByText(/生效值被全局约束收紧/)).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });
});

describe("项目覆盖：草稿、冲突与失败恢复", () => {
  it("跨资源保留草稿，跨项目与关闭会先确认", async () => {
    const { controller, onClose } = await openProjectPage();
    try {
      fireEvent.change(screen.getByLabelText("干活 模型 项目覆盖"), {
        target: { value: "claude-opus-4-1" },
      });
      expect(await screen.findByText("有未保存的改动")).toBeInTheDocument();
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "specgit" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "继续编辑" }));
      expect(
        screen.getByRole("heading", { name: /项目 · gantry/ }),
      ).toBeInTheDocument();

      openTab("档位与预算");
      const modelsTab = screen.getByRole("button", { name: "模型覆盖" });
      expect(within(modelsTab).getByText("未保存")).toBeInTheDocument();
      openTab("模型");
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
        "claude-opus-4-1",
      );

      fireEvent.click(screen.getByRole("button", { name: "返回工作区" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "放弃改动并返回" }));
      expect(onClose).toHaveBeenCalledTimes(1);
    } finally {
      await controller.stop();
    }
  });

  it("无草稿时可切换项目，页面显示新项目的覆盖值", async () => {
    const { controller } = await openProjectPage();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "specgit" }));
      await screen.findByRole("button", { name: "模型覆盖" });
      expect(
        screen.getByRole("heading", { name: /项目 · specgit/ }),
      ).toBeInTheDocument();
      openTab("档位与预算");
      expect(screen.getByLabelText("默认档位上限 项目覆盖")).toHaveValue("High");
      const levelsTab = screen.getByRole("button", { name: "档位与预算覆盖" });
      expect(within(levelsTab).getByText("覆盖 1 项")).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("412 冲突显示横幅，载入最新值后丢弃我的改动", async () => {
    const { controller, adapter } = await openProjectPage();
    try {
      fireEvent.change(screen.getByLabelText("干活 模型 项目覆盖"), {
        target: { value: "claude-opus-4-1" },
      });
      await savePreview("模型");
      adapter.simulation.injectError("settings_conflict");
      fireEvent.click(screen.getByRole("button", { name: "模拟应用设置" }));
      expect(await screen.findByText(/提交冲突/)).toBeInTheDocument();
      fireEvent.click(
        screen.getByRole("button", { name: "载入最新值（丢弃我的改动）" }),
      );
      await waitFor(() => expect(screen.queryByText(/提交冲突/)).toBeNull());
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
        "claude-sonnet-5",
      );
    } finally {
      await controller.stop();
    }
  });

  it("提交失败保留草稿，重新预览后可再次模拟应用", async () => {
    const { controller, adapter } = await openProjectPage();
    const submit = vi.spyOn(adapter, "guardedWriteSubmit");
    try {
      fireEvent.change(screen.getByLabelText("干活 模型 项目覆盖"), {
        target: { value: "claude-opus-4-1" },
      });
      await savePreview("模型");
      submit.mockResolvedValueOnce({ ok: false, network: "offline" } as never);
      fireEvent.click(screen.getByRole("button", { name: "模拟应用设置" }));
      expect(await screen.findByText(/连接中断，请稍后重试/)).toBeInTheDocument();
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
        "claude-opus-4-1",
      );
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();

      await savePreview("模型");
      await applyPreview();
      const doc = await readOverride(adapter, "prj_gantry", "models");
      expect((doc.value as { worker: { model: string } }).worker.model).toBe(
        "claude-opus-4-1",
      );
    } finally {
      submit.mockRestore();
      await controller.stop();
    }
  });
});
