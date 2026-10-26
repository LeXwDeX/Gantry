import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useSyncExternalStore, type ReactNode } from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { InboxOverlay, PairingView } from "@/features/overlays/Overlays";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import type { InboxItem } from "@/api/types";
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

afterEach(() => {
  vi.restoreAllMocks();
});

describe("InboxOverlay", () => {
  it("按项目分组、选项来自 payload，选择只调 choose 且计数同步", async () => {
    const { controller, adapter } = await startController();
    const choose = vi.spyOn(adapter, "api");
    const onApproval = vi.fn();
    try {
      render(
        <Live controller={controller}>
          {(state) => (
            <InboxOverlay
              controller={controller}
              state={state}
              onApproval={onApproval}
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      expect(screen.getByText(/需要你 · 全部项目 · 4 项/)).toBeInTheDocument();
      expect(screen.getByText("示例数据")).toBeInTheDocument();
      const gantry = screen.getByRole("region", { name: "gantry 项目" });
      const specgit = screen.getByRole("region", { name: "specgit 项目" });
      expect(within(gantry).getByText("3 项")).toBeInTheDocument();
      expect(within(specgit).getByText("1 项")).toBeInTheDocument();

      fireEvent.click(
        within(gantry).getByRole("button", { name: "审阅审批…" }),
      );
      expect(onApproval).toHaveBeenCalledWith("apr_gantry_9");

      fireEvent.click(
        within(gantry).getByRole("button", { name: "升到 Medium 并继续" }),
      );
      await waitFor(() => expect(choose).toHaveBeenCalled());
      expect(choose).toHaveBeenCalledWith(
        "inbox.choose",
        expect.objectContaining({
          inboxId: "inb_esc6",
          optionId: "upgrade",
          idempotencyKey: expect.any(String),
        }),
      );
      await waitFor(() =>
        expect(
          within(gantry).queryByRole("button", { name: "升到 Medium 并继续" }),
        ).toBeNull(),
      );
      expect(screen.getByText(/需要你 · 全部项目 · 3 项/)).toBeInTheDocument();
      expect(within(gantry).getByText("2 项")).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("补规格回到对应项目对话并设置交付上下文", async () => {
    const { controller } = await startController();
    const onClose = vi.fn();
    try {
      render(
        <Live controller={controller}>
          {(state) => (
            <InboxOverlay
              controller={controller}
              state={state}
              onApproval={vi.fn()}
              onClose={onClose}
            />
          )}
        </Live>,
      );
      const specgit = screen.getByRole("region", { name: "specgit 项目" });
      expect(
        within(specgit).getByText("watch 超时的可配置范围与默认值是什么？"),
      ).toBeInTheDocument();
      fireEvent.click(within(specgit).getByRole("button", { name: "回复补充…" }));
      await waitFor(() => expect(onClose).toHaveBeenCalled());
      expect(controller.getSnapshot().currentProjectId).toBe("prj_specgit");
      expect(controller.currentUi.view).toBe("chat");
      expect(controller.currentUi.ctx).toEqual({
        kind: "delivery",
        id: "dlv_specgit_31",
      });
    } finally {
      await controller.stop();
    }
  });

  it("安全发现忽略需二次确认", async () => {
    const { controller } = await startController();
    const dismiss = vi.spyOn(controller, "dismiss").mockResolvedValue(undefined);
    const finding: InboxItem = {
      id: "inb_security",
      project_id: "prj_gantry",
      delivery_id: "dlv_gantry_9",
      kind: "security_finding",
      severity: "blocker",
      summary: "依赖中存在已知漏洞。",
      payload: {
        source: { kind: "pr", repo: "LeXwDeX/gantry", number: 9 },
        finding: {
          severity: "high",
          category: "dependency",
          title: { text: "<img src=x onerror=alert(1)>", source: "scan", truncated: false },
        },
      },
      status: "open",
      dedup_key: "security:test",
    };
    const state = controller.getSnapshot();
    try {
      render(
        <InboxOverlay
          controller={controller}
          state={{ ...state, inbox: [...state.inbox, finding] }}
          onApproval={vi.fn()}
          onClose={vi.fn()}
        />,
      );
      expect(screen.getByText("<img src=x onerror=alert(1)>")).toBeInTheDocument();
      expect(document.querySelector("img")).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "忽略…" }));
      expect(dismiss).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "确认忽略" }));
      expect(dismiss).toHaveBeenCalledWith("inb_security");
    } finally {
      await controller.stop();
    }
  });

  it("不安全文本只作为纯文本显示", async () => {
    const { controller } = await startController("unsafe");
    try {
      const { container } = render(
        <Live controller={controller}>
          {(state) => (
            <InboxOverlay
              controller={controller}
              state={state}
              onApproval={vi.fn()}
              onClose={vi.fn()}
            />
          )}
        </Live>,
      );
      expect(
        await screen.findByText(/<iframe src=javascript:alert\(1\)>/),
      ).toBeInTheDocument();
      expect(container.querySelector("iframe")).toBeNull();
    } finally {
      await controller.stop();
    }
  });
});

