# 前端验证与完整交互覆盖矩阵

## M1 真实 API 模式与 §12 验收覆盖（#103，2026-10-27）

本节以 `docs/modules/desktop-ui.md` §12 的 M1 条目为准，记录本机替身环境（A 级）下的取证情况。下面的旧矩阵描述场景模式，保留不动。

**两种模式**：

- **场景模式**：`ScenarioIpcAdapter`，数据在浏览器内存中。
- **Mock 模式**：`MockIpcAdapter` → HTTP / SSE → `tools/mock-api`，见 [README](README.md)「真实 API 模式」。Mock 的写请求会产生事件并实时推送；服务端其他部分（引擎、forge、policy）的进展由测试经 `/__mock/events` 注入，data 按 `schemas/events` 校验（#224）。

`tests/unit/shared-modes.test.tsx` 让同一组组件测试在两种模式下各跑一遍；`e2e/mock-api.spec.ts` 在 Mock 模式下跑真实 HTTP 链路，场景模式的 e2e 仍是 `baseline.spec.ts` 和 `full-flow.spec.ts`。

**状态**：

- **两种模式**：共用组件测试或 Mock e2e 与场景测试都覆盖。
- **Mock**：只在 Mock 模式验证。
- **单测 / 场景**：由纯函数单测或场景模式覆盖，Mock 模式未单独验证。
- **外壳侧**：取证属于 desktop-shell 或 B 级环境。

