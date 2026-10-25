import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useSyncExternalStore, type ReactNode } from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { SettingsOverlay } from "@/features/overlays/Overlays";
import { buildModelsValue, parseModels, draftFromModels } from "@/features/overlays/settingsData";
import { IpcError } from "@/api/shell";
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

async function openSettings(
  initialPage?: string,
): Promise<{
  controller: DesktopController;
  adapter: Awaited<ReturnType<typeof startController>>["adapter"];
  onClose: ReturnType<typeof vi.fn>;
}> {
  const { controller, adapter } = await startController();
  const onClose = vi.fn();
  render(
    <Live controller={controller}>
      {(state) => (
        <SettingsOverlay
          controller={controller}
          state={state}
          initialPage={initialPage}
          onClose={onClose}
        />
      )}
    </Live>,
  );
  await screen.findByRole("heading", { name: "模型" });
  return { controller, adapter, onClose };
}

describe("buildModelsValue", () => {
  it("保留稀疏文档的未知键，不替未配置档位生成字段", () => {
    const base = {
      providers: {
        anthropic: {
          kind: "anthropic",
          credential: {
            type: "api_key",
            secret_ref: "model.anthropic.main",
            present: true,
            value: "stale-value-must-not-be-resent",
          },
        },
      },
      planner: { provider: "anthropic", model: "planner-1", reasoning_effort: "high", future_key: 7 },
      custom_unknown: { keep: true },
    };
    const draft = draftFromModels(parseModels(base));
    draft.worker = { provider: "anthropic", model: "worker-1", effort: "medium", hasByLevel: false };
    const value = buildModelsValue(base, draft, {
      anthropic: "sk-new-secret",
    }) as Record<string, unknown>;
    const providers = value.providers as Record<string, Record<string, unknown>>;
    const credential = providers.anthropic.credential as Record<string, unknown>;
    expect(credential.value).toBe("sk-new-secret");
    expect(credential.secret_ref).toBe("model.anthropic.main");
    const withoutSecret = buildModelsValue(base, draft, {}) as Record<string, unknown>;
    const cleanCredential = (
      (withoutSecret.providers as Record<string, Record<string, unknown>>).anthropic
        .credential as Record<string, unknown>
    );
    expect(cleanCredential.value).toBeUndefined();
    expect(value.custom_unknown).toEqual({ keep: true });
    const planner = value.planner as Record<string, unknown>;
    expect(planner.future_key).toBe(7);
    expect(value.worker).toMatchObject({
      provider: "anthropic",
      model: "worker-1",
      reasoning_effort: "medium",
    });
    expect(value.utility).toBeUndefined();
  });
});

