# 桌面界面（`desktop-ui`）

> 状态：设计 v0.1 · 里程碑：M1 起交付，M3 完成 · 泳道：`fe-desktop`（见 [milestones.md](../milestones.md)） · crate / 目录：`apps/desktop/src`（WebView 前端）

对话为中心的工程编排工作台与设置浮层：只显示服务端事实与运行状态，所有读写都经 [`desktop-shell`](desktop-shell.md) 的白名单命令；界面规格见 [prototype.md](../prototype.md)，本文把它的每条交互规则落到组件、状态与调用。

## 1. 职责

1. **工作区**：顶栏（项目 tab、角色、连接状态、"需要你"）、左栏、中栏三视图（对话 / 关系地图 / 工作树）、检视器、状态栏、断线横幅（prototype.md §2）。
2. **输入区**：多行输入、`@` 上下文、档位**上限**选择器、发送、离线排队（P-32）。
3. **审批入口与审批单**：各处只"打开审批单"，签名只在审批单里发起；审批卡 / 收件箱 / 检视器 / 审批单四处显示同一个审批的实时状态（P-33）。
4. **"需要你"**：把 open 的 `InboxItem` 投影到顶栏、tab、左栏、地图、气泡、检视器、收件箱浮层，同一项处理后各处同时消失。
5. **设置浮层**：由 JSON Schema 与 `catalog` 驱动的表单，三组页面（服务端 / 项目 / 本机），`dry_run`、签名提示、冲突处理、未保存拦截（config.md §13，P-31）。
6. **配对向导与设备页的界面**（调用 `pairing_*`、`api_request` 的设备路由、`guarded_write_*`）。
7. **状态管理**：实体缓存、按项目保存的界面状态、会话状态、待发送队列；快照 + 事件流的衔接、游标 ack、重新拉取调度（api.md §5.3）。
8. **不可信内容渲染规则的执行**（I5，§8.2）。
9. **可访问性、键盘、窄屏、本地化**（prototype.md §2.9）。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 无交付。**消费**：`schemas/openapi.yaml` 生成的 TS 类型、Mock API 服务、IPC 类型（`desktop-shell` §6） |
| M1 | 配对向导；顶栏（项目 tab 与徽标、角色、连接状态、"需要你 · n" 计数及**最小浮层**：平铺列表 + 打开 + 审批入口）；左栏（项目信息、进行中 Issue / PR / 工作树、排序与折叠、最近完成）；**对话**（收发、事件行与折叠、气泡边框与种类文字、计划卡 / 审批卡 / 升档卡 / 二选一卡、引用 chip、空态、自动滚动与分页、离线排队与"待发送"、只读设备禁用、⌘↵ / Ctrl+Enter 与输入法保护）；**检视器**（头部、需要你区、阶段条、当前阶段的节点列表、合并闸门清单、后台守护列表）；**审批单**（最小：动作 / 仓库 / PR / 完整 head SHA / 有效期 / 合并闸门 / 签名 / 拒绝，M1 的系统级确认是外壳原生确认框）；设置浮层框架 + **模型页**（含凭据写入的签名流程）；状态栏、断线横幅、证书 / 吊销提示；不可信内容渲染与 CSP 约束下的全部前端要求；无障碍基线 |
| M2 | **关系地图**（自动列式布局）；**工作树视图**；**收件箱浮层完整版**（按项目分组、各 `kind` 的内联操作）；**完整审批单**（Touch ID / Windows Hello 文案、失效流程、策略命中展示——受 OQ-U3 限制）；**`@` 上下文选择器、档位上限选择器、上限冲突提示、在对话中引用、回复**；"从分支开始任务…"（受 OQ-U6 限制）；检视器 ⋯ 菜单（放弃交付；暂停 / 继续 / 重试受 OQ-U2 限制）；"上次查看"分隔线与离开期间摘要（受 OQ-U10 限制）；本机通知开关；设置页：**档位与预算、危险动作策略、后台守护**；项目页、设备与证书、外观建议此时交付（config.md 标"未排期"，OQ-U14） |
| M3 | DAG 详情视图、图修订历史（`include_superseded`）、预算面板（`deliveries.budget`） |
| M4 | 地图按父交付单元分组、显示 `depends_on` 依赖（Ultimate）；其余无（手机端是独立模块 [`mobile`](mobile.md)） |

M1 的档位上限来源是个已知缺口（OQ-U12）：milestones 把档位选择器排在 M2，而 M1 退出标准要求"发起 Low 任务"。

## 2. 不负责

- 密钥、网络、签名、IPC 白名单、CSP、导航限制 → [`desktop-shell`](desktop-shell.md)。本模块**从不**直接联网，也不处理待签字节。
- 危险动作是否需要审批、审批绑定什么 → `policy`（I3）。本模块只展示审批状态并提供入口。
- 自然语言的意图解析（任务 / 查询 / 追问）→ `conversation`。**界面不得把自然语言解析成设置变更或控制动作**（I7）；控制只由按钮触发。
- 配置的校验规则与合并 / 收紧语义 → 服务端（config.md §11）。界面只提供 schema 驱动的输入约束并展示服务端返回的 `errors`。
- 通知文案与渠道 → `notify`；本机弹窗 → `desktop-shell`。
- 代码查看、diff 渲染、编辑 → 非目标（architecture §1）。界面只给 GitHub 跳转（经 `open_github`）。
- 设备证书签发与吊销表 → `pki`。设备页只是 `devices.*` 与 `guarded_write_*` 的前端。
- 事件 `data` 的 JSON Schema → `contract` 泳道（`schemas/events/*.json`，M0 产出）。

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| [`desktop-shell`](desktop-shell.md) | 全部 IPC：`api_request`、`events_*`、`approval_*`、`guarded_write_*`、`pairing_*`、`identity_*`、`connection_state`、`prefs_*`、`ui_state_*`、`open_*` | 本文 §4–§5；`desktop-shell` §6 |
| `api`（契约） | 实体表示、路由、错误码、事件流语义；`Card` 与 `Ref` 的线上形态 | [api.md](../interfaces/api.md) |
| `data-model`（契约） | 实体字段与枚举、状态机、事件目录、`UntrustedText` / `Ref` / `Card` / `CheckSummary` | [data-model.md](../data-model.md) |
| `config`（契约） | 设置页分组与表单来源：`schemas/config/*.json` + `GET /v1/settings/catalog`；公共规则（凭据 `SecretRef`、项目覆盖矩阵、热加载文案） | [config.md](../interfaces/config.md) §2、§13 |
| 界面规格 | 交互与视觉的权威规格 | [prototype.md](../prototype.md)；行为参考 `prototype/Main.dc.html` 底部 `renderVals()`（**参考，不复用代码**，见 prototype/README.md） |
| `contract` 泳道产物 | `schemas/openapi.yaml`（TS 类型生成）、`schemas/config/*.json`、`schemas/events/*.json`、Mock API 服务、`TestCA` | [milestones.md](../milestones.md) M0 |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| `ApiResult`（实体、列表、`problem`） | `api_request` 等 IPC | desktop-shell §6.2；体为 api.md 的实体表示 | 通道可信；**文本字段不可信**：`UntrustedText`、`Message.text`（`agent` / `event`）、`InboxItem.summary`、`Delivery.title`、Issue / PR 标题、Card 里的文本、`payload` 里的 `UntrustedText`（api.md §1.5） |
| `StreamFrame`（事件、心跳、`state`、`resync_required`） | `events_open` 的 Channel | desktop-shell §6.2；事件信封见 data-model §5.1 | 同上；事件 `data` 只当"提示"，默认按 `subject` / `correlation_id` 重新 GET（api.md §5.3） |
| `ConnectionState` / `IdentityStatus` | `connection_state`、`identity_status` | desktop-shell §6.2 | 可信 |
| `ApprovalSheetModel`、`GuardedWritePreview` | `approval_prepare`、`guarded_write_prepare` | desktop-shell §6.3 | `signed` 区可信（外壳生成）；`unsigned` 区含不可信文本，必须标注并视觉分离 |
| 用户输入 | 键盘、指针、输入法 | — | 用户文本：作为 `Message.text` 发出；在界面里仍按纯文本渲染 |
| 本机偏好与界面状态 | `prefs_get`、`ui_state_get/list` | desktop-shell §6.1 | 可信但可能损坏；解析失败按默认值处理 |
| 设置表单目录 | `catalog`、`schemas/config/*.json`（构建期打包） | config.md §2.8 | 服务端提供的枚举与约束；**前端不得硬编码**服务端才知道的值 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| IPC 调用 | `desktop-shell` | §6.1 的 `IpcAdapter` 方法；写操作的 `Idempotency-Key` 由本模块按"用户意图"生成（`crypto.randomUUID()`），重试沿用 |
| `events_ack(seq)` | `desktop-shell` | 每个帧**应用之后**才 ack（先应用、后推进游标） |
| `ui_state_set`、`prefs_set` | `desktop-shell` | 按项目的界面状态块（含待发送队列）、本机偏好；**不含**凭据与配置键 |
| 界面（DOM） | 用户 | 只用框架的文本节点 / 受控元素构建，无 `innerHTML` |
| 外链请求 | `open_external`、`open_github` | 链接旁始终显示目标主机（api.md §1.5）；非 `github.com` 主机的确认框由外壳弹出（desktop-shell §6.1） |

本模块**不**发出事件、不写任何服务端状态之外的东西；服务端因写请求产生的事件见 `desktop-shell` §11。

## 6. 对外接口

本模块是叶子，没有被其他模块调用的接口。它对测试与对 `desktop-shell` 暴露两条"缝"。

### 6.1 `IpcAdapter`（与外壳之间的缝）

```ts
interface IpcAdapter {
  identityStatus(): Promise<IdentityStatus>;
  pairingBegin(a: { pairingString: string; deviceName: string }): Promise<{ confirmationCode: string; device: Device }>;
  pairingConfirm(a: { matches: boolean }): Promise<IdentityStatus>;
  identityReset(): Promise<IdentityStatus>;
  connectionState(): Promise<ConnectionState>;
  api<R extends RouteId>(r: R, a: RouteArgs[R]): Promise<ApiResult<RouteBody[R]>>;   // 32 条 UI 可调路由
  eventsOpen(a: { afterSeq?: number }, onFrame: (f: StreamFrame) => void): Promise<void>;
  eventsAck(seq: number): Promise<void>;
  eventsClose(): Promise<void>;
  approvalPrepare(a: { approvalId: string }): Promise<ApprovalSheetModel>;
  approvalSign(a: { preparedId: string }): Promise<ApiResult<Approval>>;
  approvalDiscard(a: { preparedId: string }): Promise<void>;
  guardedWritePrepare(a: GuardedWriteArgs): Promise<GuardedWritePreview>;
  guardedWriteSubmit(a: { pendingId: string }): Promise<ApiResult<unknown>>;
  guardedWriteDiscard(a: { pendingId: string }): Promise<void>;
  prefsGet(): Promise<Prefs>;
  prefsSet(a: { patch: Partial<Prefs> }): Promise<Prefs>;
  uiStateGet(a: { projectId: string }): Promise<ProjectUiState | null>;
  uiStateSet(a: { projectId: string; state: ProjectUiState }): Promise<void>;
  uiStateList(): Promise<Record<string, ProjectUiState>>;
  openExternal(a: { url: string }): Promise<void>;
  openGithub(a: { kind: "pr" | "issue" | "commit"; repo: string; number?: number; sha?: string }): Promise<void>;
}
```

两个实现：`TauriIpcAdapter`（生产，唯一调用 `invoke` 的地方）和 `MockIpcAdapter`（测试，见 §12：直连 Mock API 的 HTTP 层，并模拟外壳的路由白名单与 `ApprovalSheetModel`）。代码里**只有** `ipc/` 目录可以引用 `@tauri-apps/api`，由 lint 规则强制。

### 6.2 纯函数接口（可直接单测）

```ts
// 同一份 open InboxItem 投影到所有表面（§7.6）
needsYou(state): { byProject: Map<ProjectId, InboxItem[]>; byDelivery: Map<DeliveryId, InboxItem[]>; total: number };

// 签名可用性（§7.8）；外壳还会独立重检，这里只决定界面禁用与文案
signAvailability(i: { conn: ConnectionState; me: Me; approval: Approval; pr: PullRequestRef | null;
                      sheet: ApprovalSheetModel | null; now: number }): { ok: true } | { ok: false; reason: SignBlock };

// 发送语义（§7.7）：上下文、上限、离线
buildSend(i: { text: string; ctx: Ref | null; levelOverride: Level | null }): MessagesPostBody;

// 不可信文本渲染（§8.2）
renderUntrusted(t: UntrustedText | string, mode: "plain" | "markdown"): RenderNode[];
```

## 7. 内部设计

### 7.1 技术形态与目录

