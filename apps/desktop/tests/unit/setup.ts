import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(() => {
  cleanup();
});

// jsdom 的 <dialog> 支持因版本而异；只补上缺失的原生行为，供共享 Modal 测试。
if (typeof HTMLDialogElement !== "undefined") {
  const proto = HTMLDialogElement.prototype;
  if (typeof proto.showModal !== "function") {
    Object.defineProperty(proto, "showModal", {
      configurable: true,
      value(this: HTMLDialogElement) {
        this.setAttribute("open", "");
      },
    });
  }
  if (typeof proto.close !== "function") {
    Object.defineProperty(proto, "close", {
      configurable: true,
      value(this: HTMLDialogElement) {
        this.removeAttribute("open");
        this.dispatchEvent(new Event("close"));
      },
    });
  }
}