| §12 条目 | 证据 | 状态 |
| --- | --- | --- |
| E2E-11 不可信内容语料 | `render.test.tsx`（plain / markdown 语料、链接主机、`javascript:` 不可点）；`baseline.spec.ts`「不可信内容…」；`mock-api.spec.ts`「E2E-11」（经 Mock 注入同一类语料：无脚本执行、无注入元素、无外部请求、链接旁显示主机） | 两种模式（CSP 报告属外壳侧） |
| 药丸与卡片只来自结构化字段 | `overlays-inbox-pairing.test.tsx`「不安全文本只作为纯文本显示」；`scenario.test.ts`「unsafe 场景…」 | 单测 / 场景 |
| lint / 构建检查 | `boundaries.test.ts`；`scripts/check-boundaries.mjs`（只有 `src/ipc/mock/` 可用 `fetch`） | 两种模式 |
| 未知 kind / 枚举不崩溃 | `conversation-simulation.test.tsx`「未知卡片安全降级」；`events.test.ts`「未知事件…」；`execution-graph.test.tsx` 未知状态；`mock-api.spec.ts` 策略拒绝（上报项的未知 `reason_code`） | 两种模式 |
| 令牌 / 状态文字 | `m1-coverage.test.ts`「Tone 映射」快照；`execution-graph-fixes.test.tsx` | 单测 / 场景 |
| E2E-12 水位线 | `events.test.ts`「较旧的快照不得覆盖较新的缓存」；`controller-fixes.test.ts` 多项快照竞态 | 单测 / 场景 |
| 断开重连：重放、无重复、ack 单调 | `mock-adapter.test.ts`「按游标回放 (after, head]…无重复」；`mock-controller.test.ts`「ack 单调推进」 | Mock |
| 重复 / 乱序帧丢弃；`cursor_ahead` → 重同步 | `events.test.ts`；`mock-adapter.test.ts`「游标超前 → resync_required」 | 两种模式 |
| `log_epoch` 变化 → 全量重同步 | `events.test.ts`；`controller-fixes.test.ts`「不复活旧 log_epoch 的消息」；`mock-adapter.test.ts` | 两种模式 |
| E2E-13 关闭后重开与 Mock 一致 | `mock-api.spec.ts`「E2E-13」（消息、交付阶段、“需要你”计数） | Mock |
| 事件风暴合并刷新 | `m1-coverage.test.ts`「500 条同一交付的事件只触发与突发窗口数相当的重新拉取」 | 单测 / 场景 |
| 恢复后“已补齐 n 件事件” | `mock-api.spec.ts`「恢复连接后从游标续流…」 | Mock |
| E2E-01 选中不改变发送上下文 | `shared-modes.test.tsx`「E2E-01」；`send.test.ts`；`mock-api.spec.ts`「E2E-01」 | 两种模式 |
| 档位选择器 | `workspace.test.tsx`「档位选择器用方向键移动…」「ctx 有交付时显示其上限…已触发的闸门不撤销」；`send.test.ts` | 单测 / 场景 |
| ⌘↵ 与输入法保护 | `workspace.test.tsx`「macOS 用 ⌘↵ 发送；IME…」 | 单测 / 场景 |
| E2E-05 只读设备 | `shared-modes.test.tsx`「E2E-05」；`mock-api.spec.ts`「E2E-05」两项（只读禁用；Mock 回 `403 forbidden_role` 时只显示失败） | 两种模式 |
| E2E-17 离线与幂等 | `mock-adapter.test.ts`「响应丢失后重试沿用同一键只产生 1 条消息」（同时断言事件流上恰好 1 条 `message.posted`）；`send.test.ts`；`baseline.spec.ts` | 两种模式 |
| 空项目空态 | `mock-api.spec.ts`「空项目」 | Mock |
| 事件行折叠 | `workspace.test.tsx`「连续后台事件折叠」；`mock-api.spec.ts` E2E-19 / 策略拒绝（新事件行并入折叠组） | 两种模式 |
| 自动滚动与分页 | `workspace.test.tsx`；`mock-api.spec.ts`「自动滚动：离开底部时…新消息 ↓」 | 两种模式 |
| E2E-16 每项目状态保存 | `baseline.spec.ts`「选择只影响检视器，项目切换…保留草稿」；`workspace.test.tsx` | 场景（跨重启范围待 OQ-U15） |
| E2E-18 “需要你”一致性 | `mock-api.spec.ts`「E2E-18」（tab、顶栏、左栏药丸、检视器同时变化）；`shared-modes.test.tsx` | 两种模式 |
| 徽标来自 `inbox.summary` | `shared-modes.test.tsx` 第 1 项 | 两种模式 |
| 各 kind 内联操作 | `overlays-inbox-pairing.test.tsx`「选项来自 payload，选择只调 choose」；`mock-api.spec.ts` 策略拒绝（`decision_needed` 选项来自 payload） | 两种模式 |
| E2E-19 外部变化 | `mock-api.spec.ts`「E2E-19」 | Mock |
| E2E-00 端到端 | `mock-api.spec.ts`「E2E-00」：在没有对话、交付、PR、审批的空项目上，用户选 Low 发任务（请求体带 `level_cap = Low`）→ 新交付与计划卡 → implement → review → PR 创建 → CI 与 review 通过 → 新审批与上报项、审批卡 → 签名 → 合并 → Issue 关闭、移入最近完成，每一步都断言界面；`full-flow.spec.ts` | 两种模式 |
| 策略拒绝上报 | `mock-api.spec.ts`「策略拒绝上报」 | Mock |
| E2E-08 签名成功流 | `mock-api.spec.ts`「E2E-08 / E2E-09」「E2E-00」（已签名 · 等待合并 → 已合并 {sha7} → Issue 已关闭 · 交付完成）；`sign.test.ts` | 两种模式 |
| E2E-03 新推送使审批单禁用 | `mock-api.spec.ts`「E2E-03」（含之后直接 grant 得 409）；`overlays-approval.test.tsx` | 两种模式 |
| E2E-04 连接中断禁用签名 | `mock-api.spec.ts`「E2E-04」；`approval-gates-mock.test.tsx`；`mock-adapter.test.ts`；`overlays-approval.test.tsx` | 两种模式 |
| E2E-06 非审批设备 | `mock-adapter.test.ts`「不请求 challenge」；`overlays-approval.test.tsx` | 两种模式 |
| E2E-07 GitHub 闸门 | `mock-api.spec.ts`「E2E-07」；`approval-gates-mock.test.tsx`；`sign.test.ts` | 两种模式 |
| 审批非待签 / 拒绝 | `mock-adapter.test.ts`「已非待签…」；`overlays-approval.test.tsx` 拒绝 | 两种模式 |
| challenge 过期、digest 变化 | `mock-api.spec.ts`「challenge 过期」（浏览器时钟快进）；`overlays-approval.test.tsx` | 两种模式 |
| E2E-09 不回传待签内容 | `mock-api.spec.ts`「E2E-09」；`mock-adapter.test.ts` | Mock |
| E2E-10 被注入的 WebView 越权 | `boundaries.test.ts`（源码无越权 API）；白名单与 `prepared_id` 伪造由 desktop-shell §12 覆盖 | 外壳侧（全栈 B 级，M1.4） |
| 已签名区与未签名区 | `overlays-approval.test.tsx`；`shared-modes.test.tsx` 审批单 | 两种模式 |
| 生物识别文案 | `sign.test.ts`「按平台给出系统确认文案」；Mock e2e「用 Touch ID 签名批准」 | 两种模式 |
| E2E-15 凭据写入需要签名 | `mock-api.spec.ts`「E2E-15」（签名链、未签名 PUT 得 403、rev 一致、值不回显不入 IPC）；`mock-adapter.test.ts` | 两种模式 |
| 中性 / 收紧改动不弹签名 | `mock-adapter.test.ts` 设置写入 dry_run；`settings-global.test.tsx` | 两种模式 |
| E2E-21 未保存拦截、412、`errors[].pointer`、applied_rev | `overlays-settings.test.tsx`；`settings-global.test.tsx`；`settings-fixes.test.tsx`；`mock-api.spec.ts`「E2E-21」 | 两种模式 |
| 不渲染无契约控件；viewer 无设置入口 | `shared-modes.test.tsx`「E2E-05」；`overlays-settings.test.tsx` | 两种模式 |
| E2E-14 吊销 | `mock-api.spec.ts`「E2E-14」（阻塞、无后续请求、重置后重新配对并正常水合）；`mock-adapter.test.ts`；Mock 侧吊销本机即断流（`tools/mock-api/test/control.test.mjs`） | 两种模式 |
| 配对向导 | `shared-modes.test.tsx`「配对向导」；`mock-api.spec.ts`「配对向导」（含不一致回到起始） | 两种模式 |
| 证书将过期 | `mock-api.spec.ts`「证书将过期」（顶栏提示；到期后阻塞向导） | Mock |
| 设备与证书页 | `shared-modes.test.tsx`；`settings-devices.test.tsx`；`mock-adapter.test.ts` 角色变更签名链 | 两种模式 |
| 阶段条 | `workspace.test.tsx`「Low 交付阶段条显示跳过…」 | 单测 / 场景 |
| 检视器 ⋯ 暂停 / 继续 / 重试 | `shared-modes.test.tsx`；`mock-api.spec.ts`（暂停后菜单变为继续）；`full-flow.spec.ts` | 两种模式 |
| 关联链按交付计算 | `m1-coverage.test.ts`「只列本交付的 Issue → PR → 工作树」 | 单测 / 场景 |
| 放弃交付确认 | `full-flow.spec.ts`「从检视器暂停、继续并确认放弃同一交付」 | 场景 |
| E2E-20 键盘与无障碍 | `baseline.spec.ts` axe 与键盘；`mock-api.spec.ts` axe；`workspace.test.tsx` 方向键与 Esc | 两种模式（未用读屏软件实测） |
| 窄屏 | `baseline.spec.ts` 390×844 | 场景 |

**为本节查明并修复的缺陷**：

- #227：闸门满足后签名按钮不会启用（审批单只看打开时的 PR 快照，违反 E2E-07）；事件流重连期间打开的审批单，恢复连接后仍不可签（违反 E2E-04）。
- 本轮：审批已消费且 PR 已合并时，交付关闭后审批卡停在“已合并”，没有推进到“Issue 已关闭 · 交付完成”（E2E-08）。
- #259：Mock 的设置 rev 与 `settings.updated` 的 seq 脱节（config.md §2.7）；每个 Mock 实例重复编译事件 schema，高负载下测试钩子超时。

**仍不在 A 级范围内的**：E2E-10 的全栈越权测试需要真实外壳与 CSP（desktop-shell §12 与 M1.4 B 级）。E2E-16 的跨重启保留范围待 OQ-U15 决定。

## 本轮验收范围

