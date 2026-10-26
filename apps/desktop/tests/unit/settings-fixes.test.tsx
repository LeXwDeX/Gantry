import { describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useSyncExternalStore, type ReactNode } from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import type { ApiResult } from "@/api/shell";
import type { SettingsDocument } from "@/api/types";
import { SettingsOverlay } from "@/features/overlays/Overlays";
import { SettingsResourcePage } from "@/features/overlays/SettingsResourcePage";
import {
  ProjectOverrideFieldRow,
  type ProjectOverrideField,
} from "@/features/overlays/settingsFields";
import { startController } from "./helpers";

type Started = Awaited<ReturnType<typeof startController>>;

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

function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
} {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}

async function openSettings(
  initialPage: string,
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
          initialPage={initialPage}
          onClose={onClose}
        />
      )}
    </Live>,
  );
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

describe("F3：设置资源页旧请求隔离", () => {
  it("旧页慢响应不得清空新页字段，且保存仍使用新资源 ETag", async () => {
    const { controller, adapter } = await startController();
    const pageSettle = deferred<void>();
    const originalApi = adapter.api.bind(adapter);
    let heldLevels = false;
    vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
      if (
        route === "settings.get" &&
        (args as { resource?: string }).resource === "levels" &&
        !heldLevels
      ) {
        heldLevels = true;
        await pageSettle.promise;
        const result = (await originalApi(
          route as never,
          args as never,
        )) as ApiResult<SettingsDocument>;
        if (result.ok && result.body) {
          const value = result.body.value as {
            low: { budget: { nodes: number } };
          };
          value.low.budget.nodes = 99;
        }
        return result;
      }
      return originalApi(route as never, args as never);
    });
    const prepare = vi.spyOn(adapter, "guardedWritePrepare");
    const view = (page: "levels" | "github") => (
      <Live controller={controller}>
        {(state) => (
          <SettingsResourcePage
            controller={controller}
            state={state}
            page={page}
            onDirtyChange={() => undefined}
          />
        )}
      </Live>
    );
    try {
      const { rerender } = render(view("levels"));
      await screen.findByRole("heading", { name: "档位与预算" });
      rerender(view("github"));
      const appId = await screen.findByLabelText("App ID");
      expect(appId).toHaveValue(482139);
      fireEvent.change(appId, { target: { value: "482141" } });

      await act(async () => {
        pageSettle.resolve();
        await Promise.resolve();
      });
      await waitFor(() => expect(screen.getByLabelText("App ID")).toHaveValue(482141));
      expect(screen.getByRole("heading", { name: "GitHub App" })).toBeInTheDocument();
      expect(screen.queryByLabelText("low nodes")).toBeNull();
      expect(screen.queryByRole("alert")).toBeNull();

      fireEvent.click(screen.getByRole("button", { name: "保存GitHub 设置" }));
      await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
      const args = prepare.mock.calls[0][0] as {
        resource: string;
        ifMatch?: string;
      };
      expect(args.resource).toBe("github");
      expect(args.ifMatch).toBe('"18160"');
    } finally {
      await controller.stop();
    }
  });

  it("旧页迟到错误不得显示在新页", async () => {
    const { controller, adapter } = await startController();
    const pageSettle = deferred<void>();
    const originalApi = adapter.api.bind(adapter);
    let heldLevels = false;
    vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
      if (
        route === "settings.get" &&
        (args as { resource?: string }).resource === "levels" &&
        !heldLevels
      ) {
        heldLevels = true;
        await pageSettle.promise;
        return {
          ok: false,
          status: 500,
          problem: { code: "internal", title: "boom", request_id: "req_test" },
        } satisfies ApiResult<SettingsDocument>;
      }
      return originalApi(route as never, args as never);
    });
    const view = (page: "levels" | "github") => (
      <Live controller={controller}>
        {(state) => (
          <SettingsResourcePage
            controller={controller}
            state={state}
            page={page}
            onDirtyChange={() => undefined}
          />
        )}
      </Live>
    );
    try {
      const { rerender } = render(view("levels"));
      await screen.findByRole("heading", { name: "档位与预算" });
      rerender(view("github"));
      const appId = await screen.findByLabelText("App ID");
      fireEvent.change(appId, { target: { value: "482141" } });

      await act(async () => {
        pageSettle.resolve();
        await Promise.resolve();
      });
      await waitFor(() => expect(screen.getByLabelText("App ID")).toHaveValue(482141));
      expect(screen.queryByRole("alert")).toBeNull();
      expect(screen.getByRole("heading", { name: "GitHub App" })).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("同页连续重载只接受最新请求", async () => {
    const { controller, adapter } = await startController();
    const firstSettle = deferred<void>();
    const originalApi = adapter.api.bind(adapter);
    let levelsCalls = 0;
    vi.spyOn(adapter, "api").mockImplementation(async (route, args) => {
      if (
        route === "settings.get" &&
        (args as { resource?: string }).resource === "levels"
      ) {
        levelsCalls += 1;
        const call = levelsCalls;
        if (call === 1) await firstSettle.promise;
        const result = (await originalApi(
          route as never,
          args as never,
        )) as ApiResult<SettingsDocument>;
        if (result.ok && result.body) {
          const value = result.body.value as {
            low: { budget: { nodes: number } };
          };
          value.low.budget.nodes = call === 1 ? 99 : 42;
        }
        return result;
      }
      return originalApi(route as never, args as never);
    });
    const view = (page: "levels" | "github") => (
      <Live controller={controller}>
        {(state) => (
          <SettingsResourcePage
            controller={controller}
            state={state}
            page={page}
            onDirtyChange={() => undefined}
          />
        )}
      </Live>
    );
    try {
      const { rerender } = render(view("levels"));
      await screen.findByRole("heading", { name: "档位与预算" });
      rerender(view("github"));
      await screen.findByLabelText("App ID");
      rerender(view("levels"));
      await waitFor(() =>
        expect(screen.getByLabelText("low nodes")).toHaveValue(42),
      );

      await act(async () => {
        firstSettle.resolve();
        await Promise.resolve();
      });
      await waitFor(() => expect(screen.getByLabelText("low nodes")).toHaveValue(42));
    } finally {
      await controller.stop();
    }
  });

  it("旧页保存后的回读不得改写新页草稿", async () => {
    const { controller, adapter } = await startController();
    const submitSettle = deferred<void>();
    const originalSubmit = adapter.guardedWriteSubmit.bind(adapter);
    vi.spyOn(adapter, "guardedWriteSubmit").mockImplementation(async (args) => {
      await submitSettle.promise;
      return originalSubmit(args);
    });
    const view = (page: "levels" | "github") => (
      <Live controller={controller}>
        {(state) => (
          <SettingsResourcePage
            controller={controller}
            state={state}
            page={page}
            onDirtyChange={() => undefined}
          />
        )}
      </Live>
    );
    try {
      const { rerender } = render(view("levels"));
      await screen.findByLabelText("low nodes");
      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      fireEvent.click(screen.getByRole("button", { name: "保存档位与预算" }));
      const submit = await screen.findByRole("button", { name: "模拟应用设置" });
      await waitFor(() => expect(submit).toBeEnabled());
      fireEvent.click(submit);

      rerender(view("github"));
      const appId = await screen.findByLabelText("App ID");
      expect(appId).toHaveValue(482139);

      await act(async () => {
        submitSettle.resolve();
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      await waitFor(() => expect(screen.getByLabelText("App ID")).toHaveValue(482139));
      expect(screen.getByRole("heading", { name: "GitHub App" })).toBeInTheDocument();
      expect(screen.queryByLabelText("low nodes")).toBeNull();
      expect(screen.queryByText(/此页已被其他设备修改/)).toBeNull();
    } finally {
      await controller.stop();
    }
  });
});

describe("F7：模型降级列表清空", () => {
  it("清空写为显式 fallback: []，恢复继承才删除键", async () => {
    const { controller, adapter } = await startController();
    try {
      const global = await adapter.api("settings.get", { resource: "models" });
      expect(global.ok).toBe(true);
      if (!global.ok || !global.body) throw new Error("no global models");
      const prepared = await adapter.guardedWritePrepare({
        kind: "settings_put",
        scope: "global",
        resource: "models",
        value: {
          ...(global.body.value as Record<string, unknown>),
          planner: {
            ...((global.body.value as { planner: Record<string, unknown> }).planner),
            fallback: ["anthropic/claude-opus-4-1"],
          },
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
      const fallback = screen.getByLabelText("大脑 降级模型 项目覆盖") as HTMLTextAreaElement;
      expect(fallback.value).toBe("");
      expect(fallback.placeholder).toContain("anthropic/claude-opus-4-1");

      fireEvent.change(screen.getByLabelText("大脑 提供方 项目覆盖"), {
        target: { value: "anthropic" },
      });
      fireEvent.change(screen.getByLabelText("大脑 模型 项目覆盖"), {
        target: { value: "claude-sonnet-4-5" },
      });
      fireEvent.change(fallback, {
        target: { value: "anthropic/claude-sonnet-4-5" },
      });
      fireEvent.change(fallback, { target: { value: "" } });
      expect(fallback.value).toBe("");
      expect(await screen.findByText("有未保存的改动")).toBeInTheDocument();

      fireEvent.click(screen.getByRole("button", { name: "保存模型覆盖" }));
      await screen.findByRole("button", { name: "模拟应用设置" });
      const preview = screen.getByLabelText("覆盖写入预览");
      expect(preview.textContent).toContain('"fallback": []');
      expect(screen.queryByText(/恢复继承（DELETE）/)).toBeNull();
      const apply = screen.getByRole("button", { name: "模拟应用设置" });
      await waitFor(() => expect(apply).toBeEnabled());
      fireEvent.click(apply);
      await screen.findByText(/模拟设置已应用/);

      const withEmpty = await readOverride(adapter, "prj_gantry", "models");
      const planner = (withEmpty.value as { planner?: { fallback?: unknown } })
        .planner;
      expect(planner).toBeDefined();
      expect(Object.hasOwn(planner as object, "fallback")).toBe(true);
      expect(planner?.fallback).toEqual([]);

      const group = screen.getByRole("group", { name: "大脑 降级模型 覆盖行" });
      fireEvent.click(within(group).getByRole("button", { name: "恢复继承" }));
      fireEvent.click(screen.getByRole("button", { name: "保存模型覆盖" }));
      await screen.findByRole("button", { name: "模拟应用设置" });
      expect(screen.getByLabelText("覆盖写入预览").textContent).not.toContain(
        '"fallback"',
      );
      fireEvent.click(screen.getByRole("button", { name: "模拟应用设置" }));
      await screen.findByText(/模拟设置已应用/);
      const inherited = await readOverride(adapter, "prj_gantry", "models");
      const inheritedPlanner = (
        inherited.value as {
          planner?: { fallback?: unknown };
        }
      ).planner;
      expect(inheritedPlanner).toBeDefined();
      expect(Object.hasOwn(inheritedPlanner as object, "fallback")).toBe(false);
    } finally {
      await controller.stop();
    }
  });

  it("共享 lines 单元格清空仍表示继承，只有降级列表保留空数组", () => {
    const otherChange = vi.fn();
    const otherField: ProjectOverrideField = {
      path: ["profiles", "default", "groups"],
      label: "档案 default 的分组",
      kind: "lines",
      tightenOnly: true,
    };
    const { unmount } = render(
      <ProjectOverrideFieldRow
        field={otherField}
        effectiveValue={["model_api"]}
        overrideValue={["model_api"]}
        onChange={otherChange}
        onClear={() => undefined}
      />,
    );
    fireEvent.change(screen.getByLabelText("档案 default 的分组 项目覆盖"), {
      target: { value: "" },
    });
    expect(otherChange).toHaveBeenCalledWith(undefined);
    unmount();

    const fallbackChange = vi.fn();
    const fallbackField: ProjectOverrideField = {
      path: ["planner", "fallback"],
      label: "大脑 降级模型",
      kind: "lines",
      tightenOnly: false,
      keepEmptyList: true,
    };
    render(
      <ProjectOverrideFieldRow
        field={fallbackField}
        effectiveValue={["anthropic/claude-opus-4-1"]}
        overrideValue={["anthropic/claude-opus-4-1"]}
        onChange={fallbackChange}
        onClear={() => undefined}
      />,
    );
    fireEvent.change(screen.getByLabelText("大脑 降级模型 项目覆盖"), {
      target: { value: "" },
    });
    expect(fallbackChange).toHaveBeenCalledWith([]);
  });
});

describe("F8：未保存确认的键盘焦点", () => {
  it("初始焦点在继续编辑，Tab 进入放弃并可用键盘完成切页", async () => {
    const user = userEvent.setup();
    const { controller } = await openSettings("levels");
    try {
      await screen.findByLabelText("low nodes");
      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      const nav = screen.getByRole("navigation", { name: "设置分类" });
      const target = within(nav).getByRole("button", { name: "外观" });
      target.focus();
      await user.keyboard("{Enter}");

      const dialog = await screen.findByRole("alertdialog", {
        name: "放弃未保存的改动",
      });
      const stay = within(dialog).getByRole("button", { name: "继续编辑" });
      const leave = within(dialog).getByRole("button", {
        name: "放弃改动并切换",
      });
      expect(stay).toHaveFocus();
      await user.tab();
      expect(leave).toHaveFocus();
      await user.keyboard("{Enter}");
      await screen.findByRole("heading", { name: "外观" });
      expect(screen.queryByRole("alertdialog")).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("Esc 取消确认、保留草稿并把焦点还给触发位置", async () => {
    const user = userEvent.setup();
    const { controller, onClose } = await openSettings("levels");
    try {
      await screen.findByLabelText("low nodes");
      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      const back = screen.getByRole("button", { name: "返回工作区" });
      back.focus();
      await user.keyboard("{Enter}");
      const dialog = await screen.findByRole("alertdialog", {
        name: "放弃未保存的改动",
      });
      expect(within(dialog).getByRole("button", { name: "继续编辑" })).toHaveFocus();

      await user.keyboard("{Escape}");
      await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
      expect(onClose).not.toHaveBeenCalled();
      expect(screen.getByLabelText("low nodes")).toHaveValue(7);
      expect(back).toHaveFocus();
    } finally {
      await controller.stop();
    }
  });

  it("Tab 在确认区循环，键盘确认后可关闭浮层", async () => {
    const user = userEvent.setup();
    const { controller, onClose } = await openSettings("levels");
    try {
      await screen.findByLabelText("low nodes");
      fireEvent.change(screen.getByLabelText("low nodes"), {
        target: { value: "7" },
      });
      const back = screen.getByRole("button", { name: "返回工作区" });
      back.focus();
      await user.keyboard("{Enter}");
      const dialog = await screen.findByRole("alertdialog", {
        name: "放弃未保存的改动",
      });
      const stay = within(dialog).getByRole("button", { name: "继续编辑" });
      const leave = within(dialog).getByRole("button", {
        name: "放弃改动并返回",
      });
      expect(stay).toHaveFocus();
      await user.tab();
      expect(leave).toHaveFocus();
      await user.keyboard("{Enter}");
      expect(onClose).toHaveBeenCalledTimes(1);
      await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    } finally {
      await controller.stop();
    }
  });
});

describe("项目页：旧项目迟到响应隔离", () => {
  async function mountProject(controller: DesktopController): Promise<void> {
    render(
      <Live controller={controller}>
        {(state) => (
          <SettingsOverlay
            controller={controller}
            state={state}
            initialPage="project:prj_gantry"
            onClose={vi.fn()}
          />
        )}
      </Live>,
    );
    await screen.findByLabelText("干活 模型 项目覆盖");
  }

  async function switchProject(name: string): Promise<void> {
    const nav = screen.getByRole("navigation", { name: "设置分类" });
    fireEvent.click(within(nav).getByRole("button", { name }));
    const dialog = screen.queryByRole("alertdialog", {
      name: "放弃未保存的改动",
    });
    if (dialog) {
      fireEvent.click(
        within(dialog).getByRole("button", { name: "放弃改动并切换" }),
      );
    }
    await waitFor(() => {
      expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(name);
      expect(screen.queryByText(/正在载入六类项目覆盖/)).toBeNull();
    });
  }

  async function stageAndApplyModels(value: string): Promise<void> {
    fireEvent.change(screen.getByLabelText("干活 模型 项目覆盖"), {
      target: { value },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存模型覆盖" }));
    const apply = await screen.findByRole("button", { name: "模拟应用设置" });
    await waitFor(() => expect(apply).toBeEnabled());
    fireEvent.click(apply);
  }

  it("A 已写入但响应迟到时，B 的草稿与实际设置都不被覆盖", async () => {
    const { controller, adapter } = await startController();
    const settle = deferred<void>();
    const originalSubmit = adapter.guardedWriteSubmit.bind(adapter);
    let held = false;
    const submit = vi
      .spyOn(adapter, "guardedWriteSubmit")
      .mockImplementation(async (args) => {
        // 先真正写入 A，只扣住返回给界面的响应。
        const result = await originalSubmit(args);
        if (!held) {
          held = true;
          await settle.promise;
        }
        return result;
      });
    try {
      await mountProject(controller);
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
        "claude-sonnet-5",
      );
      await stageAndApplyModels("claude-opus-4-1");
      await waitFor(() => expect(submit).toHaveBeenCalledTimes(1));
      const written = await readOverride(adapter, "prj_gantry", "models");
      expect(
        (written.value as { worker: { model: string } }).worker.model,
      ).toBe("claude-opus-4-1");
      expect(screen.getByText("有未保存的改动")).toBeInTheDocument();

      await switchProject("specgit");
      const field = screen.getByLabelText("干活 模型 项目覆盖");
      expect(field).toHaveValue("");
      fireEvent.change(field, { target: { value: "claude-haiku-4-5" } });

      await act(async () => {
        settle.resolve();
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      await waitFor(() => {
        expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
          "claude-haiku-4-5",
        );
        expect(screen.queryByText(/模拟设置已应用/)).toBeNull();
      });
      expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(
        "specgit",
      );
      const bDoc = await readOverride(adapter, "prj_specgit", "models");
      expect(bDoc.value).toBeNull();
      const aDoc = await readOverride(adapter, "prj_gantry", "models");
      expect(
        (aDoc.value as { worker: { model: string } }).worker.model,
      ).toBe("claude-opus-4-1");
    } finally {
      await controller.stop();
    }
  });

  it("A 的迟到 412 不得在 B 显示冲突或错误", async () => {
    const { controller, adapter } = await startController();
    const settle = deferred<void>();
    let held = false;
    const submit = vi
      .spyOn(adapter, "guardedWriteSubmit")
      .mockImplementation(async () => {
        if (!held) {
          held = true;
          await settle.promise;
        }
        return {
          ok: false,
          status: 412,
          problem: {
            code: "precondition_failed",
            title: "Precondition failed",
            request_id: "req_late_conflict",
          },
        } satisfies ApiResult<unknown>;
      });
    try {
      await mountProject(controller);
      await stageAndApplyModels("claude-opus-4-1");
      await waitFor(() => expect(submit).toHaveBeenCalledTimes(1));

      await switchProject("specgit");
      const field = screen.getByLabelText("干活 模型 项目覆盖");
      fireEvent.change(field, { target: { value: "claude-haiku-4-5" } });

      await act(async () => {
        settle.resolve();
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      expect(screen.queryByText(/提交冲突/)).toBeNull();
      expect(screen.queryByText(/此资源的覆盖已被其他设备修改/)).toBeNull();
      expect(screen.queryByRole("alert")).toBeNull();
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
        "claude-haiku-4-5",
      );
      const bDoc = await readOverride(adapter, "prj_specgit", "models");
      expect(bDoc.value).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("A 的迟到提交错误不得显示在 B", async () => {
    const { controller, adapter } = await startController();
    const settle = deferred<void>();
    let held = false;
    const submit = vi
      .spyOn(adapter, "guardedWriteSubmit")
      .mockImplementation(async () => {
        if (!held) {
          held = true;
          await settle.promise;
        }
        throw new Error("late submit failure");
      });
    try {
      await mountProject(controller);
      await stageAndApplyModels("claude-opus-4-1");
      await waitFor(() => expect(submit).toHaveBeenCalledTimes(1));

      await switchProject("specgit");
      const field = screen.getByLabelText("干活 模型 项目覆盖");
      fireEvent.change(field, { target: { value: "claude-haiku-4-5" } });

      await act(async () => {
        settle.resolve();
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      expect(screen.queryByRole("alert")).toBeNull();
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue(
        "claude-haiku-4-5",
      );
      const bDoc = await readOverride(adapter, "prj_specgit", "models");
      expect(bDoc.value).toBeNull();
    } finally {
      await controller.stop();
    }
  });

  it("提交挂起时切换项目会丢弃旧项目的受保护写入句柄", async () => {
    const { controller, adapter } = await startController();
    const settle = deferred<void>();
    const originalPrepare = adapter.guardedWritePrepare.bind(adapter);
    const originalSubmit = adapter.guardedWriteSubmit.bind(adapter);
    const pendingIds: string[] = [];
    vi.spyOn(adapter, "guardedWritePrepare").mockImplementation(async (args) => {
      const result = await originalPrepare(args);
      pendingIds.push(result.pending_id);
      return result;
    });
    let held = false;
    const submit = vi
      .spyOn(adapter, "guardedWriteSubmit")
      .mockImplementation(async (args) => {
        const result = await originalSubmit(args);
        if (!held) {
          held = true;
          await settle.promise;
        }
        return result;
      });
    const discard = vi.spyOn(adapter, "guardedWriteDiscard");
    try {
      await mountProject(controller);
      await stageAndApplyModels("claude-opus-4-1");
      await waitFor(() => expect(submit).toHaveBeenCalledTimes(1));
      expect(pendingIds).toHaveLength(1);

      await switchProject("specgit");
      await waitFor(() =>
        expect(discard).toHaveBeenCalledWith({ pendingId: pendingIds[0] }),
      );
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue("");

      await act(async () => {
        settle.resolve();
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      expect(screen.getByLabelText("干活 模型 项目覆盖")).toHaveValue("");
    } finally {
      await controller.stop();
    }
  });
});
