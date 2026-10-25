import http from "node:http";
import { pathToFileURL } from "node:url";
import { createMockApplication } from "./lib/app.mjs";

export async function startMockApi({ port = process.env.MOCK_PORT ?? 8787, heartbeatMs = 15000, specPath } = {}) {
  const app = await createMockApplication({ specPath, heartbeatMs });
  const server = http.createServer((req, res) => {
    app.handle(req, res).catch((error) => {
      if (!res.headersSent) {
        res.writeHead(500, { "Content-Type": "application/problem+json; charset=utf-8" });
      }
      res.end();
      console.error(`[mock-api] unhandled request error: ${error instanceof Error ? error.message : String(error)}`);
    });
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(Number(port), "127.0.0.1", () => {
      server.off("error", reject);
      resolve();
    });
  });
  const address = server.address();
  const actualPort = typeof address === "object" && address ? address.port : Number(port);
  const baseUrl = `http://127.0.0.1:${actualPort}`;
  let closed = false;
  async function close() {
    if (closed) {
      return;
    }
    closed = true;
    await app.close();
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
  }
  return { server, app, state: app.state, baseUrl, port: actualPort, close };
}

async function main() {
  const heartbeatMs = Number(process.env.MOCK_SSE_HEARTBEAT_MS || 15000);
  const mock = await startMockApi({ heartbeatMs });
  console.log(`[mock-api] listening on ${mock.baseUrl} (fixtures; no TLS, no auth, no CORS)`);
  const shutdown = async () => {
    await mock.close();
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error(error);
    process.exit(1);
  });
}