完整模拟前端及审计收尾，2026-10-25。只修改 `apps/desktop/**`。以下矩阵描述已有模拟范围；状态仅使用「未实现 / 仅展示 / 演示闭环 / 已联调 / 阻塞」。当前没有真实后端联调，因此不把任何项标为「已联调」。最新验收以“R1 / R2 收尾”一节为准，旧测试通过数不代表新问题已关闭。

历史 127 项单测和 22 项浏览器测试只属于基线记录，不计入本轮通过数。

「演示闭环」要求能从入口进入、完成操作、看到过程与结果，并能从失败或取消恢复；证据列给出本轮执行的测试文件与用例。生产缺口与本地完成度分列。

## 覆盖矩阵

| 功能 | 入口 | 可观察行为 | 数据来源 | 测试证据 | 状态 | 生产接入缺口 |
| --- | --- | --- | --- | --- | --- | --- |
| 模型与提供方 | 设置 → 模型 | 三类模型卡、目录约束、测试占位凭据、预览、模拟应用、回读 | `settings.get/catalog`、受保护写入 | `settings-global.test.tsx`「模型页模拟保存并回读打杂模型」；`overlays-settings.test.tsx`「保存先做 prepare…」；`full-flow.spec.ts` 设置菜单 模型 | 演示闭环 | 生成 schema、外壳签名与秘密存储 |
| 档位与预算 | 设置 → 档位与预算 | 默认档位、全部预算字段、模板与本地校验；保存与回读 | `levels`、catalog | `settings-global.test.tsx`「列出全部预算字段…」；冲突恢复用例；`full-flow.spec.ts` 设置冲突 | 演示闭环 | 正式配置联调；不定义 Q-11 追加量 |
| GitHub App | 设置 → GitHub App | App/安装/凭据字段、预览与权限错误 | `github`、catalog | `settings-global.test.tsx` GitHub App 分组；`full-flow.spec.ts` 菜单 | 演示闭环 | 真实 App、凭据和网络 |
| 沙盒与出网 | 设置 → 沙盒与出网 | sandbox/egress 字段、继承、收紧与预览 | `sandbox`、`egress` | `settings-global.test.tsx` 沙盒/出网分组；`full-flow.spec.ts` 菜单 | 演示闭环 | 真实沙盒与代理未接入 |
| 危险动作策略 | 设置 → 危险动作策略 | 动作路径、审批与升级规则、本地错误 | `policies`、catalog | `settings-global.test.tsx` 策略分组；`full-flow.spec.ts` 菜单 | 演示闭环 | 服务端最终判定 |
| 后台守护 | 设置 → 后台守护 | 分流、启用与阈值；不可放宽规则 | `guardian`、catalog | `settings-global.test.tsx` 守护分组；`full-flow.spec.ts` 菜单 | 演示闭环 | 守护服务与热加载 |
| 通知 | 设置 → 通知 | 渠道、路由、不可用渠道说明、模拟保存 | `notify`、catalog | `settings-global.test.tsx` 通知分组；`full-flow.spec.ts` 菜单 | 演示闭环 | 真实通知渠道未接入 |
| 项目模型覆盖 | 设置 → 项目 | 六类 tab、继承/覆盖、模型降级与随档推理 | `project_settings` models | `settings-project.test.tsx`「模型：降级模型与随档推理…」 | 演示闭环 | schema 与服务端合并；稀疏覆盖在适配层会报必填错误 |
| 项目档位覆盖 | 设置 → 项目 | 默认档位、预算字段、只能收紧、恢复继承 | `project_settings` levels | `settings-project.test.tsx`「档位：恢复继承后清空覆盖提交 DELETE…」；`full-flow.spec.ts` 恢复继承 | 演示闭环 | 服务端收紧与 clamped 联调 |
| 项目策略/守护覆盖 | 设置 → 项目 | 动作表、路径、阈值与守护覆盖；放宽错误 | policies、guardian | `settings-project.test.tsx` 策略与守护用例 | 演示闭环 | 服务端合并与 clamped |
| 项目沙盒/出网覆盖 | 设置 → 项目 | 可覆盖字段、集合子集、上限与放宽错误 | sandbox、egress | `settings-project.test.tsx` 沙盒与出网用例 | 演示闭环 | 服务端合并与 clamped |
| 设置公共生命周期 | 每个设置页 | 加载、脏状态、页内/离开确认、预览、412、失败恢复、回读 | ETag、受保护写入 | `overlays-settings.test.tsx`、`settings-project.test.tsx`、`settings-global.test.tsx` | 演示闭环 | 正式加载与外壳句柄生命周期 |
| 设备与证书 | 设置 → 设备 | 列表、改名、角色、吊销确认、自我/本机保护、模拟回读 | `devices.*` | `settings-devices.test.tsx`；`overlays-settings.test.tsx` 设备用例 | 演示闭环 | 真实证书、角色授权与吊销 |
| 外观与本机偏好 | 设置 → 外观 | 深色/中文限制、密度、面板宽度、折叠；保存后即时影响布局 | `prefsGet/Set` | `settings-global.test.tsx` 外观正常/错误；`app.test.tsx`「外观偏好保存后立即应用…」；`full-flow.spec.ts` | 演示闭环 | 原生持久化、系统通知；无浅色/其他语言 |
| 普通问答 | 输入框 + 开发场景 | 发送后分段生成、摘要、结算为正式 Message | 确定性脚本 → Message | `conversation-simulation.test.tsx`「普通问答分段生成后结算…」；`full-flow.spec.ts` 多节点场景 | 演示闭环 | 生产 Message 无文本增量结构 |
| 多节点任务全程 | 输入框 + 开发场景 | 回执、摘要、计划、工具、验证和完成关联同一任务 | 场景统一事实 | `conversation-simulation.test.tsx`「多节点任务工具按 call id 匹配…」；`controller-scenario.test.ts`；`full-flow.spec.ts` | 演示闭环 | 真实引擎与进度读模型 |
| 公开执行摘要 | agent 气泡、执行图 | 「模拟执行摘要」生成中/完成、可展开、状态更新保留展开 | 演示视图模型 | `conversation-simulation.test.tsx`、`execution-graph.test.tsx`、`full-flow.spec.ts` | 演示闭环 | 正式摘要结构待定义；不展示私有思考 |
| 工具活动 | agent 气泡、执行图 | 名称、命令/路径摘要、开始/结束、耗时、折叠详情、失败与重试 | 本地演示模型 | `conversation-simulation.test.tsx`「失败后重试用新 call id…」；`full-flow.spec.ts` 失败重试 | 演示闭环 | `attempt.progress` 客户端字段待补；无完整输出读取 |
| 生成控制 | 开发场景 | 分段、暂停、继续、取消、重置；取消后无迟到回复 | 演示驱动 | `conversation-simulation.test.tsx`「取消生成后无迟到回复…」；`controller-scenario.test.ts`；`full-flow.spec.ts` | 演示闭环 | 生产无停止文本生成路由 |
| 消息正文与卡片 | 对话历史 | Markdown、代码、引用、计划/审批/选择/升档/摘要、未知卡降级 | Message/Card | `workspace.test.tsx`、`conversation-simulation.test.tsx`；基线渲染测试 | 演示闭环 | 正式事件和未知枚举联调 |
| 输入与项目隔离 | 输入框、上下文选择器 | 显式 @、发送复位、项目草稿隔离、IME、失败重发 | 项目 UI、messages.post | `workspace.test.tsx`、`e2e/baseline.spec.ts`「选择只影响检视器…」 | 演示闭环 | 原生跨重启持久化范围待决 |
| 离线与重发 | 工具区断线、待发送气泡 | 离线队列、重连、不重复发送、错误原因 | 队列、事件流、场景 | `e2e/baseline.spec.ts`「离线消息留在队列…」；`controller-scenario.test.ts` 断线恢复 | 演示闭环 | 真实网络与外壳重试 |
| 历史与滚动 | 对话消息区 | 早期分页、滚动锚点、新消息提示、事件组 | messages.list、项目状态 | 基线 `render.test.tsx`/`workspace.test.tsx` 保留；本轮未改分页协议 | 演示闭环 | 大规模性能未等同验收 |
| 执行图入口与返回 | 计划卡、检视器 | 打开同一工作流，返回保留对话、焦点回到触发点 | workflows.list/get | `app.test.tsx`「从检视器打开执行图…」；`execution-graph.test.tsx`；`full-flow.spec.ts` | 演示闭环 | 已有读取契约，待真实外壳联调 |
| 执行 DAG | 执行图 | 阶段、修订号、角色、依赖边、判定与全部节点状态 | Workflow、WorkflowNode | `execution-graph.test.tsx`；`full-flow.spec.ts` 节点与替换 | 演示闭环 | 不提供 DAG 编辑器 |
| 节点与尝试详情 | 执行图 | 尝试列表、时间、状态、失败原因、用量、进度 | attempts.list/get | `execution-graph.test.tsx`；`full-flow.spec.ts` | 演示闭环 | `result_ref` 内容读取待决（Q-12） |
| 并行、失败重试与替换 | 场景、重试 | 并行工具、新尝试编号、graph_rev、superseded_by | 场景统一事实 | `full-flow.spec.ts` 失败重试；`execution-graph.test.tsx` superseded | 演示闭环 | 不把 include_superseded 当历史快照 API |
| 只读预算 | 执行图 | 已有维度、上限、消耗、单位；缺失值显示「—」；无追加按钮 | deliveries.budget | `execution-graph.test.tsx`；`full-flow.spec.ts` 预算面板 | 演示闭环 | Q-11 追加规则待决 |
| 项目与左栏 | 顶栏、左栏 | 项目切换、排序、分组折叠、最近完成、基线详情 | 项目/交付缓存 | `workspace.test.tsx` 折叠与选择用例；`app.test.tsx`；`full-flow.spec.ts` | 演示闭环 | 项目绑定、分支 Ref 无契约入口 |
| 关系地图与工作树 | 视图切换 | 自动布局、筛选、选择、关联、写权随任务变化 | 同一交付/工作树缓存 | `workspace.test.tsx` 地图/工作树用例；基线 e2e | 演示闭环 | 沙盒详情与跨交付 refs 无正式读模型 |
| 检视器与阶段条 | 选择交付 | 当前图、阶段、需要你、守护事件一致 | 同一缓存 | `workspace.test.tsx` 阶段条与选择；`full-flow.spec.ts` 控制 | 演示闭环 | 正式守护和阶段联调 |
| 暂停/继续/重试/放弃 | 检视器菜单、执行图 | 控制同一交付，放弃需确认，暂停后不产生新回复 | 控制路由 → 演示驱动 | `controller-scenario.test.ts`「交付暂停阻止脚本…」；`full-flow.spec.ts` 暂停/继续/放弃 | 演示闭环 | 真实引擎取消语义 |
| 收件箱与选择 | 顶栏、气泡、检视器 | 同一项同步出现消失；选项来自 payload；审批只打开审批单 | InboxItem 与缓存投影 | `overlays-inbox-pairing.test.tsx`；`full-flow.spec.ts` 选择与审批 | 演示闭环 | Q-11 预算追加、补丁处理不得虚构 |
| 模拟审批完整路径 | 审批卡 → 审批单 | 明示模拟批准；approver/live/pending/head/窗口校验；拒绝/过期/head 变化失效 | 适配层审批事实，无真实 signature | `overlays-approval.test.tsx` 模拟批准/拒绝/head 用例；`controller-scenario.test.ts`；`full-flow.spec.ts` | 演示闭环 | 原生签名、GitHub 消费未接入；生产 `approvalSign` 仍拒绝 |
| 模拟配对 | 未配对/吊销/证书过期界面 | 固定测试串、核对码、一致/不一致、失败后重试、重置 | 演示身份模型 | `overlays-inbox-pairing.test.tsx` 配对成功/不一致/失败恢复；`scenario.test.ts`；`full-flow.spec.ts` | 演示闭环 | 不生成密钥/证书；真实系统认证未接入 |
| 可见场景控制 | 开发场景工具区 | 脚本选择、回放/单步/暂停/继续/取消/重置、连接、角色、错误、审批失效 | 构建期隔离的场景驱动 | `conversation-simulation.test.tsx`；`full-flow.spec.ts` 全部流程 | 演示闭环 | 正式包不得包含演示驱动 |
| 异常与安全状态 | 场景控制 | 空项目、加载失败、重连、纪元变化、只读、吊销、过期 | 适配层、控制器 | 基线 `scenario.test.ts`、`events.test.ts`、`controller-scenario.test.ts`；e2e revoked/cert_expired | 演示闭环 | 真实 TLS 与证书链未接入 |
| 生命周期与一致性 | 重置、卸载、切项目、取消 | 清理计时器，项目事实不串，重复/旧事件不回退 | 场景驱动、事件应用器 | `controller-scenario.test.ts` 取消/断线/重试；`full-flow.spec.ts` 取消与重置 | 演示闭环 | 真实服务端回放待联调 |
| 生产隔离与敏感数据 | 正式构建、模拟表单 | 正式构建无演示控件、无示例回退；测试凭据不出现在 DOM；正式产物无 `simulateApproval` / `模拟批准` 字符串 | 构建条件、适配边界 | `e2e/baseline.spec.ts` 正式产物与 secret 用例；`full-flow.spec.ts` 无凭据断言；`npm run build`/`build:scenario` 与产物扫描 | 演示闭环 | 原生 CSP/白名单未验证；其他共享演示分支仍以运行时不可达为边界 |
| 桌面、窄屏、键盘 | 全部新增页面 | 1440×900 / 390×844；焦点、溢出、输入可达、状态可读 | 现有令牌与组件 | 本次 `npm run test:e2e` 66 项双视口；`app.test.tsx` 焦点；axe 自动检查 | 演示闭环 | 未做像素级比对；未用屏幕阅读器实测 |
| 随包字体 | 全部页面 | Inter、JetBrains Mono、Noto Sans SC；无远程请求；许可随包 | 本地字体资源 | `e2e/fonts.spec.ts` 双视口；构建 `FONT-LICENSES.txt` | 演示闭环 | 未验证原生安装包 |

