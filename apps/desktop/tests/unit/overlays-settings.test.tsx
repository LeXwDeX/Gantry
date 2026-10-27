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

  it("保存先做 prepare、清空凭据，再模拟应用并从服务端回读", async () => {
    const { controller, adapter } = await openSettings();
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    const submit = vi.spyOn(adapter, "guardedWriteSubmit");
    try {
      const credential = screen.getByLabelText("anthropic API key");
      fireEvent.change(credential, { target: { value: "sk-test-secret-value" } });
      const utility = await screen.findByLabelText("打杂模型 模型");
      fireEvent.change(utility, { target: { value: "anthropic/claude-sonnet-4-5" } });

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
      expect(sent).toContain("claude-sonnet-4-5");

      await waitFor(() => expect(credential).toHaveValue(""));
      expect(document.body.textContent).not.toContain("sk-test-secret-value");
      const submitButton = await screen.findByRole("button", {
        name: "模拟应用设置",
      });
      await waitFor(() => expect(submitButton).toBeEnabled());
      fireEvent.click(submitButton);
      expect(
        await screen.findByText(/模拟设置已应用/),
      ).toBeInTheDocument();
      expect(submit).toHaveBeenCalledTimes(1);
      expect(document.body.textContent).not.toContain("sk-test-secret-value");
      expect(document.body.textContent).not.toMatch(/已保存（rev/);

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
      const confirm = await screen.findByRole("button", { name: "模拟应用设置" }, { timeout: 5000 });
      await waitFor(() => expect(confirm).toBeEnabled(), { timeout: 5000 });
      fireEvent.click(confirm);

      expect(
        await screen.findByText(/此页已被其他设备修改/),
      ).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "载入最新值（丢弃我的改动）" })).toBeInTheDocument();
      expect(document.body.textContent).not.toMatch(/已保存（rev/);
      await waitFor(() =>
        expect(discard).toHaveBeenCalledWith({
          pendingId: expect.stringMatching(/^pending_scenario/),
        }),
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
    vi.spyOn(adapter, "guardedWriteSubmit").mockRejectedValue(
      new IpcError(
        "shell.key_unavailable",
        "审批签名密钥不可用，请检查设备密钥或重新配对。",
      ),
    );
    try {
      fireEvent.change(await screen.findByLabelText("打杂模型 模型"), {
        target: { value: "anthropic/claude-sonnet-4-5" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存" }));
      const confirm = await screen.findByRole("button", { name: "模拟应用设置" }, { timeout: 5000 });
      await waitFor(() => expect(confirm).toBeEnabled(), { timeout: 5000 });
      fireEvent.click(confirm);

      expect(
        await screen.findByText(
          "审批签名密钥不可用，请检查设备密钥或重新配对。",
        ),
      ).toBeInTheDocument();
      await waitFor(() =>
        expect(discard).toHaveBeenCalledWith({
          pendingId: expect.stringMatching(/^pending_scenario/),
        }),
      );
    } finally {
      await controller.stop();
    }
  });

  it("项目页按六类资源显示继承值，并可切换到档位与沙盒", async () => {
    const { controller } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "specgit" }));
      await screen.findByRole("heading", {
        name: /项目 · specgit/,
      });
      const resources = screen.getByRole("navigation", {
        name: "项目覆盖资源",
      });
      for (const name of [
        "模型",
        "档位与预算",
        "危险动作策略",
        "后台守护",
        "沙盒",
        "出网档案",
      ]) {
        expect(
          within(resources).getByRole("button", { name: `${name}覆盖` }),
        ).toBeInTheDocument();
      }
      expect(
        screen.getByRole("note", { name: "干活 模型 全局值" }),
      ).toHaveTextContent("claude-sonnet-5");

      fireEvent.click(
        within(resources).getByRole("button", { name: "档位与预算覆盖" }),
      );
      expect(
        await screen.findByLabelText("默认档位上限 项目覆盖"),
      ).toHaveValue("High");

      fireEvent.click(
        within(resources).getByRole("button", { name: "沙盒覆盖" }),
      );
      expect(
        await screen.findByLabelText("限制 mem_mb 项目覆盖"),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("项目保存使用项目作用域与已加载 ETag，模拟应用后可回读", { timeout: 15000 }, async () => {
    const { controller, adapter } = await openSettings();
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    const submit = vi.spyOn(adapter, "guardedWriteSubmit");
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "gantry" }));
      await screen.findByRole("heading", { name: /项目 · gantry/ });
      fireEvent.change(screen.getByLabelText("干活 模型 项目覆盖"), {
        target: { value: "claude-opus-4-1" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存模型覆盖" }));
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      expect(prepare).toHaveBeenCalledWith(
        expect.objectContaining({
          kind: "settings_put",
          scope: "project",
          projectId: "prj_gantry",
          resource: "models",
          value: expect.objectContaining({
            worker: expect.objectContaining({ model: "claude-opus-4-1" }),
          }),
        }),
      );
      const confirm = await screen.findByRole("button", { name: "模拟应用设置" }, { timeout: 5000 });
      await waitFor(() => expect(confirm).toBeEnabled(), { timeout: 5000 });
      fireEvent.click(confirm);
      expect(
        await screen.findByText(/模拟设置已应用/),
      ).toBeInTheDocument();
      expect(submit).toHaveBeenCalledTimes(1);
      const read = await adapter.api("project_settings.get", {
        projectId: "prj_gantry",
        resource: "models",
      });
      expect(read.ok).toBe(true);
      if (!read.ok || !read.body) throw new Error("no project models");
      expect(
        (read.body.value as { worker: { model: string } }).worker.model,
      ).toBe("claude-opus-4-1");
    } finally {
      await controller.stop();
    }
  });

  it("设备角色变更走保护预检，模拟应用后不改真实签名", async () => {
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
      fireEvent.click(
        within(device).getByRole("button", { name: "模拟角色变更" }),
      );
      expect(
        await screen.findByText(/确认将 书房 Mac 的角色改为 审批设备/),
      ).toBeInTheDocument();
      fireEvent.click(
        screen.getByRole("button", { name: "确认并模拟预检" }),
      );
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      expect(prepare).toHaveBeenCalledWith({
        kind: "device_role",
        deviceId: "dev_scenario_second",
        role: "approver",
      });
      const confirm = await screen.findByRole("button", {
        name: "模拟应用设备操作",
      });
      await waitFor(() => expect(confirm).toBeEnabled());
      fireEvent.click(confirm);
      expect(
        await screen.findByText(/模拟设备角色已更新/),
      ).toBeInTheDocument();
      expect(submit).toHaveBeenCalledTimes(1);
      const list = await adapter.api("devices.list", { limit: 100 });
      expect(list.ok).toBe(true);
      if (!list.ok || !list.body) throw new Error("no devices");
      expect(
        list.body.items.find((item) => item.id === "dev_scenario_second")
          ?.role,
      ).toBe("approver");
      expect(document.body.textContent).not.toContain("BEGIN CERTIFICATE");
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

  it("外观页提供已定义主题、语言与布局偏好", async () => {
    const { controller } = await openSettings();
    try {
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      fireEvent.click(within(nav).getByRole("button", { name: "外观" }));
      await screen.findByRole("heading", { name: "外观" });
      // 标题先于本机偏好渲染；字段等 prefsGet 返回后才出现，高负载下不能同步查询。
      expect(await screen.findByLabelText("密度")).toBeInTheDocument();
      expect(screen.getByLabelText("左栏宽度")).toBeInTheDocument();
      expect(screen.getByLabelText("检视器宽度")).toBeInTheDocument();
      expect((await screen.findAllByText(/13\.2/)).length).toBeGreaterThan(0);
      expect(screen.queryByText(/尚未提供/)).toBeNull();
    } finally {
      await controller.stop();
    }
  });
});