- TypeScript（`strict`）+ 一个声明式组件框架。**框架与状态库未选定**（OQ-U1）；本文要求状态层（`sync/`、`store/`、`render/` 的纯逻辑）**不依赖框架**，以便单测与换框架。
- 目录：

  | 目录 | 内容 |
  | --- | --- |
  | `ipc/` | `IpcAdapter` 与两个实现；唯一引用 Tauri API 的地方 |
  | `api/` | 由 `openapi.yaml` 生成的类型与路由 ID 常量；`problem` 解码；`code` → 文案 |
  | `sync/` | 水合序列、`EventApplier`、重新拉取调度器、游标 ack、重连计数 |
  | `store/` | 实体缓存、按项目界面状态、会话状态、待发送队列、选择器 |
  | `render/` | `UntrustedText`、受限 Markdown、字符清理、链接处理 |
  | `features/` | `topbar`、`leftpane`、`conversation`、`map`、`worktrees`、`inspector`、`inbox`、`approval`、`settings`、`pairing` |
  | `a11y/`、`i18n/`、`theme/` | 键盘与焦点、文案目录（按 `code` 本地化错误）、设计令牌 |

- **设计令牌**：颜色、记号、字号、间距取 prototype.md §1 与 §1.1，写成主题变量；`theme/` 预留浅色主题，M1 只交付深色（浅色未设计，OQ-U16）。字体随包（Inter、JetBrains Mono 与中文回退），**不引用远程字体**（原型引用 Google Fonts，生产禁止）。
- **状态色映射**（`Tone`）：`run`（蓝）、`heal`（中性灰，"自愈中"）、`need`（黄，只用于"需要你"）、`ok`（绿）、`idle`（暗灰，空闲 / 暂停 / 放弃）、`warn`（橙，只用于"失败且无人处理""缺规格"）。**每个 `Tone` 必须与文字同现**（prototype.md §1.1），组件库的 `StatusDot` 不允许单独使用，必须带 `label`。
- 平台相关文案（⌘↵ / Ctrl+Enter，Touch ID / Windows Hello）取自 `identity_status().device.platform`，不嗅探浏览器 UA。

### 7.2 数据流：水合、事件流与一致性

**启动序列**（`identity_status = paired` 且 `ConnectionState` 非 `revoked` / `cert_expired`）：

1. `me.get` → 角色、档案、`server.time`；`projects.list`（翻完所有页）。
2. 并发（上限 4）为**当前项目**拉：`conversations.list` → `messages.list`（`limit=50`，`order=desc`）；`deliveries.list?project_id&active=true`；`deliveries.list?project_id&stage=closed&stage=abandoned&limit=10`（最近完成）；`worktrees.list?project_id`。为**每个项目**拉 `inbox.list?project_id&status=open`（顶栏计数与 tab 徽标）。operator 以上另拉 `project_settings.get`（`levels`，`view=effective`）取默认上限。
3. 取所有响应 `X-Event-Seq` 的**最小值** `S`，`eventsOpen({ afterSeq: S })`（api.md §5.3：一个客户端一条流，不按项目各开）。
4. 其余项目在**首次切换**时才拉对话 / 交付 / 工作树；此前这些项目的事件只用于收件箱计数。
5. 活动交付的工作流（状态点与检视器要用）：选中时拉 `workflows.list?stage=<当前阶段>` + `workflows.get`；后台对最近更新的 ≤ 20 个活动交付以并发 2 预取（OQ-U18）。

**水位线（watermark）**：每个缓存条目带 `asOf`（产生它的响应的 `X-Event-Seq`）。事件 `seq ≤ asOf` 的对该条目**跳过**（快照已包含），`seq > asOf` 才触发更新。这保证了"后拉的快照"与"先到的事件"互不回退，也让未水合的项目可以安全地晚点再拉。

**帧处理**（顺序、单线程、批量提交后再渲染）：

| 帧 | 处理 |
| --- | --- |
| `event` | `seq ≤ appliedSeq` 丢弃 → `EventApplier`（§7.5）→ `appliedSeq = seq` → `eventsAck(seq)` |
| `heartbeat` | `appliedSeq = max(appliedSeq, seq)` → `eventsAck(seq)`（过滤流也靠它推进游标） |
| `state` | 更新 `session.connection`；`epoch` 增加且 `phase = live` 时开始统计"补齐事件数"，到第一个 `heartbeat` 为止，若 > 0 显示"已补齐 n 件事件" |
| `resync_required` | `resync()`：清空实体缓存（保留界面状态与待发送队列）→ 重走启动序列（不带旧游标） |

**重新拉取调度器**：事件默认只把相应资源标记为"需刷新"，调度器按资源键合并（窗口 100 ms [建议值]）、去重在途请求、并发 ≤ 4、可见资源优先、`resync` 时中止全部；读限流 600/min（api.md §3.7）是硬上限，初始水合与事件风暴都经它节流。事件 `data` 若足以更新（由 `schemas/events` 保证）才直接打补丁，否则一律重新 GET。

### 7.3 状态模型

**实体缓存**（归一化，键为 ID）：`me`、`projects`、`conversations`、`messages`（按对话、按 ID 升序）、`deliveries`、`workflows`（含节点）、`worktrees`、`inbox`、`approvals`、`devices`（仅设备页）、设置文档（仅设置浮层打开时）。每项带 `asOf`；列表另带"是否已拉全"。

**会话状态**：`connection`（`ConnectionState`）、`me.role` / `profile`、`clockOffsetMs`、`appliedSeq`、`resyncing`。

**待发送队列**（每项目）：`{ local_id, idempotency_key, text, level_cap?, refs, created_at, state: queued | sending | failed(code) }`；随 `ui_state_set` 持久化（OQ-S6、OQ-U15）。

**每个项目各自保存的界面状态**（prototype.md §2 "每个项目各自保存"；切换项目再切回时全部恢复）：

| 字段 | 类型 | 默认 | 重置 / 失效规则 |
| --- | --- | --- | --- |
| `view` | `chat` / `map` / `worktrees` | `chat` | 不重置 |
| `sel` | `{kind: issue\|pr, repo, number}` / `{kind: worktree, id}` / `{kind: base}` / `null` | `null` | 对应实体不再存在（且不在"最近完成"里）→ `null`。**选中只驱动检视器** |
| `draft` | string | `""` | 发送（入队）成功后清空 |
| `ctx` | `Ref` / `null`（= 全项目） | `null` | **每次发送后复位**；在 `@` 选择器里选 / "回复" / "在对话中引用"才改变；选择新 `ctx` 时 `levelOverride` 复位 |
| `levelOverride` | `Level` / `null` | `null` | 每次发送后复位；`null` 时显示 `ctx` 交付的上限，再回退到项目默认 |
| `needsOnly` | bool | `false` | 地图"只看需要你"过滤 |
| `showAll` | `{issues, prs, worktrees}` 各 bool | 全 `false` | 左栏各区"显示全部（n）" |
| `expandedGroups` | 事件折叠组的键集合（首条事件的 `msg_` ID） | 空 | — |
| `lastViewed` | `{ message_id, at }` | 无 | 见 §7.4 R-20 |
| `chatScroll` | `{ atBottom, anchor_message_id, offset }` | `atBottom` | 切换项目时记录、返回时还原 |

**不按项目保存（全局）**：`currentProjectId`；浮层：`overlay.inbox`、`overlay.settings { open, page }`、`overlay.sheet { approval_id }`；瞬时状态（不保存）：`@` 选择器是否展开、检视器 ⋯ 菜单、放弃确认框、提示条。设置页的未保存改动属于设置浮层自己的状态，离开前需确认（R-62）。

### 7.4 prototype.md 交互规则 → 实现映射

"状态"列的 `ui.*` 指上表的按项目状态。"M"为里程碑。路由 ID 的约定见 `desktop-shell` §6.4。

**布局与总则**

| ID | 规则（prototype.md） | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-01 | 工作区占满窗口，各栏内部滚动，输入区始终可见（§2） | `AppShell`（顶栏 / 左 / 中 / 右 / 状态栏网格）；`MessageList` 是对话视图唯一的滚动区，`Composer` 固定在底部 | 面板宽度（`prefs.layout`） | — | M1 |
| R-02 | 窄屏三栏依次堆叠（§2） | `AppShell` 响应式：≤ 900 px 单列、整页滚动、输入区仍可达 | — | — | M1 |
| R-03 | 每个项目各自保存视图、选中项、草稿、`@` 上下文、档位上限、过滤开关（§2） | `ProjectUiState` | §7.3 全表 | `ui_state_get/set/list`（防抖 500 ms [建议值]） | M1 |
| R-69 | 颜色从不单独表达含义；状态色与记号（§1.1） | `StatusDot` + `Tone` + 必带文字；`StatusPill`（"! 需要你 · 类型"） | — | 状态→`Tone`：`stage_status`、节点 `status`、工作流 `status`、open 的 `InboxItem`；"自愈中"取自当前工作流有 `running` 的 `heal` 节点（OQ-U9 / OQ-U18） | M1 |

**顶栏**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-04 | 项目 tab 切换；有待处理事项显示黄色数字徽标（§2.1） | `ProjectTabs`（`aria-current="page"`；徽标 `aria-label="需要你 n 项"`） | `currentProjectId` | `projects.list`；徽标 = `inbox.list?project_id&status=open` 的条数 + `inbox.*` 事件 | M1 |
| R-05 | "+" 添加项目 | `ProjectTabs` 末尾的禁用按钮，提示"项目在服务端绑定" | — | 无路由（api.md OQ-8；OQ-U17） | M1 |
| R-06 | "示例数据"标记（原型） | `DevBadge`，**仅在连接 Mock 服务的构建标志下显示**，生产不显示 | `build.mockServer` | — | M1 |
| R-07 | 本机·角色：审批设备 / 操作设备 / 只读设备（§2.1） | `RoleBadge` | `session.me.role/profile` | `me.get`；收到针对本机的 `device.updated` 时重新 `me.get` | M1 |
| R-08 | 单一连接指示，合并 mTLS 与事件流（§2.1、§2.8） | `ConnectionBadge`（顶栏）+ `StatusBar` 的同一状态 | `session.connection.phase` | `connection_state` + `state` 帧。文案：`live` →"{主机} · 已连接"；`reconnecting`/`connecting` →"连接中断 · 重连中"（灰）；`handshake_failed` →"无法建立安全连接"；`cert_expiring` →"证书将过期（{日期}）"；`cert_expired` →"证书已过期"；`revoked` →"此设备已被吊销"。主机名取自 `identity_status().server_host` | M1 |
| R-09 | "需要你 · n" 是**全部项目**的 open 数；点击打开浮层，按项目列出，每项可直接操作或跳转（§2.1） | `NeedsYouButton`（`aria-expanded`）+ `InboxOverlay`（`role="dialog"`，Esc 关闭，焦点回到触发按钮） | `overlay.inbox` | `needsYou(state).total`；浮层行为见 §7.6 | M1 最小 / M2 完整 |
| R-10 | 设置以全屏浮层打开，不卸载工作区；返回后草稿和选中项都在（§2.1） | `SettingsOverlay`（`position: fixed; inset: 0`，工作区组件保持挂载） | `overlay.settings` | — | M1 |

**左栏**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-11 | 项目信息：`owner/repo`、名称、默认上限、默认分支、"项目设置"链接（§2.2） | `ProjectHeader` | — | `projects.get`（`repo`、`default_branch`）；默认上限 = `levels` 生效值（operator 以上才能读 settings，viewer 显示"—"）；"项目设置"→ `overlay.settings.page = "project:<id>"` | M1 |
| R-12 | 进行中 Issue / PR / 工作树：需要你 → 运行中 → 其他；> 5 条折叠"显示全部（n）"；标题截断悬停全文（§2.2） | `WorkSection` ×3 + `WorkItem`（整行是 `<button>`，`aria-current`，`title`=完整标题） | `ui.showAll`；排序 `rank`：有 open 上报项 = 0；`stage_status = running` 且未暂停 = 1；其他 = 2，同级按交付 ID 倒序 [建议值] | `deliveries.list?active=true`（Issue 取各交付的 `issues[]`，PR 取 `pr`），`worktrees.list`。标题一律 `renderUntrusted(plain)` | M1 |
| R-13 | "! 需要你"药丸；PR 无待办时显示 CI 结果，CI 颜色只表示 CI（§2.2） | `WorkItem` 的 `StatusPill` / `CiLabel` | — | 药丸：`needsYou.byDelivery`；CI：`PullRequestRef.checks`——`failed > 0`"CI ✗"（橙）、`pending > 0`"CI 运行中"（蓝）、全空"无 CI"（灰）、否则"CI ✓"（绿） | M1 |
| R-14 | `main` 基线：点击后检视器显示分支保护、最近 CI、基于它的工作树数（§2.2） | `BaseRow` + `BaseInspector` | `ui.sel = {kind: base}` | `default_branch`、工作树计数（本地）；**分支保护与最近 CI 没有数据源**（OQ-U4），该两行显示"服务端未提供" | M2 |
| R-15 | "从分支开始任务…"：把上下文设为该分支并聚焦输入框（§2.2） | `BaseRow` 的按钮 | `ui.ctx` | `Ref` 没有分支种类（OQ-U6）；解决前按钮隐藏 | M2 |
| R-16 | 最近完成：已合并或已放弃的交付移到这里（§2.2） | `DoneSection` | — | `deliveries.list?stage=closed&stage=abandoned&limit=10`；文案"已合并 / 已放弃"取自 `stage` 与 `pr.state` | M1 |