## 上轮 F1–F9 验证记录

2026-10-25 上轮执行，保留为历史记录，不作为六项审计收尾的验收：

| 命令 | 结果 |
| --- | --- |
| `npm run lint` | 通过，含规则自检 |
| `npm run typecheck` | 通过 |
| `npm test` | 24 个文件、254 项通过 |
| `npm run build` | 通过，输出 `dist/` |
| `npm run build:scenario` | 通过，输出 `dist-scenario/` |
| `npm run test:e2e` | 64 项通过：基线 22、本轮完整流程 40、字体 2 |

上轮新增的端到端用例在 `e2e/full-flow.spec.ts`，每个视口 20 项：多节点任务与执行图、失败重试、模拟批准、选择与 head 失效、模拟拒绝、设置各菜单、外观错误与布局、设置冲突、六类项目覆盖与恢复继承、模拟配对失败恢复、取消与重置、检视器暂停/继续/放弃、只读与断线恢复。

生产隔离检查：`dist/assets/*.js` 中无 `scenario/adapter` chunk、`simulateApproval` 或 `模拟批准`；`dist-scenario/assets/adapter-*.js` 独立存在并保留模拟实现。正式构建的 E2E 断言无“开发场景”入口、无 composer、无示例回退。审批模拟分支已改为构建期 mode 常量并从正式产物移除。

