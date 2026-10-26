import type { ReactNode } from "react";
import type { MessageRole } from "@/api/types";
import { cleanText } from "./cleanText";
import { Markdown } from "./Markdown";
import "./render.css";

export interface MessageTextProps {
  role: MessageRole;
  text: string;
  onOpenExternal?: (url: string) => void;
}

/**
 * 用户消息纯文本；agent 消息用受限 Markdown；事件行保持纯文本（只显示一行摘要）。
 */
export function MessageText({
  role,
  text,
  onOpenExternal,
}: MessageTextProps): ReactNode {
  if (role === "agent") {
    return <Markdown text={text} onOpenExternal={onOpenExternal} />;
  }
  return (
    <p className="message-text message-text--plain">{cleanText(text)}</p>
  );
}
