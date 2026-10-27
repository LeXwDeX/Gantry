import { describe, expect, it } from "vitest";
import { bandEdges } from "@/features/workspace/model";

describe("关系地图连线", () => {
  it("孤 PR 画缺规格虚线；同交付的 PR→工作树仍画分支实线", () => {
    const edges = bandEdges(0, true, true, true);
    expect(edges.map((edge) => edge.kind)).toEqual(["orphan", "branch"]);
  });

  it("没有 PR 时 Issue→工作树为尚未开 PR 直连", () => {
    const edges = bandEdges(1, false, true, false);
    expect(edges).toHaveLength(1);
    expect(edges[0].kind).toBe("direct");
  });

  it("Issue→PR 为 closes，PR→工作树为分支", () => {
    const edges = bandEdges(2, true, true, false);
    expect(edges.filter((edge) => edge.kind === "closes")).toHaveLength(2);
    expect(edges.filter((edge) => edge.kind === "branch")).toHaveLength(1);
  });
});
import type { Message } from "@/api/types";
import { buildChatRows } from "@/features/workspace/model";

function message(id: string, role: Message["role"]): Message {
  return {
    id,
    created_at: "2026-10-24T12:00:00.000Z",
    updated_at: "2026-10-24T12:00:00.000Z",
    conversation_id: "cnv",
    role,
    agent_kind: role === "agent" ? "utility" : null,
    tone: role === "event" ? "info" : null,
    text: id,
    level_cap: null,
    refs: [],
    cards: [],
    delivery_id: null,
  };
}

describe("对话行分组", () => {
  it("连续事件分组；单条事件不包装成 agent 气泡", () => {
    const rows = buildChatRows([
      message("e1", "event"),
      message("e2", "event"),
      message("u1", "user"),
      message("e3", "event"),
    ]);
    expect(rows.map((row) => row.type)).toEqual(["events", "message", "events"]);
    expect(rows[0].messages).toHaveLength(2);
    expect(rows[2].messages).toHaveLength(1);
  });
});