## 上轮 F1–F9 修复范围

上轮针对 9 项问题补充回归。后续审计发现以下证据的范围不完整，不能外推为全部并发和身份生命周期正确：

| 项目 | 修复与回归证据 |
| --- | --- |
| F1 | `controller-fixes.test.ts`：同一控制器内旧身份队列隔离及显式处置；未证明适配器持久化 UI 清理，后者由本次第 4 项补齐。 |
| F2 | `controller-fixes.test.ts`：迟到刷新不改变后来选中的项目或覆盖 B 消息；未覆盖刷新与当前项目发送并发，后者由本次第 1 项补齐。 |
| F3 | `settings-fixes.test.tsx`：资源页慢响应、错误和同页重载隔离；未覆盖项目 A 的提交成功响应迟到覆盖 B，后者由本次第 2 项补齐。 |
| F4 | `controller-fixes.test.ts`：恢复连接前先加载后台项目，队列只发送一次并保留权限/身份检查。 |
| F5 | `simulation-approval-fixes.test.tsx`：跨项目或等待用户操作时拒绝冲突运行，同项目重发先结算旧交付；取消会结算旧事实。 |
| F6 | `simulation-approval-fixes.test.tsx`、`sign.test.ts`、`overlays-approval.test.tsx`：审批文案按 `signature`、交付终态和 `prState` 事实生成，不伪造签名。 |
| F7 | `settings-fixes.test.tsx`：项目降级列表清空写入显式 `fallback: []`，只有恢复继承才删除覆盖键。 |
| F8 | `settings-fixes.test.tsx`：未保存确认接管初始焦点，Tab 循环、Esc 取消和焦点恢复均有断言。 |
| F9 | `execution-graph-fixes.test.tsx`、`full-flow.spec.ts`：完成态文字改用符合 AA 的令牌，并在桌面与窄屏完成态执行图运行 axe。 |

汇合后 scoped 回归为 6 个文件、62 项通过；完整单测为 24 个文件、254 项通过；双视口端到端为 64 项通过。

## 上轮独立只读终审

上轮只读评审曾给出 `PASS`。随后双重审计确认 2 项 P1、4 项 P2，将当时实现判为 `BLOCKING`；因此撤回将历史 PASS 表述为“当前无 P0/P1”的声明。下列六项必须经过失败回归、修复、汇合验证和新终审，才能记录新的 PASS。

上轮提出的风险不是已通过验证的结论。真实后端、Tauri 安全边界、系统认证、屏幕阅读器和像素级原型比对仍未验证。

## 上轮六项审计收尾

本次起点为已含 F1–F9 修改的工作树，HEAD 为 `c9bdc12435ad2cdd41447c2ae555d426bdc21e23`。实施前记录了源文件快照和受保护契约的 SHA-256；不以 HEAD 差异冒充本轮差异。

