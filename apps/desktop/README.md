# Gantry 桌面前端

Gantry 桌面前端的**完整可交互模拟前端**：在现有 React + TypeScript + Vite 工程中覆盖工作区、设置、连续模拟对话、工具活动、公开执行摘要、执行 DAG、尝试与预算、审批和配对。界面按[原型说明](../../docs/prototype.md)与原型 HTML 实现；原生外壳、真实服务端和系统安全由其他泳道提供。

本目录不是桌面安装包，也不是已完成真实后端联调的客户端。

## 本地运行

在 `apps/desktop` 目录运行。`dev` 显式使用 scenario 模式，所有事实来自浏览器内存，不连接服务端、不调用模型、不发送通知。

```sh
npm ci
npm run dev
```

URL 的 `scenario` 参数选择初始场景：`default`、`offline`、`viewer`、`operator`、`unpaired`、`revoked`、`expired`、`unsafe`、`error`、`stale`。默认是 `default`。

## 可操作流程

工作区顶部始终显示“示例数据”。对话上方的“开发场景”工具区可以折叠，所有按钮只修改当前页面内存。

1. **普通问答 / 多节点任务**：在工具区选择“普通问答”或“多节点任务”，在输入框发送消息。界面会显示分段生成、模拟工具开始/结束、模拟执行摘要、计划卡和最终消息。完成后点击计划卡的“查看执行图”。
2. **失败与重试**：选择“失败后重试”并发送；从计划卡打开执行图，选择失败节点，点“重试失败节点”。旧尝试与旧失败工具保持原样，新尝试使用新的调用标识。
3. **选择与审批**：选择“选择与模拟审批”并发送，在对话或收件箱中选择“继续并模拟审批”；收件箱中打开审批单。场景模式提供“模拟批准（不生成签名）”和“模拟拒绝”。关闭审批单后，用工具区的“模拟 head 变化”演示失效。
4. **断线与只读**：工具区可模拟断线、恢复连接和切换只读角色；只读时输入和写操作禁用，切回后恢复。
5. **取消与重置**：任务生成中可点“取消生成”，之后不会出现迟到回复；“重置场景”清除演示创建的交付、消息、尝试和工具事实。配对或错误阻塞页仍显示工具区，可直接重置。
6. **设置**：设置是全屏浮层。全局页和项目覆盖页都可编辑、预览并“模拟应用”；切换页面前会提示未保存草稿。工具区可先注入 412 设置冲突，再在设置页观察载入最新值的恢复流程。
7. **项目覆盖**：项目页按 `config.md` §2.2 提供 models、levels、policies、guardian、sandbox、egress 六类覆盖；未覆盖字段继承全局，“只能收紧”的字段显示服务端结果或本地错误，恢复继承会生成 DELETE 预览。
8. **模拟配对**：`?scenario=unpaired` 进入配对页。可以“模拟下次配对失败”后重试，再核对固定测试确认码并确认；不会生成密钥、证书或调用系统认证。
9. **外观与布局**：外观页保存密度、左栏/检视器宽度和折叠状态；保存成功后工作区立即应用新布局。宽度越界会阻止预览。
10. **设备与证书**：设备页可改名、模拟角色变更和吊销；自己的设备角色、本机吊销受保护；iOS 不能选择审批设备。

## 检查与构建

```sh
npm run lint
npm run typecheck
npm test
npm run build
npm run build:scenario
npm run test:e2e
```

- `lint` 是静态边界检查，包含规则自检；不是安全沙盒，不能替代原生 CSP、命令白名单或签名校验。
- `build` 输出正式前端资源到 `dist/`。普通浏览器打开时应显示外壳不可用，不回落示例。
- `build:scenario` 输出独立演示资源到 `dist-scenario/`，不作为正式包分发。
- `test:e2e` 需要先完成 `build`，启动 `4173`（dev scenario）和 `4174`（正式产物预览），在 1440×900 与 390×844 两种窗口执行。报告位于 `playwright-report/`，截图和追踪位于 `test-results/`。
- 字体随包提供 Inter、JetBrains Mono 和 Noto Sans SC 中文回退；构建输出 `FONT-LICENSES.txt`，无远程字体请求。

## 数据与安全边界

```text
React 组件 → DesktopController / IpcAdapter → Tauri 外壳 → 服务端
                                          ↘ 显式开发场景替身（仅 scenario 构建）
```

- 正式实体（Message、Card、EventEnvelope、Attempt、Workflow、配置文档）通过 `IpcAdapter` 读取。工具活动、公开执行摘要和分段生成只存在于 `src/demo/types.ts` 定义的演示视图模型，不加入正式协议。
- 组件不读取 fixture，不直接联网；场景事实由 `ScenarioIpcAdapter.simulation` 统一提供，各视图共享同一份内存状态。
- “模拟批准”“模拟拒绝”“模拟配对”“模拟应用设置”只修改开发场景内存，不产生 signature、密钥、证书或网络写入。生产审批仍走 `approvalSign`，场景中始终拒绝。
- 模型凭据和配对串只在表单局部状态短暂存在；准备/开始后清空，不进入全局状态、界面持久化或日志。凭据输入只接受测试占位值。
- 审批单的模拟批准只在当前设备为审批设备、连接 live、审批 pending、确认窗口有效且 PR head 与审批载荷一致时可用。

## 当前限制

- 生产包不加载场景适配器，界面不显示演示控件；审批模拟分支按构建 mode 折叠。正式 `dist/assets/*.js` 已扫描确认不含 `simulateApproval`、`模拟批准` 或 scenario adapter chunk；场景包保留独立 adapter chunk。
- 未验证真实后端、Tauri 命令白名单与 CSP、Touch ID / Windows Hello、证书链、系统通知或安装包。
- Q-11 预算追加、Q-12 结果内容读取仍等待决；界面只展示已有预算维度与 `result_ref` 编号。
- `attempt.progress` 的正式客户端字段尚未定义；工具与摘要的关联只属于演示边界。图修订仅展示 `graph_rev`/`superseded_by`，不是完整历史快照。
- 无障碍检查使用 axe 自动扫描，未用 VoiceOver 或 NVDA 实测。原型运行时依赖的 `support.js` 不在仓库，视觉按原型源码与尺寸核对，未做逐像素截图比对。

实际执行结果、逐项覆盖矩阵和已知限制见 [VERIFICATION.md](VERIFICATION.md)。设计边界见 [DESIGN.md](DESIGN.md)。
