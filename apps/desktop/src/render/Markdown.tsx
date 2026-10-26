import type { ReactNode } from "react";
import { Fragment, useEffect, useState } from "react";
import { lexer, type Token, type Tokens } from "marked";
import { cleanText, MESSAGE_TEXT_LIMIT } from "./cleanText";
import "./render.css";

export interface MarkdownProps {
  text: string;
  onOpenExternal?: (url: string) => void;
}

interface RenderContext {
  onOpenExternal?: (url: string) => void;
  remaining: number;
  truncated: boolean;
  depth: number;
}

const MAX_TOKENS = 2000;
const MAX_DEPTH = 24;

function countTokens(tokens: Token[]): number {
  let total = 0;
  for (const token of tokens) {
    total += 1;
    const nested = (token as { tokens?: Token[] }).tokens;
    if (Array.isArray(nested)) total += countTokens(nested);
    const items = (token as { items?: Tokens.ListItem[] }).items;
    if (Array.isArray(items)) {
      for (const item of items) total += 1 + countTokens(item.tokens);
    }
  }
  return total;
}

function takeBudget(ctx: RenderContext): boolean {
  if (ctx.remaining <= 0) {
    ctx.truncated = true;
    return false;
  }
  ctx.remaining -= 1;
  return true;
}

function renderInline(tokens: Token[], ctx: RenderContext): ReactNode[] {
  if (ctx.depth > MAX_DEPTH) {
    ctx.truncated = true;
    return ["…"];
  }
  const nodes: ReactNode[] = [];
  tokens.forEach((token, index) => {
    if (!takeBudget(ctx)) return;
    nodes.push(
      <Fragment key={`i-${index}`}>
        {renderInlineToken(token, { ...ctx, depth: ctx.depth + 1 })}
      </Fragment>,
    );
  });
  return nodes;
}

function renderInlineToken(token: Token, ctx: RenderContext): ReactNode {
  switch (token.type) {
    case "text": {
      const text = token as Tokens.Text;
      if (Array.isArray(text.tokens) && text.tokens.length > 0) {
        return renderInline(text.tokens, ctx);
      }
      return cleanText(text.text);
    }
    case "escape":
      return cleanText((token as Tokens.Escape).text);
    case "strong":
      return (
        <strong>{renderInline((token as Tokens.Strong).tokens, ctx)}</strong>
      );
    case "em":
      return <em>{renderInline((token as Tokens.Em).tokens, ctx)}</em>;
    case "del":
      return <span>{renderInline((token as Tokens.Del).tokens, ctx)}</span>;
    case "codespan":
      return <code>{cleanText((token as Tokens.Codespan).text)}</code>;
    case "br":
      return <br />;
    case "link":
      return <SafeLink token={token as Tokens.Link} ctx={ctx} />;
    case "image": {
      const image = token as Tokens.Image;
      return (
        <span className="md-image">[图片: {cleanText(image.text || "无描述")}]</span>
      );
    }
    case "html":
      return (
        <span className="md-html">{cleanText((token as Tokens.HTML).raw)}</span>
      );
    default: {
      const generic = token as Tokens.Generic;
      if (Array.isArray(generic.tokens)) {
        return renderInline(generic.tokens, ctx);
      }
      return cleanText(generic.raw ?? "");
    }
  }
}

function renderBlockToken(token: Token, ctx: RenderContext): ReactNode {
  if (ctx.depth > MAX_DEPTH) {
    ctx.truncated = true;
    return null;
  }
  switch (token.type) {
    case "space":
    case "def":
      return null;
    case "paragraph":
      return <p>{renderInline((token as Tokens.Paragraph).tokens, ctx)}</p>;
    case "heading":
      return (
        <p className="md-heading">
          <strong>{renderInline((token as Tokens.Heading).tokens, ctx)}</strong>
        </p>
      );
    case "blockquote":
      return (
        <blockquote>
          {renderBlocks((token as Tokens.Blockquote).tokens, {
            ...ctx,
            depth: ctx.depth + 1,
          })}
        </blockquote>
      );
    case "list": {
      const list = token as Tokens.List;
      const children = list.items.map((item, index) => (
        <Fragment key={`li-${index}`}>
          {renderListItem(item, { ...ctx, depth: ctx.depth + 1 })}
        </Fragment>
      ));
      if (list.ordered) {
        return <ol start={typeof list.start === "number" ? list.start : 1}>{children}</ol>;
      }
      return <ul>{children}</ul>;
    }
    case "list_item":
      return renderListItem(token as Tokens.ListItem, ctx);
    case "code": {
      const code = token as Tokens.Code;
      return (
        <pre className="md-code">
          <code>{cleanText(code.text)}</code>
        </pre>
      );
    }
    case "html":
      return <pre className="md-html">{cleanText((token as Tokens.HTML).raw)}</pre>;
    case "hr":
      return <hr />;
    case "table":
      return <pre className="md-html">{cleanText((token as Tokens.Table).raw ?? "")}</pre>;
    default:
      return <p>{renderInline([token], ctx)}</p>;
  }
}