| 项目 | 修复前证据 | 修复与回归 |
| --- | --- | --- |
| 1 / P1 刷新覆盖已发送消息 | 卡住 `inbox.summary`，发送成功后释放刷新，消息数从应有的 12 回退为 11 | `controller.ts` 删除 `currentData`/`sink` 的重复提交，保留 `hydrateProject` 单次写入；`controller-fixes.test.ts` 断言新消息恰好 1 条、outbox 为 0、适配器仍有消息、再次刷新和跨项目回归通过 |
| 2 / P1 项目提交响应串页 | 真实提交 A 成功后仅扣住响应，切 B 并编辑后释放，B 变成 A 值；迟到 412 也污染 B | `SettingsOverlay.tsx` 为项目页增加 `key={page}`；`settings-fixes.test.tsx` 覆盖成功、412、抛错、句柄丢弃；B 草稿和实际设置不变，A 写入仍属于 A |
| 3 / P2 停止演示进入审批 | 真实 `inbox.choose` 选择 `keep` 后仍为 `paused/await_approval` | `adapter.ts` 按 `keep` 分流，清等待引用和本次预创建审批，复用取消结算；`simulation-approval-fixes.test.tsx` 检查交付、图、尝试、预算、收件箱、迟到推进及控制器缓存；`fix` 仍可审批 |
| 4 / P2 重置未清持久化 UI | `identityReset` 后仍有旧草稿；清空后退出会回写；审查又确认重新配对后旧队列误发及旧草稿恢复 | `adapter.ts` 清 `uiStates`、保留偏好；控制器成功重置后恢复默认 UI，只留内存 outbox；重置期间和未配对时不写 UI，重新配对后过滤旧身份队列的持久化副本。测试覆盖不编辑草稿直接重启的无队列/保留/丢弃分支、新队列恢复、显式重试、重置挂起/失败、连接回读失败及取消；配对页说明旧项仅留当前会话 |
| 5 / P2 重置后仍显示吊销 | `revoked`、`cert_expired` 重置后控制器 connection 未变 | `resetIdentity()` 回读真实 `connectionState()`，不伪造 live；单测验证未配对提示、重新配对、取消不变；`baseline.spec.ts` 双视口验证旧终态提示消失 |
| 6 / P2 验证声明过度 | 上轮 PASS 未覆盖上述消息并发、项目提交和持久化路径 | 本文件区分历史、实际修复范围和本次结果，不用历史绿灯替代验收 |

失败日志位于本机 `.work/dual-controller-red.log`（4 失败）、`dual-settings-red.log`（3 失败）、`dual-scenario-red.log`（2 失败）、`dual-controller-persist-red.log`（1 失败）。各包修复后已局部通过；这些日志按各包时点记录，不相加充当全量测试数。

初审结论为 `BLOCKING`：重新配对后保留的旧队列仍能写入新身份存储并在重启时自动发送。已补失败回归（`.work/dual-identity-final-red.log`，4 项失败），修正持久化副本过滤，不删除当前控制器内存队列；同时恢复并通过“取消重置仍保留原身份持久化队列”的有效断言。未增加持久化协议字段。

复审进一步确认旧草稿仍会在重新配对后写回。已补三分支失败回归及连接回读失败测试（`.work/dual-reset-ui-red.log`，5 项失败），成功重置时以已有默认 UI 清除旧草稿、上下文、视图和滚动位置，仅保留内存 outbox。先接受已成功的身份重置，再回读连接；后者失败也不会恢复旧身份或旧 UI。

2026-10-25 22:21 起，在上述修正完成后按以下顺序重跑，全部退出码为 0；最终日志为 `.work/dual-final-*.log`：

| 命令 | 本次实际结果 |
| --- | --- |
| `npm run lint` | 通过，含静态边界规则自检 |
| `npm run typecheck` | 通过 |
| `npm test` | 24 个文件、276 项通过（起点为 254 项，新增 22 项） |
| `npm run build` | 通过，正式资源写入 `dist/` |
| `npm run build:scenario` | 通过，演示资源写入 `dist-scenario/` |
| `npm run test:e2e` | 66 项通过：两种视口各 33 项，包含停止演示新增用例与重置提示增强断言 |

`git diff --check` 通过。两种构建仍有主包超过 500 kB 的体积警告，没有调整警告阈值或新增依赖。双视口保留全部已有 axe 检查，并新增停止演示后收件箱检查；严重/重要违规均为零。

正式 JavaScript 扫描 `simulateApproval`、`模拟批准`、`__GANTRY_SCENARIO__`、`scenario/adapter` 均无命中，且无 `adapter-*` 文件；场景产物保留独立适配器。正式包缺少外壳时不回退示例的双视口用例通过。

6 个受保护契约文件与开始时逐文件 SHA-256 一致，集合指纹为 `63b862b25d8f8ddfd8d4ccc3e54667466801324a39c345e83d30cb3e05715154`。本轮仅改动 4 个源码、4 个单测、2 个端到端文件及本验证文档，保留已有修改；没有写入 `apps/desktop/**` 外的文件。源代码与测试指纹为 `42bd66f3ac4b16532a7ab7d78b2c14b15669e6fe44a9ac7c60e65f7ac275e854`，本机差异与核对结果分别在 `.work/dual-audit-diff.patch`、`.work/dual-audit-evidence.json`。

2026-10-25 第三轮独立只读复审曾记录 `PASS`，评审记录为 `ses_fe5e8ca2d51d0QevlKQyCB9bIo`，对应上述源码与测试指纹。随后固定工作树内容标识 `38c77f76bde9eb9d9ba738acbc65019fd918bd14d2b21135ec6afeb4be24f88c` 的只读审计确认 R1、R2 两个 P1，裁决改为 `BLOCKING`。历史 PASS 保留，不再代表当前验收；项目设置隔离、停止演示分流、重置清 UI 和成功连接回读等已通过修复保留。

连接回读失败仍有非阻塞界面限制：身份重置和 UI 清理已生效，但 `connectionState()` 抛错时，界面可能暂留旧吊销/过期提示并显示错误，需要重试或重新配对。该路径的旧身份与持久化隔离已有回归，不宣称错误态文案也已完成同步。

