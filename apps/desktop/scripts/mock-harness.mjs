/**
 * 测试用的 Mock API 实例管理（只监听 127.0.0.1）。
 *
 * 场景控制本身由 tools/mock-api 的 `/__mock/*` 提供（见 tools/mock-api/README.md）；
 * 本脚本只负责在同一进程里起停多个互相隔离的 Mock 实例，并把控制请求转发给对应实例，
 * 让 vitest 的每个用例都能拿到独立状态，e2e 则使用固定端口的 default 实例。
 *
 * 控制端口路由（JSON）：
 *   POST   /instances               新建实例 → { id, baseUrl }
 *   DELETE /instances/:id           关闭实例
 *   *      /instances/:id/<action>  转发到该实例的 /__mock/<action>；`inject` 是 `events` 的别名
 */
import http from "node:http";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const serverModule = pathToFileURL(path.resolve(here, "../../../tools/mock-api/server.mjs")).href;

export async function startHarness({ port = 0, defaultPort = null, heartbeatMs = 60000 } = {}) {
  const { startMockApi } = await import(serverModule);
  const instances = new Map();
  let nextId = 0;

  async function spawn(id, mockPort) {
    const mock = await startMockApi({ port: mockPort, heartbeatMs });
    instances.set(id, mock);
    return { id, baseUrl: mock.baseUrl };
  }

  if (defaultPort !== null) await spawn("default", defaultPort);

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    const parts = url.pathname.split("/").filter(Boolean);
    const send = (status, body, contentType = "application/json") => {
      res.writeHead(status, { "Content-Type": contentType });
      res.end(body ?? "");
    };
    try {
      const raw = await new Promise((resolve) => {
        let data = "";
        req.on("data", (chunk) => (data += chunk));
        req.on("end", () => resolve(data));
      });
      if (parts[0] !== "instances") return send(404, JSON.stringify({ error: "not_found" }));
      if (parts.length === 1 && req.method === "POST") {
        return send(201, JSON.stringify(await spawn(`m${++nextId}`, 0)));
      }
      const mock = instances.get(parts[1]);
      if (!mock) return send(404, JSON.stringify({ error: "unknown_instance" }));
      if (parts.length === 2 && req.method === "DELETE") {
        instances.delete(parts[1]);
        await mock.close();
        return send(204);
      }
      const action = parts[2] === "inject" ? "events" : parts.slice(2).join("/");
      const forwarded = await fetch(`${mock.baseUrl}/__mock/${action}`, {
        method: req.method,
        headers: raw ? { "Content-Type": "application/json" } : undefined,
        body: raw || undefined,
      });
      return send(forwarded.status, await forwarded.text(), forwarded.headers.get("content-type") ?? undefined);
    } catch (error) {
      return send(500, JSON.stringify({ error: error instanceof Error ? error.message : String(error) }));
    }
  });

  await new Promise((resolve) => server.listen(port, "127.0.0.1", resolve));
  const address = server.address();
  const controlUrl = `http://127.0.0.1:${typeof address === "object" && address ? address.port : port}`;

  async function close() {
    for (const mock of instances.values()) await mock.close();
    instances.clear();
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
  }

  return { controlUrl, close };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const harness = await startHarness({
    port: Number(process.env.MOCK_CONTROL_PORT ?? 8788),
    defaultPort: Number(process.env.MOCK_PORT ?? 8787),
    heartbeatMs: Number(process.env.MOCK_SSE_HEARTBEAT_MS ?? 2000),
  });
  console.log(`[mock-harness] control ${harness.controlUrl}; default mock on ${process.env.MOCK_PORT ?? 8787}`);
  const shutdown = async () => {
    await harness.close();
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}
