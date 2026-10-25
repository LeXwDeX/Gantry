import type { Level, Ref } from "@/api/types";
import type { MessagesPostBody } from "@/api/shell";

/**
 * 发送语义（desktop-ui §7.7，P-32 / I7）：
 * 请求体只允许 text / refs / level_cap；`level_cap` 仅在用户显式改过选择器时出现。
 * `ctx` 是唯一决定 refs 的字段；调用方不得传入 sel。
 */
export function buildSend(input: {
  text: string;
  ctx: Ref | null;
  levelOverride: Level | null;
}): MessagesPostBody {
  const body: MessagesPostBody = {
    text: input.text,
    refs: input.ctx ? [input.ctx] : [],
  };
  if (input.levelOverride !== null) body.level_cap = input.levelOverride;
  return body;
}

export function canQueueSend(role: string | null | undefined): boolean {
  return role === "operator" || role === "approver";
}
