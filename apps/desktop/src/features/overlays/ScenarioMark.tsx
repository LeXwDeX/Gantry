import type { ReactNode } from "react";
import type { IpcAdapter } from "@/api/shell";
import { isScenarioAdapter } from "@/ipc";

/** 浮层头部的样例数据标记；只在示例替身下出现。 */
export function ScenarioMark({ ipc }: { ipc: IpcAdapter }): ReactNode {
  if (!isScenarioAdapter(ipc)) return null;
  return (
    <span
      className="ov-scenario-mark"
      title="开发场景：示例数据，未连接真实服务端；签名、配对与受保护保存不可用。"
    >
      示例数据
    </span>
  );
}