describe("PairingView", () => {
  async function unpaired() {
    const adapter = new ScenarioIpcAdapter("unpaired");
    const controller = new (await import("@/store/controller")).DesktopController(
      adapter,
      { refreshDelayMs: 0, now: () => TEST_NOW },
    );
    await controller.start();
    return { adapter, controller };
  }

  it("模拟配对：固定测试确认码只来自适配层，配对串用后清空且不生成真实证书", async () => {
    const { controller, adapter } = await unpaired();
    const begin = vi.spyOn(adapter, "pairingBegin");
    try {
      render(
        <Live controller={controller}>
          {(state) => <PairingView controller={controller} state={state} />}
        </Live>,
      );
      const pairingInput = screen.getByLabelText("配对串");
      expect(screen.getByText("示例数据")).toBeInTheDocument();
      expect(
        screen.getByText(/模拟配对：只在本会话内存中核对设备名与确认码/),
      ).toBeInTheDocument();
      fireEvent.change(pairingInput, {
        target: { value: "gantry+pair://example" },
      });
      fireEvent.change(screen.getByLabelText("设备名"), {
        target: { value: "测试 Mac" },
      });
      fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
      expect(await screen.findByText("492 713")).toBeInTheDocument();
      expect(begin).toHaveBeenCalledWith({
        pairingString: "gantry+pair://example",
        deviceName: "测试 Mac",
      });
      expect(pairingInput).toHaveValue("");
      expect(document.body.textContent).not.toContain("gantry+pair://example");
      expect(document.body.textContent).not.toContain("signature");
      expect(document.body.textContent).not.toContain("BEGIN CERTIFICATE");

      fireEvent.click(screen.getByRole("button", { name: "不一致" }));
      await waitFor(() =>
        expect(screen.getByText(/等待外壳丢弃待定证书/)).toBeInTheDocument(),
      );
      expect(controller.getSnapshot().identity?.state).toBe("unpaired");
    } finally {
      await controller.stop();
    }
  });

  it("模拟配对失败后回到表单，下一次重试可恢复", async () => {
    const { controller } = await unpaired();
    try {
      render(
        <Live controller={controller}>
          {(state) => <PairingView controller={controller} state={state} />}
        </Live>,
      );
      fireEvent.click(
        screen.getByRole("button", { name: "模拟下次配对失败" }),
      );
      expect(
        await screen.findByText(/下一次配对预检将模拟失败/),
      ).toBeInTheDocument();
      fireEvent.change(screen.getByLabelText("配对串"), {
        target: { value: "gantry+pair://example" },
      });
      fireEvent.change(screen.getByLabelText("设备名"), {
        target: { value: "测试 Mac" },
      });
      fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
      expect(
        await screen.findByText(/模拟配对预检失败；未生成密钥或证书/),
      ).toBeInTheDocument();
      expect(screen.getByLabelText("配对串")).toHaveValue("");

      fireEvent.change(screen.getByLabelText("配对串"), {
        target: { value: "gantry+pair://retry" },
      });
      fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
      expect(await screen.findByText("492 713")).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it("确认码只来自 IPC 响应，确认后调用 pairingConfirm", async () => {
    const { controller, adapter } = await unpaired();
    vi.spyOn(adapter, "pairingBegin").mockResolvedValue({
      confirmationCode: "482913",
      device: {
        id: "dev_new",
        name: "测试 Mac",
        platform: "macos",
        cert_serial: "serial",
        pubkey_fpr: "sha256:pub",
        approval_pubkey_fpr: "sha256:approval",
        role: "approver",
        status: "active",
        last_seen: null,
      },
    });
    const confirm = vi.spyOn(adapter, "pairingConfirm").mockResolvedValue({
      state: "paired",
      server_host: "gantry.example.com:8443",
      ca_fingerprint: "sha256:ca",
      device: {
        id: "dev_new",
        name: "测试 Mac",
        platform: "macos",
        pubkey_fpr: "sha256:pub",
        approval_pubkey_fpr: "sha256:approval",
      },
      cert: { serial: "serial", not_after: "2026-11-23T12:00:00Z", renew_after: "2026-11-13T12:00:00Z" },
      key_backend: "secure_enclave",
    });
    vi.spyOn(controller, "refresh").mockResolvedValue(undefined);
    try {
      render(
        <Live controller={controller}>
          {(state) => <PairingView controller={controller} state={state} />}
        </Live>,
      );
      fireEvent.change(screen.getByLabelText("配对串"), {
        target: { value: "gantry+pair://example" },
      });
      fireEvent.change(screen.getByLabelText("设备名"), {
        target: { value: "测试 Mac" },
      });
      fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
      expect(await screen.findByText("482913")).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "一致" }));
      await waitFor(() => expect(confirm).toHaveBeenCalledWith({ matches: true }));
      expect(
        await screen.findByText(/配对已确认/),
      ).toBeInTheDocument();
    } finally {
      await controller.stop();
    }
  });

  it.each(["revoked", "cert_expired"] as const)(
    "身份已配对但连接为 %s 时显示真实阻塞原因与重置入口",
    async (phase) => {
      const { controller } = await startController();
      try {
        const state = controller.getSnapshot();
        if (!state.identity || !state.connection) throw new Error("缺少身份或连接状态");
        render(
          <PairingView
            controller={controller}
            state={{
              ...state,
              status: "unpaired",
              identity: { ...state.identity, state: "paired" },
              connection: { ...state.connection, phase },
            }}
          />,
        );
        const expected =
          phase === "revoked"
            ? /此设备已被吊销，需要重新配对/
            : /证书已过期，需要重新配对/;
        expect(screen.getAllByText(expected).length).toBeGreaterThan(0);
        expect(screen.queryByText("设备已配对。")).not.toBeInTheDocument();
        expect(
          screen.getByRole("button", { name: "重置本机身份…" }),
        ).toBeInTheDocument();
      } finally {
        await controller.stop();
      }
    },
  );

  it("已吊销设备显示重新配对与重置入口", async () => {
    const adapter = new ScenarioIpcAdapter("revoked");
    const controller = new (await import("@/store/controller")).DesktopController(
      adapter,
      { refreshDelayMs: 0, now: () => TEST_NOW },
    );
    await controller.start();
    const reset = vi.spyOn(adapter, "identityReset");
    try {
      render(
        <Live controller={controller}>
          {(state) => <PairingView controller={controller} state={state} />}
        </Live>,
      );
      expect(screen.getAllByText(/此设备已被吊销/).length).toBeGreaterThan(0);
      fireEvent.click(screen.getByRole("button", { name: "重置本机身份…" }));
      fireEvent.click(screen.getByRole("button", { name: "确认重置" }));
      await waitFor(() => expect(reset).toHaveBeenCalledTimes(1));
    } finally {
      await controller.stop();
    }
  });
});