**对话**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-17 | 用户消息：右对齐蓝底；下方 `@上下文 · 上限 X · 时间`；离线附"待发送"（§2.3） | `UserBubble` + `PendingBadge` | — | `Message(role=user)`；`@` 标签由 `refs` 推出（空 → "全项目"）；上限：`level_cap` 非空 → "上限 X"，空 → "上限 项目默认"；待发送 / 失败来自 `outbox` | M1 |
| R-18 | agent 消息：左对齐深底、2px 种类色边框；气泡内第一行种类文字；需要人时紧跟"! 需要你 · 类型"（§2.3、P-34） | `AgentBubble` | — | `agent_kind`：`planner` 大脑紫 / `worker` 干活青 / `utility` 打杂灰；**未知或 `null` → 中性灰边框 + 文字"agent"，不回退成大脑色**；药丸仅当本消息的 `cards` 引用了 open 的 `InboxItem`（`approval`→`payload.approval_id`，`escalation` / `choice`→`inbox_id`）。[conversation.md §7.7](conversation.md) 的临时约定：`decision_needed` →`choice`（带选项），`spec_needed` →`choice`（`options: []`，表示要自由文本答复），`escalation` / `budget_exhausted` / `security_finding` →`escalation`，所以这些消息都能挂药丸（OQ-U8） | M1 |
| R-19 | 后台事件一行（圆点 + 时间 + 摘要）可点击在检视器打开；连续多条折叠为"后台处理了 n 件事 ▸"（§2.3） | `EventRow`、`EventGroup`（`aria-expanded`） | `ui.expandedGroups` | `Message(role=event)`；连续 ≥ 2 条且中间无其他类型消息即成组；点击 → `select(refs[0] 或 delivery_id 的主项)`。**事件行没有状态字段**，圆点统一为中性（OQ-U9） | M1 |
| R-20 | 黄色"上次查看 hh:mm 之后"分隔线，紧跟"离开期间摘要"卡，每项可点（§2.3） | `LastSeenDivider`、`AwayChangesCard` | `ui.lastViewed` | "上次查看"的定义与摘要来源都未定（OQ-U10）。M2 临时：本地记录离开时的最新消息 ID，回来后在其后插分隔线；摘要卡只展示本地可确定计数（离开期间新增的事件行数、当前 open 上报项数），每项可点 | M2 |
| R-21 | 执行计划卡：标题 +"● 实时"，步骤随事件实时更新（§2.3） | `PlanCard` | — | `Card.plan` → `workflows.get(workflow_id)`；`node.*`、`workflow.*` 事件触发刷新；步骤 = 节点（`role` 的中文名 + `status` + `verdict`）；`summary` 当 `UntrustedText` | M1 |
| R-22 | 审批卡：显示"合并 PR #9 @ a1b2c3d"和实时状态；只有待签名时才有"审阅并签名…"，它只打开审批单（§2.3、P-33） | `ApprovalCard` | `overlay.sheet` | `Card.approval` → `approvals.get`；`approval.*` 事件；动作名 + 目标 + `subject_digest` 前 8 位取自 `Approval`，状态文案见 §7.8 | M1 |
| R-23 | 升档卡：原因 + [升到 X 并继续] [保持当前，缩小范围] [放弃]；处理后变为"已处理：…"（§2.3） | `ChoiceCard`（升档卡与二选一同一组件）；`Card.escalation` 另由 `EscalationCard` 渲染（原因 + 证据 `Ref`，§7.6） | — | [engine.md §7.11](engine.md) 的临时约定：升档是 `decision_needed`（`reason_code = level_cap_exceeded`，选项 `upgrade` / `keep_and_shrink` / `abandon`），由 `choice` 卡承载；界面不硬编码选项，按 `inbox.get` 的 `payload.options[]` 逐个渲染按钮（文案取服务端 `label`），点击 → `inbox.choose { option_id }`，`level_cap` 的修改由引擎在 `inbox.resolved` 后执行，界面不再调 `deliveries.patch`。处理后文案取 `inbox.resolved` 事件的 `data.option_id` 对应 `label`（conversation 会追加"已处理：{label}"事件行），快照里取不到时只显示"已处理"。按钮需 operator；手机档案不能做升档（P-35，OQ-U8） | M1 |
| R-24 | 二选一直接给按钮（如 [改为修复] [维持驳回]）（§2.3） | `ChoiceCard` | — | `Card.choice { inbox_id, options[] }`；`inbox.choose`（与收件箱同一个 `inbox_id`，P-40）；`options` 为空（`spec_needed`）时不渲染按钮，改为"回复补充…"（§7.6） | M1 |
| R-25 | 引用 chip 点击只在检视器查看，**不改变**输入区上下文（§2.3、P-32） | `RefChip` | `ui.sel` | 只调用 `select(ref)`；`Ref` → 选中项的映射在 §7.9；chip 来自 `refs[]`，**不**从正文文字里解析 | M1 |
| R-26 | "回复"：把上下文设为该消息所属交付并聚焦输入框（§2.3） | `AgentBubble` 右下角按钮 | `ui.ctx = {kind: delivery, id: message.delivery_id}`，`ui.view = chat`，`levelOverride = null` | 无 API；`delivery_id` 为空或设备只读时不显示 | M2 |
| R-27 | `⌘↵` / `Ctrl+Enter` 发送；输入法组合中按回车不发送（§2.3） | `Composer` | `ui.draft` | 平台键位取自 `device.platform`；组合态判定 `event.isComposing \|\| keyCode === 229`；发送 → §7.7 | M1 |
| R-28 | `@ 上下文 ▾`：全项目 + 进行中的 Issue / PR / 工作树，可用 × 清除；默认全项目，每次发送后复位（§2.3、P-32） | `ContextPicker`（`listbox`，选项为按钮） | `ui.ctx` | 选项来自缓存的活动交付；映射 `Ref`：issue → `{kind:"issue", repo, number}`，pr 同，worktree → `{kind:"worktree", id}`，回复 / 交付 → `{kind:"delivery", id}`。**客户端只发最小字段**（data-model §1.1） | M2 |
| R-29 | 档位**上限**选择器：标签"上限"，旁显示"≤ X"；默认项目默认上限；`@` 已有交付时显示该交付上限；发送后复位；方向键移动；悬停说明各档；None 提示"只问答，不建交付"（§2.3） | `LevelCapPicker`（`radiogroup`，单一 Tab 停靠点，方向键漫游并选中） | `ui.levelOverride`；显示值 = `levelOverride ?? ctx 交付的 level_cap ?? 项目默认` | 项目默认 = `levels.default_level` 生效值（§7.2 步骤 2）。各档说明是随包的静态说明文字（对应 architecture §8 表），不属于服务端枚举 | M2 |
| R-30 | `@` 的交付已有上限而用户改了：提示"PR #7 上限 Max → High；已触发的闸门不撤销"；文字档位与选择器冲突以选择器为准，agent 回执复述实际生效上限（§2.3） | `LevelWarning` | 比较 `ctx` 交付的 `level_cap` 与 `levelOverride` | 仅比较与提示；**客户端不解析正文里的档位**。回执就是 agent 消息，原样显示 | M2 |
| R-31 | 只读设备：输入与发送禁用，占位"此设备为只读"（§2.3） | `Composer` | `session.me.role === viewer` | 服务端同样 `403 forbidden_role`，界面不依赖它 | M1 |
| R-32 | 离线时发送按钮变"稍后发送"（§2.3） | `Composer` + `OutboxFlusher` | `outbox[project]` | 恢复（`phase = live`）后按序 flush；§7.7 | M1 |
| R-33 | 空项目：对话视图内显示空态卡（描述一个任务 / 从已有 Issue 开始 / 新建 Issue），输入区照常（§2.3、UX #6） | `EmptyProjectCard` | — | 判定：对话无消息且无活动交付。"从已有 Issue 开始"需要列出未绑定的 Issue，**没有数据源**（OQ-U6）：M1 先让它聚焦输入框并提示输入 `#编号`；"新建 Issue"预填草稿"新建 Issue："（由对话理解，非 UI 解析） | M1 |

**关系地图**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-34 | 三列：Issue → Pull Request → 工作树 / 沙盒；节点卡含状态点、编号、档位徽标、状态或"! 需要你"、标题（§2.4） | `MapView`、`MapNode`（`<button>`，`aria-current`） | `ui.sel` | 节点来自活动交付：`issues[]`、`pr`、`worktree_id`→`worktrees.get`；档位徽标 = `level_cap`（单色灰框，P-34）；沙盒编号无数据（OQ-U5），工作树节点用分支名 | M2 |
| R-35 | 实线 = closes / 分支；虚线 = refs 或"尚未开 PR"直连；橙色虚线 = 缺规格（§2.4） | `MapEdge`（SVG 折线；线型之外另有图例文字） | — | 同一交付内：`issues[]`→`pr` 为 closes（实线）；`pr`→工作树为分支（实线）；无 `pr` 时 Issue→工作树为"尚未开 PR"（虚线 + 标签）；有 `pr` 无 `issues` 为缺规格（橙色虚线，指向"无关联 Issue"占位节点）。**跨交付的 refs 没有数据源**（OQ-U7），暂不画 | M2 |
| R-36 | 选中节点只高亮它所属的交付；refs 半透明保留；其余淡化（§2.4） | `MapView` | `ui.sel` → 交付 ID | 高亮 = 该交付内的节点与边；其他节点 `opacity 0.35`，淡化后仍可点击 | M2 |
| R-37 | "只看需要你"过滤开关（§2.4） | `MapToolbar` 切换按钮（`aria-pressed`） | `ui.needsOnly` | 无 open 上报项的节点淡化（沿用原型；规模大时改为隐藏，OQ-U18） | M2 |
| R-38 | 自动布局；节点很多时按 Milestone 分组（§2.4） | `MapLayout`（纯函数，可单测） | — | M2：列式自动布局——交付按 `needsYou → running → 其他` 排序，每个交付占一个纵向带，带高 = max(Issue 数, 1)，同一交付的节点对齐；M4：按 `parent_id` 分组并画 `depends_on`。"Milestone"实体不存在（OQ-U7） | M2 / M4 |

**工作树视图**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-39 | 表格：工作树、关联、沙盒、写权持有者、状态；名称列是按钮，可键盘聚焦（§2.5） | `WorktreeTable`（语义 `<table>`，名称列 `<button>`） | `ui.sel` | `worktrees.list`；关联 = 所属交付的 Issue / PR 编号；写权：`lock.holder` `engine`→"前台 agent"、`guardian`→"后台守护"（**无法区分自愈 / review**，OQ-U5）、`human`→"你（至 hh:mm）"、`null`→"—"；沙盒列无数据（OQ-U5）；人工锁的取得 / 释放界面不在原型里（OQ-U21） | M2 |

