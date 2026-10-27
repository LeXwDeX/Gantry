import type { IpcAdapter } from "@/api/shell";
import type { ScenarioIpcAdapter } from "./scenario/adapter";
import type { ScenarioName } from "./scenario/data";
import type { MockIpcAdapter } from "./mock/adapter";
import { UnavailableIpcAdapter } from "./unavailable";
import { TauriIpcAdapter } from "./tauri";

export type AdapterKind = "tauri" | "scenario" | "mock" | "unavailable";

export interface AdapterOptions {
  /** 测试可强制选择；生产不传。 */
  forceKind?: AdapterKind;
  /** 测试可强制场景名；生产不传。 */
  scenario?: ScenarioName;
  /** 测试可模拟非 Tauri 浏览器；生产不传。 */
  hasTauri?: boolean;
  /** mock 模式的 Mock API 根地址；浏览器默认同源（经 Vite 代理）。 */
  mockBaseUrl?: string;
  /** mock 模式初始是否已配对；默认读 `?paired=0`。 */
  mockPaired?: boolean;
}

/** 从 `?scenario=` 读取显式开发场景；空值等同 default，未知值也按 default。 */
export function scenarioFromLocation(search: string): ScenarioName {
  const params = new URLSearchParams(search);
  if (!params.has("scenario")) return "default";
  const value = params.get("scenario");
  const allowed: ScenarioName[] = [
    "default",
    "offline",
    "viewer",
    "operator",
    "unpaired",
    "revoked",
    "expired",
    "unsafe",
    "error",
    "stale",
  ];
  return value && (allowed as string[]).includes(value) ? (value as ScenarioName) : "default";
}

export function detectAdapterKind(
  options: AdapterOptions = {},
): AdapterKind {
  if (options.forceKind) return options.forceKind;
  if (import.meta.env.MODE === "scenario") return "scenario";
  if (import.meta.env.MODE === "mock") return "mock";
  const hasTauri =
    options.hasTauri ??
    (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window);
  return hasTauri ? "tauri" : "unavailable";
}

/**
 * 适配器工厂。
 * - `--mode scenario`：动态加载示例数据；正常生产构建中该分支被静态条件折叠，
 *   不会把示例数据带入主包。
 * - `--mode mock`：浏览器直连 Mock API 的外壳替身（同样不进入正式包）。
 * - Tauri 运行时：真实外壳。
 * - 其他生产环境：明确的外壳不可用错误，**绝不**回落到示例。
 */
export async function createIpcAdapter(
  options: AdapterOptions = {},
): Promise<IpcAdapter> {
  // 生产构建把 MODE 折叠为 "production"，示例替身的动态 import 随之被移除。
  const scenarioRequested =
    import.meta.env.MODE === "scenario" ||
    (import.meta.env.MODE !== "production" &&
      options.forceKind === "scenario");
  if (scenarioRequested) {
    const { ScenarioIpcAdapter: ScenarioAdapter } = await import(
      "./scenario/adapter"
    );
    return new ScenarioAdapter(
      options.scenario ?? scenarioFromLocation(window.location.search),
    );
  }
  const mockRequested =
    import.meta.env.MODE === "mock" ||
    (import.meta.env.MODE !== "production" && options.forceKind === "mock");
  if (mockRequested) {
    const { MockIpcAdapter: MockAdapter } = await import("./mock/adapter");
    return new MockAdapter({
      baseUrl: options.mockBaseUrl ?? "",
      paired:
        options.mockPaired ??
        new URLSearchParams(window.location.search).get("paired") !== "0",
    });
  }
  return detectAdapterKind(options) === "tauri"
    ? new TauriIpcAdapter()
    : new UnavailableIpcAdapter();
}

export function isScenarioAdapter(
  adapter: IpcAdapter,
): adapter is ScenarioIpcAdapter {
  return "isScenario" in adapter && (adapter as { isScenario?: unknown }).isScenario === true;
}

export function isMockAdapter(adapter: IpcAdapter): adapter is MockIpcAdapter {
  return "isMock" in adapter && (adapter as { isMock?: unknown }).isMock === true;
}

export type { MockIpcAdapter, ScenarioIpcAdapter, ScenarioName };
export type {
  Simulation,
  SimulationAction,
  SimulationScript,
  SimulationScriptId,
  SimulationSnapshot,
} from "@/demo/types";
export { UnavailableIpcAdapter } from "./unavailable";
export { TauriIpcAdapter } from "./tauri";
export { fromUnknownError } from "./errors";
export { IpcError } from "@/api/shell";
