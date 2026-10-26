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
import type { SettingsCatalog } from "@/api/types";
import { SettingsOverlay } from "@/features/overlays/Overlays";
import {
  PROJECT_RESOURCES,
  ProjectOverrideFieldRow,
  projectOverrideFields,
  projectOverrideTables,
} from "@/features/overlays/settingsFields";
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

const PAGE_TITLES: Record<string, string> = {
  models: "模型",
  levels: "档位与预算",
  github: "GitHub App",
  sandbox: "沙盒与出网",
  policies: "危险动作策略",
  guardian: "后台守护",
  notify: "通知",
  appearance: "外观",
};

async function openSettings(
  page: string,
  scenario: Parameters<typeof startController>[0] = "default",
  beforeRender?: (
    adapter: Awaited<ReturnType<typeof startController>>["adapter"],
  ) => void,
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
          initialPage={page}
          onClose={onClose}
        />
      )}
    </Live>,
  );
  await screen.findByRole("heading", { name: PAGE_TITLES[page] ?? "设置" });
  return { controller, adapter, onClose };
}

async function applyPreview(): Promise<void> {
  const button = await screen.findByRole("button", { name: "模拟应用设置" });
  await waitFor(() => expect(button).toBeEnabled());
  fireEvent.click(button);
  await screen.findByText(/模拟设置已应用/);
}