**检视器**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-40 | 头部：编号 / 类型 / 状态（需要你时黄色）/ 标题 /"上限 X · 实际 Y"及自动加码原因 / 关联链；refs 单独一行"相关"（§2.6、UX #11） | `InspectorHeader` | `ui.sel` → 所属 `Delivery` | `level_cap`、`level_effective`；原因取最近的 `delivery.level_changed` 事件 `data`（schema 未出，OQ-U9）；关联链 = 同一交付内 Issue → PR → 分支，**按交付计算，不跨交付**；"相关"行无数据（OQ-U7） | M1 |
| R-41 | 操作：[在对话中引用]；[⋯] 暂停 / 继续、重试失败节点、放弃交付…（确认框写明关闭哪个 PR、保留哪个分支）（§2.6、P-37） | `InspectorActions`、`ActionMenu`（`role="menu"`）、`ConfirmAbandon`（`role="alertdialog"`） | 瞬时状态 | 引用：设 `ui.ctx` 为选中项的 `Ref`、切到对话、聚焦输入框。放弃：`deliveries.abandon`（带可选 `reason`）。暂停 / 继续 / 重试**没有路由**（OQ-U2）：菜单里不显示这三项。确认框的"关闭 PR、保留分支"依赖引擎对放弃的语义（OQ-U19） | M2 |
| R-42 | 需要你：该交付所有 open 的上报项，每项带操作按钮（§2.6） | `NeedsYouSection` + `InboxItemView`（与浮层同一组件） | — | `needsYou.byDelivery`；按钮见 §7.6 | M1 |
| R-43 | 交付阶段 7 段：规格 / 计划 / 实现 / 提出 / 评审 / 合并 / 关闭；已完成绿、当前强调色、未到灰、跳过的阶段画斜纹并写"跳过"（§2.6） | `StageBar`（每段带文字，非仅颜色） | — | `Delivery.stage`；`abandoned` 时整条置灰并标"已放弃"；"跳过"：`level_effective = Low` 时 `plan` 为跳过（data-model §4.1）；有 `delivery.stage_changed` 历史时按实际未访问的阶段补充 | M1 |
| R-44 | 当前阶段的图：只列当前阶段的节点；合并阶段改为"合并闸门"清单（§2.6） | `StageGraph`、`MergeGates` | — | `workflows.list?stage=` 取 `graph_rev` 最大的 → `workflows.get`；节点显示 `role` 中文名、`status`、`verdict`；`review` 阶段 Low 档无图，显示"等待 CI"+ `checks`。合并闸门见 §7.8 | M1（M3 加详情） |
| R-45 | 后台守护：守护处理过的事项与处置方式（§2.6） | `GuardianLog` | — | 该交付 `delivery_id` 的 `Message(role=event)`，最新 10 条（OQ-U9：没有结构化的"处置记录"） | M1 |
| R-46 | 只读设备上，操作按钮禁用（§2.6） | 所有写操作控件统一经 `can(action)` | `session.me.role` | §7.13 角色矩阵 | M1 |
| R-47 | "需要你"是同一个上报项，处理后对话与检视器同时消失（§2.6） | 所有表面从同一 `needsYou()` 投影 | — | `inbox.resolved` / `dismissed` 事件；乐观更新**不做**，以响应与事件为准 | M1 |
| R-57 | 未选中任何项：提示"选中只影响这里，不会改变消息发送的上下文"（§2.8） | `InspectorEmpty` | `ui.sel = null` | — | M1 |

**审批单**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-48 | 右侧抽屉，内容：动作、仓库、PR、完整 40 位 head SHA（等宽可复制）、改动（文件数 / 增删行 / 文件列表 / 策略命中 / GitHub diff 链接）、合并闸门、有效期、签名按钮（§2.7） | `ApprovalSheet`（`role="dialog" aria-modal="true"`，焦点陷阱，Esc 关闭，焦点回到触发处） | `overlay.sheet` | 全部"已签名区"字段来自 `ApprovalSheetModel.signed`；"未签名信息"区（PR 标题、闸门、改动）标注"未签名信息，仅供参考"并与已签名区视觉分离（api.md §6.5 规则 2）。有效期有三个不同的数（OQ-U20）：审批有效期 `Approval.expires_at`、本次确认窗口 `challenge_expires_at`，分别标注。**改动明细没有数据源**（OQ-U3）：先显示"服务端未提供改动明细"+ `open_github {kind: commit, sha}` 链接；合并方法（如 squash）不在签名载荷里，不得写进已签名区 | M1 |
| R-49 | 打开审批单时向服务端取最新状态；签名载荷使用审批单上显示的 SHA（§2.7） | 同上 | 同上 | 打开即 `approvalPrepare`（含 `GET approval` + `GET challenge` + 自检）。challenge 到期（默认 120 s）→ 面板顶部"已过期 · 重新载入"，重新 `prepare` 后**重新显示**，用户须重新核对 | M1 |
| R-50 | 禁用签名并写明原因：连接中断；本机不是审批设备（提示去哪台设备）；GitHub 侧闸门未满足；审批已不在待签状态（§2.7、P-33） | `SignGate`（按钮 `disabled` + 原因文字 `role="status"`） | — | `signAvailability()`，§7.8 | M1 |
| R-51 | 签名后审批卡依次显示：已签名 · 排队 merge → 已合并 → Issue 已关闭 · 交付完成；对话追加对应事件行（§2.7） | `ApprovalProgress`（卡 / 单 / 检视器共用） | — | `approvals.get` 状态 + `outbox.*` 事件 + `Delivery.pr.state` + `Delivery.stage`；**对话里的事件行是服务端消息，客户端不自行拼造** | M1 |
| R-52 | 新推送使审批失效：所有入口显示"已失效（旧 SHA → 新 SHA），重新评审中"，按钮消失（§2.7） | `ApprovalProgress` | — | `approval.invalidated` 事件；旧 = `Approval.subject_digest`，新 = `Delivery.pr.head_sha`；审批单打开时同步变灰并显示横幅，`approvalDiscard` | M1 |
| R-53 | "拒绝"把交付退回评审（§2.7） | `DenyButton` | — | `approvals.deny { reason? }`（带 `Idempotency-Key`）；是否"退回评审"由服务端决定，界面只显示 `Approval.status = denied` 与后续阶段变化 | M1 |

**状态与异常、无障碍**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-54 | 连接中断：顶栏与状态栏变灰"重连中"；对话上方横幅"界面可能不是最新；签名已禁用；新消息会在恢复后发送"；恢复后提示已补齐的事件数（§2.8） | `OfflineBanner`（`role="status"`） | `session.connection` | `state` 帧；补齐数见 §7.2 | M1 |
| R-55 | 外部变化（GitHub 上合并 / 关闭、Outbox 失败、策略拒绝、对账偏差）以事件行呈现；相关上报项自动关闭并注明"已在 GitHub 上处理"（§2.8） | `EventRow`、`InboxItemView` | — | 事件行是服务端消息；上报项自动关闭来自 `inbox.resolved`；"已在 GitHub 上处理"的注明需要服务端在 `data` 或消息里给出原因（OQ-U9），否则显示"已关闭" | M1 |
| R-56 | 证书将过期 / 已吊销：顶栏连接状态显示对应文字（§2.8） | `ConnectionBadge` | `ConnectionState` | `revoked` / `cert_expired` 时出现阻塞的配对向导（§7.12） | M1 |
| R-58 | 顶部"跳到输入框"跳转链接（§2.9） | `SkipLink` | — | 聚焦 `#composer` | M1 |
| R-59 | 所有可点击元素都是真实按钮，有可见焦点框（§2.9） | 全局样式 + lint（禁止 `div onClick`） | — | — | M1 |
| R-60 | 选中项 `aria-current`，视图 tab `aria-selected`，档位选择器单一 Tab 停靠点（§2.9） | `ViewTabs`（`role="tablist"`）、各列表项、`LevelCapPicker` | — | — | M1 / M2 |
| R-70 | 状态栏：连接状态、最近一条事件（可点）、运行中沙盒数（§2） | `StatusBar` | — | 最近事件 = 当前项目最后一条 `Message(role=event)`，点击 → 切到对话并滚动到它；沙盒数无数据源（OQ-U5），临时取已加载工作流里 `running` 且 `exec ≠ direct` 的节点数，数据不全时不显示该项 | M1 |

**设置**

| ID | 规则 | 组件 | 状态 | 数据 / 调用 | M |
| --- | --- | --- | --- | --- | --- |
| R-62 | 以全屏浮层打开，"返回工作区"回到原状态；有未保存改动时返回先确认（§3） | `SettingsOverlay`、`ConfirmLeaveBar`（`role="alertdialog"`） | `overlay.settings`、页内 `dirty` | `dirty` = 草稿与已加载文档的深度比较 | M1 |
| R-63 | 分组导航：服务端（模型、档位与预算、GitHub App、沙盒与出网、危险动作策略、后台守护、通知）/ 项目（每个项目一页）/ 本机（设备与证书、外观）（§3，P-31） | `SettingsNav` | `overlay.settings.page` | 条目由 config.md §13 的里程碑决定：未交付的页显示"尚未提供"并链到 config.md；viewer 看不到设置入口（api.md §2.2） | M1 起 |
| R-64 | 模型页：三张卡（大脑 / 干活 / 打杂），含用途说明、模型、推理深度；被项目覆盖时注明"被 n 个项目覆盖：…"（§3） | `SettingsModels`、`ModelTierCard`、`ProviderCredentialField` | 页内草稿 | `settings.get models` + `settings.catalog`。模型下拉 = `catalog.models.capabilities` 的键（`worker` / `planner` 过滤掉不支持工具调用的）；推理深度 = 所选模型的 `reasoning_efforts`，planner / worker 另有"随档位"（`by_level`）。"被 n 个项目覆盖"：对每个项目 `project_settings.get models`（n+1 请求，仅在该页打开时；OQ-U13）。凭据输入框只写不读，已设置显示 `present`（config.md §2.4） | M1 |
| R-65 | "每个档位里三类模型各做什么"表，与 architecture 的角色表一致（§3） | `LevelModelTable` | — | 随包静态说明（来源 architecture §8、§9），带一个快照测试对比；`catalog` 不提供这张表（OQ-U13） | M1 |
| R-66 | 保存：需要审批设备签名；保存后提示新配置从运行中节点的下一个节点开始生效（§3、Settings 原型） | `SaveBar` | 页内 `saving` / `lastSaved { rev }` | **以服务端为准**：`guardedWritePrepare` 返回的 `requires_signature` 决定是否出现"此改动需要审批设备签名"；中性 / 收紧类不签名（config.md §2.5，与 prototype.md §3 的"保存需要签名"字面不同，见 OQ-U13）。保存成功文案按 config.md §2.6：判定类键"下一次判定起"，worker 配置"下一个尝试起"；并用 `settings.status` 的 `applied_rev` 显示"某模块尚未应用 rev N" | M1 |
| R-67 | 项目页：每个项目一页，只列可覆盖的键；继承全局 / 覆盖；安全相关的键标"只能收紧"（§3） | `SettingsProject` | 页内草稿 | 行集合来自 config.md §2.2 的覆盖矩阵（schema 注解 + `catalog`）；继承值 = `project_settings.get ?view=effective`（灰显，附 `clamped[]`）；覆盖 = `guardedWritePrepare{settings_put, scope: project}`；回到继承 = `settings_delete`。"只能收紧"的输入被约束在不宽于全局的范围，仍以服务端 `override_loosens_global` 为准 | M2 |
| R-68 | 本机：设备与证书、外观（§3、config.md §13） | `SettingsDevices`、`SettingsAppearance` | — | 设备：`devices.list` / `devices.rename`；改角色 `guarded_write{device_role}`、吊销 `guarded_write{device_revoke}`；本机证书信息取 `identity_status()`；"配对令牌发放"无路由（OQ-U14）。外观：`prefs_set`（主题 / 密度 / 语言 / 通知开关 / 面板宽度），不含凭据与配置键 | M2（建议） |

### 7.5 事件 → 状态更新

默认策略：事件只把资源标记为"需刷新"，由调度器重新 GET（`subject` 是被改动的实体，`correlation_id` 通常是所属交付，api.md §5.3 与 data-model §5.1 示例）。`data` 的字段以 `schemas/events/*.json` 为准，出现后可逐步改为直接打补丁。下表给出每类事件刷新什么、影响哪些界面；**未知事件类型一律忽略并推进游标**。

| 事件 | 刷新 | 界面影响 |
| --- | --- | --- |
| `project.added` / `project.updated` | `projects` | tab、左栏项目信息 |
| `settings.updated` | 若设置浮层打开且 resource 相同：标记该页"已被修改"；`levels` 变化时重取默认上限；`settings.status` | 设置页冲突横幅；输入区默认上限 |
| `conversation.created` | `conversations?project_id` | — |
| `message.posted` | 事件含完整 `Message` 时直接合并，否则拉最新一页合并（按 `id` 去重，包括本机刚发的那条） | 对话；自动滚动；状态栏最近事件 |
| `delivery.created` / `delivery.stage_changed` / `delivery.level_changed` / `delivery.abandoned` | `delivery:{id}`；`stage_changed` 同时刷新其当前阶段工作流 | 左栏、地图、检视器阶段条 |
| `workflow.*`、`node.*`、`attempt.*` | `workflows:{correlation_id}` / `workflow:{id}` | 计划卡、检视器的图、"自愈中"状态 |
| `attempt.progress` | 不刷新；若检视器正显示该尝试，更新进度文字 | 仅展示（data-model 注明不驱动状态机） |
| `patch.validated` / `patch.rejected` / `policy.denied` | 无（服务端会同时发事件行消息） | — |
| `sandbox.created` / `sandbox.destroyed` | 无 | 状态栏沙盒数不依赖它（OQ-U5） |
| `worktree.*` | `worktree:{id}`、`worktrees:{project}` | 工作树列表 / 视图、写权列 |
| `inbox.raised` / `inbox.resolved` / `inbox.dismissed` | `inbox_item:{id}`、`inbox:{project}` | 所有"需要你"表面（§7.6）；`resolved` 的 `data.option_id` 缓存，用于"已处理：…" |
| `approval.requested` / `granted` / `consumed` / `invalidated` / `expired` / `denied` | `approval:{id}` | 审批卡、收件箱、检视器、审批单；`invalidated` 使打开的审批单禁用并 `approvalDiscard` |
| `outbox.*` | 无（用于审批进度："排队 merge"） | `ApprovalProgress` |
| `forge.synced` / `forge.<kind>` / `forge.drift_detected` | `delivery:{correlation_id}`（PR 快照）、`approval`（若有待签） | 左栏 CI、检视器闸门、签名可用性 |
| `budget.charged` / `budget.exhausted` | 预算面板打开时 `deliveries.budget` | M3 预算面板 |
| `device.paired` / `device.updated` / `device.revoked` | `devices`；针对本机的 `updated` → `me.get`；`revoked` 且是本机 → 等待外壳的 `revoked` 状态 | 角色徽标、设备页 |