上轮只验证首次重新配对后旧队列不写入新身份存储；后续再次终态会重新保存旧项，已确认为 R2，不能归入“从未重置”的限制。上轮第 1 项只关闭重复提交旧快照的路径，更早的项目快照提交仍覆盖新消息，已确认为 R1。两项修复与新证据见下节。未执行重置、终态退出后直接重新配对的跨重启代际恢复，以及重置前已发出的原生 `uiStateSet` 与重置命令之间的执行顺序仍未验收。`keep` 分流未扩大到通用取消或 expire/head 的生命周期变更。

并行实施阶段曾有一次 `overlays-settings` 找不到“密度”标签的失败；单文件复跑与随后全量复跑通过，未修改该测试或降低断言。本次主力最终顺序验证未出现该失败，不能据此宣称所有运行环境均无偶发问题。

## R1 / R2 收尾

本轮只修复两个已确认 P1，不重做此前已通过的修复。开始时核对 HEAD `c9bdc12435ad2cdd41447c2ae555d426bdc21e23`、未提交文件及审计快照，结果完全一致。起点完整内容标识为 `38c77f76bde9eb9d9ba738acbc65019fd918bd14d2b21135ec6afeb4be24f88c`，记录在 `.work/r1-start.json`。审计材料来自本机 `gantry-frontend-r1-audit-20261025T225832` 目录。

| 问题 | 失败证据 | 最小修正与回归 |
| --- | --- | --- |
| R1 / P1 项目快照覆盖已发送消息 | 在 `project_settings.get` 返回处挂起，发送成功后释放，界面消息消失；事件触发的消息列表刷新也存在同类覆盖 | `controller.ts` 在项目快照提交时，仅为同一对话补回读取期间新增且未在返回列表中的确认消息，复用 `mergeMessages`；事件刷新在响应后读取当前缓存并核对对话与水位。回归验证发送恰好一次、消息恰好一条、outbox 为零、再次刷新、普通列表更新、新对话隔离和重同步清旧纪元缓存；保留原 `inbox.summary` 与跨项目回归 |
| R2 / P1 再次终态重新持久化已隔离队列 | 真实离线 `send()` 入队，成功重置并重新配对后，再进入 `revoked` / `cert_expired`，关闭时旧项重新写入存储 | `controller.ts` 记录成功重置时的内存代际阈值；终态持久化仍保留当前身份项，但排除此前成功重置隔离的项。不增加持久化字段，不删除内存内容。两种终态均验证旧项不恢复、不自动发送，新身份真实离线项仍可持久化和恢复；旧内存项显式重试恰好一次。既有取消重置、重置失败、连接回读失败、不编辑草稿即重启等回归保留 |

先添加回归再修源码：`.work/r1-red.log` 为 4 项失败、25 项通过，失败分别是 R1 两条交错和 R2 两种终态；修复后 `.work/r1-green.log` 为 29 项通过。再补 3 项边界测试及下述 2 项生命周期回归，`controller-fixes.test.ts` 最终为 34 项。审计提供的 9 项探针原样复制为临时测试，在最终源码上执行后移除，全部通过，日志为 `.work/r1-audit-probe-final.log`，不计入正式单测数：R1 为界面 1 / 存储 1 / outbox 0；R2 两种终态均为 `persisted=[]`、`posted=[]`。

首轮独立终审 `ses_fe5e8c481ab0EclNZbV1L4QhC3` 给出 `BLOCKING`：刷新挂起期间，当前事件流的终态帧因读取代际变化被丢弃，无法进入重置流程。此前探索测试的失败不是验收豁免，已恢复并参数化为两种终态；`.work/r1-lifecycle-red.log` 两项失败，修复后两项通过。最小修正在 `openStream` 回调：仍先拒绝旧流令牌，仅让当前流的 `revoked` / `cert_expired` 帧越过读取代际检查，立即进入终态。回归继续完成重置、重新配对、新消息发送、释放旧读取，并确认旧快照不能覆盖新身份、旧队列仍隔离、旧流迟到终态帧不能影响新身份。未扩展为通用事件调度改造。

首轮完整检查的 285 项单测与 66 项浏览器测试日志保留在 `.work/r1-first-final-*.log`，不代替修正后的验证。2026-10-25 23:55 起，主力在 `apps/desktop` 再按下列顺序执行，全部退出码为 0。最终日志为 `.work/r1-final-*.log`：

| 命令 | 本轮实际结果 |
| --- | --- |
| `npm run lint` | 通过，含静态边界规则自检 |
| `npm run typecheck` | 通过 |
| `npm test` | 24 个文件、287 项通过，比起点新增 11 项 |
| `npm run build` | 通过，正式主包 597.93 kB |
| `npm run build:scenario` | 通过，保留独立场景适配器 |
| `npm run test:e2e` | 66 项通过，1440×900 与 390×844 各 33 项，重试次数为 0 |

`git diff --check` 通过，日志为 `.work/r1-final-diff-check.log`。浏览器用例、视口、axe 严重/重要违规门槛与起点字节一致，已有检查通过。正式 `dist` 无 `__GANTRY_SCENARIO__`、`simulateApproval`、`模拟批准`、`scenario/adapter`，无 `adapter-*` 文件，扫描日志为 `.work/r1-final-dist-scan.log`；缺外壳不回落示例的两种视口用例通过。两个构建仍有超过 500 kB 的包体积警告，未调整阈值。

本轮仅修改 `src/store/controller.ts`、`tests/unit/controller-fixes.test.ts` 与本文件，另有 `.work/` 本机证据。6 份受保护契约与起点逐文件 SHA-256 一致，未修改依赖、配置或 `apps/desktop/**` 外文件。当前实现标识为 `f36aece08109a9d2df329871f3dbee22543ece4715c82b79e0875667effd620d`，计算口径沿用审计：非忽略文件中排除 `.md` 和 `.work/`，按路径及文件 SHA-256 汇总；不是 HEAD，也不是上轮源码与测试指纹。真实增量见 `.work/r1-final-diff.patch`，核对结果见 `.work/r1-final-evidence.json`，可在仓库根执行 `node apps/desktop/.work/r1-final-check.mjs` 复核。

修正后的独立只读复审结论为 `PASS`，R1、R2 及刷新期间终态帧的生命周期阻塞均已关闭，未发现新的阻塞缺陷。评审记录为 `ses_fe5e8c481ab0EclNZbV1L4QhC3`，对应上述实现标识。审核者交叉读取实际源码、精确增量、红绿回归、最终命令日志和指纹证据，未执行测试或修改文件。此 PASS 仅适用于 R1/R2 固定验收路径及本轮新增的生命周期回归。

