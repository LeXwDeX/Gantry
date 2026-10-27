import { createRoot, type Root } from "react-dom/client";
import type { ConnectionState, StreamFrame } from "@/api/shell";
import { createIpcAdapter, isMockAdapter, isScenarioAdapter } from "@/ipc";
import { DesktopController } from "@/store/controller";
import { App } from "./App";
import "@/theme/index.css";
import "./app.css";

declare global {
  interface Window {
    /** 仅 scenario 构建安装；供 Playwright 控制示例连接与事件，不提供签名入口。 */
    __GANTRY_SCENARIO__?: {
      readonly scenario: string;
      emitFrame(frame: StreamFrame): void;
      setConnection(phase: ConnectionState["phase"]): void;
      setRole(role: "viewer" | "operator" | "approver"): void;
      bumpLogEpoch(): void;
      ackedSeq(): number | null;
    };
    /** 仅 mock 构建安装；供 Playwright 模拟外壳侧的网络与身份变化，不提供签名入口。 */
    __GANTRY_MOCK__?: {
      disconnect(): void;
      reconnect(): void;
      dropStream(): void;
      revoke(): void;
      expireCertificate(): void;
      swallowNextWriteResponse(): void;
      calls(): ReadonlyArray<{ command: string; args: Record<string, unknown> }>;
      externalOpens(): ReadonlyArray<{ url: string }>;
      githubOpens(): ReadonlyArray<unknown>;
      ackedSeq(): number | null;
    };
  }
}

const found = document.getElementById("root");
if (!found) throw new Error("缺少 #root 挂载点。");
const host: HTMLElement = found;

const root = createRoot(host);
root.render(
  <main className="app-blocking" aria-label="正在启动 Gantry">
    <p role="status" className="app-blocking__status">
      正在启动…
    </p>
    {import.meta.env.MODE === "scenario" ? <span className="app-scenario-badge">示例数据</span> : null}
  </main>,
);

async function main(root: Root): Promise<void> {
  const adapter = await createIpcAdapter();
  const controller = new DesktopController(adapter);

  if (import.meta.env.MODE === "scenario" && isScenarioAdapter(adapter)) {
    window.__GANTRY_SCENARIO__ = {
      scenario: adapter.scenario,
      emitFrame: (frame) => adapter.emitFrame(frame),
      setConnection: (phase) => adapter.setConnection(phase),
      setRole: (role) => adapter.setRole(role),
      bumpLogEpoch: () => adapter.bumpLogEpoch(),
      ackedSeq: () => adapter.ackedSeq,
    };
  }

  if (import.meta.env.MODE === "mock" && isMockAdapter(adapter)) {
    window.__GANTRY_MOCK__ = {
      disconnect: () => adapter.disconnect(),
      reconnect: () => adapter.reconnect(),
      dropStream: () => adapter.dropStream(),
      revoke: () => adapter.revoke(),
      expireCertificate: () => adapter.expireCertificate(),
      swallowNextWriteResponse: () => adapter.swallowNextWriteResponse(),
      calls: () => adapter.calls,
      externalOpens: () => adapter.externalOpens,
      githubOpens: () => adapter.githubOpens,
      ackedSeq: () => adapter.ackedSeq,
    };
  }

  // 不使用 StrictMode：controller 的 start/stop 会在开发态重复打开事件流。
  root.render(<App controller={controller} />);

  import.meta.hot?.dispose(() => {
    root.unmount();
    void controller.stop();
  });
}

void main(root).catch((error: unknown) => {
  const message =
    error instanceof Error ? error.message : "无法启动桌面前端。";
  root.render(
    <main className="app-blocking" aria-label="启动失败">
      <section className="app-blocking__card">
        <h1 className="app-blocking__title">启动失败</h1>
        {import.meta.env.MODE === "scenario" ? <span className="app-scenario-badge">示例数据</span> : null}
        <p role="alert" className="app-blocking__error">
          {message}
        </p>
      </section>
    </main>,
  );
});
