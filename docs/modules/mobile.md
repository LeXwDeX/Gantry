# 手机端（`mobile`）

> 状态：设计 v0.1 · 里程碑：M4 · 泳道：`fe-mobile`（见 [milestones.md](../milestones.md)） · crate / 目录：`apps/mobile`（iOS，UIKit）

手机端只做一件事：给 agent 发消息，并在"需要你"出现时做不需要签名、不增加花费的轻量答复（D-11、P-35）；审批签名、升档、关系地图、工作树、设置、设备管理都不在手机上。界面规格见 [prototype.md §4](../prototype.md#4-手机端--对话)，行为参考 `prototype/Mobile.dc.html`。

## 1. 职责

1. **配对**：扫码（或手动粘贴）配对串 → 在安全隔区（Secure Enclave）生成 P-256 密钥 → 生成 CSR → 经单向 TLS 配对端点（固定 CA 指纹）提交 → 校验证书链 → 展示 6 位确认码供人工核对 → 身份存入钥匙串（Keychain）（api.md §1.3，architecture §7.1）。
2. **互相认证 TLS（mTLS）客户端**：只实现手机档案的 10 条路由（api.md §2.3、§4.1；配对端点 `POST /v1/pair` 另计）；其余路由在客户端里**根本没有对应方法**。
3. **事件流**：一条服务端推送事件（Server-Sent Events，SSE）流；快照 + 事件流衔接、游标、退避重连、`close` 帧处理（api.md §5）。
4. **对话屏**：项目切换、消息流（用户 / agent / 事件行 / 卡片）、输入区、"需要你 · n"（prototype.md §4）。
5. **"需要你"的手机侧处理**：能答复的（补规格、二选一）就地处理；必须在桌面的（签名审批、升档）清楚说明。
6. **推送（M4）**：注册、接收、点击直达对应气泡。**注册路由未定义，是 M4 退出标准的阻塞项**（OQ-M2）。
7. **不可信内容渲染**（I5）：与桌面端同一套规则，用原生富文本而不是网页。
8. **无障碍**：动态字体、VoiceOver、点击目标 ≥ 44 pt。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0–M3 | 无交付。M0 起可消费 Mock API 服务与 `TestCA`（M0 契约冻结；`fe-mobile` 泳道从 M4 开工） |
| M4 | 全部：配对、身份存储、10 条路由的客户端、事件流、对话屏、"需要你"处理、不可信内容渲染、无障碍、推送接收与直达（取决于 OQ-M2 的契约）。建议内部顺序：① 配对 + 网络 + 事件流 ② 对话屏 ③ "需要你" ④ 推送 |

## 2. 不负责

- **审批签名**：永远在桌面。手机端不持有审批能力——即使设备角色被改成 approver，手机档案下审批路由也是 `403 forbidden_profile`（api.md §2.3）；客户端里没有 `challenge`、`grant`、审批创建的任何代码。
- **升档**：增加花费，只在桌面处理（P-35）。
- 关系地图、工作树、检视器、设置、设备管理、预算、工作流 → 手机端不提供（prototype.md §4 "不提供"）。
- 档位上限选择：手机发出的消息固定使用项目默认上限（`level_cap` 不发送）。
- 意图解析 → `conversation`；通知文案与 APNs 发送 → `notify`（M4 `ops`/`notify` 的 APNs 实现）；证书签发 → `pki`。
- 桌面端 → [`desktop-shell`](desktop-shell.md)、[`desktop-ui`](desktop-ui.md)。手机端没有 WebView，也没有 IPC 边界。

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `api`（契约） | 手机档案 10 条路由、配对端点、事件流、错误码、`Idempotency-Key`、ETag 之外的通用约定 | [api.md](../interfaces/api.md)：§1.3、§2.3、§3、§4.2、§4.3、§4.5、§4.8、§5 |
| `data-model`（契约） | `Message`、`Card`、`Ref`、`InboxItem`、事件信封、`UntrustedText` | [data-model.md](../data-model.md) |
| `pki`（契约） | 配对串格式、CA 指纹含义、证书链形状、续期 | [pki.md](pki.md)（§7.4.2 配对串需适合二维码、§7.1 指纹、§7.7 续期，多为 pki.md 自己的 [提议]）；desktop-shell OQ-S3 / OQ-S5 同样适用 |
| `notify`（契约） | APNs 推送载荷、令牌注册 | [notify.md](notify.md)（§7.9：载荷 ≤ 4 KB，只放模板文案与 `inbox_id` / `project_id` / `kind` / `badge`；令牌注册路由是 N-11，未定）；api.md OQ-2 |
| 界面规格 | 手机对话屏规则 | [prototype.md §4](../prototype.md#4-手机端--对话) |
| Mock API 服务、`TestCA` | M4 之前的开发与测试替身 | [milestones.md](../milestones.md) "替身" |
| iOS 系统框架 | UIKit、Security（安全隔区密钥、钥匙串）、Foundation `URLSession`、AVFoundation（扫码）、UserNotifications | 平台文档；各能力由实现者核实（OQ-M1） |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 配对串 | 扫码 / 粘贴 | 地址 + CA 指纹 + 一次性 token（[pki.md §7.4.2](pki.md) 建议的 `gantry+pair://…` URL 形式，适合二维码） | 用户给定，严格校验；token 视为秘密 |
| API 响应 | 服务端（mTLS） | 手机档案 10 条路由的响应 | 通道可信；`Message.text`（`agent` / `event`）、`InboxItem.summary`、`UntrustedText` **不可信** |
| 事件流帧 | 服务端 | 业务事件（data-model §5.1 信封）、`heartbeat`、`close`；服务端只下发 `project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked`（api.md §5.4） | 同上 |
| 推送通知（M4） | APNs | 载荷由 `notify` 定义（OQ-M2） | 只当"提示"：点击后一律重新向服务端取数据，不信载荷里的文本 |
| 用户输入 | 触摸 / 键盘 / 听写 | 文本、选项点击 | 用户文本，按纯文本发送 |
| 系统时钟 | OS | — | 与 `GET /v1/me` 的 `server.time` 比对，偏差 > 60 s 警告 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| `POST /v1/conversations/{id}/messages` | 服务端 | 体只含 `text`、`refs`（回复时），不含 `level_cap`；带 `Idempotency-Key`（api.md §4.5） |
| `POST /v1/inbox/{id}/choose` | 服务端 | 仅对 `spec_needed` / `decision_needed`；`{ option_id, note? }` |
| `POST /v1/pair` | 配对监听器（单向 TLS） | api.md §1.3 |
| 事件流订阅 | 服务端 | `GET /v1/events`（一条，`Last-Event-ID`） |
| 推送令牌注册（M4） | 服务端 | **路由未定义**（OQ-M2） |
| 本地通知 / 角标（M4） | iOS | 固定模板 |
| 外链 | 系统浏览器 | 先显示目标主机并确认 |

## 6. 对外接口

手机端是叶子，没有被其他模块调用的接口。下面是内部分层的草案，用来固定"只有 10 条路由"这一约束。

```swift
/// 手机档案的全部服务端访问。不得出现这 10 个方法之外的网络方法。
protocol GantryAPI {
    func me() async throws -> Me                                                   // GET  /v1/me                              #2
    func projects(cursor: String?) async throws -> Page<Project>                   // GET  /v1/projects                        #3
    func project(_ id: ProjectID) async throws -> Project                          // GET  /v1/projects/{id}                   #4
    func conversations(project: ProjectID) async throws -> Page<Conversation>      // GET  /v1/projects/{id}/conversations     #10
    func messages(conversation: ConversationID, cursor: String?, limit: Int) async throws -> Page<Message>  // GET …/messages  #11
    func postMessage(conversation: ConversationID, body: PostMessage, key: IdempotencyKey) async throws -> Message  // POST …/messages  #12
    func inbox(project: ProjectID?, status: InboxStatus, cursor: String?) async throws -> Page<InboxItem>        // GET  /v1/inbox  #21
    func inboxItem(_ id: InboxID) async throws -> InboxItem                        // GET  /v1/inbox/{id}                      #22
    func choose(_ id: InboxID, option: String, note: String?, key: IdempotencyKey) async throws -> InboxItem    // POST …/choose  #42
    func events(after: Int?) -> AsyncThrowingStream<StreamFrame, Error>            // GET  /v1/events                          #41
}
struct PostMessage: Encodable { let text: String; let refs: [Ref]? }               // 没有 level_cap、没有其他键
```

`GantryAPI` 的 `Page.next_cursor` 对客户端不透明。身份、事件流解析、存储、渲染各自是独立组件（§7.1）。

## 7. 内部设计

### 7.1 结构

- Swift + UIKit（D-11）。单向数据流：`Store`（实体缓存 + 会话状态 + 界面状态）由事件与响应驱动；视图控制器订阅 `Store` 的选择器，列表用可差分数据源（diffable data source）。
- 模块：`Identity`（密钥、证书、钥匙串）、`Net`（`URLSession` 委托、`GantryAPI` 实现、SSE 解析）、`Sync`（水合、事件应用、水位线、游标）、`Store`、`Render`（不可信文本）、`Features/{Pairing, Conversation, NeedsYou, Push}`。
- 水位线规则与桌面端相同（[desktop-ui §7.2](desktop-ui.md#72-数据流水合事件流与一致性)）：每个缓存条目带产生它的响应的 `X-Event-Seq`，事件 `seq ≤ asOf` 的跳过。

### 7.2 身份与配对

- **密钥**：`SecKey` 永久密钥，`kSecAttrTokenIDSecureEnclave`、ECDSA P-256；不可导出。手机端不做审批签名，所以**只有 mTLS 这一把密钥，没有 desktop-shell OQ-S1 的双钥问题**；访问控制不要求生物识别（否则后台重连会弹提示）。可访问性级别 `WhenUnlockedThisDeviceOnly`，不进备份 [提议]。
- **CSR**：自行编码 PKCS#10（或用成熟库），由安全隔区密钥签名；服务端忽略并重写 subject。
- **证书**：配对响应的 `certificate_chain`（`[叶子, 中间 CA]`）存钥匙串并与密钥组成 `SecIdentity`，供 `URLSession` 的客户端证书挑战使用。
- **服务端校验**：`URLSessionDelegate` 的服务端信任挑战里不使用系统信任库，只接受"链中有一张证书的 SHA-256 等于已固定指纹，且链内签名与有效期有效、叶子主机匹配"（固定对象见 desktop-shell OQ-S3）。TLS 1.3 only。
- **配对流程**：与桌面一致（desktop-shell §7.2）——解析 → 生成密钥 → CSR → `POST /v1/pair` → 校验链与公钥 → 暂存 → 展示确认码 → 用户核对 → 落盘。第一台设备的角色为 `approver`、之后默认 `operator`（P-23）；手机端若恰好是第一台，角色是 `approver`，但档案仍限制路由，界面按手机档案裁剪，不因角色显示任何审批入口。配对界面（扫码取景框、手动粘贴、确认码页）原型未覆盖，按系统规范设计。
- **吊销 / 过期**：`close(device_revoked)` 或 `401 device_revoked` → 停止请求、删除钥匙串中的密钥与证书、回到配对页；`close(cert_expired)` / 握手连续失败 → 提示重新配对（续期路由未定，api.md OQ-12）。

### 7.3 数据流

**水合**（`GET /v1/me` 通过后）：`me` → `projects` → 当前项目 `conversations` → `messages`（`limit=50`，`order=desc`，界面反转）；每个项目 `inbox?project_id&status=open`（徽标与"需要你"）。取所有响应 `X-Event-Seq` 的最小值 `S`，`events(after: S)`。其余项目首次切换时才拉对话。

**前后台**：事件流只在前台保持。进入后台即断开；回到前台：若进程仍在内存且游标有效，带游标续传，否则（被系统回收）重新水合。后台期间的变化靠推送提醒（OQ-M8）。

**帧处理**：与桌面一致——`seq ≤ appliedSeq` 丢弃、应用后推进游标、心跳推进游标（过滤流）、`409 cursor_ahead` → 清缓存重水合、`close(lagging)` 立即带游标重连、退避 1 s 起至 30 s 全抖动、45 s 无字节判定断线（api.md §3.7、§5）。服务端对手机档案**只下发** `project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked`，客户端不得依赖其他事件（例如没有 `approval.*` 与 `delivery.*`）。

**事件 → 刷新**：

| 事件 | 刷新 |
| --- | --- |
| `message.posted` | 事件含完整 `Message` 时直接合并，否则拉最新一页合并（按 `id` 去重） |
| `inbox.raised` / `resolved` / `dismissed` | `GET /v1/inbox/{id}`，更新徽标、"需要你"、相关气泡 |
| `conversation.created` / `project.*` | 重取对话 / 项目列表 |
| `device.revoked`（本机） | §7.2 |
| 其他 | 忽略并推进游标 |

### 7.4 prototype.md §4 规则 → 实现映射

| ID | 规则（prototype.md §4） | 组件 | 状态 | 数据 / 调用 |
| --- | --- | --- | --- | --- |
| M-01 | 顶部：项目切换，项目列表带"需要你"徽标 | `ProjectSwitcherButton` + `ProjectPickerViewController` | `currentProjectID` | `projects`；徽标 = `inbox?project_id&status=open` 条数 + `inbox.*` 事件 |
| M-02 | "需要你 · n"，点击依次跳到对应气泡 | `NeedsYouButton`；`NeedsYouCursor`（循环指针） | 有序 open 项列表、当前指针 | n = 当前项目 open 项数；每次点击滚动到下一项的气泡并短暂高亮；气泡 = 卡片引用该项的消息（`approval` → `payload.approval_id`，`escalation` / `choice` → `inbox_id`；[conversation.md §7.7](conversation.md) 的临时约定里 `spec_needed` 用 `choice` + 空选项承载，所以也能定位）；找不到气泡（气泡不在已加载页且翻 10 页仍无）→ 弹出该项的详情半屏 |
| M-03 | 连接状态 | `ConnectionIndicator` | `phase`（`live` / `reconnecting` / `revoked` / `cert_expired`） | 事件流状态；与桌面同一合并规则（mTLS + 心跳） |
| M-04 | 对话流规则与桌面一致：上次查看分隔线、种类文字 + 边框、"! 需要你 · 类型" | `MessageCell` 家族（`UserBubbleCell`、`AgentBubbleCell`、`EventRowCell`、`EventGroupCell`、`DividerCell`、`AwayCardCell`） | 每项目 `lastViewed` | `agent_kind`：`planner` 大脑紫 / `worker` 干活青 / `utility` 打杂灰，**未知 → 中性灰 + "agent"**；2 pt 边框 + 气泡内第一行种类文字（D-12，无头像）；药丸仅当卡片引用 open 项；事件行连续 ≥ 2 条折叠；"上次查看"与离开期间摘要见 OQ-M5 |
| M-05 | 用户气泡下方 `@上下文 · 上限 X · 时间` | `UserBubbleCell` | — | `refs` → 上下文标签；`level_cap` 非空显示"上限 X"，空显示"上限 项目默认" |
| M-06 | 可在手机上答复的：补规格、二选一——气泡内给快捷按钮或"回复补充" | `ChoiceCardView`、`ReplyButton` | — | `Card.choice` / `decision_needed` 的选项 → `choose(option_id)`（带 `Idempotency-Key`，与桌面同一个 `inbox_id`，P-40）；`options` 为空的 `choice`（`spec_needed`）→"回复补充"：聚焦输入框，上下文 = `{kind: delivery, id: delivery_id}`（自由文本答复走发消息，api.md §4.8）。**升档例外**：[engine.md §7.11](engine.md) 把升档放进 `decision_needed`（`reason_code = level_cap_exceeded`），而 P-35 要求升档只在桌面；临时处理为该 `reason_code` 的项按"需要在桌面处理"显示、不给按钮（OQ-M9） |
| M-07 | 必须在桌面完成的：签名审批——写明"手机不能签名 · 等待桌面签名（MacBook Pro 在线 / 离线）" | `DesktopOnlyNotice` | — | `Card.approval` / `approval_needed`：固定文案"手机不能签名 · 等待桌面签名"。**在线 / 离线状态没有数据源**（手机档案没有设备路由，OQ-M3），不显示；项被 `resolved` 后改为"已处理" |
| M-08 | 升档会增加花费，也只在桌面处理 | `DesktopOnlyNotice` | — | `escalation` 项不提供按钮（`choose` 会 `403 forbidden_profile`）；`reason_code = level_cap_exceeded` 的 `decision_needed` 同样不提供；任何 `choose` 收到 `403 forbidden_profile` 时回退为"需要在桌面处理"并刷新项 |
| M-09 | 输入区：可增高的多行输入框；上方"回复 · @上下文 · 上限 X（项目默认）"；固定项目默认上限 | `ComposerView`（自增高 `UITextView`，上限 5 行 [建议值]）+ `ContextLine` | `draft`、`replyTarget` | 发送体 `{text, refs: replyTarget ? [delivery ref] : []}`，**不含 `level_cap`**；`ContextLine` 无法显示 X 的具体档位（手机档案无设置路由，OQ-M4），显示"项目默认"；发送后 `replyTarget` 复位（与桌面 P-32 一致） |
| M-10 | 推送通知点开后直达对应气泡 | `PushRouter` | — | 载荷里的 `inbox_id` / `message_id` → 先水合再定位（§7.6）；载荷由 OQ-M2 定 |
| M-11 | 不提供：关系地图、工作树、设置、审批签名 | — | — | 客户端无对应代码；契约测试扫描网络层（§12） |
| M-12 | 只读设备（`viewer`）不能发消息（api.md §2.2） | `ComposerView` 禁用 | `me.role` | 占位"此设备为只读，不能发消息" |
| M-13 | 离线 / 断线时发送 | `OutboxFlusher` | 每项目待发送队列 | 与桌面同一规则：入队显示"待发送"，恢复后按序发送，沿用 `Idempotency-Key`；`4xx` 失败项可编辑重发 |

### 7.5 "需要你"的手机侧处理

| `InboxItem.kind` | 手机上显示与操作 |
| --- | --- |
| `spec_needed` | `missing[]` 逐条显示；"回复补充"（发消息） |
| `decision_needed` | `payload.options[]` → 按钮 → `choose`；`reason_code = level_cap_exceeded`（升档）不给按钮；服务端若返回 `403 forbidden_profile` 则回退为"需要在桌面处理" |
| `escalation` | 只读：原因与证据（`Ref` 仅显示编号）；"需要在桌面处理"，无按钮 |
| `approval_needed` | "手机不能签名 · 等待桌面签名" |
| `budget_exhausted` / `security_finding` | 只读摘要 +"需要在桌面处理"（手机没有 `dismiss`） |
| 未知 `kind` | 显示 `summary`（纯文本）+"需要在桌面处理" |

`summary`、`UntrustedText` 一律纯文本。open 项数量与 tab 徽标、应用角标（M4）同源。

### 7.6 推送（M4）

- 注册：授权通知 → 获得设备令牌 → **注册到服务端（路由未定义，OQ-M2）**。
- 载荷由 `notify` 定义；本规格要求的最小集合：`project_id`、`inbox_id`（必要时 `message_id`）。展示文案由服务端模板生成，**不透传不可信文本**（config.md §9）；锁屏上是否显示项目名由用户的系统设置决定。
- 点击：应用冷启动或回到前台 → 水合 → 用 `inbox_id` 取 `GET /v1/inbox/{id}` → 切到对应项目 → 定位气泡（M-02 的规则）。载荷里的任何字段只当"提示"。
- 角标：= 全部项目 open 项数（前台由事件更新；后台由推送载荷的 `badge`，若 `notify` 提供）。

### 7.7 不可信内容渲染

规则与 [desktop-ui §8.2](desktop-ui.md#82-不可信内容渲染规则) 相同，落到原生控件：

- 用 `NSAttributedString` 自己构造（语法树 → 属性字符串），**不得**用 `NSAttributedString(data:options:documentType: .html)`、不使用 `WKWebView` / `SFSafariViewController` 渲染服务端内容。
- 受限 Markdown：段落、换行、强调、行内代码、围栏代码块、引用块、列表、显式链接；标题降级为加粗；图片只显示 `[图片: alt]`，不发请求；原始 HTML 作为转义文本显示。
- 链接：只允许 `https`（其他协议按普通文本显示，不可点击）；渲染为"文字 · 目标主机名"，主机名始终可见（国际化域名同时显示 punycode；链接文字像 URL 但与真实主机不符时突出真实主机）；点击弹出确认框显示完整目标主机，确认后用 `UIApplication.open` 交给系统浏览器。手机端没有外壳，确认框由应用自己弹出。
- 文本清理：去控制字符；双向控制字符显示为可见转义；零宽字符在标识符、SHA、主机名、按钮标签里显示为可见标记。
- 药丸、卡片、"手机不能签名"提示只由结构化字段生成，不因正文里出现某些字样而生成。
- 长文本折叠为"展开"（`Message.text` 上限 16 KiB，api.md §3.7）。

### 7.8 本机存储与隐私

| 数据 | 位置 | 说明 |
| --- | --- | --- |
| 私钥 | 安全隔区 | 不可导出 |
| 证书链、服务端地址、已固定 CA 指纹、`device_id` | 钥匙串（`ThisDeviceOnly`） | 不含秘密以外的内容；不进 iCloud 钥匙串 |
| 消息 / 实体缓存 | 仅内存 | 不落盘（OQ-M6） |
| 草稿、"上次查看"、待发送队列 | 内存；是否落盘见 OQ-M6 | 待发送队列含用户文本，落盘需用 Data Protection |
| 应用快照 | 进入后台时用遮罩覆盖界面 | 防止任务切换器截图泄露内容 [提议] |

### 7.9 无障碍

- 动态字体（`UIFontMetrics`），布局随字号增长；点击目标 ≥ 44 × 44 pt（原型已按此）。
- VoiceOver：气泡朗读为"{种类} 说：{文本}。需要你：{类型}"；事件折叠组可展开；输入区、按钮有标签；药丸朗读文字。
- 颜色不单独表达含义（种类文字、药丸文字都在）；尊重"减弱动态效果"；深色为默认（浅色未设计，OQ-M1）。

## 8. 不变式与安全要求

| 不变式 | 手机端的做法 |
| --- | --- |
| I3 放行唯一决策点 | 客户端没有任何审批相关代码；`approval_needed` 只展示"手机不能签名"。服务端的 `403 forbidden_profile` 是第二道，不是唯一一道 |
| I4 状态事实源 | 只显示服务端消息与上报项；不推断交付状态 |
| I5 外部文本不可信 | §7.7；`Message.text`（agent / event）、`InboxItem.summary`、`UntrustedText` 当数据；推送载荷只当提示 |
| I6 客户端不执行、不持凭据 | 私钥在安全隔区、不可导出；不持有 GitHub / 模型凭据；没有任何设置路由，所以不会接触 `SecretRef`；日志不含消息正文与配对 token |
| I7 配置与工作分离 | 没有设置界面与路由；消息体只含 `text` / `refs`；手机端固定项目默认上限 |
| D-11 / D-12 | 手机端只做对话；无头像，边框颜色 + 种类文字区分 agent |

**安全要求（编号供测试引用）**

| # | 要求 |
| --- | --- |
| M-S1 | 网络层只实现 §6 的 10 个方法；构建期扫描网络层源码，不得出现这 10 条之外的路径字面量（含 `/v1/approvals`、`/v1/deliveries`、`/v1/settings`、`/v1/devices`、`/v1/worktrees`） |
| M-S2 | TLS 1.3 only；不使用系统信任库；只信固定 CA 指纹；App Transport Security 保持默认严格 |
| M-S3 | 私钥不可导出；吊销 / 重置时删除钥匙串条目 |
| M-S4 | 不使用 `WKWebView`、不解析 HTML；服务端文本只进属性字符串 |
| M-S5 | 日志（`os_log`）对用户文本与服务端文本使用 `private`；不记录配对 token、`Idempotency-Key` 以外的请求头值 |
| M-S6 | 进入后台时遮罩界面 [提议]；不把内容写入通知正文以外的系统位置（如 Spotlight） |
| M-S7 | 不集成第三方分析 / 崩溃上报 SDK（客户端只与已配对的服务端通信） |

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 握手失败 / 断网 | 退避重连；连续 3 次 TLS 告警 →"设备可能已被吊销或证书已过期"，由用户决定重新配对（不自动清除） |
| 事件流断开 / 45 s 空闲 | 带游标重连；恢复后提示已补齐的事件数 |
| `close(device_revoked)` / `401 device_revoked` | 停止请求，清除密钥与证书，回到配对页 |
| `409 cursor_ahead` | 清缓存重水合 |
| `429` / `503` | 按 `Retry-After` 退避 |
| `POST messages` 网络失败 / `5xx` | 沿用同一 `Idempotency-Key` 重试 |
| `403 forbidden_role` | 输入区禁用，提示"此设备为只读" |
| `403 forbidden_profile` | 对 `choose`：回退为"需要在桌面处理"；其他路由：客户端 bug，记日志 |
| `422 validation_failed`（`refs` 解析失败、内容过长） | 显示"无法发送：引用已失效 / 内容过长"，保留文本 |
| `409 conflict`（`choose` 时项已非 `open`） | 刷新该项，显示最终状态 |
| 推送点击但对应项已处理 | 定位到对话底部并提示"该事项已处理" |
| 系统时钟偏差 > 60 s | 警告 |
| 未知卡片 `kind` / 未知事件 / 未知枚举 | 降级显示或忽略，不崩溃 |

## 10. 配置

本机配置，无服务端 settings（手机档案没有 settings 路由）。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| 事件流空闲 / 退避 | 45 s / 1 s→30 s 全抖动 | api.md §3.7 |
| 单页消息数 / 内存保留 | 50 / 500 | [建议值] |
| 输入区最大行数 | 5 | [建议值] |
| "需要你"跳转翻页上限 | 10 页 | [建议值] |
| 离开多久算"离开" | 60 s | [建议值]，OQ-M5 |
| 应用快照遮罩 | 开 | [提议] |
| 通知权限 | 系统设置 | — |

## 11. 可观测性

- 不发事件。写请求在服务端产生的事件：`message.posted`（发消息）、`inbox.resolved`（`choose`）、`device.paired`（配对），`actor` 为 `{kind: "device", id}`。
- 日志：`os_log` 分类（`net`、`sync`、`render`、`push`），用户与服务端文本一律 `private`；不记录请求 / 响应体。
- 诊断（本机调试页，发布版隐藏或需手势）：连接状态、游标、重连次数、证书到期、`key_backend`；不含业务内容。
- 无第三方遥测（M-S7）。系统自带的崩溃日志（MetricKit / 设备分析）是否收集由用户的系统设置决定。

## 12. 测试与验收

**测试替身**：Mock API 服务（M0，`schemas/openapi.yaml` 生成，事件流回放录制数据）+ `TestCA`；Mock 必须按设备 `platform = ios` 执行**手机档案**（非白名单路由 `403 forbidden_profile`、事件流只下发白名单类型）。单元测试用 XCTest，界面流程用 XCUITest（模拟器），推送用 `xcrun simctl push` 注入载荷。

**milestones fe-mobile 退出标准（M4）**

- [ ] **配对**：模拟器里扫码（可用粘贴路径等价）→ 确认码页 → 一致 → 进入对话屏；不一致 → 回到起点并删除密钥；`pair_token_invalid` 各种原因显示同一提示；配对串指纹与服务端证书不符 → 失败。
- [ ] **发消息**：输入文本发送 → Mock 收到 `{text}`（回复时另有 `refs`），**没有** `level_cap`、没有其他键；用户气泡出现，元信息"@全项目 · 上限 项目默认"；服务端回复经事件流出现，种类文字 + 边框颜色正确。
- [ ] **收到上报通知**：Mock 产生 `inbox.raised` 并触发推送（`simctl push` 载荷含 `project_id`、`inbox_id`）→ 点击通知 → 应用水合后直达对应项目与气泡，"需要你"计数一致。**依赖 OQ-M2 的注册路由契约；在它定下之前，此项用载荷直注入的方式测试直达逻辑，注册逻辑留空**。

**档案与路由**

- [ ] M-S1 契约测试：网络层源码只含 10 条路由；除配对端点外，其余 31 条 mTLS 路由在客户端里不可能被发起（编译期没有方法）。
- [ ] 手机档案的服务端拒绝（Mock 返回 `403 forbidden_profile`）被正确处理：`choose` 在 `escalation` 项上 → 回退为"需要在桌面处理"。
- [ ] 事件流只含白名单类型时界面正常；收到白名单外的类型（Mock 故意注入）被忽略且游标照常前进。

**对话与上下文**

- [ ] 回复：点击带 `delivery_id` 的 agent 气泡的"回复"→ 输入区上方显示"回复 · @…"，发送体 `refs = [{kind: "delivery", id}]`；发送后复位为全项目。
- [ ] 只读设备（`viewer`）：输入禁用并写"此设备为只读，不能发消息"。
- [ ] 离线：断开连接发消息 → "待发送"；恢复后只产生 1 条 `message.posted`（沿用同一 `Idempotency-Key`）；`422` 失败项可编辑重发。
- [ ] 自动滚动、向上翻页保持锚点；5000 条消息滚动无明显卡顿。
- [ ] 事件行连续 ≥ 2 条折叠；点击展开。

**"需要你"**

- [ ] 计数 = open 项数；点击"需要你 · n"依次跳到对应气泡（含需要翻页才能找到的气泡）；找不到气泡的项弹出详情半屏。
- [ ] `decision_needed` / 二选一卡片点击选项 → `choose`，项变"已处理"；`spec_needed` "回复补充" 聚焦输入框并设置上下文。
- [ ] `approval_needed`：显示"手机不能签名 · 等待桌面签名"，没有任何签名 / 批准按钮；审批在桌面完成后（`inbox.resolved`）变为"已处理"。
- [ ] `escalation` 与升档（`decision_needed` + `reason_code = level_cap_exceeded`）：显示"需要在桌面处理"，没有按钮；Mock 对其 `choose` 返回 `403 forbidden_profile` 时界面回退为同样的提示。

**连接与游标**

- [ ] 事件流断开重连：带 `Last-Event-ID`，无重复无丢失；过滤流只有心跳时游标推进；`409 cursor_ahead` → 重水合。
- [ ] 进入后台断开，回到前台续传或重水合后界面状态一致。
- [ ] 吊销：`close(device_revoked)` → 回到配对页，钥匙串条目被删除。

**不可信内容与安全**

- [ ] 渲染语料（与 desktop-ui E2E-11 同一份语料文件）：`<script>`、`<img onerror>`、`javascript:` 链接、文字与目标不符的链接、远程图片、双向控制字符、零宽字符、超长字符串——无 HTML 被解析、无网络请求发出、图片只显示 `[图片: alt]`、链接先确认主机。
- [ ] 正文里写"! 需要你 · 签名审批"、"手机不能签名"、"批准"不会生成药丸或按钮。
- [ ] 静态检查：不引用 `WKWebView`；不调用 `NSAttributedString` 的 HTML 初始化；无第三方分析 SDK。
- [ ] TLS：错误 CA、过期证书、主机不符均被拒；系统信任库中的合法公共证书同样被拒；低于 TLS 1.3 被拒。
- [ ] 安全隔区密钥不可导出（真机核对）；日志脱敏检查：把消息正文、配对 token 作为哨兵跑一遍，日志中检索不到。

**无障碍**

- [ ] 动态字体最大档下布局可用；VoiceOver 朗读气泡、药丸、折叠组；XCUITest 的无障碍审计无严重问题；点击目标 ≥ 44 pt。

## 13. 开放问题

| # | 问题 | 本文的临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-M1 | iOS 最低版本、是否支持 iPad、浅色主题 / 深色跟随系统、其他语言；各平台 API（安全隔区密钥与 `SecIdentity` 的组合、`URLSession` 流式读取）的具体用法需实现者核实 | 深色 + `zh-CN`；iPhone；最低版本由实现者在 M4 开工时提出 | 设计、`fe-mobile` |
| OQ-M2 | **APNs 令牌注册路由未定义**（api.md OQ-2），推送载荷与 `notify` 的 APNs 实现未写。M4 退出标准"收到上报通知"依赖它 | 路由留空；载荷最小集 `project_id` + `inbox_id`；测试用载荷直注入 | api.md、`notify`、`ops`；**阻塞 M4 退出标准** |
| OQ-M3 | prototype.md §4 要求写"等待桌面签名（MacBook Pro 在线 / 离线）"，但手机档案没有设备路由，拿不到桌面在线状态与设备名 | 只写"手机不能签名 · 等待桌面签名" | prototype.md、api.md §2.3 |
| OQ-M4 | 输入区上方"上限 X（项目默认）"需要知道项目默认档位，但 `levels.default_level` 只在 settings 里，手机档案没有 settings 路由 | 显示"上限 项目默认"，不带具体档位；agent 回执会复述实际生效的上限 | prototype.md、api.md、config.md |
| OQ-M5 | "上次查看"的定义与"离开期间摘要"的来源（同 desktop-ui OQ-U10）；手机端是否本地生成 | 不显示摘要卡；仅在离开 ≥ 60 s 且有新消息时插入分隔线 [建议值] | `conversation`、api.md |
| OQ-M6 | 本机持久化范围：消息缓存、草稿、待发送队列、上次查看是否落盘（隐私 vs 离线可用） | 实体缓存仅内存；其余首版也仅内存，待决 | 隐私、离线体验 |
| OQ-M7 | 配对串格式与 CA 指纹固定对象、确认码核对失败的处置、证书续期（同 desktop-shell OQ-S3 / S4 / S5） | 与桌面同一临时处理 | `pki`、api OQ-12 |
| OQ-M8 | 后台策略：是否使用静默推送 / 后台刷新；进程被回收后的续传 | 仅前台保持事件流；回收后重新水合；后台靠可见推送提醒 | 体验、`notify` |
| OQ-M9 | 同 desktop-ui OQ-U8：`spec_needed` 没有合适的 `Card.kind`（conversation.md 暂用 `choice` + 空选项）；升档用 `escalation` 还是 `decision_needed` 未统一（api.md / P-35 靠 `kind` 拒绝手机 `choose`，engine.md 把升档放进 `decision_needed`，手机因此可能升档） | 药丸与定位只认卡片引用；`reason_code = level_cap_exceeded` 的项当作桌面专有；`403 forbidden_profile` 兜底 | data-model、api.md、`engine`、`conversation` |
| OQ-M10 | 是否提供最小的"本机信息 / 重新配对"页（prototype.md §4 "不提供设置"） | 在应用设置（iOS 系统设置）之外，仅在吊销 / 过期时进入配对页；不另做页面 | 设计 |
| OQ-M11 | 应用分发（TestFlight / App Store）、证书与签名、最小权限（相机、通知）的申请文案 | 本文不涉及 | `ops` |
