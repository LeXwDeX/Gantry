# Gantry 桌面前端

React + TypeScript + Vite 前端基线，按[工作区与设置原型](../../docs/prototype.md)实现。此目录不是完整的桌面安装包。原生外壳和服务端由其他泳道提供。

## 本地运行

在 `apps/desktop` 目录运行。需要满足 Vite 要求的 Node.js 和 npm；浏览器测试使用本机 Google Chrome。

```sh
npm ci
npm run dev
```

`dev` 显式使用 `scenario` 模式。界面上的“示例数据”标记必须始终可见。这是内存场景替身，不会连接真实服务端，不会执行 agent，也不会写入真实项目。

URL 的 `scenario` 参数用于选择场景，例如 `/?scenario=viewer`。场景包括 `default`、`offline`、`viewer`、`operator`、`unpaired`、`revoked`、`expired`、`unsafe`、`error` 和 `stale`。

## 检查与构建

```sh
npm run lint
npm run typecheck
npm test
npm run build
npm run build:scenario
npm run test:e2e
```

- `build` 输出正式前端资源到 `dist/`。在普通浏览器中打开时应提示外壳不可用，不得自动回退到示例数据。
- `build:scenario` 输出独立的演示资源到 `dist-scenario/`。不要将它当成正式包分发。
- `test:e2e` 需要先完成 `build`，会启动两个本地服务：`4173` 为开发场景，`4174` 为正式产物预览。测试使用 1440×900 和 390×844 两种窗口。
- 浏览器报告在 `playwright-report/`，失败追踪与截图在 `test-results/`。这些文件不进入版本控制。
- `lint` 包含静态规则自检，检查直接联网、HTML 注入、原生 API 目录边界及不合语义的点击元素。它不是安全沙盒，不能替代原生外壳的 CSP 和命令白名单。

## 数据边界

```text
React 组件 → DesktopController / IpcAdapter → Tauri 外壳 → 服务端
                                          ↘ 显式开发场景替身
```

组件不能直接联网，也不能直接引用 Tauri API。外链经过 `IpcAdapter`，不可信文本按纯文本或受限 Markdown 显示。

审批签名只传外壳准备好的 `preparedId`，受保护写入只传 `pendingId`。场景替身始终拒绝签名、受保护提交和配对，不伪造成功。模型凭据只保留在表单局部状态，准备写入后清空，不进入全局状态、界面持久化或日志。

## 当前边界

- 类型按现有[接口文字契约](../../docs/interfaces/api.md)与[桌面外壳规格](../../docs/modules/desktop-shell.md)手工转写，不是由 OpenAPI 生成。共享 `schemas/` 到位后需核对和替换。
- 开发场景不是契约泳道的正式 Mock API。浏览器测试只证明当前前端与场景替身的行为，不能证明真实协议联调成功。
- 通用 schema 表单、没有契约的数据字段以及尚未排期的设置页不在本次基线内。界面明确显示“尚未提供”或“服务端未提供”，不生成假数据。
- 尚未验证真实后端、Tauri 命令白名单、原生安全策略、Touch ID、Windows Hello、证书与配对、系统通知或安装包。
- 场景状态只在当前会话内保留。跨重启持久化范围仍需按外壳契约的待决项确认，不使用浏览器存储替代。

视觉范围及验收约束见 [DESIGN.md](DESIGN.md)。完整模块验收清单见 [desktop-ui.md](../../docs/modules/desktop-ui.md#12-测试与验收)，本次基线不等于 M1 至 M4 全部完成。

本次实际检查结果、独立评审和已知限制见 [VERIFICATION.md](VERIFICATION.md)。