function renderListItem(item: Tokens.ListItem, ctx: RenderContext): ReactNode {
  if (ctx.depth > MAX_DEPTH) {
    ctx.truncated = true;
    return null;
  }
  return <li>{renderBlocks(item.tokens, { ...ctx, depth: ctx.depth + 1 })}</li>;
}

function renderBlocks(tokens: Token[], ctx: RenderContext): ReactNode[] {
  const nodes: ReactNode[] = [];
  tokens.forEach((token, index) => {
    if (!takeBudget(ctx)) return;
    nodes.push(
      <Fragment key={`b-${index}`}>
        {renderBlockToken(token, ctx)}
      </Fragment>,
    );
  });
  return nodes;
}

function SafeLink({
  token,
  ctx,
}: {
  token: Tokens.Link;
  ctx: RenderContext;
}): ReactNode {
  const text = cleanText(plainText(token.tokens) || token.text);
  const safe = safeHttps(token.href);
  if (!safe) {
    return <span className="md-link md-link--blocked">{text}</span>;
  }
  const host = cleanText(safe.host, true);
  if (!ctx.onOpenExternal) {
    return (
      <span className="md-link md-link--disabled">
        {text}
        <span className="md-link__host"> · {host}</span>
      </span>
    );
  }
  const open = ctx.onOpenExternal;
  return (
    <button
      type="button"
      className="md-link"
      onClick={() => open(safe.url)}
      title={`在系统浏览器打开 ${host}`}
    >
      {text}
      <span className="md-link__host"> · {host}</span>
    </button>
  );
}

function plainText(tokens: Token[]): string {
  return tokens
    .map((token) => {
      switch (token.type) {
        case "text":
        case "escape":
        case "codespan":
          return (token as Tokens.Text | Tokens.Escape | Tokens.Codespan).text;
        case "image":
          return (token as Tokens.Image).text;
        case "br":
          return " ";
        default: {
          const nested = (token as Tokens.Generic).tokens;
          return Array.isArray(nested) ? plainText(nested) : "";
        }
      }
    })
    .join("");
}

function safeHttps(href: string): { url: string; host: string } | null {
  let parsed: URL;
  try {
    parsed = new URL(href);
  } catch {
    return null;
  }
  if (parsed.protocol !== "https:") return null;
  if (parsed.username || parsed.password) return null;
  return { url: parsed.toString(), host: parsed.hostname };
}

/**
 * 受限 Markdown：解析成语法树后映射到组件，不经过 HTML 字符串。
 * 禁止标题（降为加粗段落）、图片（文字占位）、原始 HTML（转义文本）、表格、自动链接。
 */
export function Markdown({ text, onOpenExternal }: MarkdownProps): ReactNode {
  const [expanded, setExpanded] = useState(false);
  useEffect(() => {
    setExpanded(false);
  }, [text]);

  const cleaned = cleanText(text);
  const tooLong = cleaned.length > MESSAGE_TEXT_LIMIT;
  const source = tooLong && !expanded ? cleaned.slice(0, MESSAGE_TEXT_LIMIT) : cleaned;
  const tokens = lexer(source, { gfm: false, breaks: true });
  const ctx: RenderContext = {
    onOpenExternal,
    remaining: MAX_TOKENS,
    truncated: false,
    depth: 0,
  };
  const content = renderBlocks(tokens, ctx);
  return (
    <div className="markdown">
      {content}
      {ctx.truncated ? <p className="md-note">内容过长，已截断显示。</p> : null}
      {tooLong ? (
        <button
          type="button"
          className="md-expand"
          onClick={() => setExpanded((value) => !value)}
          aria-expanded={expanded}
        >
          {expanded ? "收起" : `展开（还有 ${cleaned.length - MESSAGE_TEXT_LIMIT} 个字符）`}
        </button>
      ) : null}
    </div>
  );
}

export { countTokens };
