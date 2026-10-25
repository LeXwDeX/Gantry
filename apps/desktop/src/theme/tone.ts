/** 状态色记号（desktop-ui §7.1）。每种 Tone 必须与文字同现。 */
export type Tone = "run" | "heal" | "need" | "ok" | "idle" | "warn";

export const TONE_LABELS: Record<Tone, string> = {
  run: "运行中",
  heal: "自愈中",
  need: "需要你",
  ok: "通过",
  idle: "空闲",
  warn: "异常",
};

export function toneClass(tone: Tone): string {
  return `tone-${tone}`;
}

/** 事件行 `Message.tone` → 视觉 Tone；未知值按 info（中性）。 */
export function eventTone(
  tone: "info" | "ok" | "run" | "warn" | string | null,
): Tone {
  switch (tone) {
    case "ok":
      return "ok";
    case "run":
      return "run";
    case "warn":
      return "warn";
    default:
      return "heal";
  }
}