describe("SettingsOverlay", () => {
  it("全屏载入模型、三张卡、静态档位表与通知导航", async () => {
    vi.spyOn(Date, "now").mockReturnValue(TEST_NOW);
    const { controller, onClose } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      expect(within(nav).getByRole("button", { name: "模型" })).toHaveAttribute(
        "aria-current",
        "page",
      );
      expect(within(nav).getByRole("button", { name: "通知" })).toBeInTheDocument();
      expect(within(nav).getByRole("button", { name: "设备与证书" })).toBeInTheDocument();
      expect(screen.getByText("大脑模型")).toBeInTheDocument();
      expect(screen.getByText("干活模型")).toBeInTheDocument();
      expect(screen.getByText("打杂模型")).toBeInTheDocument();
      expect(screen.getByText("示例数据")).toBeInTheDocument();
      expect(
        screen.getByText("开发场景 · 未连接真实服务端"),
      ).toBeInTheDocument();
      expect(
        screen.queryByText("gantry.example.com:8443 · 已连接"),
      ).toBeNull();
      expect(
        screen.getByRole("heading", { name: "每个档位里三类模型各做什么" }),
      ).toBeInTheDocument();
      expect(screen.getByText("Ultimate")).toBeInTheDocument();
      expect(screen.getAllByText("未被项目覆盖").length).toBeGreaterThan(0);
      expect(screen.queryByText("xhigh")).toBeNull();
      expect(controller.getSnapshot().status).toBe("ready");
      expect(onClose).not.toHaveBeenCalled();
    } finally {
      await controller.stop();
    }
  });

  it("未保存改动拦截返回与 Esc，确认后才关闭", async () => {
    const { controller, onClose } = await openSettings();
    try {
      const planner = await screen.findByLabelText("大脑模型 模型");
      fireEvent.change(planner, { target: { value: "anthropic/claude-opus-4-1" } });
      expect(await screen.findByText("有未保存的改动")).toBeInTheDocument();

      fireEvent.click(screen.getByRole("button", { name: "返回工作区" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      expect(onClose).not.toHaveBeenCalled();

      fireEvent.click(screen.getByRole("button", { name: "继续编辑" }));
      expect(screen.queryByRole("alertdialog")).toBeNull();

      const dialog = screen.getByRole("dialog");
      fireEvent(dialog, new Event("cancel", { cancelable: true }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      expect(onClose).not.toHaveBeenCalled();

      fireEvent.click(screen.getByRole("button", { name: "放弃改动并返回" }));
      expect(onClose).toHaveBeenCalledTimes(1);
    } finally {
      await controller.stop();
    }
  });

  it("未保存改动拦截导航，确认后才切换页面", async () => {
    const { controller } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.change(await screen.findByLabelText("大脑模型 模型"), {
        target: { value: "anthropic/claude-opus-4-1" },
      });
      fireEvent.click(within(nav).getByRole("button", { name: "设备与证书" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "继续编辑" }));
      expect(
        screen.getByRole("heading", { name: "模型" }),
      ).toBeInTheDocument();

      fireEvent.click(within(nav).getByRole("button", { name: "设备与证书" }));
      fireEvent.click(screen.getByRole("button", { name: "放弃改动并切换" }));
      expect(
        await screen.findByRole("heading", { name: "设备与证书" }),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("保存只做 prepare、清空凭据并在外壳不连接时禁止提交", async () => {
    const { controller, adapter } = await openSettings();
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    const submit = vi.spyOn(adapter, "guardedWriteSubmit");
    const discard = vi.spyOn(adapter, "guardedWriteDiscard");
    try {
      const credential = screen.getByLabelText("anthropic API key");
      fireEvent.change(credential, { target: { value: "sk-test-secret-value" } });
      const worker = await screen.findByLabelText("干活模型 模型");
      fireEvent.change(worker, { target: { value: "anthropic/claude-opus-4-1" } });

      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      const args = prepare.mock.calls[0][0];
      expect(args).toMatchObject({
        kind: "settings_put",
        scope: "global",
        resource: "models",
        ifMatch: '"18235"',
      });
      const sent = JSON.stringify(args);
      expect(sent).toContain("sk-test-secret-value");
      expect(sent).toContain("model.anthropic.main");
      expect(sent).toContain("anthropic/claude-opus-4-1".split("/")[1]);

      await waitFor(() => expect(credential).toHaveValue(""));
      expect(document.body.textContent).not.toContain("sk-test-secret-value");
      expect(await screen.findByText(/示例模式不提交、不保存/)).toBeInTheDocument();
      expect(
        screen.getByText(/无法提交：外壳未连接|无法提交：示例模式/),
      ).toBeInTheDocument();
      const submitButton = screen.getByRole("button", { name: "确认提交" });
      expect(submitButton).toBeDisabled();
      expect(submit).not.toHaveBeenCalled();
      expect(document.body.textContent).not.toMatch(/已保存（rev/);

      fireEvent.change(worker, { target: { value: "anthropic/claude-sonnet-5" } });
      await waitFor(() =>
        expect(discard).toHaveBeenCalledWith({ pendingId: "pending_scenario" }),
      );
    } finally {
      await controller.stop();
    }
  });

  it("凭据预检失败后清空输入并禁止用旧状态再次保存", async () => {
    const { controller, adapter } = await openSettings();
    vi.spyOn(adapter, "guardedWritePrepare").mockRejectedValue(
      new IpcError("shell.not_connected", "外壳未连接，示例模式不能预检。"),
    );
    try {
      const credential = screen.getByLabelText("anthropic API key");
      fireEvent.change(credential, { target: { value: "sk-test-failure" } });
      fireEvent.change(await screen.findByLabelText("干活模型 模型"), {
        target: { value: "anthropic/claude-opus-4-1" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      expect(
        await screen.findByText(/凭据值已从界面清除，请重新输入凭据后再保存/),
      ).toBeInTheDocument();
      expect(credential).toHaveValue("");
      expect(document.body.textContent).not.toContain("sk-test-failure");
      expect(screen.getByRole("button", { name: "保存" })).toBeDisabled();
    } finally {
      await controller.stop();
    }
  });

  it("412 显示冲突横幅，不自动合并也不显示保存成功", async () => {
    const { controller, adapter } = await openSettings();
    const discard = vi.spyOn(adapter, "guardedWriteDiscard");
    const base = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: {},
      ifMatch: '"18235"',
    });
    vi.spyOn(adapter, "guardedWritePrepare").mockResolvedValue({
      ...base,
      valid: true,
      errors: [],
      warnings: [],
      can_submit: { ok: true },
    });
    vi.spyOn(adapter, "guardedWriteSubmit").mockResolvedValue({
      ok: false,
      status: 412,
      problem: {
        code: "precondition_failed",
        title: "precondition failed",
        request_id: "test",
      },
    });
    try {
      fireEvent.change(await screen.findByLabelText("打杂模型 模型"), {
        target: { value: "anthropic/claude-sonnet-4-5" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      const confirm = await screen.findByRole("button", { name: "确认提交" });
      await waitFor(() => expect(confirm).toBeEnabled());
      fireEvent.click(confirm);

      expect(
        await screen.findByText(/此页已被其他设备修改/),
      ).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "载入最新值（丢弃我的改动）" })).toBeInTheDocument();
      expect(document.body.textContent).not.toMatch(/已保存（rev/);
      await waitFor(() =>
        expect(discard).toHaveBeenCalledWith({ pendingId: "pending_scenario" }),
      );
    } finally {
      await controller.stop();
    }
  });

  it("提交抛错时丢弃 pending 并保留用户可见错误", async () => {
    const { controller, adapter } = await openSettings();
    const base = await adapter.guardedWritePrepare({
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: {},
      ifMatch: '"18235"',
    });
    vi.spyOn(adapter, "guardedWritePrepare").mockResolvedValue({
      ...base,
      valid: true,
      errors: [],
      warnings: [],
      can_submit: { ok: true },
    });
    const discard = vi.spyOn(adapter, "guardedWriteDiscard");
    try {
      fireEvent.change(await screen.findByLabelText("打杂模型 模型"), {
        target: { value: "anthropic/claude-sonnet-4-5" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      const confirm = await screen.findByRole("button", { name: "确认提交" });
      await waitFor(() => expect(confirm).toBeEnabled());
      fireEvent.click(confirm);

      expect(
        await screen.findByText(
          "审批签名密钥不可用，请检查设备密钥或重新配对。",
        ),
      ).toBeInTheDocument();
      await waitFor(() =>
        expect(discard).toHaveBeenCalledWith({ pendingId: "pending_scenario" }),
      );
    } finally {
      await controller.stop();
    }
  });

  it("项目页显示继承与生效值，不提供预算/路径伪配置", async () => {
    const { controller } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "specgit" }));
      const levelText = await screen.findByText("默认档位上限");
      expect(
        screen.getByRole("heading", { name: /项目 · specgit/ }),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("region", { name: "项目档位与模型覆盖" }),
      ).toHaveAttribute("tabindex", "0");
      const levelRow = levelText.closest("tr");
      expect(levelRow).not.toBeNull();
      expect(levelRow?.textContent).toContain("Medium");
      const modelRow = screen.getByText("干活模型").closest("tr");
      expect(modelRow?.textContent).toContain("claude-sonnet-5");
      expect(screen.queryByText("预算模板")).toBeNull();
      expect(screen.queryByText("额外需审批的路径")).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("项目保存使用项目作用域与已加载 ETag，示例模式不提交", async () => {
    const { controller, adapter } = await openSettings();
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    const submit = vi.spyOn(adapter, "guardedWriteSubmit");
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "specgit" }));
      await screen.findByText("默认档位上限");
      fireEvent.change(screen.getByLabelText("默认档位上限 项目覆盖"), {
        target: { value: "High" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存档位" }));
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      expect(prepare).toHaveBeenCalledWith({
        kind: "settings_put",
        scope: "project",
        projectId: "prj_specgit",
        resource: "levels",
        value: { default_level: "High" },
        ifMatch: '"0"',
      });
      const confirm = await screen.findByRole("button", { name: "确认提交档位" });
      expect(confirm).toBeDisabled();
      expect(await screen.findByText(/示例模式不提交、不保存/)).toBeInTheDocument();
      expect(submit).not.toHaveBeenCalled();
      expect(document.body.textContent).not.toMatch(/已保存（rev/);
    } finally {
      await controller.stop();
    }
  });

  it("设备角色变更走受保护预检，示例模式不伪成功", async () => {
    const { controller, adapter } = await openSettings();
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    const submit = vi.spyOn(adapter, "guardedWriteSubmit");
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "设备与证书" }));
      await screen.findByText("书房 Mac");
      const device = screen.getByRole("article", { name: "书房 Mac" });
      fireEvent.change(within(device).getByLabelText("书房 Mac 角色"), {
        target: { value: "approver" },
      });
      fireEvent.click(within(device).getByRole("button", { name: "应用角色" }));
      expect(
        await screen.findByText(/确认将 书房 Mac 的角色改为 审批设备/),
      ).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "确认并预检" }));
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      expect(prepare).toHaveBeenCalledWith({
        kind: "device_role",
        deviceId: "dev_scenario_second",
        role: "approver",
      });
      expect(await screen.findByRole("button", { name: "提交" })).toBeDisabled();
      expect(submit).not.toHaveBeenCalled();
      expect(document.body.textContent).not.toContain("设备角色已更新。");
    } finally {
      await controller.stop();
    }
  });

  it("设备页列出公开证书信息且禁止修改自己的角色", async () => {
    const { controller } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "设备与证书" }));
      expect(await screen.findByText("Susan 的 MacBook")).toBeInTheDocument();
      expect(screen.getAllByText("5f3a9c01d27be4468a1c").length).toBeGreaterThan(0);
      const ownSelect = screen.getByLabelText("Susan 的 MacBook 角色");
      expect(ownSelect).toBeDisabled();
      expect(screen.getByText(/不能修改自己的设备角色/)).toBeInTheDocument();
      expect(screen.getByText("书房 Mac")).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("未排期页面诚实占位", async () => {
    const { controller } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "外观" }));
      expect(await screen.findByText(/尚未提供/)).toBeInTheDocument();
      expect(screen.getByText(/13\.2/)).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });
});
