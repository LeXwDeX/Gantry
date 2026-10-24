# 收件箱与通知（`notify`）

> 状态：设计 v0.1 · 里程碑：M1 起（M1 收件箱核心、上报与计数 → M2 桌面通知联调 → M4 APNs 推送）· 泳道：`be-engine`（M1 上报与计数，P-63）；APNs 归 M4 `ops` · crate / 目录：`crates/notify`（crate 名 `gantry-notify`）

`notify` 把"需要人决定的事"落成一条可去重、可处理、可计数的上报项（收件箱，inbox），再按设置里的路由把提醒推到人看得见的渠道。**收件箱是唯一的记录源，推送只是提醒**（D-04：没有问题不打扰；[config.md §9](../interfaces/config.md#9-notify)：收件箱写入不可配置，`notify` 只决定是否 / 往哪推）。

标注约定：**[提议]** = 骨架没定、本规格给的默认，可被推翻；**[建议值，待调优]** = 只是起点的数值；**Nxx** = 见 §13 的开放问题，实现者不得自行拍板。

## 1. 职责

1. **上报项（`InboxItem`）生命周期**：`raise`（创建）、去重（deduplication）、`resolve`（系统关闭）、`dismiss`（人忽略）、`choose`（人选择一个选项）。状态机与字段以 [data-model §3](../data-model.md#inboxitem--approval) 为准，HTTP 形状以 [api.md §4.8](../interfaces/api.md#48-inbox) 为准，本模块实现它们背后的**服务端语义**。
2. **事件**：每次状态变化在同一事务里追加 `inbox.raised` / `inbox.resolved` / `inbox.dismissed` 并更新投影（projection）（[data-model §5.2](../data-model.md#52-事件类型目录)）。
3. **查询与计数**：按项目 / 状态 / 类型列表；按项目的 open 计数（`GET /v1/inbox/summary`，手机可用，P-63；顶栏"需要你 · n"、项目 tab 徽标、手机端汇总的数据来源，[prototype §2.1](../prototype.md)）。
4. **自动关闭**：审批已结算、交付单元已关闭 / 放弃、被新的上报取代时，把失效的 open 项置为 `resolved`，避免"幽灵待办"（§7.5）。
5. **通知管线**：评估 `notify.routes`，为命中的服务端渠道生成文案、合并、限速、发送、记录结果；渠道由 `Notifier` trait（[architecture §11](../architecture.md#11-扩展点)）抽象。**桌面系统通知不经服务端渠道**：由 `desktop-shell` 收到实时 `inbox.raised`（`severity ≥ warn`）时弹出，不新增事件（P-63，§7.8）；M4 实现 APNs 推送（服务端渠道）。
6. **输入清洗**：`summary`、`note` 的长度与控制字符约束；通知文案只用可信字段（I5）。

### 里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 契约与替身：`core` 中 `InboxKind` / `Severity` / `InboxStatus` / `Resolution` 类型、`core::ports::InboxPort`（P-51）；`schemas/events/inbox.*.json`（由 `contract` 泳道产出，本规格 §7.3 给草案）；`contract` 泳道交付的内存实现 `FakeInbox`（实现 `InboxPort`，P-51）与记录发送的 `FakeNotifier`，供 `engine` / `guardian` / `conversation` / `forge` 测试 |
| M1 | 收件箱核心（`be-engine` 泳道，P-63）：`raise` / 去重（`dedup_key`）/ `choose` / `dismiss` / `resolve` / 查询 / 按项目计数（`GET /v1/inbox/summary` 的业务逻辑）/ 三个事件；自动关闭规则 R1、R2；`status` 与 `catalog` 所需的健康与渠道可用性。六个 `kind` 的存储与校验都要支持，但 M1 实际产生的只有 `approval_needed`（merge 审批）、`escalation`（`policy` v0 对依赖清单 / lockfile 的拒绝并上报，P-19；App 缺权限；Outbox 终态失败；升档）、`decision_needed`（workflow 补丁的 `manual_action`，P-48；gate 判定）。**M1 没有服务端推送渠道**：上报通过对话里的 `escalation` / `choice` / `approval` 卡片和 `GET /v1/inbox` 可见 |
| M2 | 路由评估、文案（模板 + 可选 `classify` 润色）、合并窗口与限速（为 M4 的服务端渠道准备，可先以 `FakeNotifier` 验收）；`notify` 资源热加载；全部六个 `kind`（`security_finding`、`spec_needed`、`budget_exhausted` 随守护与准入进入）；与 `fe-desktop` 的收件箱 / 系统通知联调（桌面通知由 `inbox.raised` 触发，P-63） |
| M3 | 无新增交付。Max 档的 `REJECT` / `BLOCKED` 复用 `decision_needed` / `escalation`；回归项：高上报量下的去重与合并压力测试 |
| M4（`ops` 泳道，P-63） | `ApnsNotifier`；APNs 密钥经 `secrets`（P-59）；推送令牌存储与吊销清理；`mobile_push` 渠道在 schema 中解封；Q-07 决定后的离线渠道（若有）；退出标准"手机端能收到上报通知" |

## 2. 不负责

- **HTTP 路由、鉴权、角色、客户端档案、`Idempotency-Key`、ETag**：归 `api`（[api.md §2、§3、§4.8](../interfaces/api.md)）。本模块只返回领域结果，错误到 HTTP 码的映射见 §6。
- **决定"何时上报、上报什么选项、`reason_code` 词表"**：归调用方（`engine` / `guardian` / `policy` / `forge` / `conversation`）。选项 `id` 对 `notify` 是**不透明字符串**。
- **`choose` 之后真正执行动作（放弃、重新规划、升档……）**：归订阅 `inbox.resolved` 的 `engine`（及 `conversation`）。`notify` 不调用它们。
- **审批的创建、验签、状态机**：归 `policy`（I3）。`approval_needed` 只能经审批路由处理，`choose` 对它一律拒绝。
- **对话气泡与卡片的渲染**：归 `conversation` / `desktop-ui`；卡片只引用 `inbox_id`（[data-model §1.1](../data-model.md#11-共用值类型)）。
- **操作系统弹窗、本机通知开关、点击跳转**：归 `desktop-shell`（[config.md §13.2](../interfaces/config.md#132-外观)：两层都开才弹）；触发源是实时的 `inbox.raised`（`severity ≥ warn`），本模块不为此发额外事件（P-63）。
- **settings 的读写与 V18 校验**：归 `api` + `core`；`notify` 只读取 `notify.*` 的生效视图，并提供渠道可用性给 `catalog`。
- **任何 GitHub 写**（含 @ 提及）：只能经 `forge` Outbox（I2）。`notify` 不直接写 GitHub。
- **邮件、GitHub 提及渠道**：Q-07 待决，schema 里是保留 kind，本规格不实现（N-10）。
- **静默处理的审计记录**：走事件日志（守护的处置），不进收件箱（N-08）。
- **推送令牌的注册路由**：归 `api`，契约未定（[api.md OQ-2](../interfaces/api.md#10-开放问题)，N-11）。

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `core` | 端口 `core::ports::InboxPort`（本模块实现）与 `TxHandle`（P-51）；ID（`inb_`…）、`InboxKind` / `Severity` / `InboxStatus`、payload 的 JSON Schema 校验、事件信封 | [data-model §1–§3、§5](../data-model.md)；[core.md §6.10](core.md) |
| `store` | 在一个事务内追加事件并更新投影；带持久游标订阅事件日志；`inbox_items` 投影表（含 `dedup_key` 部分唯一索引、`inbox_summary`）与 `notify_*` 表 | [store.md §6、§7.2](store.md) |
| `models`（**可选**，P-59；经 `TextComposer` 注入） | 打杂模型（`classify` 角色）润色通知文案；不可用时回退模板 | [architecture §9](../architecture.md#9-三类模型)、[config.md §9](../interfaces/config.md#9-notify) |
| settings 生效视图 | `notify.channels`、`notify.routes`；`settings.updated` 触发重载 | [config.md §9、§2.6](../interfaces/config.md) |
| `secrets`（M4，P-59） | APNs 认证密钥（`notify` 使用方视图） | [secrets.md](secrets.md) |
| 事件日志的上游事件（只读订阅） | `approval.*`、`delivery.abandoned`、`delivery.stage_changed`、`device.revoked`、`settings.updated`（`notify`） | [data-model §5.2](../data-model.md#52-事件类型目录) |

被依赖：`api`（收件箱路由与 `GET /v1/inbox/summary` 的业务逻辑）、`engine`、`guardian`、`forge`、`conversation`（经 `core::ports::InboxPort` 调用；`policy` 只给 `ReportHint`，由调用方上报）。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 上报请求 `RaiseRequest` | 进程内调用（`InboxPort::raise`）：`engine`、`guardian`、`forge`、`conversation` | §6；`payload` 形状按 `kind` 见 [data-model InboxItem](../data-model.md#inboxitem--approval) | 调用方可信；`summary` 与 `payload` 里可能带不可信文本，须标注（§8） |
| `dismiss` / `choose` | `api` 转交，带 `DeviceActor`（设备、角色、客户端档案） | [api.md §4.8](../interfaces/api.md#48-inbox) | 设备已鉴权；`note` 是用户文本，限长 |
| 系统关闭 `resolve` | 调用方或本模块的自动关闭规则 | §7.5 | 可信 |
| 上游事件 | 事件日志订阅 | §3 | 可信（本地事件） |
| `notify.*` 设置 | settings 生效视图 | [config.md §9](../interfaces/config.md#9-notify) | 可信（已校验） |
| 推送令牌（M4） | `api` 的令牌注册路由（未定） | N-11 | 设备已鉴权；令牌格式须校验 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| `inbox.raised` / `inbox.resolved` / `inbox.dismissed` | 事件日志 → SSE（手机档案也可见，[api.md §5.4](../interfaces/api.md#54-过滤)） | 信封见 data-model §5.1；`data` 草案见 §7.3 |
| 查询结果、按项目计数 | `api`（列表 / 详情 / `GET /v1/inbox/summary`）、内部调用方 | `InboxItem`（[api.md §4.8](../interfaces/api.md#48-inbox)）；`InboxSummary` |
| 桌面系统通知的触发 | 实时 `inbox.raised`（`severity ≥ warn`）经 SSE 到达 `desktop-shell`，由它弹窗 | §7.8；P-63 |
| APNs 推送（M4） | Apple 推送服务 | §7.9 |
| 健康与已应用 rev | `GET /v1/settings/status` 的 `modules.notify` | [config.md §2.8](../interfaces/config.md#28-catalog-与-status-的响应结构) |
| 渠道可用性 | `GET /v1/settings/catalog` 的 `notify.channels` | 同上 |

## 6. 对外接口

### 6.1 领域服务（进程内，Rust 草案，只写签名与语义）

```rust
/// 收件箱端口，定义在 core::ports（P-51），签名以本节为准。所有写操作 = 一个事务内“追加事件 + 更新投影”。
#[async_trait]
pub trait InboxPort: Send + Sync {
    /// 创建。同一 dedup_key 已有 open 项 → 返回它（Deduped），不产生事件。
    /// tx = Some：并入调用方事务（例如 engine 在“交付单元进入 waiting_human”的同一事务里上报）；TxHandle 由 store::WriteTx 实现。
    async fn raise(&self, tx: Option<&mut dyn TxHandle>, req: RaiseRequest) -> Result<RaiseOutcome, RaiseError>;

    /// 系统关闭：open → resolved。幂等；目标不是 open 则原样返回，不报错。
    async fn resolve(&self, tx: Option<&mut dyn TxHandle>, target: ResolveTarget, how: Resolution)
        -> Result<Vec<Written<InboxItem>>, ResolveError>;

    /// 人忽略：open → dismissed。已 dismissed 则原样返回；resolved 则 Err(NotOpen)。
    async fn dismiss(&self, id: &InboxId, by: &DeviceActor, note: Option<Note>)
        -> Result<Written<InboxItem>, InboxError>;

    /// 人选择：open → resolved，事件带 option_id。
    async fn choose(&self, id: &InboxId, option: &OptionId, note: Option<Note>, by: &DeviceActor)
        -> Result<Written<InboxItem>, ChooseError>;

    async fn get(&self, id: &InboxId) -> Result<Written<InboxItem>, InboxError>;
    async fn list(&self, q: InboxQuery) -> Result<Page<InboxItem>, InboxError>;   // 游标分页，默认 status=open
    /// GET /v1/inbox/summary（P-63）：按项目的 open 计数，手机档案可用。
    async fn summary(&self) -> Result<InboxSummary, InboxError>;
}

pub struct RaiseRequest {
    pub project_id: Option<ProjectId>,   // InboxItem.project_id（P-63）；缺省时由 delivery_id 解析；两者都无 = 全局项
    pub delivery_id: Option<DeliveryId>,
    pub kind: InboxKind,                 // approval_needed / decision_needed / spec_needed / budget_exhausted / escalation / security_finding
    pub severity: Severity,              // info / warn / blocker，由调用方定
    pub summary: SafeSummary,            // 只能由模板或 UntrustedText::to_summary() 构造（§8）
    pub payload: serde_json::Value,      // 按 kind 做 schema 校验，失败 = Err(InvalidPayload)，不静默丢弃；
                                         // escalation / spec_needed / decision_needed 的 payload.options[] 为 {id, label, primary?}（P-63）
    pub dedup_key: DedupKey,             // InboxItem.dedup_key（P-63），§7.2；必填
    pub supersedes: Vec<DedupKey>,       // 可选：同事务里把这些 open 项置为 resolved(superseded)
    pub actor: Actor,                    // engine / guardian / forge / conversation
    pub causation_id: Option<EventId>,
}
pub enum RaiseOutcome { Created(Written<InboxItem>), Deduped(Written<InboxItem>) }
pub struct Written<T> { pub item: T, pub last_seq: Seq }   // last_seq 用于 api 的 X-Event-Seq；无变化的幂等调用返回该项最近一次事件的 seq

pub enum ResolveTarget { Id(InboxId), Approval(ApprovalId), DedupKey(DedupKey), Delivery { id: DeliveryId, kinds: Option<Vec<InboxKind>> } }
pub enum Resolution { Answered { message_id: MessageId }, ApprovalSettled { approval_id: ApprovalId, status: ApprovalStatus },
                      DeliveryClosed, DeliveryAbandoned, Superseded { by: InboxId }, External { reason: String }, System { reason: String } }

pub struct DeviceActor { pub device_id: DeviceId, pub role: DeviceRole, pub profile: ClientProfile }   // 由 api 在鉴权后构造
/// GET /v1/inbox/summary 的响应体（P-63）；by_project 只含 open 数 > 0 的项目
pub struct InboxSummary { pub total: u32, pub by_project: Vec<ProjectOpenCount /* {project_id, open, by_severity} */>, pub no_project: u32 }
```

```rust
/// 服务端渠道。M4：ApnsNotifier。新增服务端渠道只实现此 trait（桌面通知在客户端，P-63，不实现此 trait）。
#[async_trait]
pub trait Notifier: Send + Sync {
    fn channel(&self) -> ChannelKind;                 // mobile_push（email、github_mention 为保留 kind；desktop 的可用性由客户端连接情况给出）
    fn availability(&self) -> Availability;           // 供 catalog.notify.channels：{available, reason?}
    async fn send(&self, n: &Notification) -> Result<SendReceipt, SendError>;   // 至少一次；SendError 区分可重试 / 永久
}

/// 文案润色（可选，P-59）。实现者经 models 的 ModelRouter::invoke_direct 调用打杂模型（classify 角色）；返回 None → 回退模板。
#[async_trait]
pub trait TextComposer: Send + Sync {
    async fn compose(&self, facts: &TrustedFacts, deadline: Duration) -> Option<ComposedText>;
}
```

### 6.2 HTTP 路由（由 `api` 承载，业务逻辑在本模块）

路由本身不在此复制，见 [api.md §4.1 #21 #22 #23 #42、§4.8](../interfaces/api.md#41-路由总表) 与新增的 `GET /v1/inbox/summary`（viewer 起，手机档案可用，P-63）。领域错误到 HTTP 的映射：

| 场景 | 领域结果 | HTTP（[api.md §3.1](../interfaces/api.md#31-错误applicationproblemjsonrfc-9457)） |
| --- | --- | --- |
| `id` 不存在 | `NotFound` | `404 not_found` |
| `dismiss` / `choose` 时已是 `resolved`（或 `choose` 时已是 `dismissed`） | `NotOpen` | `409 conflict`，`reason: invalid_state` |
| `choose` 的 `option_id` 不在 `payload.options[]`（含 payload 根本没有 `options`） | `OptionUnknown` | `422 validation_failed`，`errors[].pointer = /option_id`，`code = enum` |
| `choose` 作用于 `approval_needed` | `UseApprovalRoute` | `409 conflict`，`reason: invalid_state`（`reason` 取值待契约确认，N-12） |
| 手机档案对 `escalation` / `approval_needed` 调 `choose` | `ProfileForbidden` | `403 forbidden_profile`（[api.md §2.3、§4.1 注 `✓*`](../interfaces/api.md#23-客户端档案与手机端子集)，P-35）。**判定放在本模块**，因为它依赖 `kind` |
| 已 `dismissed` 再 `dismiss` | 幂等成功 | `200` 原样返回 |
| 同一 `Idempotency-Key` 重放 | 由 `api` 层处理 | 原样返回首次响应 |

角色检查（`dismiss` / `choose` 需 operator；读需 viewer）与 `Idempotency-Key` 归 `api`，本模块不重复检查，但 `DeviceActor` 必须带齐用于审计与档案判定。

## 7. 内部设计

### 7.1 状态机

```
            raise
 (无) ───────────▶ open ──choose──▶ resolved      终态
                   │   ──resolve─▶ resolved      终态
                   └──dismiss───▶ dismissed      终态
```

- 终态不可回到 `open`。同一问题再次出现 = 新的 `inb_`（去重只看 `open` 项）。
- 并发写（两台设备同时 `choose`，或 `choose` 与自动关闭竞争）靠事务内的条件更新串行化：`UPDATE … WHERE id = ? AND status = 'open'`，影响 0 行则重读决定"幂等成功"还是 `NotOpen`。先提交者胜，后者得到 `409`。

### 7.2 去重（`dedup_key`）

- `dedup_key`（`InboxItem` 字段，P-63）是**调用方给出的、标识"这一次触发的不可重复的原因"的字符串**；`notify` 保证"同一 key 至多一个 `open` 项"（投影表上的部分唯一索引 `UNIQUE(dedup_key) WHERE status = 'open'`，兜底并发）。
- 命中 open 项 → `Deduped`，**不产生事件、不重复通知**；调用方崩溃重放也因此安全（至少一次（at-least-once）调用 + 幂等）。
- 已 `resolved` / `dismissed` 之后再次 `raise` 同一 key → 新项（`dismiss` 之后是否应抑制一段时间，N-07）。
- 同一问题的"内容更新"（例如准入第二轮 `missing[]` 变了）：用 `supersedes` 在同一事务里把旧项置为 `resolved(superseded)` 并创建新项，而不是原地改 `payload`（事件只追加，没有"改 payload"事件）。
- key 的构造由调用方规格定义；下表是 **[提议]** 的原则性模板，用来让各模块对齐：

| `kind` | 模板示例 | 说明 |
| --- | --- | --- |
| `approval_needed` | `approval:{approval_id}` | 一份审批一项；审批因新推送失效后重新请求是新的 `approval_id`，自然是新项 |
| `decision_needed` | `decision:{delivery_id}:{node_id}:{reason_code}:{graph_rev}` | gate `REJECT` 后重新规划会 `graph_rev + 1`，之后的新 `REJECT` 是新项；workflow 补丁的 `manual_action`（P-48）用 `decision:{delivery_id}:manual_action:{patch_sha256}` |
| `spec_needed` | `spec:{delivery_id}:{admission_round}` | 追问轮次变化用 `supersedes` 取代上一轮 |
| `budget_exhausted` | `budget:{delivery_id}:{dimension}` | — |
| `escalation` | `esc:{delivery_id}:{reason_code}:{target}` | `target` 为节点、PR 或补丁摘要；`policy` 拒绝受保护路径补丁用补丁 sha256；升档一律用 `escalation`（P-63），`reason_code = level_cap_exceeded` |
| `security_finding` | `sec:{repo}:{finding 来源 Ref}:{category}:{标题哈希}` | 同一 finding 在多次推送间重复出现只算一项 |

### 7.3 事件与投影

事件信封见 [data-model §5.1](../data-model.md#51-信封)。`subject` = `inb_…`，`project_id` = 上报项所属项目（全局项为 `null`），`correlation_id` = `delivery_id`（若有），`actor`：`raise` 为调用方模块、`dismiss` / `choose` 为 `{kind:"device", id}`、自动关闭为 `{kind:"system", id:"notify"}`。`data` 草案 **[提议]**（须经契约 PR 写入 `schemas/events/`）：

| 事件 | `data` |
| --- | --- |
| `inbox.raised` | `{item: InboxItem 全量快照（含 `project_id`、`dedup_key`，P-63）, supersedes?: [inb_…]}` |
| `inbox.resolved` | `{inbox_id, resolution, option_id?, note?, message_id?, approval_id?, reason?}`。`resolution` ∈ `chosen` / `answered` / `approval_settled` / `delivery_closed` / `delivery_abandoned` / `superseded` / `external` / `system`。`chosen` 必带 `option_id`（api.md §4.8 已规定）；其余按需 |
| `inbox.dismissed` | `{inbox_id, note?}` |

投影表（`store` 同一事务维护，可由事件重建）：

| 表 | 关键列 |
| --- | --- |
| `inbox_items` | `id`、`project_id?`、`delivery_id?`、`kind`、`severity`、`summary`、`payload`（JSON）、`status`、`dedup_key`、`resolution?`（JSON）、`created_at`、`updated_at`、`last_seq`。索引：`UNIQUE(dedup_key) WHERE status='open'`；`(status, project_id, severity)`；`(delivery_id, status)`；游标（keyset）分页用 `id`（ULID） |
| `notify_dispatch` | `(inbox_id, channel)` 主键、`status`、`attempts`、`next_at`、`last_error`（脱敏）；只记服务端渠道（M4 起），结论不进事件日志 |
| `notify_cursor` | 通知调度器已处理到的事件 `seq`（单行） |
| `push_tokens`（M4） | `device_id`、`platform`、`token`、`registered_at`、`invalidated_at?` |

### 7.4 `choose` / `dismiss` / `resolve` 的服务端语义

**`choose(id, option_id, note, by)`**——校验顺序固定，先失败先返回：

1. 取项；不存在 → `NotFound`。
2. `kind = approval_needed` → `UseApprovalRoute`（I3：审批只能经签名路由）。
3. 客户端档案为手机且 `kind ∈ {escalation, approval_needed}` → `ProfileForbidden`（`spec_needed`、`decision_needed` 允许，P-35；升档一律是 `escalation`，所以手机档案不能升档，P-63）。
4. `status ≠ open` → `NotOpen`。
5. `payload.options[]` 里没有该 `option_id` → `OptionUnknown`。
6. 事务内条件更新为 `resolved`，追加 `inbox.resolved`（`resolution = chosen`，`option_id`，`note`，`actor` = 设备）。
7. **不调用 `engine`**。`engine`（或创建该项的模块）订阅 `inbox.resolved`，按 `delivery_id` / `payload` 里的标识和 `option_id` 执行；执行失败是它们的错误，不回滚本次选择（选择是事实）。选项 `id` 的词表和语义归创建者的规格（N-03）。

**`dismiss(id, note, by)`**：operator 起（手机档案不可用，[api.md §2.3](../interfaces/api.md#23-客户端档案与手机端子集)）。`open → dismissed`，追加 `inbox.dismissed`。语义是"我看到了，不再提醒我"：**不改变任何交付单元的状态**，也不通知 `engine`（`engine` 不得依赖收件箱状态推进，否则 `dismiss` 会变成隐蔽的审批旁路）。`dismiss` 能否作用于 `approval_needed` / `blocker`、是否抑制再次出现，N-07。

**`resolve(target, how)`**：只给进程内调用。`open → resolved`，追加 `inbox.resolved`；目标已非 `open` 则跳过（幂等）。自由文本答复路径：`conversation` 处理完用户消息后对相关 open 项 `resolve(…, Answered{message_id})`（[api.md §4.8](../interfaces/api.md#48-inbox) 末段）；哪些项被该消息解决由 `conversation` 判断（N-16）。

### 7.5 自动关闭规则

`notify` 内置一个订阅者，按事件把失效的 open 项关闭。**只关闭，不创建、不通知**（`resolved` 从不触发推送）。

| # | 触发事件 | 关闭谁 | `Resolution` |
| --- | --- | --- | --- |
| R1 | `approval.granted` / `approval.denied` / `approval.invalidated` / `approval.expired` / `approval.consumed`（按 `approval_id` 匹配 `approval_needed.payload.approval_id`） | 对应的 `approval_needed` | `ApprovalSettled{status}`。审批失效后若需要重新审批，`policy` 会请求新审批并由调用方上报新项 |
| R2 | `delivery.abandoned`；`delivery.stage_changed` 且 `data.to ∈ {closed, abandoned}` | 该交付单元所有 open 项 | `DeliveryAbandoned` / `DeliveryClosed`（覆盖"有人直接在 GitHub 上合并或关闭"的外部变化，交付单元进终态由 `engine` 依 `forge.*` 完成，[prototype §2.8](../prototype.md)） |
| R3 | `RaiseRequest.supersedes` | 列出的 open 项 | `Superseded{by}` |
| R4 | 调用方显式 `resolve` | 目标项 | 调用方给出 |

不做的：不因阶段推进（如 `spec → plan`）猜测性关闭 `spec_needed`——那是 `conversation` / `engine` 通过 R4 明确表达的事。`forge.*` 事件不直接触发关闭（避免与 `engine` 对同一事实的两套解读）；`origin = self` 的 ForgeEvent 不产生任何通知，本模块结构上满足（[forge-actions §4.5](../interfaces/forge-actions.md#45-origin-对各消费者的含义)）：通知只由 `inbox.raised` 驱动。

### 7.6 计数

`summary()` 返回 `InboxSummary{total, by_project: [{project_id, open, by_severity}], no_project}`，全部来自 `status = 'open'` 的投影（store 的 `inbox_summary`），与 `list(status=open)` 必须一致；它是 `GET /v1/inbox/summary` 的响应体（P-63）。

- 客户端用 `GET /v1/inbox/summary` 得到初值（手机档案可用），再按 `inbox.raised`（+1）、`inbox.resolved` / `inbox.dismissed`（−1，按事件信封的 `project_id` 归属）增量维护；`log_epoch` 变化（备份恢复，P-67）时重新取初值——所以三类事件的信封 `project_id` 必须等于该项的 `project_id`，且 `inbox.resolved` / `inbox.dismissed` 的 `data` 要能定位 `inbox_id`。
- 顶栏"需要你 · n" = `total`（含全局项）；项目 tab 徽标 = `by_project[prj]`。`info` 级也计入（[prototype §2.1](../prototype.md)："open 事项数"）。
- 内部消费者（`SettingsStatus` 之类）直接调用 `summary()`。

### 7.7 通知管线

调度器是 `gantryd` 内的一个任务，**只由事件日志驱动**（不由 `raise` 直接调用渠道），所以 `raise` 的事务一旦提交，崩溃重启后通知也不会丢：

1. 从 `notify_cursor` 之后读事件；对每个 `inbox.raised`：
2. **路由评估**：取当前 `notify.routes`（每次发送读最新，[config.md §2.6](../interfaces/config.md#26-热加载语义)），命中 `kinds` 的路由的 `channels` 取并集，只保留**服务端渠道**（`mobile_push`，M4），过滤掉未启用或不可用的渠道。无渠道 → 记 `notify_dispatch(status=suppressed, reason=no_route)`，结束（收件箱项照常存在；桌面渠道由客户端处理，§7.8；V18 已对紧急类无渠道给 `urgent_unrouted` 警告）。
3. **文案**：§7.7.1。
4. **合并与限速**：同一 `(渠道, 项目)` 在 `coalesce_window_s` 内到达的多项——第一项立即发，其余合并为窗口结束时的一条"另有 n 件事需要你处理"；每渠道每小时超过 `rate_cap_per_hour` 的部分只记 `suppressed(rate_limited)`，不发送（项仍在收件箱）。数值见 §10。
5. **发送**：`Notifier::send`；`SendError` 可重试则指数退避（2 s 起，上限 300 s，至多 5 次，超过 `fresh_until` 即放弃，过期的提醒无意义）；永久错误直接记 `failed`。
6. 在 `notify_dispatch` 写最终结论（不发事件，P-63），推进 `notify_cursor`。重放时先查 `notify_dispatch`，已有终结论则跳过（at-least-once 发送，但同一 `(inbox_id, channel)` 的终结论只写一次）。
7. 从备份恢复后（`log_epoch` 加一，P-67）`notify_cursor` 可能大于事件日志 head：钳制到 head，**不重发**历史通知（只处理 `fresh_until` 未过期的项）。

#### 7.7.1 文案（I5）

- 标题固定 `Gantry · {owner/repo}`（全局项为 `Gantry`）；正文由**每个 `kind` 的固定模板 + 可信事实**生成，事实只取：`kind`、`severity`、`Ref` 里的编号（`PR #12` / `Issue #5`，来自交付单元绑定）、`reason_code`、`dimension` 这类枚举。**不使用** `summary`、`UntrustedText`、交付单元标题、PR / Issue 标题、finding 标题。

| `kind` | 正文模板 |
| --- | --- |
| `approval_needed` | `{ref} 需要你的审批` |
| `decision_needed` | `{ref} 有一项决定需要你做` |
| `spec_needed` | `{ref} 的规格需要补充` |
| `budget_exhausted` | `{ref} 的预算已用完（{dimension}）` |
| `escalation` | `{ref} 需要你处理` |
| `security_finding` | `发现安全问题，需要你查看`（不含 finding 内容） |

- 可选润色：`TextComposer` 用 `classify` 角色，**输入只有上述可信事实**，超时 `compose_timeout_ms` 或失败 → 模板；输出校验为纯文本、长度上限、不含 URL / Markdown / `@`，不通过也回退模板。润色不得决定路由、严重度或是否发送。
- 渠道看不到的东西：推送正文绝不含仓库内容、代码、密钥或不可信文本（APNs 经 Apple，桌面通知可能显示在锁屏 / 投屏，二者都是信息外泄面）。

### 7.8 桌面系统通知（M2，客户端触发，P-63）

服务端没有办法直接弹桌面通知，由已连接的 `desktop-shell` 弹；**触发源就是 `inbox.raised`，不新增事件**（P-63）：

1. `desktop-shell` 收到**实时**的 `inbox.raised`，且 `item.severity ≥ warn`、服务端 `notify.channels.desktop.enabled = true` 且 `notify.routes` 对该 `kind` 命中 `desktop`（客户端经设置路由读取生效值并随 `settings.updated` 刷新）、本机开关打开时弹窗；点击后按 `inbox_id` 定位对话气泡 / 检视器。
2. 断线重连回放的旧事件**不弹**（以事件 `ts` 与本地接收时刻判断，超过 `desktop_fresh_s` 即视为回放），其内容并入"上次查看 / 离开期间摘要"（[prototype §2.3](../prototype.md)）；`log_epoch` 变化后的全量重同步同样不弹。
3. 文案由客户端按 §7.7.1 的同一组模板生成（只用 `kind`、`severity`、`Ref` 编号等可信字段，不用 `summary`）。
4. 桌面端关闭 = 没有人弹窗；上报仍在收件箱，重新打开后可见。**桌面关闭时的离线提醒不存在**，这是 Q-07 的现状（M4 之前无解，N-10）。

### 7.9 APNs 渠道（M4）**[提议，细节以 Apple 官方文档为准，需核实]**

- 传输：HTTP/2 到 APNs（生产 / 沙箱两个端点由配置选择）；令牌式认证（用 `.p8` 私钥签 ES256 JWT，复用而不是每次重签，刷新间隔以 Apple 文档为准）。`gantryd` 直接出站，不经 Egress（Egress 服务沙盒与 `models`，architecture §3）。
- 载荷：只放模板文案、`inbox_id`、`project_id`、`kind`、`badge`（当前 open 总数）；**≤ 4 KB 且不含 `summary` / 不可信文本**。`apns-collapse-id` 用 `inbox_id` 或项目合并键，使合并通知替换旧通知；`apns-expiration` 取 `fresh_until`。
- 令牌：存 `push_tokens`；APNs 返回"令牌失效 / 已注销"→ 立即标记 `invalidated_at` 不再发送；**设备被吊销（`device.revoked` 事件）→ 立即清除该设备全部令牌**。日志里令牌只记前后几位。
- 谁收：`platform = ios` 且 `status = active` 的设备；路由决定 `kind` 是否推送，不按角色过滤（手机不能签名的 `approval_needed` 仍可提醒，点开后文案写明需在桌面处理，[prototype §4](../prototype.md)）。
- 凭据：APNs 密钥属于 `SecretRef`，设置键、读取方清单、令牌注册路由、以及"运营者自备 Apple 开发者账号直连还是经中继"都未定（N-11）。

### 7.10 并发与持久化小结

- 所有状态变化走 `store` 事务；`notify` 自己不持有跨请求内存状态（合并窗口计时器丢了也只是多发 / 晚发一条，由 `notify_dispatch` 兜底去重）。
- 调度器单实例（`gantryd` 单进程）；如将来多实例，需要以 `notify_cursor` 行加租约，本版不做。

## 8. 不变式与安全要求

| 不变式 | 本模块如何保证 |
| --- | --- |
| I2 GitHub 写入唯一路径 | `notify` 不调用任何 GitHub 写接口。若将来实现 `github_mention` 渠道，必须入队 `forge` Outbox（`create_comment`，[forge-actions §3.1](../interfaces/forge-actions.md#31-总表)），并要处理出站净化会中和 `@` 提及的冲突（§2.4，N-10） |
| I3 放行唯一决策点 | `choose` 拒绝 `approval_needed`，审批只经签名路由；`dismiss` 不改变交付单元状态，`engine` 不得依赖收件箱状态推进；自动关闭 R1 只跟随 `policy` 的审批状态，不反向影响 |
| I4 状态各有事实源 | 收件箱是本地事实源（事件日志 + 投影）；R2 跟随 `engine` 对 GitHub 事实的解读，不自行读 GitHub |
| I5 外部文本不可信 | `SafeSummary` 只能由模板或 `UntrustedText::to_summary()`（截断、去控制字符 / 双向控制符、折叠换行）构造，类型系统上禁止把 `String` 直接当 `summary`；通知文案只用可信事实（§7.7.1）；沙盒来源的 `escalate` 提议（[worker-protocol §5.4](../interfaces/worker-protocol.md)）进入收件箱前由调用方包装；日志里的 `summary` 截断并转义 |
| I6 客户端不持凭据 | APNs 密钥只在服务端（经 `secrets`，P-59）；推送令牌是设备标识不是凭据，但同样不进日志全文；`inbox.*` 事件、通知里没有任何凭据 |
| I7 配置与工作分离 | 路由 / 渠道只经 `notify` 设置资源（operator 起写，手机档案无 settings）；`dismiss` / `choose` 的请求体不含配置字段（api 层 `422 unknown_field`）；本模块的数值常量（§10）不进设置页 |

补充安全要求：

- **防注意力 DoS**：沙盒可通过 `escalate` 提议诱发上报。上限由上游（`sandbox.io.artifacts.proposals_max_count`，[config.md §7](../interfaces/config.md#7-sandbox)）与去重共同限制；`notify` 额外对每个 `(渠道, 项目)` 做合并与每小时上限（§7.7），保证洪水不会变成推送洪水。
- **不吞上报**：校验失败（`InvalidPayload`）、存储不可用时 `raise` 返回错误，调用方必须处理（重试或自行回退到 `escalation` 的日志告警），**不得静默丢弃**。
- **note 是用户文本**：限长（§10），原样存、原样回显，不当作指令；`engine` 把它带入 prompt 时仍须按外部文本包装。

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| `raise` 时 payload 不符合 `kind` 的 schema | `Err(InvalidPayload)`，不落库；调用方记错误并重试 / 上报；日志带 `kind` 与校验路径 |
| 并发 `raise` 同一 `dedup_key` | 部分唯一索引兜底，失败方重读返回 `Deduped` |
| `raise` 提交后进程崩溃 | 事件与投影同事务，不会有"有投影无事件"；通知由调度器依 `notify_cursor` 补发 |
| 事件已写、通知未发时崩溃 | 重启后调度器从 `notify_cursor` 继续；`notify_dispatch` 去重 |
| 渠道临时失败（APNs 5xx / 429、网络） | 指数退避，至多 5 次，超过 `fresh_until` 放弃并记 `failed`；不影响收件箱 |
| 渠道永久失败（令牌失效、凭据缺失） | 令牌失效 → 清除令牌；凭据缺失 → 渠道 `availability = unavailable`，`modules.notify.health = degraded`，`detail` 说明；路由命中该渠道时记 `suppressed(channel_unavailable)` |
| `TextComposer` 超时 / 出错 / 输出不合规 | 回退模板，记指标，不阻塞发送 |
| `notify` 设置缺失 / 非法 | 以内置默认（N-09）运行并置 `health = degraded`（设置写入已被校验，非法只可能来自版本升级后的 schema 变化）；不得因此停写收件箱 |
| 从旧备份恢复 | 投影由事件重建；`notify_cursor` 钳制到 head；R1 / R2 在回放中把已失效的项关闭；不重发旧通知；客户端因 `log_epoch` 变化全量重取 `GET /v1/inbox/summary`（P-67） |
| 两台设备竞争 `choose` / `dismiss` | 先提交者胜，后者 `409 conflict`（`invalid_state`）；`Idempotency-Key` 重放由 `api` 处理 |
| 事件订阅断档（`store` 订阅失败） | 调度器与自动关闭器重连后从各自游标继续；游标落后过多时告警（指标 `notify_lag_events`） |

## 10. 配置

**设置（settings）**：`notify.channels.<kind>.enabled`、`notify.routes.<id>`，语义、校验 V18、热加载（每次发送读最新）、不可项目覆盖，均以 [config.md §9、§11、§2.6](../interfaces/config.md#9-notify) 为准，本规格不重复。`mobile_push` 在 M4 之前是保留 kind（schema 不接受）。

**内部常量**（不是设置，I7；做成服务端静态常量，可由引导配置覆盖，不散落在代码里）：

| 常量 | 默认 | 说明 |
| --- | --- | --- |
| `summary_max_bytes` | 1024 **[建议值，待调优]** | `SafeSummary` 上限，超出截断并标 `…` |
| `note_max_bytes` | 2048 **[建议值，待调优]** | `choose` / `dismiss` 的 `note` 上限；超出 → `422`（`errors[].code = range`） |
| `coalesce_window_s` | 10 **[建议值，待调优]** | §7.7 合并窗口 |
| `rate_cap_per_hour` | 60 **[建议值，待调优]** | 每 `(渠道, 项目)` |
| `desktop_fresh_s` | 120 **[建议值，待调优]** | 客户端判定"实时"`inbox.raised` 的窗口（§7.8） |
| `compose_timeout_ms` | 3000 **[建议值，待调优]** | 润色超时 |
| `retry_base_s` / `retry_cap_s` / `retry_max` | 2 / 300 / 5 **[建议值，待调优]** | 渠道发送重试 |
| `apns.expiration_s`（M4） | 3600 **[建议值，待调优]** | `apns-expiration` |

内置默认路由（`notify` 资源从未写过时）未定，见 N-09；本规格的临时处理：渠道 `desktop` 启用，紧急类（`approval_needed` / `security_finding` / `budget_exhausted` / `escalation`）与普通类（`decision_needed` / `spec_needed`）都路由到 `desktop`，即 [config.md §12.1](../interfaces/config.md#121-全局配置) 示例。

## 11. 可观测性

**事件**：`inbox.raised` / `inbox.resolved` / `inbox.dismissed`（data-model §5.2 已有）。不新增通知事件（P-63）；服务端渠道的发送结论只在 `notify_dispatch` 与指标里。

**日志**（结构化，不含 `summary` 全文）：`inbox_id`、`kind`、`severity`、`dedup_key` 的哈希、`outcome`（created / deduped / resolved / dismissed / chosen）、`actor`；通知：`channel`、`status`、`attempts`、耗时。不可信文本截断并转义后才允许进日志。

**指标 [提议]**：

| 指标 | 类型 | 标签 |
| --- | --- | --- |
| `gantry_inbox_open` | gauge | `kind`、`severity` |
| `gantry_inbox_raise_total` | counter | `kind`、`outcome`（created / deduped / invalid） |
| `gantry_inbox_transition_total` | counter | `to`（resolved / dismissed）、`resolution` |
| `gantry_notify_sent_total` | counter | `channel`、`status`（sent / failed / suppressed）、`reason` |
| `gantry_notify_latency_seconds` | histogram | `channel`（`inbox.raised` 提交到服务端渠道发送完成） |
| `gantry_notify_lag_events` | gauge | — |
| `gantry_notify_compose_total` | counter | `result`（model / template / fallback） |

**健康**：`GET /v1/settings/status` 的 `modules.notify` = `{applied_rev, health, detail?}`（`ok` / `degraded` / `unhealthy`，[config.md §2.8](../interfaces/config.md#28-catalog-与-status-的响应结构)）；渠道不可用、调度滞后过大时 `degraded`，存储不可写时 `unhealthy`。`catalog.notify.channels` 由各 `Notifier::availability()` 汇总。

## 12. 测试与验收

用 `FakeInbox` / 内存 `store` / `FakeNotifier` / 可注入时钟；`Notifier` 提供一套可复用的一致性测试，`ApnsNotifier`（M4）要过；`FakeInbox` 与真实 `InboxPort` 实现共用一套一致性用例。

**创建与去重**

- [ ] `raise` 一次 → 一个 `inbox.raised`、投影一行 `open`，`Written.last_seq` 等于该事件 `seq`。
- [ ] 同一 `dedup_key` 在 `open` 期间再次 `raise` → `Deduped`，无新事件、无新通知；返回同一 `inb_`。
- [ ] 50 个并发任务 `raise` 同一 key → 恰好一个 `Created`，其余 `Deduped`。
- [ ] `resolved` 或 `dismissed` 之后同一 key 再 `raise` → 新 `inb_`。
- [ ] `supersedes` 在同一事务里把旧项置 `resolved(superseded)` 并创建新项；中途失败则两者都不变。
- [ ] 六个 `kind` 的非法 `payload`（缺字段、多字段、类型错）→ `InvalidPayload`，不落库；合法 `payload` 通过。
- [ ] 模拟"事件已追加但投影更新失败"的故障注入：事务整体回滚，不出现只有一半的状态。
- [ ] 并入调用方事务（`tx = Some`）：调用方事务回滚时上报也回滚。

**choose / dismiss / resolve**

- [ ] `choose` 合法选项 → `resolved`，`inbox.resolved{resolution:"chosen", option_id}`，`engine` 的 fake 订阅者收到；`note` 原样记录。
- [ ] `choose` 的 `option_id` 不在 `payload.options[]` → `OptionUnknown`（映射 `422`，`pointer=/option_id`）；`payload` 无 `options` 时同样；`options[]` 元素形状为 `{id, label, primary?}`，多余字段或缺 `label` → `InvalidPayload`（P-63）。
- [ ] `choose` 对 `approval_needed` → `UseApprovalRoute`（`409`）；对已 `resolved` / `dismissed` → `NotOpen`（`409`）。
- [ ] 手机档案：`choose` 对 `spec_needed`、`decision_needed` 成功，对 `escalation`、`approval_needed` → `ProfileForbidden`；桌面档案对 `escalation` 成功。
- [ ] 两台设备同时 `choose` 不同选项 → 恰好一个成功，另一个 `409`，事件只有一条 `inbox.resolved`。
- [ ] `dismiss`：`open → dismissed` 产生 `inbox.dismissed`；再次 `dismiss` 返回原项、无新事件；对 `resolved` → `NotOpen`。`dismiss` 之后交付单元状态未变，`engine` fake 未被调用。
- [ ] `resolve(Answered{message_id})`、`resolve` 对已终态项 → 幂等无事件。

**自动关闭**

- [ ] R1：`approval.granted` / `denied` / `invalidated` / `expired` / `consumed` 各自把对应 `approval_needed` 置 `resolved(approval_settled)`，且只关闭 `approval_id` 匹配的项。
- [ ] R2：`delivery.abandoned`、`delivery.stage_changed → closed` 关闭该交付单元全部 open 项，不影响其他交付单元。
- [ ] 自动关闭从不产生通知；`forge.pr_merged`（`origin=self`）不产生任何通知。
- [ ] 事件重放两次，结果相同（幂等）。

**计数与查询**

- [ ] 3 个项目、各类 `kind` / `severity` 混合：`summary()`（`GET /v1/inbox/summary`）的 `total`、`by_project`、`by_severity` 与 `list(status=open)` 逐项一致；全局项只计入 `total` 与 `no_project`；手机档案可调用（P-63）。
- [ ] 经 `inbox.raised` / `inbox.resolved` / `inbox.dismissed` 事件增量维护的客户端计数，与 `summary()` 在任意操作序列后一致（属性测试）；每个 `inbox.*` 事件信封的 `project_id` 等于 `item.project_id`。
- [ ] `list` 的游标分页稳定：分页期间插入新项不重复、不漏；默认 `status=open`；`project_id` / `kind` 过滤正确。

**通知管线（M2；服务端渠道用 `FakeNotifier` 验收）**

- [ ] 路由命中服务端渠道 → `FakeNotifier` 收到一次发送，`notify_dispatch` 记 `sent`；未命中或渠道关闭 → `suppressed`，收件箱项仍存在；任何情况下都不产生新事件类型（P-63）。
- [ ] 修改 `notify.routes` 后下一次发送使用新路由，无需重启。
- [ ] 窗口内连续 5 项 → 第一项立即通知，其余合并为 ≤ 1 条追加通知；超过 `rate_cap_per_hour` 的部分 `suppressed(rate_limited)`。
- [ ] 崩溃注入：`raise` 提交后、通知发送前杀进程 → 重启后恰好发送一次；`notify_cursor` 超前于 head（模拟备份恢复）→ 不重发。
- [ ] `TextComposer` 超时 / 异常 / 输出含 URL 或 `@` → 回退模板；回退后文案仍符合 §7.7.1 模板。
- [ ] 注入测试：`summary`、PR 标题、finding 标题含 `@everyone`、Markdown 链接、`javascript:`、ANSI 转义、RTL 覆盖字符、超长串——通知标题与正文**不含**其中任何一段；`SafeSummary` 构造去掉控制符并截断。
- [ ] 渠道永久失败（令牌失效 / 凭据缺失）→ 渠道 `unavailable`，`modules.notify.health = degraded`，不阻塞 `raise`。
- [ ] `desktop-shell` 契约测试（与 `fe-desktop` 联调）：实时 `inbox.raised` 且 `severity ≥ warn` 弹窗；`severity = info` 不弹；超过 `desktop_fresh_s` 的回放事件不弹；本机开关关闭不弹；服务端 `notify.channels.desktop.enabled = false` 时本机开关打开也不弹（P-63）。

**集成（与 `api`）**

- [ ] 路由 #21 #22 #23 #42 的成功与错误对照 §6.2 全部命中（`404` / `409` / `422` / `403 forbidden_profile` / 幂等重放）；`inbox.*` 事件对手机档案可见、`settings.*` 不可见。
- [ ] 含 `Idempotency-Key` 的 `choose` 重放：只产生一条 `inbox.resolved`。

**APNs（M4）**

- [ ] 载荷 ≤ 4 KB，且不含 `summary` 与不可信文本（用注入串断言）；`apns-collapse-id`、`apns-expiration` 按 §7.9 设置。
- [ ] APNs 返回令牌失效 → 令牌被标记失效、后续不再发送；`device.revoked` → 该设备全部令牌立即清除，之后零推送。
- [ ] 5xx / 429 → 退避重试，过期放弃；JWT 复用与刷新符合 Apple 的间隔要求（以官方文档为准）。
- [ ] 手机端收到 `approval_needed` 推送，点开后文案写明需在桌面签名（milestones M4 退出标准"手机端能收到上报通知"）。

## 13. 开放问题

骨架没定、本规格给了临时处理的一律在此；实现者不得自行拍板，需要时开 Issue。

| # | 问题 | 本文的临时处理 | 影响 |
| --- | --- | --- | --- |
| N-01 | **已决：见裁决 C17 / C5（P-63、P-51）：`notify` 归 `be-engine` 泳道（M1 上报与计数），APNs 归 M4 `ops`；`FakeInbox` 为 M0 替身**。原问题：`notify` 没有被 milestones 的任何泳道登记；M1 退出标准要求"拒绝并上报"、审批要进收件箱，M2 `fe-desktop` 有收件箱 / 系统通知，M4 的 APNs 归 `ops` 泳道（`fe-mobile` 才有手机端）。M0 替身清单也没有 `FakeInbox` / `FakeNotifier` | 已落实到文首与 §1 里程碑表 | milestones、`contract` |
| N-02 | **已决：见裁决 C17 / P-63（`InboxItem` 增加 `project_id` 与 `dedup_key`）；`inbox.*` 事件 `data` 形状仍按 §7.3 草案待契约 PR**。原问题：data-model 的 `InboxItem` **没有 `project_id`**，但 `GET /v1/inbox?project_id=`、项目 tab 徽标、事件信封的 `project_id` 都需要；`delivery_id?` 可空的项（如 App 缺权限）无法归属项目。也没有 `dedup_key`，`inbox.*` 事件的 `data` 形状（尤其 `inbox.resolved` 对系统关闭的 `resolution`）未定义 | 已落实到 §6.1、§7.2、§7.3 | `contract`、`api`、`desktop-ui` |
| N-03 | **部分已决：见裁决 C17 / P-63（`escalation`、`spec_needed`、`decision_needed` 的 `payload.options[]` 为 `{id, label, primary?}`）；选项 `id` 与 `reason_code` 词表仍归创建者规格，预算耗尽后的选项仍未决**。原问题：data-model 的 `escalation`、`spec_needed`、`budget_exhausted` payload 都**没有 `options[]`**，但 api.md §4.8 / P-35 / P-40 让 `choose` 作用于 `escalation`（升档）与 `spec_needed`（选择题）；`Card.choice.options[]` 是 `{id,label}`，api.md 另写 `primary?`。选项 `id` 词表（`abandon` / `replan` / `upgrade`…）与 `reason_code` 词表归谁未定。预算耗尽后用户能做什么也未定（config OQ-10） | `choose` 只认 `payload.options[]`；没有则 `OptionUnknown`；选项语义归创建者规格 | `engine`、`guardian`、data-model |
| N-04 | **已决：见裁决 C17 / P-63（桌面系统通知由 `inbox.raised`（`severity ≥ warn`）触发，不新增事件；`inbox.notified` 提议撤回）**。原问题：**桌面系统通知的触发机制在任何契约里都没有**：config 说"服务端 `notify.channels.desktop` 决定是否发出"，但事件目录里只有 `inbox.*`。本规格提议新增 `inbox.notified`（含 `fresh_until`）；备选是 `desktop-shell` 对每个 `inbox.raised` 都弹窗（与 config 的两层开关语义冲突） | 已落实到 §7.8 | data-model、`desktop-shell`、api.md §5.4 |
| N-05 | **已决：见裁决 C13 / P-59（`notify` 依赖 `models`（可选，打杂模型生成通知文案）与 `secrets`（M4 APNs 密钥））**。原问题：architecture §4 中 `notify` 只依赖 `core`、`store`，但 config / architecture §9 规定通知文案由 `utility`（`classify`）生成，需要 `models` | 已落实到 §3 | architecture §4 |
| N-06 | **已决：见裁决 C17 / P-63（新增 `GET /v1/inbox/summary`，按项目的 open 计数，手机可用）**。原问题：没有计数路由；客户端只能分页取完 open 项再增量维护 | 已落实到 §6.1、§7.6 | api.md、`desktop-ui`、`mobile` |
| N-07 | `dismiss` 的边界：能否忽略 `approval_needed`（审批仍 `pending` 时项消失）或 `blocker`；`dismissed` 之后同一问题再次出现是否抑制一段时间（D-04 vs "不吞上报"）；`dismiss` 是否应解除 `waiting_human` | `dismiss` 不改交付单元状态；对所有 kind 允许；不抑制 | `engine`、`desktop-ui` |
| N-08 | 静默处理是否进收件箱：design.md 写"控制台收件箱记录处理与跳过的理由"，config.md §6.2 也说"不影响收件箱与事件日志里的审计记录"，但 `InboxItem.kind` 没有对应类型，原型把它放在检视器的"后台守护"区块 | 静默处理的审计只走事件日志，不创建 `InboxItem` | `guardian`、config.md |
| N-09 | `notify` 资源的内置默认（路由、渠道）未定；按 `severity` 路由、免打扰时段不支持（config OQ-15） | §10 的临时默认 | config.md、`desktop-ui` |
| N-10 | **Q-07 离线通知渠道**：M1–M3 桌面端关闭时没有任何提醒。`email`、`github_mention` 是保留 kind；若用 `github_mention`，必须走 Outbox（I2），而 `forge` 出站净化会把 `@user` 用反引号中和（forge-actions §2.4），二者冲突；推荐的"手机端推送"在 M4 | 不实现；仅预留 `Notifier` 扩展点 | `forge`、config.md、产品 |
| N-11 | **APNs（M4）**（(b) 中"`secrets` 读取方没有 `notify`"**已决：见裁决 C13 / P-59**，其余未决）：(a) 推送令牌注册路由与存储未定（api.md OQ-2；Device 无该字段；手机档案白名单要加这条路由）；(b) APNs 密钥与参数（Team ID、Key ID、topic、环境）没有设置键；(c) 自托管下运营者要有 Apple 开发者账号并自签 iOS 应用才能直连 APNs，否则需要中继服务——两种路线未定；(d) 推送文案可含哪些信息（仓库名？PR 编号？） | §7.9 的草案；全部待决 | api.md、config.md、`secrets`、`mobile`、`ops` |
| N-12 | **已决：见裁决 C26（api §3.3 必带幂等键清单加 `choose`）；`choose` 对 `approval_needed` 的 `409` `reason` 取值仍待契约确认**。原问题：api.md 小不一致：§3.3 "必带 `Idempotency-Key`" 清单没有 `choose`，而 §4.1 #42 标"必"；OQ-9 的"只有 dismiss"已过时；`choose` 对 `approval_needed` 返回 `409` 但 `reason` 未定 | 必带幂等键；`reason: invalid_state` | api.md |
| N-13 | **已决：见裁决 C17 / P-63（升档一律用 `escalation`，手机档案不可 `choose`）；`reason_code` 与选项词表由 `engine` 规格定义**。原问题："需要升档"用哪个 `kind`：架构 §8 与原型都有此上报，api.md 暗示是 `escalation`（会增加花费，手机不可选）；其 `reason_code`（如 `level_cap_exceeded`）与选项 `upgrade` / `keep_scope` / `abandon` 由谁定义；升档是否需要 approver（P-36 说是控制，operator 即可） | 已落实到 §7.2、§7.4 | `engine`、api.md |
| N-14 | **已决（`store` 规格已存在：`WriteTx` 实现 `core::ports::TxHandle`，持久游标见 store.md §6.2）**。原问题：`store` 规格尚未存在：调用方事务如何传入（`StoreTx`）、事件订阅是否支持持久游标 + 至少一次 + 回放 | 已落实到 §3、§6.1 | `store` |
| N-15 | 存量上报项的保留与归档：投影里 `resolved` / `dismissed` 项是否永久保留 | 永久保留（事件日志本就永久） | `store`、`deploy`（备份体积） |
| N-16 | 自由文本答复由 `conversation` 判断解决了哪些 open 项；消息与上报项的关联靠 `Message.refs` / `cards`，而 `Ref.kind` 没有 `inbox` | `conversation` 依 `delivery_id` + `kind` 调 `resolve(Answered)`；是否给 `Ref` 加 `inbox` 待定 | `conversation`、data-model |
