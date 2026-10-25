import { describe, expect, it } from "vitest";
import {
  createIpcAdapter,
  detectAdapterKind,
  isScenarioAdapter,
  scenarioFromLocation,
} from "@/ipc";
import { UnavailableIpcAdapter } from "@/ipc/unavailable";

describe("适配器工厂", () => {
  it("非 Tauri 生产返回明确的外壳不可用错误，不回落示例", async () => {
    expect(detectAdapterKind({ forceKind: "unavailable" })).toBe("unavailable");
    expect(detectAdapterKind({ hasTauri: false })).toBe("unavailable");
    const adapter = await createIpcAdapter({ forceKind: "unavailable" });
    expect(adapter).toBeInstanceOf(UnavailableIpcAdapter);
    await expect(adapter.identityStatus()).rejects.toMatchObject({
      code: "shell.unavailable",
    });
    await expect(
      adapter.guardedWriteSubmit({ pendingId: "pending" }),
    ).rejects.toMatchObject({ code: "shell.unavailable" });
  });

  it("Tauri 运行时选择原生适配器", () => {
    expect(detectAdapterKind({ hasTauri: true })).toBe("tauri");
  });

  it("显式场景选择示例适配器，并标记为示例", async () => {
    expect(detectAdapterKind({ forceKind: "scenario" })).toBe("scenario");
    const adapter = await createIpcAdapter({
      forceKind: "scenario",
      scenario: "viewer",
    });
    expect(isScenarioAdapter(adapter)).toBe(true);
    if (isScenarioAdapter(adapter)) {
      expect(adapter.scenario).toBe("viewer");
      expect(adapter.isScenario).toBe(true);
    }
  });

  it("query 参数选择场景，未知值回退 default", () => {
    expect(scenarioFromLocation("?scenario=offline")).toBe("offline");
    expect(scenarioFromLocation("?scenario=unknown")).toBe("default");
    expect(scenarioFromLocation("")).toBe("default");
  });
});
