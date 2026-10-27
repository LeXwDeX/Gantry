import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import Ajv2020 from "ajv/dist/2020.js";
import { describe, expect, it } from "vitest";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import type { StreamFrame } from "@/api/shell";

// 场景适配器发出的事件 data 必须能通过 xtask 生成的契约 schema（#75）。
function eventSchema(type: string): object {
  const here = dirname(fileURLToPath(import.meta.url));
  const file = resolve(here, "../../../../schemas/events", `${type}.json`);
  return JSON.parse(readFileSync(file, "utf8")) as object;
}

function validator(type: string) {
  const ajv = new Ajv2020({ allErrors: true, strict: false });
  return ajv.compile(eventSchema(type));
}

async function openAdapter() {
  const adapter = new ScenarioIpcAdapter("default");
  const frames: StreamFrame[] = [];
  await adapter.eventsOpen({}, (frame) => frames.push(frame));
  const events = (type: string) =>
    frames.flatMap((frame) =>
      frame.t === "event" && frame.envelope.type === type ? [frame.envelope] : [],
    );
  return { adapter, events };
}

describe("场景适配器事件契约", () => {
  it("压低上限只发 kind = cap，data 通过 schema，level_effective 派生为 min", async () => {
    const { adapter, events } = await openAdapter();
    const result = await adapter.api("deliveries.patch", {
      deliveryId: "dlv_gantry_5",
      levelCap: "Low",
    });
    if (!result.ok) throw new Error("patch failed");
    expect(result.body?.level_cap).toBe("Low");
    expect(result.body?.level_effective).toBe("Low");

    const emitted = events("delivery.level_changed");
    expect(emitted).toHaveLength(1);
    expect(emitted[0].data).toEqual({ from: "High", to: "Low", kind: "cap", reason: "user" });
    const validate = validator("delivery.level_changed");
    expect(validate(emitted[0].data), JSON.stringify(validate.errors)).toBe(true);
  });

  it("抬高上限不抬 level_effective，事件仍通过 schema", async () => {
    const { adapter, events } = await openAdapter();
    const result = await adapter.api("deliveries.patch", {
      deliveryId: "dlv_gantry_5",
      levelCap: "Max",
    });
    if (!result.ok) throw new Error("patch failed");
    expect(result.body?.level_effective).toBe("High");
    const [event] = events("delivery.level_changed");
    const validate = validator("delivery.level_changed");
    expect(validate(event.data), JSON.stringify(validate.errors)).toBe(true);
  });

  it("同值不发事件", async () => {
    const { adapter, events } = await openAdapter();
    await adapter.api("deliveries.patch", { deliveryId: "dlv_gantry_5", levelCap: "High" });
    expect(events("delivery.level_changed")).toHaveLength(0);
  });
});
