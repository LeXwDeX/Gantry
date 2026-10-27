/**
 * 场景模式与真实 API（Mock）模式共用的组件测试支撑（#103）。
 *
 * 两种模式的数据集不同，所以共用测试只断言“界面呈现 == 适配器返回的事实”，
 * 事实一律经同一个 `IpcAdapter` 读出，而不是写死某个数据集里的名字。
 */
import { render, screen, waitFor } from "@testing-library/react";
import { vi, type MockInstance } from "vitest";
import { App } from "@/App";
import type { IpcAdapter } from "@/api/shell";
import type { RouteArgs, RouteBody, RouteId } from "@/api/routes";
import { DesktopController } from "@/store/controller";
import { MockIpcAdapter } from "@/ipc/mock/adapter";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { startMockApi, type RunningMockApi } from "./mockApi";
import { TEST_NOW } from "./helpers";

export type Mode = "scenario" | "mock";
export const MODES: Mode[] = ["scenario", "mock"];

/** Mock 夹具里本机设备的 ID。 */
const MOCK_SELF = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";

export interface Mounted {
  mode: Mode;
  adapter: IpcAdapter;
  controller: DesktopController;
  mock: RunningMockApi | null;
  apiSpy: MockInstance<IpcAdapter["api"]>;
  /** 经同一适配器读出事实；失败直接抛出。 */
  read<R extends RouteId>(route: R, args: RouteArgs[R]): Promise<RouteBody[R]>;
  /** 某路由被调用时的参数列表。 */
  calls<R extends RouteId>(route: R): RouteArgs[R][];
  stop(): Promise<void>;
}

export interface MountOptions {
  role?: "viewer" | "operator";
  paired?: boolean;
  /** 只启动控制器，不渲染 App。 */
  renderApp?: boolean;
}

export async function mountMode(mode: Mode, options: MountOptions = {}): Promise<Mounted> {
  let mock: RunningMockApi | null = null;
  let adapter: IpcAdapter;
  let now: () => number;
  if (mode === "scenario") {
    adapter = new ScenarioIpcAdapter(
      options.paired === false ? "unpaired" : (options.role ?? "default"),
    );
    now = () => TEST_NOW;
  } else {
    mock = await startMockApi();
    if (options.role) await mock.mutate("devices", MOCK_SELF, { role: options.role });
    adapter = new MockIpcAdapter({
      baseUrl: mock.baseUrl,
      reconnectDelayMs: 20,
      paired: options.paired ?? true,
    });
    // Mock 的服务端时间固定在夹具里；界面时钟对齐到它，避免审批与 challenge 被判过期。
    const me = await globalThis.fetch(`${mock.baseUrl}/v1/me`).then((r) => r.json());
    const offset = Date.parse((me as { server: { time: string } }).server.time) - Date.now();
    now = () => Date.now() + offset;
  }
  const apiSpy = vi.spyOn(adapter, "api");
  const controller = new DesktopController(adapter, {
    refreshDelayMs: 0,
    persistDelayMs: 0,
    prefetchWorkflows: false,
    now,
  });
  if (options.renderApp === false) {
    await controller.start();
  } else {
    render(<App controller={controller} />);
  }
  await waitFor(
    () => {
      const status = controller.getSnapshot().status;
      if (status !== "ready" && status !== "unpaired") throw new Error(`status=${status}`);
    },
    { timeout: 15_000 },
  );
  if (options.renderApp !== false && controller.getSnapshot().status === "ready") {
    await screen.findByRole("navigation", { name: "项目" });
  }

  async function read<R extends RouteId>(route: R, args: RouteArgs[R]): Promise<RouteBody[R]> {
    const result = await adapter.api(route, args);
    if (!result.ok || result.body === null) throw new Error(`${route} failed`);
    return result.body;
  }

  return {
    mode,
    adapter,
    controller,
    mock,
    apiSpy,
    read,
    calls: <R extends RouteId>(route: R) =>
      apiSpy.mock.calls.filter(([r]) => r === route).map(([, a]) => a as RouteArgs[R]),
    async stop() {
      await controller.stop();
      await mock?.close();
    },
  };
}
