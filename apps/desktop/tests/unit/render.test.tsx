import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { cleanText } from "@/render/cleanText";
import { Markdown } from "@/render/Markdown";
import { UntrustedText } from "@/render/UntrustedText";
import { Modal } from "@/a11y/Modal";

describe("cleanText", () => {
  it("去掉 C0/C1 控制字符但保留换行和制表", () => {
    expect(cleanText("a\u0000b\nc\td\u007f")).toBe("ab\nc\td");
  });

  it("双向控制字符变为可见转义", () => {
    expect(cleanText("safe\u202Etxt")).toContain("[U+202E]");
  });

  it("标识符中的零宽字符可见，普通文本中移除", () => {
    expect(cleanText("a\u200Bb", true)).toContain("[U+200B]");
    expect(cleanText("a\u200Bb", false)).toBe("ab");
  });
});

describe("受限 Markdown", () => {
  it("原始 HTML 只作为文本，不产生脚本或图片请求", () => {
    const { container } = render(
      <Markdown
        text={'<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>'}
      />,
    );
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain("<script>");
    expect(container.textContent).toContain("<img");
  });

  it("图片只显示文字占位", () => {
    const { container } = render(
      <Markdown text={"![图示](https://evil.example/p.png)"} />,
    );
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain("[图片: 图示]");
  });

  it("只有 https 链接可点击，主机名始终可见", () => {
    const open = vi.fn();
    render(
      <Markdown
        text={"[点我](https://evil.example/path)"}
        onOpenExternal={open}
      />,
    );
    const button = screen.getByRole("button", { name: /点我/ });
    expect(button).toHaveTextContent("evil.example");
    fireEvent.click(button);
    expect(open).toHaveBeenCalledWith("https://evil.example/path");
  });

  it("javascript / http 链接不可点击", () => {
    render(
      <Markdown
        text={"[危险](javascript:alert(1)) [普通](http://example.com/)"}
        onOpenExternal={vi.fn()}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.getByText(/危险/)).toBeInTheDocument();
  });

  it("无外链回调时链接没有按钮", () => {
    render(<Markdown text={"[点我](https://evil.example/)"} />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("超长文本折叠并可展开", () => {
    const text = "长".repeat(16 * 1024 + 20);
    render(<Markdown text={text} />);
    const expand = screen.getByRole("button", { name: /展开/ });
    fireEvent.click(expand);
    expect(screen.getByRole("button", { name: "收起" })).toBeInTheDocument();
  });
});

describe("UntrustedText", () => {
  it("只按纯文本渲染并标记截断", () => {
    const { container } = render(
      <UntrustedText
        value={{
          text: "<b>不是标签</b>",
          source: "test",
          truncated: true,
        }}
      />,
    );
    expect(container.querySelector("b")).toBeNull();
    expect(container.textContent).toContain("<b>不是标签</b>");
    expect(container.textContent).toContain("（已截断）");
  });
});

function ModalHarness() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>
        打开
      </button>
      <Modal open={open} label="测试对话框" onClose={() => setOpen(false)}>
        <button type="button">内容</button>
      </Modal>
    </>
  );
}

describe("Modal", () => {
  it("使用原生 dialog，Esc 关闭并恢复触发按钮焦点", () => {
    const { container } = render(<ModalHarness />);
    const trigger = screen.getByRole("button", { name: "打开" });
    trigger.focus();
    fireEvent.click(trigger);
    const dialog = container.querySelector("dialog");
    expect(dialog).not.toBeNull();
    expect(screen.getByRole("button", { name: "内容" })).toBeInTheDocument();
    fireEvent(dialog as HTMLDialogElement, new Event("cancel", { cancelable: true }));
    expect((dialog as HTMLDialogElement).open).toBe(false);
    expect(document.activeElement).toBe(trigger);
  });
});
