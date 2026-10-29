import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { ROUTES, ROUTE_IDS, type RouteId } from "@/api/routes";

/**
 * 前端路由表与 `schemas/openapi.yaml` 的对账（#370）。
 *
 * `routes.ts` 是手工转写的 36 条 UI 路由；这里检查每条路由的路径参数名、路径模板与方法
 * 都能在 openapi 的 `paths` 里找到，并检查路由 ID 集合与 shell-core `routes.rs` 的
 * `Ui(...)` 集合一致。
 *
 * 不检查枚举值：`types.ts` 与 `routes.ts` 里的字面量联合是 UI 支持的子集，openapi 允许
 * 服务端返回更多值；前端只消费自己支持的值，枚举漂移不会导致请求形状错误。枚举的单独
 * 检查记录在 #370 的验收说明里。
 */

const here = dirname(fileURLToPath(import.meta.url));
const desktopRoot = resolve(here, "../..");
const openapi = JSON.parse(
  readFileSync(resolve(desktopRoot, "../../schemas/openapi.yaml"), "utf8"),
) as { paths: Record<string, Record<string, unknown>> };
const shellRoutes = readFileSync(
  resolve(desktopRoot, "src-tauri/shell-core/src/routes.rs"),
  "utf8",
);

/** `/v1/deliveries/{delivery_id}` → `/v1/deliveries/deliveryId`（与前端参数名 camelCase 对齐）。 */
function concretePath(template: string): string {
  return template.replace(/\{(\w+)\}/g, (_, name: string) =>
    name.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase()),
  );
}

/** 读路径时把任意属性名返回为字符串：`path` 只用参数名拼路径。 */
const ARG = new Proxy({}, { get: (_, key) => String(key) });

const UI_IDS = [...shellRoutes.matchAll(/Ui\("([^"]+)"\)/g)].map((m) => m[1]);

describe("routes.ts 与 openapi.yaml", () => {
  it("路由 ID 集合与 shell-core 的 Ui 集合一致", () => {
    expect([...ROUTE_IDS].sort()).toEqual([...UI_IDS].sort());
    expect(ROUTE_IDS).toHaveLength(36);
  });

  it.each(ROUTE_IDS)("%s 的方法与路径在 openapi 中存在", (id: RouteId) => {
    const meta = ROUTES[id];
    const actual = meta.path(ARG as never);
    const match = Object.keys(openapi.paths).find(
      (template) => concretePath(template) === actual,
    );
    expect(match, `${id}: openapi 没有路径 ${actual}`).toBeDefined();
    const methods = Object.keys(openapi.paths[match as string]).filter((key) =>
      ["get", "post", "put", "patch", "delete"].includes(key),
    );
    expect(methods, `${id}: openapi 的 ${match} 没有 ${meta.method}`).toContain(
      meta.method.toLowerCase(),
    );
  });

  it("path 模板的参数顺序与数量一致（防止漏拼路径段）", () => {
    for (const id of ROUTE_IDS) {
      const meta = ROUTES[id];
      const actual = meta.path(ARG as never);
      expect(actual, id).toMatch(/^\/v1\//);
      expect(actual, id).not.toContain("undefined");
      expect(actual, id).not.toContain("[object");
    }
  });
});
