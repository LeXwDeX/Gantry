import type { ReactNode } from "react";
import type { IpcAdapter } from "@/api/shell";
import { isScenarioAdapter } from "@/ipc";

/** 浮层头部的样例数据标记；只在示例替身下出现。 */
export function ScenarioMark({ ipc }: { ipc: IpcAdapter }): ReactNode {
  if (!isScenarioAdapter(ipc)) return null;
  return (
    <span
      className="ov-scenario-mark"
      title="开发场景：示例数据，未连接真实服务端；模拟审批、配对与保存只改内存，不生成真实签名或证书。"
    >
      示例数据
    </span>
  );
}
