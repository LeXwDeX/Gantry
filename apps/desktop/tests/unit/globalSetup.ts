import type { TestProject } from "vitest/node";
// @ts-expect-error -- 纯 JS 测试脚本，无类型声明。
import { startHarness } from "../../scripts/mock-harness.mjs";

declare module "vitest" {
  export interface ProvidedContext {
    mockControlUrl: string;
  }
}

/** 为所有测试文件启动一个 Mock API 场景控制端口；各文件按需新建独立实例。 */
export default async function setup(project: TestProject): Promise<() => Promise<void>> {
  const harness = (await startHarness({ heartbeatMs: 60_000 })) as {
    controlUrl: string;
    close(): Promise<void>;
  };
  project.provide("mockControlUrl", harness.controlUrl);
  return () => harness.close();
}