describe("设置：档位与预算", () => {
  it("列出全部预算字段，正常保存预算并回读一致", async () => {
    const { controller, adapter } = await openSettings("levels");
    try {
      expect(screen.getByLabelText("默认档位上限")).toHaveValue("Medium");
      expect(screen.getByLabelText("None 档每日 token 上限")).toHaveValue(500000);
      expect(screen.getByLabelText("权重 input")).toHaveValue(1);
      for (const level of ["low", "medium", "high", "max", "ultimate"]) {
        for (const suffix of [
          "utility tokens",
          "planner tokens",
          "worker tokens",
          "nodes",
          "rounds",
          "wall_time_s",
          "concurrency",
          "forge_writes",
          "forge_writes_per_hour",
        ]) {
          expect(
            screen.getByLabelText(`${level} ${suffix}`),
          ).toBeInTheDocument();
        }
      }
      expect(screen.getByLabelText("low 图模板")).toHaveValue("low.single");

      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      expect(screen.getByText("有未保存的改动")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "保存档位与预算" }));
      await applyPreview();

      const read = await adapter.api("settings.get", { resource: "levels" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no levels doc");
      const value = read.body.value as {
        low: { budget: { nodes: number } };
      };
      expect(value.low.budget.nodes).toBe(7);
      expect(read.body.rev).toBe(18103);
      expect(screen.getByText(/回读一致/)).toBeInTheDocument();
      const status = screen.getByRole("region", { name: "设置应用状态" });
      expect(within(status).getByText("engine")).toBeInTheDocument();
      expect(
        within(status).getAllByText(/已应用最新 rev|尚未应用最新 rev/).length,
      ).toBeGreaterThan(0);
    } finally {
      await controller.stop();
    }
  });

  it("预算为 0 时显示字段错误并阻止预览", async () => {
    const { controller } = await openSettings("levels");
    try {
      fireEvent.change(screen.getByLabelText("low rounds"), {
        target: { value: "0" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存档位与预算" }));
      expect(
        await screen.findByText(/请先修正标出的 1 项字段错误/),
      ).toBeInTheDocument();
      expect(screen.getAllByText("预算必须是正整数。").length).toBeGreaterThan(0);
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("自动加码规则独立保存并回读", async () => {
    const { controller, adapter } = await openSettings("levels");
    try {
      fireEvent.change(screen.getByLabelText("large_diff raise_to"), {
        target: { value: "Max" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存自动加码" }));
      await applyPreview();
      const read = await adapter.api("settings.get", { resource: "policies" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no policies doc");
      const value = read.body.value as {
        escalation: { rules: Record<string, { raise_to: string }> };
      };
      expect(value.escalation.rules.large_diff.raise_to).toBe("Max");
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：GitHub App", () => {
  it("测试占位凭据只写不读，准备后清空并回读 present", async () => {
    const { controller, adapter } = await openSettings("github");
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    try {
      const secret = screen.getByLabelText("App 私钥 新值");
      fireEvent.change(secret, { target: { value: "test-placeholder-private-key" } });
      fireEvent.change(screen.getByLabelText("App ID"), {
        target: { value: "482140" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存GitHub 设置" }));
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      expect(JSON.stringify(prepare.mock.calls[0][0])).toContain(
        "test-placeholder-private-key",
      );
      await waitFor(() => expect(secret).toHaveValue(""));
      expect(document.body.textContent).not.toContain(
        "test-placeholder-private-key",
      );
      expect(
        screen.getByText(/开发场景只做内存模拟应用，不产生真实签名/),
      ).toBeInTheDocument();
      await applyPreview();
      const read = await adapter.api("settings.get", { resource: "github" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no github doc");
      const value = read.body.value as {
        app: { app_id: number; private_key: { present: boolean } };
      };
      expect(value.app.app_id).toBe(482140);
      expect(value.app.private_key.present).toBe(true);
      expect(JSON.stringify(read.body)).not.toContain(
        "test-placeholder-private-key",
      );
    } finally {
      await controller.stop();
    }
  });

  it("非测试占位值被拒绝，清空输入并要求重新输入", async () => {
    const { controller } = await openSettings("github");
    try {
      fireEvent.change(screen.getByLabelText("App 私钥 新值"), {
        target: { value: "sk-live-real-looking-secret" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存GitHub 设置" }));
      expect(
        await screen.findByText(/凭据值已从界面清除，请重新输入凭据后再保存/),
      ).toBeInTheDocument();
      expect(screen.getAllByText(/示例模式只接受明确标注的测试占位值/)[0]).toBeInTheDocument();
      const submit = screen.getByRole("button", { name: "模拟应用设置" });
      expect(submit).toBeDisabled();
      expect(document.body.textContent).not.toContain("sk-live-real-looking-secret");
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：沙盒与出网", () => {
  it("非法镜像阻止保存，修正后沙盒保存回读", async () => {
    const { controller, adapter } = await openSettings("sandbox");
    try {
      const image = screen.getByLabelText("基础镜像");
      const original = (image as HTMLInputElement).value;
      fireEvent.change(image, { target: { value: "ghcr.io/example/worker:latest" } });
      fireEvent.click(screen.getByRole("button", { name: "保存沙盒" }));
      expect(
        await screen.findByText(/基础镜像必须以 @sha256 固定摘要/),
      ).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();

      fireEvent.change(image, { target: { value: original } });
      fireEvent.change(screen.getByLabelText("并发布盒上限"), {
        target: { value: "4" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存沙盒" }));
      await applyPreview();
      const read = await adapter.api("settings.get", { resource: "sandbox" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no sandbox doc");
      expect(
        (read.body.value as { max_concurrent_sandboxes: number })
          .max_concurrent_sandboxes,
      ).toBe(4);
    } finally {
      await controller.stop();
    }
  });

  it("GitHub 主机进入白名单被拒绝", async () => {
    const { controller } = await openSettings("sandbox");
    try {
      fireEvent.change(screen.getByLabelText("npm hosts"), {
        target: { value: "registry.npmjs.org\ngithub.com" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存出网" }));
      expect(
        (await screen.findAllByText(/属于硬性拒绝范围/)).length,
      ).toBeGreaterThan(0);
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("operator 放宽出网档案需要审批角色，预览不可提交", async () => {
    const { controller } = await openSettings("sandbox", "operator");
    try {
      const npm = screen.getByLabelText("model_only 分组");
      fireEvent.click(within(npm).getByLabelText("npm"));
      fireEvent.click(screen.getByRole("button", { name: "保存出网" }));
      const submit = await screen.findByRole("button", { name: "模拟应用设置" });
      expect(submit).toBeDisabled();
      expect(
        screen.getByText(/此改动需要审批设备签名，而本机不是审批设备/),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：危险动作策略", () => {
  it("列出目录动作并保存补丁上限", async () => {
    const { controller, adapter } = await openSettings("policies");
    try {
      expect(screen.getByLabelText("merge_pr 处置")).toHaveValue("approve");
      expect(screen.getByLabelText("rerun_failed_jobs 处置")).toHaveValue("auto");
      fireEvent.change(screen.getByLabelText("max_files"), {
        target: { value: "150" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存危险动作策略" }));
      await applyPreview();
      const read = await adapter.api("settings.get", { resource: "policies" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no policies doc");
      expect((read.body.value as { patch: { max_files: number } }).patch.max_files).toBe(
        150,
      );
    } finally {
      await controller.stop();
    }
  });

  it("审批有效期越界显示字段错误并阻止预览", async () => {
    const { controller } = await openSettings("policies");
    try {
      fireEvent.change(screen.getByLabelText("审批有效期"), {
        target: { value: "30" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存危险动作策略" }));
      expect(
        await screen.findByText(/审批有效期不得小于 60/),
      ).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("operator 放宽动作处置需要审批角色", async () => {
    const { controller } = await openSettings("policies", "operator");
    try {
      fireEvent.change(screen.getByLabelText("resolve_review_thread_human 处置"), {
        target: { value: "approve" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存危险动作策略" }));
      const submit = await screen.findByRole("button", { name: "模拟应用设置" });
      expect(submit).toBeDisabled();
      expect(
        screen.getByText(/此改动需要审批设备签名，而本机不是审批设备/),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：后台守护", () => {
  it("收紧分流并保存，巡检过小显示错误", async () => {
    const { controller, adapter } = await openSettings("guardian");
    try {
      fireEvent.change(screen.getByLabelText("nit 分流"), {
        target: { value: "escalate" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存守护设置" }));
      await applyPreview();
      const read = await adapter.api("settings.get", { resource: "guardian" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no guardian doc");
      expect(
        (read.body.value as { triage: { nit: string } }).triage.nit,
      ).toBe("escalate");

      fireEvent.change(screen.getByLabelText("巡检周期"), {
        target: { value: "100" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存守护设置" }));
      expect(
        (await screen.findAllByText(/巡检周期不得小于 300/)).length,
      ).toBeGreaterThan(0);
      expect(screen.queryByRole("button", { name: "模拟应用设置" })).toBeNull();
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：通知", () => {
  it("关闭桌面通道触发 urgent 覆盖错误，恢复后保存路由", async () => {
    const { controller, adapter } = await openSettings("notify");
    try {
      fireEvent.click(screen.getByLabelText("桌面通道"));
      fireEvent.click(screen.getByRole("button", { name: "保存通知设置" }));
      expect(
        await screen.findByText(/urgent 类上报没有任何已启用渠道可达/),
      ).toBeInTheDocument();

      fireEvent.click(screen.getByLabelText("桌面通道"));
      const normalKinds = screen.getByRole("group", { name: "normal 上报类型" });
      fireEvent.click(within(normalKinds).getByLabelText("escalation"));
      fireEvent.click(screen.getByRole("button", { name: "保存通知设置" }));
      await applyPreview();
      const read = await adapter.api("settings.get", { resource: "notify" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no notify doc");
      const routes = (read.body.value as {
        routes: Record<string, { kinds: string[] }>;
      }).routes;
      expect(routes.normal.kinds).toContain("escalation");
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：外观本机偏好", () => {
  it("保存密度与面板宽度并回读", async () => {
    const { controller, adapter } = await openSettings("appearance");
    try {
      fireEvent.change(screen.getByLabelText("密度"), {
        target: { value: "comfortable" },
      });
      fireEvent.change(screen.getByLabelText("左栏宽度"), {
        target: { value: "260" },
      });
      fireEvent.click(screen.getByRole("button", { name: "预览本机偏好保存" }));
      expect(await screen.findByText(/密度 compact → comfortable/)).toBeInTheDocument();
      fireEvent.click(
        screen.getByRole("button", { name: "模拟应用到本机偏好" }),
      );
      expect(
        await screen.findByText(/已保存到本机偏好并回读一致/),
      ).toBeInTheDocument();
      const prefs = await adapter.prefsGet();
      expect(prefs.density).toBe("comfortable");
      expect(prefs.layout.leftWidth).toBe(260);
      expect(screen.getByLabelText("密度")).toHaveValue("comfortable");
    } finally {
      await controller.stop();
    }
  });

  it("宽度越界显示错误并禁用预览", async () => {
    const { controller } = await openSettings("appearance");
    try {
      fireEvent.change(screen.getByLabelText("左栏宽度"), {
        target: { value: "20" },
      });
      expect(
        (await screen.findAllByText(/左栏宽度必须在 180–480 px/)).length,
      ).toBeGreaterThan(0);
      expect(
        screen.getByRole("button", { name: "预览本机偏好保存" }),
      ).toBeDisabled();
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：冲突与失败恢复", () => {
  it("412 显示冲突横幅，载入最新值后恢复", async () => {
    const { controller, adapter } = await openSettings("levels");
    try {
      adapter.simulation.injectError("settings_conflict");
      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存档位与预算" }));
      const submit = await screen.findByRole("button", { name: "模拟应用设置" });
      await waitFor(() => expect(submit).toBeEnabled());
      fireEvent.click(submit);
      expect(
        await screen.findByText(/此页已被其他设备修改/),
      ).toBeInTheDocument();
      fireEvent.click(
        screen.getByRole("button", { name: "载入最新值（丢弃我的改动）" }),
      );
      await waitFor(() =>
        expect(screen.queryByText(/此页已被其他设备修改/)).toBeNull(),
      );
      expect(
        await screen.findByRole("button", { name: "保存档位与预算" }),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("载入失败显示重试，恢复后重新载入", async () => {
    let failGuardian = true;
    const { controller, adapter } = await openSettings(
      "guardian",
      "default",
      (adapter) => {
        const original = adapter.api.bind(adapter);
        vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
          if (
            failGuardian &&
            route === "settings.get" &&
            (args as { resource?: string }).resource === "guardian"
          ) {
            return { ok: false, network: "offline" } as never;
          }
          return original(route, args);
        });
      },
    );
    try {
      expect(
        screen.getByText(/连接中断，请稍后重试/),
      ).toBeInTheDocument();
      failGuardian = false;
      fireEvent.click(screen.getByRole("button", { name: "重试" }));
      expect(
        await screen.findByLabelText("nit 分流"),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });
});

describe("设置：项目覆盖字段过滤", () => {
  it("六类资源只暴露 §2.2 允许的键", async () => {
    const { controller, adapter } = await startController();
    try {
      const catalogResult = await adapter.api("settings.catalog", {});
      expect(catalogResult.ok).toBe(true);
      if (!catalogResult.ok || !catalogResult.body) throw new Error("no catalog");
      const catalog: SettingsCatalog = catalogResult.body;

      for (const resource of PROJECT_RESOURCES) {
        expect(
          projectOverrideFields(resource, catalog, {
            profiles: { default: { groups: ["model_api"] } },
          }).length,
        ).toBeGreaterThan(0);
      }

      const levels = projectOverrideFields("levels", catalog);
      expect(levels.some((field) => field.path.includes("graph_template"))).toBe(false);
      expect(levels.some((field) => field.path[0] === "token_weights")).toBe(false);
      expect(
        levels.find(
          (field) => field.path.join("/") === "low/budget/nodes",
        )?.tightenOnly,
      ).toBe(true);

      const models = projectOverrideFields("models", catalog);
      expect(models.some((field) => field.path[0] === "providers")).toBe(false);
      expect(models.some((field) => field.path[0] === "retry")).toBe(false);
      expect(
        models.some((field) => field.path.join("/") === "worker/model"),
      ).toBe(true);

      const policies = projectOverrideFields("policies", catalog);
      expect(policies.some((field) => field.path[0] === "allow_globs")).toBe(false);
      expect(
        policies.some((field) => field.path[0] === "actions"),
      ).toBe(true);
      const policyTables = projectOverrideTables("policies");
      expect(policyTables.map((table) => table.path.join("/"))).toContain("paths");
      expect(policyTables.map((table) => table.path.join("/"))).toContain(
        "escalation/rules",
      );
      expect(policyTables.every((table) => table.tightenOnly)).toBe(true);

      const guardian = projectOverrideFields("guardian", catalog);
      expect(
        guardian.some((field) => field.path[0] === "patrol_interval_s"),
      ).toBe(false);

      const sandbox = projectOverrideFields("sandbox", catalog);
      expect(sandbox.some((field) => field.path[0] === "io")).toBe(false);
      expect(sandbox.some((field) => field.path[0] === "base_image")).toBe(false);
      expect(
        sandbox.some((field) => field.path.join("/") === "limits/mem_mb"),
      ).toBe(true);

      const egress = projectOverrideFields("egress", catalog, {
        profiles: { default: { groups: ["model_api"] } },
      });
      expect(
        egress.find((field) => field.path.join("/") === "profiles/default/groups")
          ?.tightenOnly,
      ).toBe(true);
      expect(egress.some((field) => field.path[0] === "groups")).toBe(false);
      expect(egress.some((field) => field.path[0] === "injection")).toBe(false);
    } finally {
      await controller.stop();
    }
  });

  it("只读继承值行显示生效值并可恢复继承", () => {
    const onChange = vi.fn();
    const onClear = vi.fn();
    render(
      <ProjectOverrideFieldRow
        field={{
          path: ["low", "budget", "nodes"],
          label: "low nodes",
          kind: "integer",
          min: 1,
          tightenOnly: true,
        }}
        effectiveValue={6}
        overrideValue={4}
        onChange={onChange}
        onClear={onClear}
      />,
    );
    expect(screen.getByText("low nodes")).toBeInTheDocument();
    expect(screen.getByText("只能收紧")).toBeInTheDocument();
    expect(screen.getByText("6")).toBeInTheDocument();
    const input = screen.getByLabelText("low nodes 项目覆盖");
    expect(input).toHaveValue(4);
    fireEvent.change(input, { target: { value: "5" } });
    expect(onChange).toHaveBeenCalledWith(5);
    fireEvent.click(screen.getByRole("button", { name: "恢复继承" }));
    expect(onClear).toHaveBeenCalledTimes(1);
  });
});

describe("设置：模型页模拟保存文字与切页保护", () => {
  it("模型页模拟保存并回读打杂模型", async () => {
    const { controller, adapter } = await openSettings("models");
    try {
      fireEvent.change(screen.getByLabelText("打杂模型 模型"), {
        target: { value: "anthropic/claude-sonnet-4-5" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      const submit = await screen.findByRole("button", {
        name: "模拟应用设置",
      });
      await waitFor(() => expect(submit).toBeEnabled());
      fireEvent.click(submit);
      expect(
        await screen.findByText(/模拟设置已应用/),
      ).toBeInTheDocument();
      const read = await adapter.api("settings.get", { resource: "models" });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no models doc");
      expect(
        (read.body.value as { utility: { model: string } }).utility.model,
      ).toBe("claude-sonnet-4-5");
    } finally {
      await controller.stop();
    }
  });

  it("operator 写模型凭据需要审批角色", async () => {
    const { controller } = await openSettings("models", "operator");
    try {
      fireEvent.change(screen.getByLabelText("anthropic API key"), {
        target: { value: "test-placeholder-key" },
      });
      fireEvent.change(screen.getByLabelText("干活模型 模型"), {
        target: { value: "anthropic/claude-opus-4-1" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      const submit = await screen.findByRole("button", {
        name: "模拟应用设置",
      });
      expect(submit).toBeDisabled();
      expect(
        screen.getByText(/此改动需要审批设备签名，而本机不是审批设备/),
      ).toBeInTheDocument();
      expect(document.body.textContent).not.toContain("test-placeholder-key");
    } finally {
      await controller.stop();
    }
  });

  it("模型页标注开发场景与测试占位凭据", async () => {
    const { controller } = await openSettings("models");
    try {
      expect(
        screen.getByText(/开发场景：保存预览与提交只修改内存模型设置并立即回读/),
      ).toBeInTheDocument();
      expect(
        screen.getAllByText(/仅测试占位值，勿输入真实凭据/).length,
      ).toBeGreaterThan(0);
    } finally {
      await controller.stop();
    }
  });

  it("资源页未保存改动拦截切页，确认后才切换", async () => {
    const { controller } = await openSettings("levels");
    try {
      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      expect(await screen.findByText("有未保存的改动")).toBeInTheDocument();
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "外观" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      fireEvent.click(
        screen.getByRole("button", { name: "放弃改动并切换" }),
      );
      await screen.findByLabelText("密度");
      expect(
        screen.getByRole("heading", { name: "外观" }),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("外观页开放全部已有本机偏好字段且只给已设计选项", async () => {
    const { controller } = await openSettings("appearance");
    try {
      for (const label of [
        "主题",
        "语言",
        "密度",
        "本机系统通知",
        "左栏宽度",
        "检视器宽度",
        "折叠左栏",
        "折叠检视器",
      ]) {
        expect(screen.getByLabelText(label)).toBeInTheDocument();
      }
      expect(screen.getByLabelText("主题")).toHaveValue("dark");
      expect(screen.getByLabelText("语言")).toHaveValue("zh-CN");
      expect(screen.getByLabelText("主题").querySelectorAll("option")).toHaveLength(1);
      expect(screen.getByLabelText("语言").querySelectorAll("option")).toHaveLength(1);
    } finally {
      await controller.stop();
    }
  });
});
