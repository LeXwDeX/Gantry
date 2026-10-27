import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import ts from 'typescript';

// 静态回归检查，不替代原生外壳的 CSP、白名单与签名校验。
const forbidden = new Set([
  'innerHTML', 'outerHTML', 'insertAdjacentHTML', 'dangerouslySetInnerHTML',
  'eval', 'Function', 'fetch', 'XMLHttpRequest', 'WebSocket', 'EventSource',
  'sendBeacon', 'localStorage', 'sessionStorage',
]);

function check(file, text) {
  file = file.split(path.sep).join('/');
  const source = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const errors = [];
  const report = (node, reason) => {
    const { line } = source.getLineAndCharacterOfPosition(node.getStart(source));
    errors.push(`${file}:${line + 1}: ${reason}`);
  };
  const visit = (node) => {
    // 只有 Mock API 外壳替身可以联网；它不进入正式包（工厂按构建 mode 折叠）。
    const mockNetwork = file.startsWith('src/ipc/mock/') && node.text === 'fetch';
    if (ts.isIdentifier(node) && forbidden.has(node.text) && !mockNetwork) {
      report(node, `禁止直接使用 ${node.text}`);
    }
    if (ts.isElementAccessExpression(node) && ts.isStringLiteral(node.argumentExpression)
      && forbidden.has(node.argumentExpression.text)) {
      report(node, `禁止间接使用 ${node.argumentExpression.text}`);
    }
    if (ts.isStringLiteralLike(node)) {
      if (node.text.startsWith('@tauri-apps/') && !file.startsWith('src/ipc/')) {
        report(node, 'Tauri API 只能由 src/ipc/ 引用');
      }
      if (/^\s*(javascript|vbscript):/i.test(node.text)) {
        report(node, '禁止可执行 URL');
      }
    }
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression)) {
      const call = node.expression;
      if (call.expression.getText(source) === 'document' && ['write', 'writeln'].includes(call.name.text)) {
        report(node, '禁止 document.write');
      }
      if (call.name.text === 'createElement' && node.arguments[0]
        && ts.isStringLiteral(node.arguments[0]) && ['script', 'iframe'].includes(node.arguments[0].text)) {
        report(node, '禁止动态 script / iframe');
      }
      if (call.expression.getText(source) === 'window' && call.name.text === 'open') {
        report(node, '外链必须经 IpcAdapter 打开');
      }
    }
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const tag = node.tagName.getText(source);
      if (['script', 'iframe'].includes(tag)) report(node, '禁止 script / iframe 元素');
      for (const attr of node.attributes.properties) {
        if (!ts.isJsxAttribute(attr)) continue;
        const name = attr.name.getText(source);
        if (['div', 'span', 'p', 'li'].includes(tag) && name === 'onClick') {
          report(attr, '交互应使用原生按钮或链接');
        }
        if (name === 'style') report(attr, '使用 CSS 类，避免内联样式');
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return errors;
}

assert.equal(check('src/example.tsx', '// fetch()\nconst label = "innerHTML"; <button onClick={send}>发送</button>').length, 0);
assert.equal(check('src/ipc/tauri.ts', 'import { invoke } from "@tauri-apps/api/core";').length, 0);
assert.equal(check('src/ipc/mock/adapter.ts', 'globalThis.fetch(url)').length, 0);
assert.ok(check('src/ipc/mock/adapter.ts', 'new EventSource(url)').length > 0);
assert.ok(check('src/ipc/tauri.ts', 'fetch(url)').length > 0);
for (const unsafe of [
  'fetch("https://example.com")', 'window["localStorage"]',
  'el.innerHTML = text', '<div onClick={send} />', '<script />',
  'document.createElement("iframe")', 'document.write(text)',
  'window.open(url)', 'import { invoke } from "@tauri-apps/api/core"',
  '<p style={{ color: "red" }} />',
]) assert.ok(check('src/example.tsx', unsafe).length > 0, unsafe);

const errors = [];
async function scan(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) await scan(file);
    else if (/\.[cm]?[jt]sx?$/.test(file)) errors.push(...check(file, await readFile(file, 'utf8')));
  }
}
await scan('src');
if (errors.length) {
  console.error(errors.join('\n'));
  process.exitCode = 1;
} else {
  console.log('静态边界检查通过（含规则自检）。');
}