### 7.6 "需要你"：单一投影

`needsYou(state)` 从缓存的 `inbox`（`status = open`）计算，所有表面只读它，**不各自存一份**：

| 表面 | 内容 |
| --- | --- |
| tab 徽标 | `byProject[p].length` |
| 顶栏"需要你 · n" | `total`（全部项目） |
| 左栏 / 地图节点 | 交付的 `delivery_id` 命中 → "! 需要你"药丸；地图节点状态文字改为"! 需要你" |
| 气泡 | 见 R-18：卡片引用了 open 的上报项才有药丸 |
| 检视器"需要你"区 | `byDelivery[d]` 的全部项 |
| 收件箱浮层 | 按项目分组，每项一个 `InboxItemView` |

`InboxItemView` 按 `kind` 渲染（浮层与检视器共用）。`summary` 与所有 `UntrustedText` 一律按纯文本渲染；未知 `kind` 只显示 `summary` + "忽略"。

| `kind` | 内容 | 操作（均需 operator，只读设备禁用） |
| --- | --- | --- |
| `approval_needed` | 动作 + 目标 + `subject_digest` 前 8 位（取自 `Approval`） | "审阅并签名…" → 打开审批单（**不直接签名**）；审批只经审批路由处理：`choose` 对它返回 `409`（api.md §4.8），界面也不提供 `dismiss` |
| `decision_needed` | `payload.options[]`、`reason_code` | 每个选项一个按钮 → `inbox.choose { option_id }`（带 `Idempotency-Key`）；`primary` 选项强调色。升档、放弃 / 重新规划（gate `REJECT`）、GitHub 状态外部改变后的继续 / 放弃，都是这一类（engine.md §7.11） |
| `escalation` | `reason_code`、证据 `Ref` 链接 | data-model 的 `escalation` payload 没有 `options[]`（engine.md 因此把"需要按钮"的情形都放进 `decision_needed`，OQ-U8）；若服务端仍带了 `options[]` 就同上渲染，否则只给"忽略"（`inbox.dismiss`，对阻塞项不解除暂停，界面文案写明"忽略不会继续任务"） |
| `spec_needed` | `missing[] { aspect, question }` 逐条列出 | "回复补充…" → 切到该项目对话，`ctx = {kind: delivery, id: delivery_id}`，聚焦输入框（自由文本答复走发消息，api.md §4.8） |
| `budget_exhausted` | `dimension` / `limit` / `used` | "忽略"（没有追加预算的路由，config.md OQ-10） |
| `security_finding` | `finding.severity` / `category` / `title`（`UntrustedText`）、来源 `Ref` | "查看"（选中来源）；"忽略…"需二次确认 |

浮层中点击条目标题行：切换到该项目、`sel` 设为该交付的主项（有 PR 取 PR，否则 Issue）、关闭浮层。

### 7.7 发送：上下文、上限与离线

- **选中不影响发送**（P-32）：`sel` 与 `ctx` 是两个独立字段，没有任何代码路径从 `sel` 写 `ctx`。`ctx` 只能由：`@` 选择器、"回复"、"在对话中引用"、（M2+）"从分支开始任务…"设置，由"×"、发送、切换 `ctx` 清除 / 复位。
- **`buildSend`**：`{ text, refs: ctx ? [toRef(ctx)] : [], level_cap: levelOverride ?? 省略 }`。请求体只允许这三个键（api.md §4.5，I7）。`level_cap` 省略 = 服务端用 `levels.default_level` 的生效值。**仅当用户显式改过选择器才发送 `level_cap`**；`Message.level_cap` 对已有交付的语义（是否会改变该交付上限）骨架没定（OQ-U11），界面只负责提示，不假设。
- **发送序列**：生成 `idempotency_key` → 入队（`state = queued`，立即渲染为"待发送"气泡）→ `phase = live` 且队首时 `messages.post` → `201`：用返回的 `Message` 替换本地项（之后 `message.posted` 事件按 `id` 去重）→ 复位 `draft` / `ctx` / `levelOverride`（入队即复位，失败不回填 `ctx`，失败项保留原文本供"编辑后重发"）。
- **失败处理**：网络错误 / `5xx` / `429` / `idempotency_in_progress` → 保持 `queued`，退避后沿用同一 key 重试；`403 forbidden_role` / `422 validation_failed`（含引用解析失败）/ `413` → `failed(code)`，显示本地化原因与"编辑后重发 / 删除"；`401 device_revoked` 交给外壳的状态机。
- **离线**：`phase ≠ live` 时发送按钮文案为"稍后发送"，消息入队并附"待发送"；恢复后按项目内 FIFO、一次一条地发出。队列经 `ui_state_set` 持久化（OQ-S6、OQ-U15）。
- **只读设备**：输入框与按钮禁用，占位"此设备为只读，不能发消息"；设备角色在运行中降为 viewer 时，队列里的项以 `forbidden_role` 失败。
- **自动滚动**：用户在底部时新消息自动滚到底；否则出现"新消息 ↓"，不抢焦点（UX #22）。向上滚动时用 `next_cursor` 分页取更早消息并保持滚动锚点。内存中每对话保留 ≤ 1000 条 [建议值]，虚拟列表渲染。

### 7.8 审批单与签名可用性

**一个审批，多处显示**：`ApprovalCard`、收件箱 / 检视器的 `approval_needed` 项、`ApprovalSheet` 都读同一个 `approvals[id]`，状态文案由一个函数生成：

| `Approval.status` | 文案（`Tone`） |
| --- | --- |
| `pending` | 待签名（`need`）；PR 类且 `Delivery.pr.head_sha ≠ subject_digest` 时：代码已更新（旧→新），等待服务端使其失效 / 重新评审（`idle`），不给按钮 |
| `granted` | 已签名 · 排队 merge（`run`） |
| `consumed` | `pr.state = merged` →"已合并 {sha8}"（`ok`）；Issue 全部关闭且 `stage = closed` →"Issue 已关闭 · 交付完成"（`ok`）；否则"已执行，等待 GitHub 确认"（`run`） |
| `invalidated` | 已失效（{旧 sha8} → {新 sha8}），重新评审中（`idle`），按钮消失 |
| `expired` | 已过期（`idle`） |
| `denied` | 已拒绝，回到评审（`idle`） |

**`signAvailability`**（第一个命中者生效，文案取自 prototype.md §2.7 与 Main 原型）：

| 优先级 | 条件 | `reason` | 文案 |
| --- | --- | --- | --- |
| 1 | `connection.phase ≠ live` | `offline` | 连接中断，暂不能签名。 |
| 2 | `me.profile ≠ desktop` 或 `me.role ≠ approver` | `not_approver` | 此设备不能审批：请在审批设备（{设备名}）上签名。设备名取 `devices.list` 里其他 `active` 的 approver，没有则写"审批设备" |
| 3 | `approval.status ≠ pending`（含 `invalidated`） | `not_pending` | 此审批已不在待签状态（{最终状态}）。 |
| 4 | PR 类动作：`pr.head_sha ≠ approval.subject_digest` | `stale` | 代码已更新，审批即将失效。 |
| 5 | GitHub 侧闸门未满足（下） | `gates_unmet` | 先在 GitHub 上完成必需的 review 与 checks，才能签名。（缺哪项逐条列出） |
| 6 | `sheet` 缺失、challenge 已过期或自检失败 | `expired` / `self_check_failed` | 已过期 · 重新载入 / 服务端数据不一致，无法签名。 |

**GitHub 闸门（M1 临时规则，OQ-U4）**：未满足 = `!checks.required_ok` 或 `mergeable_state ∈ {dirty, blocked, behind, draft, unknown}` 或 `pr.draft` 或 `pr.state ≠ open` 或 `review_state = changes_requested`；`clean` / `unstable` / `has_hooks` 视为满足。快照 `synced_at` 超过 10 分钟 [建议值] 时在闸门清单旁标"快照 hh:mm"，**不据此放行或阻止**（服务端在 `grant` 与消费时复核）。清单四行与原型一致：必需 checks @SHA、GitHub review（未 approve 时给"去 GitHub 上 approve ↗"，`open_github`）、merge queue（`Project.merge_queue_enabled` 与 `in_merge_queue`）、Gantry 签名（本审批状态）。

**签名按钮**：文案 M1 为"签名批准（系统确认）"；M2 起"用 Touch ID 签名批准" / "用 Windows Hello 签名批准"（按平台）。点击只调用 `approvalSign({ preparedId })`；响应前按钮置为"等待系统确认…"并禁用；响应后**只按返回的 `Approval.status` 更新**（不做乐观显示）；`409 stale_head` 显示"代码在你确认期间已更新，审批已失效"。

### 7.9 检视器的数据来源与选中映射

- `sel` → 交付：`issue` / `pr` 在缓存的活动交付与最近完成里按 `repo` + `number` 查找，`worktree` 按 `id`，`base` 单独处理。找不到（例如旧引用）→ 检视器显示"该项不在进行中"并给 GitHub 跳转（`open_github`）。
- `Ref` → 选中：`delivery` → 该交付的主项（PR，否则第一个 Issue，否则工作树）。
- 当前阶段的图：`workflows.list?stage=<stage>` 取 `graph_rev` 最大的；没有（Low 档的 review、spec）→ 显示阶段说明与等待对象（CI / 审批 / 用户）。节点中文名：`plan` 计划、`build` 实现、`verify` 验证、`single` 单节点实现、`review:<视角>` 评审·视角、`reasoner` 预演、`arbiter` 裁决、`summarize` 汇总、`admission` 需求准入、`heal` 自愈、`answer` 问答、`intent` 意图、`classify` 分类；未知 `role` 显示原值。
- 尝试结果内容的读取路由未定（api.md OQ-11），检视器只显示尝试的状态、时间、`failure_class`。

### 7.10 关系地图布局（M2）

`layoutMap(deliveries, worktrees, needsYou) → { nodes, edges, size }` 是纯函数：列 x 固定（Issue / PR / 工作树），交付按排序后纵向堆叠，每个交付一个带；边为带圆角的正交折线，画在节点之下；每种线型同时在图例里写文字。键盘：节点是按钮，Tab 顺序 = 先按交付、再 Issue → PR → 工作树；选中后 Esc 清除选中。节点数 > 60 时开启"只看需要你"的默认建议提示 [建议值]。

### 7.11 设置浮层

- **表单来源**：`schemas/config/<resource>.json`（构建期打包）+ `catalog`（运行期）。通用 `SchemaForm` 支持：对象、枚举、整数（带 `minimum` / `maximum`）、布尔、字符串、以 id 为键的对象表（增删行）、字符串数组、`SecretRef`（`x-gantry-secret`）。页面布局由每页一个小的布局描述控制；M1 的模型页用定制布局，其余回退到通用渲染。
- **加载**：`settings.get`（取 `ETag` / `rev`），项目页另取 `?view=effective`。**视图只读取值 `value`**；`SecretRef.value` 永远不会出现，已设置显示"已设置 ●●●● [更换]"。
- **保存流程**：本地草稿 → `guardedWritePrepare`（`dry_run`）→ 有 `errors` 则按 `pointer`（相对 `value`）标到对应字段，`code` 本地化；有 `warnings` 显示黄色提示但可继续；`relaxes[]` 非空时在保存条写"此改动放宽了安全限制"；`requires_signature` 为真时显示"此改动需要审批设备签名"，本机不是 approver 则**禁用保存**并说明（跨设备签名的限制见 OQ-S7）→ `guardedWriteSubmit` → 成功：更新 `rev`、提示"已保存（rev N）"和生效说明；`412 precondition_failed`：横幅"此页已被其他设备修改"，提供"载入最新值（丢弃我的改动）"，**不自动合并**。
- **秘密值**：只存在于输入框组件的局部状态里，`prepare` 调用后立即清空输入框并把"待写入"标记为占位；不进入 store、不写 `ui_state`、不进日志；组件卸载时清空。
- **未保存拦截**（R-62）与"返回工作区"保持工作区状态不变。
- **生效提示**：`settings.status` 轮询仅在刚保存后进行（退避，直到 `applied_rev ≥ rev` 或 30 s [建议值]）；之后依靠 `settings.updated` 事件。
- **没有契约的控件**：原型的"测试连接"没有路由（OQ-U13），不渲染。

### 7.12 配对向导与设备页

