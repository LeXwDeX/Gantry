/**
 * Mock API 场景控制（测试专用，只监听 127.0.0.1）。
 *
 * tools/mock-api 只回放夹具：写请求不产生事件，也没有注入 / 断流 / 请求日志接口。
 * 本脚本在同一进程内启动 Mock 实例，并通过一个小的控制端口提供 desktop-ui §12
 * 要求的“场景控制”最小子集。缺口已向 contract 泳道登记，Mock 自带控制面后可删除本文件。
 *
 * 控制端口路由（JSON）：
 *   POST   /instances                     新建实例 → { id, baseUrl }
 *   POST   /instances/:id/reset           以夹具重建（端口不变）
 *   POST   /instances/:id/inject          追加事件 { type, subject, data?, project_id? }，并断开现有流
 *   POST   /instances/:id/drop-streams    断开现有事件流
 *   POST   /instances/:id/mutate          { collection, id, patch } 合并到 state[collection].items 中匹配项；
 *                                         或 { set, value } 设置顶层标量（如 logEpoch）
 *   GET    /instances/:id/requests        请求日志 [{ method, path, operationId }]
 *   DELETE /instances/:id                 关闭
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
    const send = (status, body) => {
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(body === undefined ? "" : JSON.stringify(body));
    };
    try {
      const raw = await new Promise((resolve) => {
        let data = "";
        req.on("data", (chunk) => (data += chunk));
        req.on("end", () => resolve(data));
      });
      const body = raw ? JSON.parse(raw) : {};
      if (parts[0] !== "instances") return send(404, { error: "not_found" });
      if (parts.length === 1 && req.method === "POST") {
        return send(201, await spawn(`m${++nextId}`, 0));
      }
      const id = parts[1];
      const mock = instances.get(id);
      if (!mock) return send(404, { error: "unknown_instance" });
      const action = parts[2];
      const state = mock.app.state;
      if (!action && req.method === "DELETE") {
        instances.delete(id);
        await mock.close();
        return send(204);
      }
      if (action === "reset" && req.method === "POST") {
        const mockPort = mock.port;
        await mock.close();
        return send(200, await spawn(id, mockPort));
      }
      if (action === "inject" && req.method === "POST") {
        const seq = state.head + 1;
        const envelope = {
          id: `evt_mock_${seq}`,
          seq,
          ts: state.me.server.time,
          type: body.type,
          subject: body.subject,
          project_id: body.project_id === undefined ? "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA" : body.project_id,
          actor: body.actor ?? { kind: "engine", id: "engine" },
          causation_id: null,
          correlation_id: body.correlation_id ?? body.subject,
          data: body.data ?? {},
        };
        state.events.push(envelope);
        state.head = seq;
        await mock.app.close();
        return send(200, envelope);
      }
      if (action === "drop-streams" && req.method === "POST") {
        await mock.app.close();
        return send(204);
      }
      if (action === "mutate" && req.method === "POST") {
        if (typeof body.set === "string") {
          state[body.set] = body.value;
          return send(200, { [body.set]: state[body.set] });
        }
        const collection = state[body.collection];
        const items = Array.isArray(collection?.items) ? collection.items : null;
        if (!items) return send(400, { error: "unknown_collection" });
        if (body.push) {
          items.push(body.push);
          return send(200, body.push);
        }
        const item = items.find((entry) => entry.id === body.id);
        if (!item) return send(404, { error: "unknown_item" });
        Object.assign(item, body.patch ?? {});
        return send(200, item);
      }
      if (action === "requests" && req.method === "GET") {
        return send(200, mock.app.requestLog);
      }
      return send(404, { error: "not_found" });
    } catch (error) {
      return send(500, { error: error instanceof Error ? error.message : String(error) });
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