边界：R2 的重启后自动发送反例只属于场景适配器影响证据，不外推生产配对策略。从未重置的终态跨进程代际恢复、非终态下程序化重置的事件流失效行为仍未验收；新对话列表成功但消息列表失败时的旧缓存展示也未在本轮修正。本轮未验证所有事件交错、原生在途 IPC、真实后端、证书、系统认证、签名、安装包或完整无障碍，不代表完整 M0/M1 或生产安全验收。

## 视觉、键盘与无障碍

- 双视口均为 1440×900 与 390×844；新增设置、执行图、审批、配对和工具区在这些尺寸下无页面横向溢出。
- 工作区、设置、收件箱、审批单和完成态执行图执行 axe（wcag2a/2aa/21aa）自动检查，未发现 serious/critical；这不是完整 WCAG 认证。
- 执行图返回、设置返回、收件箱 Esc、审批关闭均通过 `app.test.tsx` 和 e2e 验证焦点回到触发点。
- 原型依赖的 `support.js` 不在仓库，未做像素级截图比对；按原型源码、令牌和几何核对。
- 基线截图位于 `test-results/baseline-*/{desktop,narrow}/{workspace,map,worktrees,settings,inbox,approval}.png`，由最近一次 e2e 生成，不进入版本控制。

## 仍待生产接入或外壳验证

- 真实后端、Tauri 命令白名单、CSP、mTLS、证书链、Touch ID / Windows Hello、系统通知和安装包均未验证。
- 共享源码仍包含部分正式/演示双路径，以保持场景测试和生产外壳共用组件；本轮只对安全敏感的模拟审批路径做了构建期剔除。其余真实后端、Tauri 原生安全边界、系统认证、通知、安装包和像素级原型比对仍未验证。
- Q-11 预算追加、Q-12 结果内容读取、`attempt.progress` 正式字段等待决，不由前端自行定义。
- 项目稀疏模型覆盖仍受场景适配层必填校验限制；worker 非空 `fallback` 会因 claude-code 不支持而降级报错，预览中会明确显示。这些是生产联调前需要解决的适配层/契约问题。
- 屏幕阅读器、真实触控与系统级快捷键仍需人工验证。

## 历史基线记录

日期：2026-10-25。范围仅 `apps/desktop`。没有修改后端、共享契约或 `schemas/`，没有提交或推送。

## 实际执行

在 macOS、Node.js 26.12.0、npm 11.20.0 和本机 Google Chrome 下执行：

| 命令 | 结果 |
| --- | --- |
| `npm run lint` | 通过，含静态规则自检 |
| `npm run typecheck` | 通过，包含应用、单测及浏览器测试 |
| `npm test` | 14 个文件、127 项通过 |
| `npm run build` | 通过，输出 `dist/` |
| `npm run build:scenario` | 通过，输出 `dist-scenario/` |
| `npm run test:e2e` | 22 项通过，桌面和窄屏各 11 项 |

浏览器检查覆盖：项目草稿隔离、选中不改变发送上下文、只读权限、离线队列、收件箱键盘关闭与焦点恢复、审批摘要与签名禁用、不可信内容无远程请求、正式包不回退示例、设置秘密值清空与未保存拦截、运行中吊销及证书过期阻塞。

工作区、关系地图、工作树、模型设置、收件箱与审批单运行了 axe 自动检查，无严重或重要违规。此结果不是完整的 WCAG 合规认证；未用 VoiceOver 或 NVDA 实测。

## 视觉核对

- 窗口为 1440×900 与 390×844。检查深色令牌、紧凑排版、240px 左栏、300px 检视器、200px 设置导航、三张模型卡和540px审批抽屉。
- 已查看工作区、设置、地图及审批截图，并检查窄屏堆叠、输入区可达和页面无横向溢出。表格、地图允许容器内滚动。
- 修复了连接文字对比度、设置表格键盘滚动、左栏重复标题、设置浮层误显示真实连接和浮层缺少示例标记。
- 原型依赖的 `support.js` 不在仓库中，不能直接运行原型。因此采用原型 HTML 与 `docs/prototype.md` 的尺寸、令牌和结构核对，不声称做过逐像素截图比对。
- 截图位于 `test-results/` 各测试目录中的 `workspace.png`、`map.png`、`worktrees.png`、`settings.png`、`inbox.png`、`approval.png`。报告位于 `playwright-report/index.html`。这些是本地验证产物，下一次测试会覆盖，不进入版本控制。

## 独立评审

历史独立终审给出 `PASS`。该结论仅属于早期基线，不替代本次“六项审计收尾”的验证和独立终审。

对终审指出的可直接修复问题，已补上：受保护提交失败后的句柄丢弃、停止期间开启事件流的竞态保护、失败消息编辑时恢复上下文与上限、项目预览的离开拦截，以及表格键盘焦点。修复后重新执行了上表全部命令。

## 已知限制

- 从全局收件箱打开尚未载入项目的审批，可能因缺少交付缓存而保守禁签。先切换到所属项目，再重新打开审批单。此项需在真实外壳联调时完善。
- 只读设备可以从收件箱查看审批单，部分对话和检视器入口仍禁用，入口行为尚未完全统一。
- 设置冲突提示使用全局修订计数，无关设置更新也可能触发保守冲突提示。需重新载入再编辑，不自动合并。
- 待确认配对的跨重启恢复、TLS 握手失败后的恢复入口仍需原生外壳联调。场景配对仅拒绝，不证明这些恢复流程可用。
- 取消系统签名确认后采用保守策略丢弃句柄，需重新打开或载入审批单。场景配对失败文案仍较泛化。
- 吊销前尚未落盘的短时间窗口内，草稿与待发队列仅在内存；进程强退可能丢失。跨重启保留范围仍按外壳待决项处理。
- 场景替身不等于正式 Mock API；所有协议类型为文字契约转写。真实后端、CSP、命令白名单、Touch ID、Windows Hello、证书、系统通知和安装包均未验证。

本次交付是可运行、可测试的前端基线，不是完整 M1–M4 验收或生产发布。
