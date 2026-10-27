/**
 * 前端测试会在进程内启动 tools/mock-api（tests/unit/globalSetup.ts）。
 * 它的依赖不在本包里；缺失时按其 lockfile 安装一次（npm ci，可复现）。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const mockDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../tools/mock-api");
if (!existsSync(path.join(mockDir, "node_modules", "ajv-formats"))) {
  console.log("[pretest] 安装 tools/mock-api 依赖（npm ci）…");
  const result = spawnSync("npm", ["ci", "--prefix", mockDir, "--no-audit", "--no-fund"], {
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  process.exit(result.status ?? 1);
}
