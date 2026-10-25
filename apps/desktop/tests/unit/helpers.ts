import { waitFor } from "@testing-library/react";
import type { EventEnvelope } from "@/api/types";
import { DesktopController, type DesktopControllerOptions } from "@/store/controller";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import type { ScenarioName } from "@/ipc/scenario/data";

export const TEST_NOW = Date.parse("2026-10-24T12:59:00.000Z");

export async function startController(
  scenario: ScenarioName = "default",
  options: DesktopControllerOptions = {},
): Promise<{ adapter: ScenarioIpcAdapter; controller: DesktopController }> {
  const adapter = new ScenarioIpcAdapter(scenario);
  const controller = new DesktopController(adapter, {
    refreshDelayMs: 0,
    persistDelayMs: 0,
    prefetchWorkflows: false,
    now: () => TEST_NOW,
    ...options,
  });
  await controller.start();
  await waitFor(() => {
    if (controller.getSnapshot().status !== "ready") {
      throw new Error(`status=${controller.getSnapshot().status}`);
    }
  });
  return { adapter, controller };
}

export function event(
  seq: number,
  type: string,
  subject: string,
  projectId: string | null = "prj_gantry",
  data: unknown = {},
): EventEnvelope {
  return {
    id: `evt_test_${seq}`,
    seq,
    ts: "2026-10-24T12:59:00.000Z",
    type,
    subject,
    project_id: projectId,
    actor: { kind: "engine", id: "engine" },
    causation_id: null,
    correlation_id: subject,
    data,
  };
}

export function waitForResync(controller: DesktopController): Promise<void> {
  return waitFor(() => {
    const snapshot = controller.getSnapshot();
    if (snapshot.resyncing || snapshot.status !== "ready") {
      throw new Error("still resyncing");
    }
  });
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
