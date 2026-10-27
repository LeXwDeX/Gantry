// 契约行为用例集对 Mock API 的运行入口；真实 `gantry-api` 的入口在 shared/real.mjs。
import test from "node:test";
import { startMockApi } from "../server.mjs";
import { sharedBehavior } from "./shared/behavior.mjs";

let mock;

test.before(async () => {
  mock = await startMockApi({ port: 0, heartbeatMs: 40 });
});

test.after(async () => {
  await mock.close();
});

sharedBehavior({ test, base: () => mock.baseUrl, name: "mock" });
