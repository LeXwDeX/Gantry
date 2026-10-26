export const MESSAGE_TEXT_LIMIT = 16 * 1024;

const BIDI_CONTROLS = new Set([
  0x202a, 0x202b, 0x202c, 0x202d, 0x202e, 0x2066, 0x2067, 0x2068, 0x2069,
]);

const ZERO_WIDTH = new Set([0x200b, 0x200c, 0x200d, 0x2060, 0xfeff]);

/**
 * 所有不可信文本共用的清理：
 * - 去掉 C0 / C1 控制字符（保留换行与制表）；
 * - 双向控制字符显示为可见转义；
 * - 零宽字符在 identifier 模式下显示为可见标记，其余场景移除。
 */
export function cleanText(text: string, identifier = false): string {
  let output = "";
  for (const char of text) {
    const code = char.codePointAt(0);
    if (code === undefined) continue;
    if (code === 0x0a || code === 0x09) {
      output += char;
      continue;
    }
    if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) continue;
    if (code === 0x2028 || code === 0x2029) {
      output += "\n";
      continue;
    }
    if (BIDI_CONTROLS.has(code)) {
      output += `[U+${code.toString(16).toUpperCase().padStart(4, "0")}]`;
      continue;
    }
    if (ZERO_WIDTH.has(code)) {
      if (identifier) {
        output += `[U+${code.toString(16).toUpperCase().padStart(4, "0")}]`;
      }
      continue;
    }
    output += char;
  }
  return output;
}

export function truncateForDisplay(
  text: string,
  limit = MESSAGE_TEXT_LIMIT,
): { text: string; truncated: boolean } {
  if (text.length <= limit) return { text, truncated: false };
  return { text: text.slice(0, limit), truncated: true };
}
