// 契约行为用例集对真实 `gantry-api` 的运行入口。由 `crates/gantry-api/tests/conformance.rs`
// 启动测试服务后以 `GANTRY_API_URL` 调用；单独运行时缺少该变量即失败。
import assert from "node:assert/strict";
import test from "node:test";
import { sharedBehavior } from "./behavior.mjs";

const url = process.env.GANTRY_API_URL;
assert.ok(url, "GANTRY_API_URL must point at a running gantry-api test server");

sharedBehavior({ test, base: () => url, name: "gantry-api" });