- **配对向导**（原型未覆盖，prototype.md §5）：当 `identity_status.state ∈ {unpaired, revoked, cert_expired}` 阻塞整个工作区。步骤：① 粘贴服务端配对串 + 设备名 → `pairingBegin`；② 显示服务端地址、CA 指纹（分组显示）与大号 6 位确认码，提示核对服务端控制台，按钮"一致 / 不一致" → `pairingConfirm`；③ 完成后进入水合。`revoked` / `cert_expired` 文案不同："此设备已被吊销 / 证书已过期，需要重新配对"，附 `identityReset`。扫码入口不提供（OQ-S3）。
- **设备页**（M2 建议）：`devices.list`（含已吊销）、重命名（operator）、改角色与吊销（approver + 签名；不能改自己，附 `409 self_role_change` 提示）、本机证书序列号与到期时间。

### 7.13 角色门槛矩阵

界面统一用 `can(action)`；服务端仍会 `403`（api.md §2），界面只为体验。

| 动作 | 最低角色 | 禁用 / 隐藏表现 |
| --- | --- | --- |
| 浏览一切（含审批只读、设备列表） | viewer | — |
| 发消息、`choose`、`dismiss`、放弃交付、人工锁、重命名设备 | operator | 按钮禁用，悬停说明"此设备为只读" |
| 读设置 | operator | viewer 隐藏设置入口与"默认上限"数值 |
| 写设置（中性 / 收紧） | operator | — |
| 写设置（放宽、仅 approver 键、带 secret `value`）、改角色、吊销、签名、拒绝审批 | approver + 桌面档案 | 保存 / 签名禁用并说明原因 |

### 7.14 键盘与可访问性

- 全部交互元素为真实 `<button>` / `<a>` / `<textarea>`；可见焦点框；对话框有焦点陷阱并在关闭时还原焦点；Esc 关闭浮层 / 抽屉 / 菜单。
- `aria-current`（选中项、当前 tab）、`aria-selected`（视图 tab）、`aria-expanded`（折叠组、菜单、浮层按钮）、`aria-pressed`（地图过滤）、`radiogroup`（档位）。
- 动态区：断线横幅与"已补齐 n 件事件"用 `role="status"`；新的"需要你"用礼貌的 `aria-live` 提示一次，不朗读消息正文。
- 颜色不单独表达含义（`StatusDot` 必带文字）；对比度按 WCAG AA 检查令牌；尊重系统"减少动态效果"。
- 文案目录化：所有界面字符串与 `problem.code` 的本地化走 `i18n/`；默认 `zh-CN`，其他语言未排期（OQ-U16）。

## 8. 不变式与安全要求

