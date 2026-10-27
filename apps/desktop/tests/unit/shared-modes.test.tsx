import { afterEach, describe, expect, it } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { projectName } from "@/features/workspace/model";
import { MODES, mountMode, type Mode, type Mounted } from "./modes";

let mounted: Mounted | null = null;

afterEach(async () => {
  await mounted?.stop();
  mounted = null;
});

async function mount(mode: Mode, options?: Parameters<typeof mountMode>[1]): Promise<Mounted> {
  mounted = await mountMode(mode, options);
  return mounted;
}

function nav(): HTMLElement {
  return screen.getByRole("navigation", { name: "项目" });
}

function leftPane(): HTMLElement {
  return screen.getByRole("complementary", { name: "项目信息" });
}

function inspector(): HTMLElement {
  return screen.getByRole("complementary", { name: "检视器" });
}

/** 切到第一个有对话的项目，返回其项目 ID。 */
async function projectWithConversation(m: Mounted): Promise<string> {
  const projects = await m.read("projects.list", { limit: 100 });
  for (const project of projects.items) {
    const conversations = await m.read("conversations.list", { projectId: project.id, limit: 1 });
    if (conversations.items.length > 0) {
      fireEvent.click(within(nav()).getByRole("button", { name: new RegExp(projectName(project)) }));
      await waitFor(() => expect(m.controller.getSnapshot().currentProjectId).toBe(project.id));
      // 首次切到的项目才开始拉对话与交付；等它们载入完成再操作。
      await waitFor(
        () => {
          const data = m.controller.getSnapshot().projectData[project.id];
          expect(data?.conversation).toBeTruthy();
          expect(screen.queryByText("正在载入工作区…")).toBeNull();
        },
        { timeout: 10_000 },
      );
      return project.id;
    }
  }
  throw new Error("没有带对话的项目");
}

