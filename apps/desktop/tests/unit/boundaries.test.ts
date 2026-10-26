import { execFileSync } from "node:child_process";
import path from "node:path";
import { describe, expect, it } from "vitest";

describe("静态边界检查", () => {
  it("禁用的 HTML 注入、网络调用、内联样式与 Tauri 越界引用为零", () => {
    const root = path.resolve(process.cwd());
    const output = execFileSync("node", ["scripts/check-boundaries.mjs"], {
      cwd: root,
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(output).toContain("静态边界检查通过");
  });
});