对应 [architecture.md §5](../architecture.md#5-信任边界与不变式)。

### 8.1 不变式如何保证

| 不变式 | 本模块的做法 |
| --- | --- |
| I3 放行唯一决策点 | 界面**没有**放行逻辑：审批只展示状态，签名只在审批单里发起，且只传 `prepared_id`；不存在"批准"按钮之外的签名入口（R-22/R-23/R-42 的入口只打开审批单）；`signAvailability` 只决定禁用，外壳与服务端都会独立复核 |
| I4 状态各有事实源 | 交付事实（PR 状态、checks、review）显示 GitHub 快照并保留 `synced_at`；本地不推断"已合并"，只信 `Delivery.pr.state`；冲突以服务端 / 事件为准，不做乐观更新 |
| I5 外部文本不可信 | §8.2 渲染规则；Chrome 元素（药丸、审批卡、签名区）只由结构化字段生成，**绝不**因正文里出现某些字样而生成 |
| I6 客户端不执行、不持凭据 | 不引用任何网络 API（CSP 也不允许）；没有执行代码的功能；设置里的秘密值只在输入框局部状态中存在，提交即清；不显示、不缓存任何凭据 |
| I7 配置与工作分离 | 工作区没有模型、预算、策略控件；消息体只含 `text` / `level_cap` / `refs`；`level_cap` 是**任务级**上限（api.md §8.2）；设置只在设置浮层；**不把自然语言解析成控制动作**；档位上限选择器的说明文字不得暗示它修改全局配置 |
| D-09 / D-12 | 模型配置只在设置页；agent 气泡用边框颜色与种类文字区分，不显示头像 |

### 8.2 不可信内容渲染规则

| 数据 | 渲染方式 |
| --- | --- |
| `UntrustedText`（Issue / PR 标题、卡片 `summary`、`finding.title`…） | 纯文本；单行场景 CSS 截断 + `title` 悬停全文；`truncated = true` 追加"…（已截断）" |
| `Message.text`，`role = user` | 纯文本（保留换行），不渲染 Markdown |
| `Message.text`，`role = agent` / `event` | **受限 Markdown**（下） |
| `InboxItem.summary`、`Delivery.title`、分支名、工作树名、仓库名 | 纯文本；分支名等宽 |
| `Approval` 中的 `action` / `target` / `subject_digest`、`ApprovalSheetModel.signed` | 纯文本 / 等宽，已签名区；digest 前 12 位醒目 |
| `problem.detail` | **不展示**（外壳也已去掉）；按 `code` 本地化 |
| 任何其他字符串 | 纯文本。禁止把字符串当 HTML、URL、CSS、选择器、模板使用 |

**受限 Markdown**：解析成语法树后映射到自己的组件，不经 HTML 字符串。允许：段落、换行、强调 / 加粗、行内代码、围栏代码块（不做语法高亮的脚本执行）、引用块、有序 / 无序列表、链接。标题降级为加粗段落。**不允许**：原始 HTML（作为转义文本显示）、图片（只显示 `[图片: alt]`，不发请求）、表格、脚注、自动链接（只有显式 `[文字](https://…)`）。链接只允许 `https`（`http` 与其他协议按普通文本显示，不可点击）；渲染为"文字 · 目标主机名"，主机名**始终可见**，不依赖点击（国际化域名同时显示 punycode；链接文字本身像 URL 但与真实目标主机不符时突出真实主机）；点击调用 `open_external`，由外壳（可信）校验并对非 `github.com` 主机弹出原生确认框，界面不再自弹第二个确认框。

**文本清理**（所有上述渲染共用）：去除 C0 / C1 控制字符（保留换行与制表）；双向控制字符（U+202A–U+202E、U+2066–U+2069）显示为可见转义；零宽字符（U+200B–U+200D、U+2060、U+FEFF）在标识符、SHA、主机名、按钮标签里显示为可见标记；超出 `Message.text` 上限（16 KiB）的部分折叠为"展开"。

**防伪造**：药丸、卡片、审批单、"已签名区"的内容只来自结构化字段（`cards`、`InboxItem`、`ApprovalSheetModel.signed`）；`#5` / `PR #9` 形式的引用 chip 只来自 `refs[]`，不从正文里匹配；正文里写"! 需要你"不会产生药丸。**构建约束**：lint 禁止 `innerHTML`、`outerHTML`、`insertAdjacentHTML`、`document.write`、框架的 raw-HTML 属性、`eval` / `new Function`、`javascript:` URL、动态 `<script>` / `<iframe>`；CSP 的 `require-trusted-types-for 'script'` 作为第二道（desktop-shell §7.6）。

## 9. 错误与恢复

界面统一按 `problem.code` 分支，文案走 `i18n/`，`detail` 不展示（api.md §3.1）。

| 失败 | 处理 |
| --- | --- |
| `ApiResult.network = timeout / offline / aborted` | 读：退避重试，列表显示"刷新失败 · 重试"；写：沿用 `Idempotency-Key` 重试（§7.7）；同时依赖外壳的连接状态机 |
| `forbidden_role` | 禁用对应入口并写"需要 {required_role} 角色"；消息发送失败项标 `failed` |
| `forbidden_profile` | 客户端 bug：记日志，不重试 |
| `signature_required`（设置、设备写入） | 不应发生（界面总是走 `guarded_write`）；发生则视为外壳 bug 并显示通用错误 |
| `signature_invalid` / `challenge_invalid` | 提示并"重新载入"审批单（重新 `prepare`） |
| `stale_head` | "代码在你确认期间已更新，审批已失效"；关闭审批单，刷新审批与 PR |
| `approval_not_pending` / `approval_mismatch` | 刷新审批并显示最终状态；设置 / 设备写入：回到保存条，提示重试 |
| `conflict`（`reason`：`invalid_state`、`self_role_change`、`worktree_locked`…） | 刷新对应实体，按 `reason` 本地化提示（`worktree_locked` 附持有者与 `lease_until`） |
| `validation_failed` | 表单：`errors[].pointer` 标到字段；消息：显示通用"无法解析引用 / 内容过长"，保留文本 |
| `precondition_failed`（`412`）/ `precondition_required` | 设置冲突横幅；后者是 bug |
| `payload_too_large` / `unsupported_media_type` / `idempotency_key_reuse` / `invalid_request` | 记日志，显示通用错误；不重试（后两者是 bug） |
| `rate_limited` / `unavailable` | 按 `Retry-After` 退避；对话顶部提示"服务端繁忙，稍后自动重试" |
| `internal` | 读：退避重试；写：同 key 重试 |
| `cursor_ahead` / `cursor_expired`（经 `resync_required`） | `resync()`，保留界面状态与待发送队列，顶部提示"已重新同步" |
| `device_revoked`（经外壳 `revoked`） | 阻塞的配对向导，清空内存缓存；待发送队列保留到重新配对后询问是否丢弃（旧服务端的项目 ID 可能不同，外壳在 `identity_reset` 时清除 `ui_state`） |
| 事件应用抛错（未知形状、缺字段） | 该事件对应资源改为"重新 GET"；仍推进游标；记诊断计数。**绝不因单个事件让整个界面崩溃** |
| 资源在选中后消失（交付被清理） | `sel` 置空，检视器提示 |
| `ui_state` / `prefs` 读取失败 | 使用默认值，不弹错 |
| 渲染异常 | 组件级错误边界：只替换出错的卡片 / 行为"此内容无法显示"，其余照常 |
| 时钟偏差告警 | 顶栏 tooltip 提示"本机时间与服务端相差 {n} 秒"，审批倒计时用外壳校正后的值 |

## 10. 配置

本模块只有**本机**配置，不是服务端 settings（I7 不适用）。偏好经 `prefs_set`，构建期常量写在代码里。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| 主题 / 密度 / 语言 | 深色 / 紧凑 / `zh-CN` | M1 只有深色；`prefs` |
| 面板宽度、折叠 | 左栏约 240 px、检视器约 300 px | prototype.md §2；`prefs.layout` |
| 本机系统通知开关 | 开 | 只决定本机是否弹窗（config.md §13.2） |
| 单页消息数 / 内存保留 | 50 / 1000 | [建议值] |
| 左栏折叠阈值 | 5 | prototype.md §2.2 |
| 刷新合并窗口 / 并发 | 100 ms / 4 | [建议值] |
| 水合：后台预取工作流数 / 并发 | 20 / 2 | [建议值]（OQ-U18） |
| `ui_state` 防抖 | 500 ms | [建议值] |
| 快照陈旧提示 | 10 分钟 | PR 快照 `synced_at`；[建议值] |
| 设置保存后 `status` 轮询 | 退避至 30 s | [建议值] |
| 构建标志 `mockServer` | 生产 `false` | 控制"示例数据"徽标（R-06） |

## 11. 可观测性

- 本模块**不发事件**。写请求在服务端产生的事件见 `desktop-shell` §11。
- **日志**：通过外壳的诊断通道，界面只记录结构化的"类别 + 代码"（路由 ID、`problem.code`、事件类型、`resync` 次数），**不记录任何 `UntrustedText`、消息正文、`SecretRef.value`**。
- **计数器**（随 `diagnostics_snapshot` 导出，进程内）：`events_applied`、`events_skipped_watermark`、`events_unknown_type`、`refetch_issued{resource}`、`resync_count`、`render_errors`、`outbox_failed{code}`、`sign_blocked{reason}`。
- **性能预算**（Mock 上测量，[建议值]）：冷启动水合（50 个活动交付）≤ 2 s；事件突发 500 条 / s 时主线程单次批处理 ≤ 50 ms；5000 条消息的对话滚动保持 60 fps（虚拟列表）。
- **A11y**：CI 里对每个主要界面运行自动化无障碍检查（无严重 / 重要违规）。

## 12. 测试与验收

**测试替身**：UI 层测试用 `MockIpcAdapter`（§6.1）直连 **Mock API 服务**（M0 由 `schemas/openapi.yaml` 生成，事件流回放录制数据；milestones）。全栈 E2E 用真实外壳 + Mock API + `TestCA`。E2E 需要 Mock 具备"场景控制"能力（缺哪项向 `contract` 泳道提 Issue）：向事件流注入指定事件、断开 / 恢复连接、返回指定错误、切换设备角色与档案、改变 PR 的 head / checks / `review_state`、`/__mock` 之类的请求日志（用于断言"没发出的请求"）。UI 层 E2E 用 Playwright 驱动；真实外壳的全栈 E2E 使用 Tauri 的 WebDriver 支持，**macOS 上能否使用需实现者核实**，不支持的平台用 `MockIpcAdapter` 路径兜底，真机清单手测。

勾选项标注里程碑。"E2E-n"可直接对应测试用例名。

**milestones.md 中与 `fe-desktop` 相关的退出标准 → 本模块测试**

| 里程碑 | 退出标准（节选） | 对应测试 |
| --- | --- | --- |
| M1 | 端到端：对话发起 Low 任务 → PR 创建 → CI 通过 → 用户在桌面端审批 merge → Issue 关闭 | E2E-00、E2E-08 |
| M1 | 任务进行中关闭桌面端，任务继续；重新打开后事件流从游标续上，界面状态一致 | E2E-13、E2E-12 |
| M1 | 写入模型凭据需要 approver 设备签名；没有签名的写入被拒绝 | E2E-15（外壳侧见 desktop-shell §12） |
| M1 | 吊销设备后，其已建立的事件流在心跳间隔内断开 | E2E-14（外壳侧见 desktop-shell §12） |
| M1 | 改动 workflow / 依赖清单 / lockfile 的补丁被策略拒绝并上报 | "策略拒绝上报"一项（界面侧：事件行 + 上报项正确呈现） |
| M2 | 审批后出现新推送，审批自动失效并重新进入评审 | E2E-03 |
| M2 | 改动 `.github/workflows/` 的补丁进入"需要审批"，签名批准后才提交推送 | "`push_branch` 补丁审批"一项 |
| M2 | security 类 finding 被上报，不会被静默关闭 | "`security_finding` 呈现"一项 |
| M3 | Max 档任务的每一轮评审都有 arbiter 的唯一裁决 | "DAG 详情"一项（界面侧：每轮显示 arbiter 的 verdict） |
| M4 | 一个 Milestone 被拆成多个交付单元并行推进，依赖顺序正确 | "按 `parent_id` 分组"一项（界面侧：分组与依赖边） |

M1–M4 里不属于界面的标准（沙盒隔离、webhook 去重、崩溃恢复等）由对应泳道验收。

**基础与渲染**

- [ ] [M1] **E2E-11 不可信内容语料**：`renderUntrusted` 语料测试（plain 与 markdown 两种模式），并在真实页面里经 Mock 注入同样内容：`<script>`、`<img src=x onerror=…>`、`<iframe>`、`<svg onload>`、`javascript:` / `data:` / `vbscript:` 链接、`[x](https://evil.example/)`（文字与目标不符）、`![t](https://evil.example/p.png)`、双向控制字符、零宽字符、超长无空格字符串、嵌套 Markdown 炸弹。断言：DOM 中没有注入的元素 / 属性 / 事件处理器；没有发出任何网络请求（Mock 请求日志与 CSP 报告均为空）；图片只显示 `[图片: alt]`；链接旁始终显示目标主机，点击只调用 `open_external`（`http:` / `javascript:` 链接不可点击，IPC 记录里没有对应调用）。
- [ ] [M1] 药丸与卡片只来自结构化字段：agent 消息正文含"! 需要你 · 签名审批"、"审阅并签名…"、伪造的 `PR #9` 字样时，不出现药丸、按钮或 chip。
- [ ] [M1] lint / 构建检查：源码中无被禁的 HTML 注入 API；只有 `ipc/` 引用 Tauri API；无 `div onClick`。
- [ ] [M1] 未知 `agent_kind` / 未知 `Card.kind` / 未知事件类型 / 未知枚举值：不崩溃；未知 `agent_kind` 灰边框 + "agent"，不是大脑色；未知 `Card.kind` 降级显示"请升级客户端"。
- [ ] [M1] 令牌 / 状态：所有 `StatusDot` 都有文字；`Tone` 映射的快照测试；橙色只出现在"失败且无人处理""缺规格"两处。

**同步与游标**

- [ ] [M1] E2E-12 水位线：先到的事件 `seq ≤ asOf` 不改变缓存；后拉的快照不被旧事件回退。
- [ ] [M1] 事件流中途断开再重连：Mock 重放 `(游标, head]`，界面无重复消息、无丢失；`events_ack` 序列单调；过滤流里仅心跳推进游标。
- [ ] [M1] 重复帧、乱序帧（`seq` 回退）被丢弃；`409 cursor_ahead` → `resync_required` → 清缓存重水合，界面状态（草稿、`sel`、待发送队列）保留。
- [ ] [M1] E2E-13（M1 退出标准）：发起任务后关闭应用，Mock 里任务继续产生事件；重新打开 → 重新水合并续流，最终界面（消息、交付阶段、上报项、审批状态）与 Mock 的真值逐项一致。
- [ ] [M1] 事件风暴（500 条 / s）下合并刷新：对同一资源的 GET 次数 ≤ 突发窗口数，且不超过读限流。
- [ ] [M1] 恢复后第一个心跳到达时显示"已补齐 n 件事件"，n 等于重连后应用的事件数。

**对话、上下文与离线**

- [ ] [M1] **E2E-01 选中项不改变发送上下文**：依次通过左栏、地图（M2）、引用 chip、事件行、检视器选中不同项；输入区仍显示"@ 全项目"；发送一条消息，Mock 收到的请求体 `refs` 为 `[]`，用户气泡元信息为"@全项目"。
- [ ] [M2] **E2E-02 上下文仅显式改变**：`@` 选择器 / 回复 / 在对话中引用 各自设置 `ctx`（请求体 `refs` 为对应 `Ref`：issue、pr、worktree、delivery）；"×"清除；发送后 `ctx` 与 `levelOverride` 复位，`draft` 清空；其他项目的状态不受影响。
- [ ] [M2] 档位选择器：默认显示项目默认；`ctx` 有交付时显示其上限；方向键漫游且只有一个 Tab 停靠点；None 档提示"只问答，不建交付"；用户未改动时请求体**不含** `level_cap`，改动后含；上限与交付不同时出现"PR #7 上限 Max → High；已触发的闸门不撤销"。
- [ ] [M1] ⌘↵ / Ctrl+Enter 按平台发送；输入法组合态按回车不发送；普通回车换行。
- [ ] [M1] **E2E-05 只读设备不能发消息**：设备角色 `viewer`：输入框与发送禁用、占位"此设备为只读，不能发消息"；"回复"、选择类按钮、放弃交付禁用；设置入口隐藏；通过测试钩子强行发送时 Mock 回 `403 forbidden_role`，界面显示失败而不崩溃。
- [ ] [M1] **E2E-17 离线与幂等**：断开连接 → 发消息显示"待发送"、按钮"稍后发送"；恢复后自动发送且 Mock 只产生 1 条 `message.posted`；POST 响应丢失（Mock 吞掉响应）后重试沿用同一 `Idempotency-Key`，仍只有 1 条；`422` 失败项标 `failed`，可"编辑后重发"。
- [ ] [M1] 空项目：对话视图内显示空态卡，输入区可用；地图（M2）与工作树（M2）各有空态。
- [ ] [M1] 事件行：连续 ≥ 2 条折叠为"后台处理了 n 件事"，展开状态按项目保存；点击事件行在检视器打开对应项且不改变 `ctx`。
- [ ] [M1] 自动滚动：在底部时新消息自动滚动；离开底部时出现"新消息 ↓"；向上翻页保持锚点。
- [ ] [M1] **E2E-16 每项目状态保存**：项目 A 设置 `view`、`sel`、`draft`、`needsOnly`、`showAll`、折叠组；切到 B 再切回，全部恢复；A 的 `ctx` 不泄漏到 B；重启后恢复范围按 OQ-U15 的决定（至少草稿与待发送队列，若采纳）。
- [ ] [M2] 上次查看分隔线出现在离开后新增的第一条消息之前；摘要卡每项可点。

**"需要你"一致性**

- [ ] [M1] **E2E-18**：Mock 创建一个 open 的 `approval_needed`：tab 徽标、顶栏计数、左栏药丸、气泡药丸（卡片引用）、检视器"需要你"区同时出现；审批完成（或在 GitHub 上被外部合并 → `inbox.resolved`）后同时消失，没有残留。
- [ ] [M1] 各 `kind` 的内联操作：`decision_needed`（含升档三选项）/ 带 `options` 的上报项调用 `inbox.choose`（带 `Idempotency-Key`） 且卡片与收件箱同步变为"已处理"；`spec_needed` "回复补充…" 切到对话并设 `ctx`；`approval_needed` 只有"审阅并签名…"；只读设备全部禁用。
- [ ] [M1] **E2E-19 外部变化**：GitHub 上直接合并 / 关闭（Mock 注入 `forge.*` + `inbox.resolved`）→ 出现事件行，上报项消失，审批卡显示最终状态而不是可点的按钮。

**审批与签名**

- [ ] [M1] **E2E-00 端到端（UI 侧）**：Mock 脚本化一条完整路径——用户发送 Low 任务消息 → agent 回复与计划卡 → 阶段依次推进 → PR 创建 → CI 通过 → 出现 `approval_needed` 与审批卡 → 用户在审批单签名 → 合并 → Issue 关闭、交付移入"最近完成"；全程界面状态与 Mock 真值一致，没有需要手动刷新的地方。
- [ ] [M1] 策略拒绝上报：Mock 注入 `policy.denied` 事件、对应的事件行消息与一个上报项（改动 workflow 文件的补丁被拒）→ 左栏 / 检视器 / 浮层出现"需要你"，事件行可点开检视器；未知的 `reason_code` 不导致崩溃。
- [ ] [M1] **E2E-08 签名成功流**：打开审批单 → 显示动作、仓库、PR、完整 40 位 head SHA（等宽、可复制）、闸门、倒计时 → 确认 → 审批卡依次显示"已签名 · 排队 merge"→"已合并 {sha8}"→"Issue 已关闭 · 交付完成"；按钮随状态消失；左栏该交付移入"最近完成"。
- [ ] [M1] **E2E-03 新推送使审批单禁用**：审批单打开且待签时，Mock 注入新推送与 `approval.invalidated`（新 head）→ 审批单即时禁用签名并显示"已失效（旧 SHA → 新 SHA），重新评审中"；审批卡、收件箱项、检视器同时变为失效且**没有**可点击的签名按钮；`approvalDiscard` 被调用；之后在 Mock 侧直接 `grant` 会得到 `409 stale_head`。
- [ ] [M1] **E2E-04 连接中断时签名禁用**：断开事件流 → 顶栏 / 状态栏变灰"重连中"、横幅出现、审批单签名按钮禁用并写"连接中断，暂不能签名。"；此期间对 `approvalSign` 的任何调用不会产生 `grant` 请求；恢复后按钮可用并提示"已补齐 n 件事件"。
- [ ] [M1] **E2E-06 非审批设备**：角色 `operator`（及 `viewer`）打开审批单 → 只读展示，签名禁用并写"请在审批设备（{名}）上签名"；Mock 计数确认**没有**请求 challenge。
- [ ] [M1] **E2E-07 GitHub 闸门**：`checks.required_ok = false`、或 `mergeable_state = blocked`、或 `review_state = changes_requested` 时签名禁用并列出缺项，"去 GitHub 上 approve ↗"调用 `open_github`；闸门满足后（Mock 变更 + `forge.synced`）按钮即时启用。
- [ ] [M1] 审批已非待签（`granted` / `consumed` / `denied` / `expired`）：按钮消失并显示最终状态；"拒绝"调用 `approvals.deny` 并显示"已拒绝，回到评审"。
- [ ] [M1] challenge 过期（Mock 缩短 TTL）：审批单提示"已过期 · 重新载入"，重新载入后重新显示全部字段；中途 `digest` 变化时高亮并要求重新核对。
- [ ] [M1] **E2E-09 不回传待签内容**：录制全部 IPC 调用，断言 `approvalSign` 的参数只有 `preparedId`，`guardedWriteSubmit` 只有 `pendingId`；没有任何调用携带 `signature`、`nonce`、`payload`、`subject_digest`。
- [ ] [M1] **E2E-10 被注入的 WebView 越权尝试**（全栈）：在页面里执行脚本调用不在白名单的命令、给 `approval_sign` 加额外字段、`fetch` 外部地址、伪造 `prepared_id`、`window.open`、改 `location`——全部失败，且 Mock 的请求日志中没有对应请求、没有 `grant`。
- [ ] [M1] 已签名区与未签名区：PR 标题含 HTML / 伪造文字时只出现在"未签名信息，仅供参考"区；已签名区字段全部来自 `ApprovalSheetModel.signed`。
- [ ] [M2] `push_branch` 补丁审批：审批单已签名区显示 `target.branch`、`path_rules` 与补丁摘要（`sha256:`），签名流程与 `merge_pr` 相同；Mock 里签名前补丁变化 → 审批失效提示。
- [ ] [M2] `security_finding` 呈现：上报项显示 `finding.severity` / `category` / `title`（纯文本），"忽略…"需二次确认；确认前后各处"需要你"计数正确；未忽略前不会因为其他事件而消失。
- [ ] [M2] 生物识别文案：按平台显示"用 Touch ID / Windows Hello 签名批准"；系统提示文案由外壳生成（`desktop-shell` 的测试覆盖）。

**设置**

- [ ] [M1] **E2E-15 凭据写入需要签名**（M1 退出标准的 UI 侧）：模型页填入凭据 → 保存 → 出现"此改动需要审批设备签名"→ 完成签名 → 保存成功显示 `rev`；输入框已清空并显示"已设置"；无签名的写入（Mock 直接 `PUT`）被 `403 signature_required`；`GET`、`settings.updated` 事件、IPC 日志、`ui_state` 中检索不到凭据值。
- [ ] [M1] 中性 / 收紧类改动：`dry_run` 返回 `requires_signature = false`，保存不弹签名；operator 设备遇到需要签名的改动时保存禁用并说明。
- [ ] [M1] **E2E-21**：未保存改动 → 点"返回工作区"出现确认条，继续编辑 / 放弃改动并返回均正确；返回后工作区的草稿、`sel` 完好；`412` → 横幅"此页已被其他设备修改"，不自动合并；保存后显示"某模块尚未应用 rev N"直到 `applied_rev` 追上；`errors[].pointer` 标到对应字段。
- [ ] [M1] 设置页不渲染没有契约的控件（"测试连接"）；viewer 看不到设置入口。
- [ ] [M2] 项目页：行集合与"只能收紧"标记符合 config.md §2.2；`override_loosens_global` 标到字段；"回到继承"调用 `settings_delete`。

**吊销、配对与证书**

- [ ] [M1] **E2E-14 吊销**：Mock 吊销本机设备并关流 → 外壳转 `revoked` → 界面出现阻塞的配对向导、内存缓存被清空、无任何后续请求；用户重新配对后正常水合。
- [ ] [M1] 配对向导：粘贴配对串 → 确认码显示 → 一致 → 进入工作区；不一致 → 回到起始；`pair_token_invalid` 的各种原因显示同一提示。
- [ ] [M1] 证书将过期（Mock 设置近到期证书）：顶栏显示"证书将过期（日期）"；到期后 `cert_expired` 阻塞向导。

**检视器、地图、工作树**

- [ ] [M1] 阶段条：7 段都带文字；`level_effective = Low` 时"计划"画斜纹并写"跳过"；`abandoned` 整条置灰；合并阶段显示"合并闸门"清单（四行）。
- [ ] [M1] 关联链按交付计算，不跨交付；同一 PR 属于另一交付时不出现在链里。
- [ ] [M2] 地图：节点 / 边由交付推导（closes、分支、尚未开 PR、缺规格）；选中只高亮所属交付，其余淡化；"只看需要你"过滤；图例有文字；键盘可操作；节点很多时布局不重叠（`layoutMap` 纯函数的属性测试：无节点重叠、边不穿过无关节点）。
- [ ] [M2] 工作树表：列与映射符合 R-39；名称按钮可键盘聚焦；`lock.holder = human` 显示到期时间。
- [ ] [M2] 检视器 ⋯：放弃交付显示确认框（写明将关闭哪个 PR、保留哪个分支）后调用 `deliveries.abandon`，之后交付移入"最近完成"；只读设备禁用；暂停 / 继续 / 重试不出现（OQ-U2）。
- [ ] [M3] DAG 详情视图（含被替换节点的修订历史）：每轮评审显示 arbiter 的唯一 `verdict`，`include_superseded=true` 时被替换节点以"已替换"标注；预算面板展示 `limits` / `used` 且无修改控件。
- [ ] [M4] 地图按 `parent_id` 分组并显示 `depends_on`。

**无障碍与键盘**

- [ ] [M1] **E2E-20**：只用键盘完成 跳转到输入框 → 发送消息 → 打开审批单 → 关闭并回到触发点；所有可点击元素可聚焦且有可见焦点框；对话框焦点陷阱与 Esc；`aria-current` / `aria-selected` / `aria-expanded` 正确；自动化无障碍检查无严重违规。
- [ ] [M1] 窄屏（≤ 900 px）三栏堆叠、输入区可达、无横向滚动。

## 13. 开放问题

骨架没定的点。"临时处理"只是让本文可实现，**不构成决定**；涉及契约的缺口应先单独提契约 PR（AGENTS.md）。外壳相关的开放问题见 [`desktop-shell` §13](desktop-shell.md#13-开放问题)（OQ-S*）。

| # | 问题 | 本文的临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-U1 | 前端框架与状态库未选（architecture §10 只写"TypeScript 前端"） | 状态层与渲染规则不依赖框架（§7.1） | 全模块 |
| OQ-U2 | **暂停 / 继续 / 重试失败节点没有路由也没有事件**：P-37 与 prototype.md §2.6 要求这三项确定性控制，api.md §4.1 只有 `PATCH level_cap` 与 `abandon` | 检视器 ⋯ 菜单只显示"放弃交付…"；Mock 里按 `501` 处理 | api.md、`engine`、`conversation` |
| OQ-U3 | **审批单"改动"（文件列表、增删行、命中策略的文件与原因）没有数据源**：api.md 没有 PR 改动 / 补丁内容的路由，`Delivery` 里也没有 | 显示"服务端未提供改动明细"+ GitHub commit 链接；"策略命中"一栏不显示 | api.md、`policy`、`repo` |
| OQ-U4 | 合并闸门所需字段不足：必需 review 数、分支保护状态、`main` 的最近 CI 不在 `Project` / `PullRequestRef` 里 | §7.8 的临时规则（`required_ok`、`mergeable_state`、`review_state`）；`main` 基线只显示分支名与工作树数 | data-model、`forge` |
| OQ-U5 | **运行中沙盒数、工作树的沙盒列、写权持有者区分（前台 agent / 后台自愈 / 后台 review）没有数据源**：`Worktree.lock.holder` 只有 `engine` / `guardian` / `human` | 沙盒列留空；写权按 holder 映射；状态栏沙盒数取已加载工作流里运行中的沙盒节点（数据不全则不显示） | data-model、api.md |
| OQ-U6 | **`Ref` 没有分支种类**，"@main"/"从分支开始任务…"无法表达；"从已有 Issue 开始"需要未绑定 Issue 的列表，没有路由 | 隐藏"从分支开始任务…"；空态的"从已有 Issue 开始"聚焦输入框并提示输入 `#编号` | data-model §1.1、api.md |
| OQ-U7 | 跨交付的 `refs` 关系（"PR #8 refs #4"）、"相关："行、Milestone 分组没有数据源（`IssueRef` / `PullRequestRef` 不带关系类型，也没有 Milestone 实体） | 地图不画 refs 边，检视器不显示"相关"行；M4 以 `parent_id` 分组 | data-model、`forge` |
| OQ-U8 | 上报项细节，几份规格各自提了临时方案但**契约没统一**：① data-model 的 `escalation` payload 没有 `options[]` 而 api.md §4.8 说可带——engine.md 绕开，"需要按钮"一律用 `decision_needed`（含升档，`reason_code = level_cap_exceeded`）；② 但 api.md §4.1 / P-35 靠 `InboxItem.kind` 区分手机能不能 `choose`（`escalation` 禁止、`decision_needed` 允许），升档若是 `decision_needed` 手机就能升档，与"升档只在桌面"冲突；③ `spec_needed` 用 `choice` 卡 + 空选项暂代（conversation.md OQ-6）；④ "创建 Issue 并关联"（原型的孤儿 PR 上报）没有 kind 也没有路由；⑤ "已处理：…"取不到所选项（`InboxItem` 不记录） | 界面按 `payload.options[]` 渲染、不按 `kind` 硬编码；空选项 ="回复补充…"；"创建 Issue 并关联"不渲染；已处理文案取 `inbox.resolved` 事件缓存，取不到显示"已处理" | data-model、api.md、`engine`、`conversation`；`mobile` 同受影响 |
| OQ-U9 | 事件行没有状态字段（圆点颜色）；"后台守护"没有结构化处置记录；`level_effective` 自动加码的原因不在 `Delivery` 里；"已在 GitHub 上处理"的原因字段 | 事件行圆点中性；守护区列出该交付的事件行；加码原因取 `delivery.level_changed` 的 `data`（schema 出来后核对） | data-model、`schemas/events`、`guardian` |
| OQ-U10 | "上次查看"的定义（何时记录、离开多久算离开）；"离开期间摘要"由谁生成（原型写"打杂 agent"，但 `Card.summary` 只有单个 `delivery_id` + 文本，装不下多项可点击摘要，也没有"用户回来了"的触发路由） | M2 临时：本地记录并只展示本地可确定的计数（§7.4 R-20）；"离开 ≥ 60 s 且有新消息才显示"[建议值] | `conversation`、api.md、手机端同样受影响 |
| OQ-U11 | `Message.level_cap` 发给已有交付时是否改变该交付的上限：prototype.md §2.3 的提示暗示会，api.md §4.5 只说"任务级上限"；[conversation.md §7.3](conversation.md) 临时采用"桌面档案会改（`set_level_cap`），手机档案忽略"，并自己标为 OQ-3 | 只在用户显式修改时才发送 `level_cap`；界面不预判结果，以 agent 回执为准 | `conversation`、api.md |
| OQ-U12 | **M1 的档位上限来源**：milestones 把档位选择器放在 M2，但 M1 退出标准是 Low 档端到端 | M1 不显示选择器、不发送 `level_cap`，要求 M1 演练环境的 `levels.default_level` 为 `Low`（需 `ops` 或直接用 settings API 配置）；备选是把选择器提前到 M1 | milestones、`ops` |
| OQ-U13 | 设置页：原型"保存需要审批设备签名"与 config.md §2.5（只有放宽 / 凭据 / 仅 approver 键需要）不一致；"测试连接"无路由；"每个档位里三类模型各做什么"表无服务端来源；"被 n 个项目覆盖"需要 n+1 请求；operator 设备发起 relax 的跨设备流程 | 以 `dry_run` 的 `requires_signature` 为准；不渲染"测试连接"；表用随包静态文字 + 快照测试；n+1 请求仅在该页打开时发；operator 遇到需签名改动时禁用保存 | prototype.md、config.md、api.md |
| OQ-U14 | 设备与证书页：config.md §13.1 列了"配对令牌发放"，api.md 没有该路由（令牌由主机上的 `gantry pair` 产生）；页面里程碑"未排期"；证书续期 | 页面不提供令牌发放；建议 M2 交付设备列表 / 改名 / 改角色 / 吊销 | config.md、api.md、milestones |
| OQ-U15 | 界面状态跨重启的保留范围（草稿、`ctx`、`levelOverride`、待发送队列、`lastViewed`）；prototype.md 只要求"切换项目再切回恢复" | 会话内全部保留；跨重启持久化草稿、`view`、`needsOnly`、`lastViewed` 和待发送队列 [提议]，`sel` / `ctx` / `levelOverride` 不持久化 | `desktop-shell`（OQ-S6） |
| OQ-U16 | 浅色主题、其他语言、最小窗口尺寸、高对比度模式未设计 | 令牌可切换，M1 只有深色 + `zh-CN` | 设计 |
| OQ-U17 | 添加项目（"+"）：项目绑定由什么触发、有无路由（api.md OQ-8） | 禁用按钮 | api、`forge`、`deploy` |
| OQ-U18 | 规模与 N+1：左栏状态点需要每个交付的工作流状态（自愈 / 暂停），目前每个交付要多发请求；`Delivery` 没有活动摘要字段。地图"只看需要你"是淡化还是隐藏 | 后台预取 ≤ 20 个、并发 2；淡化（沿用原型） | api.md、`engine` |
| OQ-U19 | 放弃交付的确认文案依赖放弃的语义（关闭 PR 不合并、保留分支、Issue 保持打开）——引擎规格未写 | 按原型文案，待引擎规格核对后改 | `engine` |
| OQ-U20 | 审批"有效期"：原型写 30 分钟，配置 `policies.approval.ttl_s` 默认 24 小时，challenge 默认 120 s，内部动作审批 300 s——审批单只能分别标注 | 审批单显示三者：审批有效期 `Approval.expires_at`、本次确认窗口 `challenge_expires_at` | prototype.md、config.md、api.md |
| OQ-U21 | 工作树人工锁（`worktrees.lock_put` / `lock_delete`）在工作区是否提供入口；prototype.md 没有 | 不提供入口，只显示持有者 | prototype.md |
| OQ-U22 | 尝试结果（`result_ref`）读取路由未定（api.md OQ-11） | 检视器只显示尝试元数据 | api.md |
| OQ-U23 | 系统通知点击后的导航需要外壳向 UI 发 `navigate` 帧（desktop-shell §6.2 已列为 M2），以及从通知定位到 `inbox_id` 的映射 | M2 实现时一并确认 | `desktop-shell` |
