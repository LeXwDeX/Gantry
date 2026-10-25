import type { ReactNode } from "react";
import type { UntrustedText as UntrustedTextValue } from "@/api/types";
import { cleanText } from "./cleanText";
import "./render.css";

export interface UntrustedTextProps {
  value: UntrustedTextValue | string;
  className?: string;
  identifier?: boolean;
  as?: "span" | "p" | "div";
}

/**
 * 不可信文本只按纯文本渲染；不解析 HTML、URL 或 Markdown。
 * `truncated` 为服务端标记，追加显式提示。
 */
export function UntrustedText({
  value,
  className,
  identifier = false,
  as = "span",
}: UntrustedTextProps): ReactNode {
  const raw = typeof value === "string" ? value : value.text;
  const cleaned = cleanText(raw, identifier);
  const display =
    typeof value !== "string" && value.truncated
      ? `${cleaned} …（已截断）`
      : cleaned;
  const props = {
    className: ["untrusted-text", className].filter(Boolean).join(" "),
    title: typeof value !== "string" && value.truncated ? cleaned : undefined,
  };
  if (as === "p") return <p {...props}>{display}</p>;
  if (as === "div") return <div {...props}>{display}</div>;
  return <span {...props}>{display}</span>;
}