// 整个 App 渲染加上 Mock 的真实 HTTP 往返，比单组件测试慢；给足时间避免高负载下误报。
describe.each(MODES)("共用组件测试（%s 模式）", { timeout: 20_000 }, (mode) => {
  it("项目 tab 与徽标来自 projects.list 与 inbox.summary；顶栏计数等于 total_open（P-63）", async () => {
    const m = await mount(mode);
    const projects = await m.read("projects.list", { limit: 100 });
    const summary = await m.read("inbox.summary", {});
    for (const project of projects.items) {
      const tab = within(nav()).getByRole("button", { name: new RegExp(projectName(project)) });
      const open = summary.by_project.find((entry) => entry.project_id === project.id)?.open ?? 0;
      if (open > 0) {
        expect(within(tab).getByLabelText(`需要你 ${open} 项`)).toBeInTheDocument();
      } else {
        expect(within(tab).queryByLabelText(/需要你/)).toBeNull();
      }
    }
    expect(screen.getByRole("button", { name: `需要你 · ${summary.total_open}` })).toBeInTheDocument();
  });

  it("E2E-01：左栏选中不改变发送上下文；请求体 refs 为空且不含 level_cap", async () => {
    const m = await mount(mode);
    const projectId = await projectWithConversation(m);
    const active = await m.read("deliveries.list", { projectId, active: true, limit: 50 });
    if (active.items.length > 0) {
      const rows = await within(leftPane()).findAllByRole("button", { name: /#\d+/ });
      fireEvent.click(rows[0]);
      await waitFor(() => expect(m.controller.getSnapshot().ui[projectId]?.sel).not.toBeNull());
    }
    expect(screen.getByTitle("选择这条消息的上下文")).toHaveTextContent("@ 全项目");
    const input = screen.getByLabelText("给 agent 发消息");
    fireEvent.change(input, { target: { value: "共用测试消息" } });
    await waitFor(() => expect(m.controller.getSnapshot().ui[projectId]?.draft).toBe("共用测试消息"));
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    // 刚切换的项目可能还在载入对话：消息先入队，载入后再发送。
    await waitFor(
      () => {
        const s = m.controller.getSnapshot();
        expect(
          m.calls("messages.post"),
          `notice=${s.notice} phase=${s.connection?.phase} outbox=${JSON.stringify(s.ui[projectId]?.outbox)}`,
        ).toHaveLength(1);
      },
      { timeout: 10_000 },
    );
    const [args] = m.calls("messages.post");
    expect(args.body.refs).toEqual([]);
    expect("level_cap" in args.body).toBe(false);
    expect(args.idempotencyKey).toMatch(/^[A-Za-z0-9_-]{16,64}$/);
    // 发送成功后消息列表还会刷新并替换节点；在 waitFor 内查询，避免拿到已卸下的旧节点。
    await waitFor(() =>
      expect(within(screen.getByRole("main")).getByText("共用测试消息")).toBeInTheDocument(),
    );
  });

  it("E2E-05：只读设备隐藏设置、禁用输入", async () => {
    await mount(mode, { role: "viewer" });
    expect(screen.queryByRole("button", { name: "设置" })).toBeNull();
    expect(screen.getByLabelText("给 agent 发消息")).toBeDisabled();
  });

  it("检视器 ⋯ 暂停：调用 deliveries.pause 并带 Idempotency-Key（P-56）", async () => {
    const m = await mount(mode);
    const projectId = m.controller.getSnapshot().currentProjectId;
    let deliveries = await m.read("deliveries.list", { projectId: projectId ?? undefined, active: true, limit: 50 });
    if (!deliveries.items.some((d) => d.stage_status !== "paused")) {
      await projectWithConversation(m);
      deliveries = await m.read("deliveries.list", {
        projectId: m.controller.getSnapshot().currentProjectId ?? undefined,
        active: true,
        limit: 50,
      });
    }
    const target = deliveries.items.find(
      (d) => d.stage_status !== "paused" && (d.issues.length > 0 || d.pr),
    );
    if (!target) throw new Error("没有可暂停的交付");
    const label = target.issues[0] ? `#${target.issues[0].number}` : `PR #${target.pr?.number}`;
    const row = (
      await within(leftPane()).findAllByRole("button", { name: new RegExp(`^${label} `) })
    ).at(0);
    if (!row) throw new Error("左栏没有该交付");
    fireEvent.click(row);
    fireEvent.click(within(inspector()).getByRole("button", { name: "更多操作" }));
    fireEvent.click(within(inspector()).getByRole("menuitem", { name: "暂停" }));
    await waitFor(() => expect(m.calls("deliveries.pause")).toHaveLength(1));
    const [args] = m.calls("deliveries.pause");
    expect(args.deliveryId).toBe(target.id);
    expect(args.idempotencyKey).toMatch(/^[A-Za-z0-9_-]{16,64}$/);
  });

  it("审批单：从审批入口打开，显示 signed 区完整摘要与目标", async () => {
    const m = await mount(mode);
    const approvals = await m.read("approvals.list", { status: "pending", limit: 50 });
    const approval = approvals.items[0];
    if (!approval) throw new Error("没有待签审批");
    if (approval.delivery_id) {
      const delivery = await m.read("deliveries.get", { deliveryId: approval.delivery_id });
      const projects = await m.read("projects.list", { limit: 100 });
      const project = projects.items.find((p) => p.id === delivery.project_id);
      if (project) fireEvent.click(within(nav()).getByRole("button", { name: new RegExp(projectName(project)) }));
    }
    const entry = (await screen.findAllByRole("button", { name: "审阅审批…" }))[0];
    fireEvent.click(entry);
    const sheet = await screen.findByRole("dialog");
    expect(await within(sheet).findByText(approval.subject_digest)).toBeInTheDocument();
    expect(within(sheet).getByText("未签名信息，仅供参考")).toBeInTheDocument();
  });

  it("设置 · 设备与证书：列表等于 devices.list（含已吊销）", async () => {
    const m = await mount(mode);
    const devices = await m.read("devices.list", { limit: 100 });
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(await screen.findByRole("button", { name: /设备与证书/ }));
    await screen.findByRole("heading", { name: "设备与证书" });
    for (const device of devices.items) {
      expect(await screen.findByRole("article", { name: device.name })).toBeInTheDocument();
    }
  });

  it("设置 · 模型页：从服务端载入并显示凭据“已设置”而不回显值", async () => {
    await mount(mode);
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(await screen.findByRole("button", { name: /^模型/ }));
    expect((await screen.findAllByRole("heading", { name: /模型/ })).length).toBeGreaterThan(0);
    await waitFor(() => expect(screen.queryByText(/正在载入/)).toBeNull(), { timeout: 10_000 });
    expect(screen.getAllByText(/已设置/).length).toBeGreaterThan(0);
  });

  it("配对向导：确认码来自 IPC 响应，一致后调用 pairingConfirm", async () => {
    const m = await mount(mode, { paired: false });
    expect(await screen.findByRole("main", { name: "配对设备" })).toBeInTheDocument();
    const begin = m.adapter.pairingBegin.bind(m.adapter);
    let code = "";
    m.adapter.pairingBegin = async (a) => {
      const result = await begin(a);
      code = result.confirmationCode;
      return result;
    };
    fireEvent.change(screen.getByLabelText("配对串"), { target: { value: "gantry+pair://example" } });
    fireEvent.change(screen.getByLabelText("设备名"), { target: { value: "测试 Mac" } });
    fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
    await waitFor(() => expect(code).not.toBe(""));
    expect(await screen.findByText(code)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "一致" }));
    await waitFor(() => expect(m.controller.getSnapshot().identity?.state).toBe("paired"), {
      timeout: 10_000,
    });
  });
});
