# 事件存储（`store`）

> 状态：设计 v0.1 · 里程碑：M0（冻结接口与 `Store::open_in_memory`，P-51）、M1 · 泳道：`be-engine` · crate / 目录：`crates/gantry-store`

`store` 是服务端唯一的持久化边界：追加式事件日志（event log）是编排状态的事实源（I4），投影（projection）是它在同一事务里维护、可整体重建的视图，Outbox 表保证 GitHub 写动作"先落盘、后执行"（I2），内容寻址存储放尝试结果和被拒产物。其他模块不直接接触 SQLite。

读者：实现 `store` 的 agent；以及 `engine`、`forge`、`policy`、`notify`、`pki`、`repo`、`api` 的实现者（读 §6、§7.4、§7.6 即可）。

标注 **[提议]** 的是骨架没定、本文给的默认；**OQ-n** 见 §13，不要自行拍板。契约优先级见 [AGENTS.md](../../AGENTS.md)。

## 1. 职责

1. **追加式事件日志**：全局单调递增的 `seq`（[data-model §5.1](../data-model.md#51-信封)）；只追加（数据库触发器强制，应用代码也没有 UPDATE / DELETE 的入口）；事件 `ts` 在 `seq` 序下非递减；任意事件可带可选的 `dedup_key`，唯一索引兜底（[forge-actions §6.1 G1](../interfaces/forge-actions.md#61-保证) 要求"`store` 唯一索引兜底"，`conversation` 的消息去重也用它）。
2. **投影**：事件追加与投影更新在**同一个事务**里完成（data-model §5.1）。投影是事件的确定性折叠，可全量重建；追加时调用 `core` 的状态机，**非法转移直接拒绝**，使日志里不可能出现非法状态序列。`store` 内置 data-model 全部实体的投影器；其他模块可注册自己的投影器去维护 `<module>_*` 表（§7.10）。
3. **读一致性**：一次读取拿到"数据 + 该快照已反映到的最大 `seq`"（[api.md §3.6](../interfaces/api.md#36-读一致性x-event-seq)）。
4. **事件流原语**：带过滤的 `scan`（含"已扫描到的 `seq`"，供心跳推进过滤后的游标）、回放加实时无缝衔接的 `tail`、游标超前检查（`cursor_ahead`）；内部消费者的持久游标；列表接口用的键集分页（keyset pagination）游标。
5. **Outbox 表**（architecture §4）：幂等入队、原子认领 + 租约、完成 / 失败 / 退避 / 取消、依赖级联、同槽合并、崩溃后重放（[forge-actions §2](../interfaces/forge-actions.md#2-outbox-通用规则)）。**执行**不归 `store`，归 `forge`。
6. **settings 存取**：按 `(scope, resource)` 保留每一个 rev 的文档；`rev` 即 `settings.updated` 的 `seq`；计算 `config_rev(project)`（[config.md §2.3](../interfaces/config.md#23-存储事件版本追溯)）。
7. **内容寻址存储（content-addressable storage, CAS）** **[提议，OQ-3]**：`Attempt.result_ref` 指向的结果包、进度记录、被拒产物（architecture §6）。
8. **模块私有表、迁移与投影器注册** **[提议，OQ-2、OQ-4]**：同一个 SQLite 文件里，其他模块（`forge` 的 `forge_webhook_inbox`、`policy` 的 `policy_challenges`、`pki`、`notify` 的表等）注册自己的迁移和（可选的）投影器，在同一事务里与事件一起写。
9. **备份、恢复与完整性校验**：一致性快照、投影与事件的一致性检查、投影全量重建。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 冻结 §6 的公开签名，并提供内存实现 `Store::open_in_memory`（`testing` feature，裁决 C5 / P-51 列为 M0 替身），让其他泳道并行开发；`store` 自己的完整实现从 M1 开始 |
| M1 | `be-engine` 泳道明确要求"事件日志、投影、SSE 游标"。本文展开为：§1 的 1–6、8、9 全部；M1 用到的实体投影（项目、对话、消息、交付单元及 Issue / PR 快照、工作流 / 节点 / 尝试、工作树及锁、沙盒、收件箱、审批、设备、预算台账、settings）；Outbox 全部操作（`coalesce_key` 除外；失败语义按 P-53）；会话台账（`BudgetLedger.subject = conversation`，P-58）；收件箱按项目的 open 计数（`GET /v1/inbox/summary`，P-63）；CAS 的 `put` / `get` / `has`；SQLite 备份；启动恢复与 `log_epoch`（P-67）；`Store::open_temp` / `open_in_memory`；崩溃注入测试 |
| M2 | 随上层模块推进 [提议，milestones 未单列]：Outbox 的 `coalesce_key`（状态清单评论，guardian）与优先级调度；审计查询 `audit`（`sec` 的审计日志）；审批失效 / 过期的到期扫描查询；多节点图所需的预算计数全量启用 |
| M3 | 图改写：`workflow.revised`、`node.superseded` 的投影与 `include_superseded` 查询；settings 历史版本列表（config.md OQ-25）；CAS 的隔离区与保留期（取决于 OQ-3） |
| M4 | 父子交付单元：`parent_id` / `depends_on` 查询、父交付单元预算台账汇总；手机档案的事件过滤（`EventFilter.exclude` 已满足，无需新接口）；通知所需的推送令牌表（经 OQ-4 的模块私有表） |

## 2. 不负责

| 事 | 归谁 |
| --- | --- |
| 状态机、事件类型与 `data` 形状、ID、`IdempotencyKey` 的定义 | `core`；`store` 调用它们校验与折叠 |
| 事件**语义**：什么时候该发什么事件、谁有资格发 | 各发出方模块（`engine`、`forge`、`policy`…）；`store` 只校验形状与状态转移 |
| SSE 帧编码、心跳、`close` 帧、按角色 / 档案过滤、`Retry-After` | `api`；`store` 只给 `scan` / `tail` / `check_cursor` 原语 |
| GitHub 访问、webhook 验签、对账、Outbox **执行**（probe、限流闸、令牌桶） | `forge`；`store` 只提供 `count_succeeded_since` 等数据原语 |
| 危险动作是否放行、审批验签、challenge / nonce 的业务规则 | `policy`、`api`；`store` 只保证审批状态机与"只能消费一次"的原子性 |
| 设备证书、CA、吊销表 | `pki`（表可借 OQ-4 的机制放同库） |
| 密钥值 | `secrets`；事件里只有 `SecretRef`（I6） |
| 沙盒产物的校验（白名单、补丁校验） | `sandbox`、`repo`；`store` 把 CAS 内容当**不透明字节**，不解析不执行 |
| 备份的调度、传输、保留 | `deploy` |
| 设备吊销的日志外副本 `revocations.journal` 及其恢复后重放 | `pki`（P-67）；`store` 只在 `restore_from` 后把 `log_epoch` 加一 |
| 事件和 CAS 的保留期 | 未定（OQ-5）；本文不删除任何事件 |

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `core` | `Id<K>`、`IdGen`、`Clock`、`Timestamp`、`Event` / `NewEvent` / `EventType` / `Seq`、`validate_event_data`、全部状态机（[core §6.4](core.md#64-状态机)）、`IdempotencyKey`、`jcs`、`Sha256Digest`、实体类型、`Resource` / `Scope` | [core.md §6](core.md#6-对外接口)、[data-model](../data-model.md) |
| 外部库 | `sqlx`（SQLite 特性，捆绑 SQLite ≥ 3.37 以使用 `STRICT` 表）、`tokio` | architecture §10 |

`store` 不依赖 `secrets`、`policy`、`engine` 等任何其他服务端模块（architecture §4 依赖图）。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 追加请求（`NewEvent`，经 `WriteTx`） | `engine`、`conversation`、`guardian`、`policy`、`notify`、`models`、`repo`、`sandbox`、`forge`、`pki`、`api` | [core §6.5](core.md#65-事件) | 调用方可信；`data` 里可含 `UntrustedText`（GitHub / 沙盒文本），**原样存储、不解释、不规范化** |
| Outbox 请求 | `engine`、`guardian`、`repo`（经入队）、`forge`（执行） | §6.3 | 同上 |
| 读请求与分页游标 | `api` 转发客户端的筛选与游标 | §6.2 | 游标**不可信**：必须严格解码并校验所属资源 |
| CAS 写入 | `sandbox`（collect 的产物）、`engine`（结果包清单） | 字节 | 沙盒产物**不可信**；存为不透明对象 |
| 引导配置 | `deploy` | §10 | 可信 |
| `Clock`、`IdGen` | 组装时注入 | [core §6.1](core.md#61-标识符时间基础值) | 可信 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| 已提交的事件（`Event`） | `api`（SSE）、`engine`、`guardian`、`notify`、`policy` 等订阅者 | `scan` / `tail`；信封见 data-model §5.1 |
| 提交后的 `head` 通知 | 进程内订阅者 | `watch::Receiver<Seq>` |
| 投影查询结果 | `api`、各模块 | §6.2；`X-Event-Seq` 来自同一快照 |
| Outbox 行 | `forge`（认领执行）、`engine`（读状态） | `OutboxAction`（data-model §3） |
| CAS 对象 | `engine`、`api`（未来读取路由，api.md OQ-11） | `Sha256Digest` → 字节 |
| 备份目录 | `deploy` 的备份脚本 | §7.8 |
| 日志与指标 | `ops` | §11 |

## 6. 对外接口

只写签名与语义。`core` 的类型见 [core.md](core.md)。

### 6.1 打开与事务

```rust
pub struct StoreDeps { pub clock: Arc<dyn Clock>, pub ids: Arc<IdGen> }

/// 其他模块在组装时交给 Store::open 的注册项（OQ-2、OQ-4）。
pub struct ModuleSpec {
    pub module: &'static str,                          // 表名前缀：<module>_*
    pub migrations: &'static [&'static str],           // 按序、只增；版本记在 meta
    pub projector: Option<Arc<dyn Projector>>,         // 可选：维护本模块的表
}
pub trait Projector: Send + Sync {
    fn module(&self) -> &'static str;
    fn version(&self) -> u32;                          // 变化 → 启动时重建该投影器的表
    fn handles(&self) -> &[TypePattern];               // 只收到命中的事件
    fn owns(&self) -> &[ProjectionEntity];             // 声明接管哪些 data-model 实体的投影；store 内置投影器对被接管的实体停用（OQ-2）
    /// 与事件追加同一事务；必须确定性：只用事件自身字段（含 ev.ts），不读时钟、不读环境、不做外部 IO。
    fn apply(&self, ev: &Event, conn: &mut ModuleConn<'_>) -> Result<(), ProjectError>;
    fn reset(&self, conn: &mut ModuleConn<'_>) -> Result<(), ProjectError>;   // 重建前清空自己的表
}

#[derive(Clone)]
pub struct Store { /* Arc<Inner> */ }
impl Store {
    /// 取得数据目录的排他文件锁（第二个实例得到 AlreadyRunning）；执行迁移；
    /// 校验"投影已应用到的 seq == 日志 head"，不等或任一投影器 version 变化则重建对应投影；
    /// 完成后才返回，之后可服务。
    pub async fn open(cfg: StoreConfig, deps: StoreDeps, modules: &[ModuleSpec]) -> Result<Store, StoreError>;
    pub async fn open_temp(deps: StoreDeps) -> Result<Store, StoreError>;        // 临时目录，Drop 时清理；测试用
    /// testing feature：SQLite 内存库（shared-cache URI），语义与 open 相同（单写者、快照读、投影、Outbox、CAS 放内存）。
    /// 即其他规格书里的 InMemoryStore；崩溃类测试需要 open_temp。
    pub async fn open_in_memory(deps: StoreDeps, modules: &[ModuleSpec]) -> Result<Store, StoreError>;

    pub async fn begin_write(&self) -> Result<WriteTx, StoreError>;         // 单写者，FIFO 排队
    pub async fn snapshot(&self) -> Result<ReadSnapshot, StoreError>;      // 读快照（见 §7.4）
    pub fn events(&self) -> EventLog;
    pub fn cursors(&self) -> Cursors;
    pub fn cas(&self) -> Cas;
    pub fn log_identity(&self) -> LogIdentity;                              // {log_id, log_epoch}；log_epoch 每次从备份恢复加一（P-67）

    pub async fn backup_to(&self, dest_dir: &Path) -> Result<BackupReport, StoreError>;
    pub async fn verify(&self, level: VerifyLevel) -> Result<VerifyReport, StoreError>;     // Quick / Full
    pub async fn rebuild_projections(&self) -> Result<RebuildReport, StoreError>;
    pub async fn close(self) -> Result<(), StoreError>;
}

pub struct WriteTx { /* 持有写锁与一个 BEGIN IMMEDIATE 事务 */ }
impl WriteTx {
    pub fn now(&self) -> Timestamp;  // 事务开始时取一次 Clock，保证同一事务内的时间一致且不早于上一个事件的 ts
    /// 校验 data → 校验状态转移 → 插入 → 折叠投影；每次 append 在一个 SAVEPOINT 里，失败只回滚这一次。
    pub async fn append(&mut self, ev: NewEvent) -> Result<Appended, StoreError>;
    /// 带乐观并发守卫：opts.expect_subject_seq = Some(Some(s)) 时，要求该 subject 当前最后一个事件的 seq 恰为 s；Some(None) 要求该 subject 还没有事件；
    /// 不符则 PreconditionFailed(SubjectMoved { actual })。engine 的 DeliveryActor 用它避免读-改-写之间被他人插入。
    pub async fn append_with(&mut self, ev: NewEvent, opts: AppendOpts) -> Result<Appended, StoreError>;
    pub async fn advance_cursor(&mut self, consumer: &str, seq: Seq) -> Result<()>;   // 与本事务的写入原子（conversation："追加消息 + 推进游标"）
    pub fn read(&mut self) -> TxReader<'_>;                 // 能读到本事务尚未提交的写入；实现 ProjectionRead
    pub fn outbox(&mut self) -> OutboxTx<'_>;
    pub fn module_conn(&mut self, module: &'static str) -> ModuleConn<'_>;   // OQ-4
    pub async fn commit(self) -> Result<Committed, StoreError>;   // Committed { first_seq, last_seq }；Drop 未提交 = 回滚
    pub async fn rollback(self) -> Result<(), StoreError>;
}
pub struct AppendOpts { pub expect_subject_seq: Option<Option<Seq>> }   // 外层 None = 不守卫；Some(None) = 要求该 subject 尚无事件
pub enum Appended { New(Event), Duplicate { existing_seq: Seq } }   // Duplicate 仅在 dedup_key 已存在时出现，且不产生任何副作用
```

调用约定（写进文档注释，评审时检查）：

- **一个逻辑操作一个事务**：状态变化 + 对应事件 + Outbox 入队放进同一个 `WriteTx`。
- **事务内不做外部 IO**（不调 GitHub、不调模型、不起子进程、不 `await` 无界的东西）。持有超过 `write_tx_max_hold_ms` 的事务被强制回滚并返回 `TxTimeout`。
- **先检查后写入都在事务内**：`If-Match` 的 rev 比较、审批可消费性、工作树锁，一律 `tx.read()` 读取后在同一事务里 `append`；单写者保证中间不会插入他人的写。
- 副作用（推送通知、触发后续动作）由订阅 `tail` 的消费者做，**不要在提交前后内联**。

### 6.2 读取

```rust
pub struct Page { pub limit: u32 /* 1..=200，默认 50 */, pub cursor: Option<PageCursor>, pub order: Order }
pub struct Paged<T> { pub items: Vec<T>, pub next_cursor: Option<PageCursor> }
pub struct PageCursor(String);   // 不透明；内含 资源标签 + 排序键 + 方向；跨资源复用或被篡改 → StoreError::InvalidCursor

/// ReadSnapshot 与 TxReader 实现同一 trait。
pub trait ProjectionRead {
    fn head_seq(&self) -> Seq;                                               // 与本 trait 其余读取同一快照
    // 项目与对话
    async fn project(&mut self, id: &ProjectId) -> Result<Option<Project>>;
    async fn project_by_repo(&mut self, repo: &RepoSlug) -> Result<Option<Project>>;
    async fn projects(&mut self, p: &Page) -> Result<Paged<Project>>;
    async fn conversations(&mut self, project: &ProjectId) -> Result<Vec<Conversation>>;
    async fn messages(&mut self, c: &ConversationId, p: &Page) -> Result<Paged<Message>>;
    // 交付单元
    async fn delivery(&mut self, id: &DeliveryId) -> Result<Option<Delivery>>;
    async fn deliveries(&mut self, q: &DeliveryQuery, p: &Page) -> Result<Paged<Delivery>>; // project_id、stage[]、stage_status、active
    async fn delivery_by_pr(&mut self, repo: &RepoSlug, number: u64) -> Result<Option<Delivery>>;    // forge 把 PR / Issue 事件解析到交付单元
    async fn delivery_by_issue(&mut self, repo: &RepoSlug, number: u64) -> Result<Vec<Delivery>>;
    async fn pr_snapshot(&mut self, repo: &RepoSlug, number: u64) -> Result<Option<PullRequestRef>>;
    async fn issue_snapshot(&mut self, repo: &RepoSlug, number: u64) -> Result<Option<IssueRef>>;
    async fn budget(&mut self, d: &DeliveryId) -> Result<Option<BudgetLedger>>;
    async fn conversation_budget(&mut self, c: &ConversationId, day: Date) -> Result<Option<BudgetLedger>>; // 会话台账（subject = conversation，P-58），按 UTC 日
    // 图
    async fn workflows(&mut self, d: &DeliveryId, stage: Option<Stage>, p: &Page) -> Result<Paged<Workflow>>;
    async fn workflow(&mut self, id: &WorkflowId, include_superseded: bool) -> Result<Option<WorkflowWithNodes>>;
    async fn attempts(&mut self, n: &NodeId, p: &Page) -> Result<Paged<Attempt>>;   // 默认按 n 升序
    async fn attempt(&mut self, id: &AttemptId) -> Result<Option<Attempt>>;
    // 工作树、收件箱、审批、设备
    async fn worktree(&mut self, id: &WorktreeId) -> Result<Option<Worktree>>;
    async fn worktrees(&mut self, q: &WorktreeQuery, p: &Page) -> Result<Paged<Worktree>>;
    async fn inbox_item(&mut self, id: &InboxId) -> Result<Option<InboxItem>>;
    async fn inbox(&mut self, q: &InboxQuery, p: &Page) -> Result<Paged<InboxItem>>;       // project_id、status（默认 open）、kind
    async fn inbox_open_by_dedup_key(&mut self, key: &str) -> Result<Option<InboxItem>>;   // InboxItem.dedup_key（P-63）
    async fn inbox_summary(&mut self) -> Result<Vec<InboxProjectCount>>;                   // GET /v1/inbox/summary：按项目的 open 计数
    async fn approval(&mut self, id: &ApprovalId) -> Result<Option<Approval>>;
    async fn approvals(&mut self, q: &ApprovalQuery, p: &Page) -> Result<Paged<Approval>>; // status、delivery_id
    async fn due_approvals(&mut self, now: Timestamp, limit: u32) -> Result<Vec<ApprovalId>>; // 已过期仍为 pending / granted，供 policy 扫描
    async fn device(&mut self, id: &DeviceId) -> Result<Option<Device>>;
    async fn device_by_cert_serial(&mut self, serial: &str) -> Result<Option<Device>>;     // mTLS 握手后的映射（api.md §2.1）
    async fn devices(&mut self, p: &Page) -> Result<Paged<Device>>;
    // settings
    async fn settings_current(&mut self, scope: &Scope, r: Resource) -> Result<SettingsEntry>;            // 从未写过：rev = 0，value = None
    async fn settings_at_rev(&mut self, scope: &Scope, r: Resource, rev: Seq) -> Result<Option<SettingsEntry>>; // 精确 rev，OQ-7
    async fn settings_as_of(&mut self, scope: &Scope, r: Resource, seq: Seq) -> Result<SettingsEntry>;    // ≤ seq 的最近一次
    async fn config_rev(&mut self, project: Option<&ProjectId>) -> Result<Seq>;                           // config.md §2.3
}
pub struct SettingsEntry { pub rev: Seq, pub value: Option<serde_json::Value> }   // value 是稀疏文档，已剥离 secret
```

事件流：

```rust
pub struct EventFilter {
    pub project_ids: Option<BTreeSet<ProjectId>>,   // None = 全部；project_id 为 NULL 的全局事件**始终**命中（api.md §5.4）
    pub include: Vec<TypePattern>,                  // 空 = 全部类型
    pub exclude: Vec<TypePattern>,                  // 先 include 后 exclude：手机档案白名单、settings.* 可见性由 api 用它表达
}
pub enum TypePattern { Exact(EventType), Prefix(String) }       // "message.*" → Prefix("message.")

pub struct ScanPage { pub events: Vec<Event>, pub scanned_to: Seq, pub head: Seq }
pub struct TailBatch { pub events: Vec<Event>, pub scanned_to: Seq }   // events 可为空而 scanned_to 前进

impl EventLog {
    pub async fn head(&self) -> Result<Seq>;
    /// after > head 返回 Err(CursorAhead { head })（api 映射为 409 cursor_ahead）。
    pub async fn check_cursor(&self, after: Seq) -> Result<()>;
    /// 返回 seq > after 且命中过滤的至多 limit 条。scanned_to 的语义见 §7.5。
    pub async fn scan(&self, after: Seq, f: &EventFilter, limit: u32) -> Result<ScanPage>;
    /// 先回放 (after, head]，再无缝切到实时；不丢不重；不持有任何每订阅者的内存队列（慢消费者只是读得慢）。
    pub fn tail(&self, after: Seq, f: EventFilter) -> EventStream;      // Stream<Item = Result<TailBatch>>
    pub fn watch_head(&self) -> watch::Receiver<Seq>;
    pub async fn get(&self, id: &EventId) -> Result<Option<Event>>;
    pub async fn get_by_seq(&self, seq: Seq) -> Result<Option<Event>>;
    pub async fn by_subject(&self, s: &Subject, p: &Page) -> Result<Paged<Event>>;
    pub async fn by_dedup_key(&self, key: &str) -> Result<Option<Seq>>;
    /// 反查 settings.updated.secret_changes[] 里的 (ref, version)，供 secrets 的暂存项启动对账（secrets.md §7.4）[提议]。
    pub async fn by_secret_change(&self, name: &SecretName, version: u64) -> Result<Option<Seq>>;
    pub async fn audit(&self, q: &AuditQuery, p: &Page) -> Result<Paged<Event>>;  // M2：按 type 前缀 / actor / subject / 时间范围
}

pub struct Cursors;   // 内部消费者的持久游标
impl Cursors {
    pub async fn load(&self, consumer: &str) -> Result<Seq>;                 // 不存在 = Seq(0)
    pub async fn advance(&self, consumer: &str, seq: Seq) -> Result<()>;    // 只增不减；回退返回 Err
}
```

### 6.3 Outbox

```rust
/// store 层的行插入请求；对外的入队端口是 core::ports::OutboxPort（forge 实现，请求类型是 `core::ports::EnqueueRequest`），forge 推导出 target / intent 后调用本方法。
pub struct OutboxInsert {
    pub project_id: ProjectId, pub delivery_id: DeliveryId, pub kind: ForgeActionKind,
    pub target: String, pub intent: serde_json::Value,       // 供派生幂等键（forge-actions §3.2 的 target / intent）
    pub payload: serde_json::Value,
    pub requires_approval: Option<ApprovalId>,   // 入队方按 PolicyPort 的判定写入（P-62）
    pub depends_on: Vec<ActionId>,
    pub priority: Priority, pub not_before: Option<Timestamp>, pub epoch: u32,
    pub coalesce_key: Option<String>,                        // 同一 (delivery, target, slot) 的 upsert_status_comment，M2
    pub enqueued_by: EnqueuedBy,                             // {module, causation_id}
}
pub struct Enqueued { pub action: OutboxAction, pub created: bool }

impl OutboxTx<'_> {
    /// 幂等：键 = IdempotencyKey::derive(...)。键已存在返回已有行（created = false），不新建、不发事件；
    /// 已有行为 failed / cancelled 时原样返回，调用方要重做必须 epoch + 1。
    pub async fn enqueue(&mut self, req: OutboxInsert) -> Result<Enqueued>;
    /// 原子认领：选出一个就绪行 → executing，写租约，attempts += 1（重放不加），追加 outbox.executing。
    /// 就绪 = queued 且 not_before 已到且依赖全部 succeeded；或 executing 且租约已过期（replay = true）。
    /// ClaimOpts { lease: Duration, exclude_deliveries: BTreeSet<DeliveryId> }——预算已耗尽的交付单元由 forge 传入排除（forge-actions §2.7），其行保持 queued。
    pub async fn claim(&mut self, opts: ClaimOpts) -> Result<Option<Claimed>>;
    pub async fn record_effects(&mut self, id: &ActionId, effects: Vec<ExpectedEffect>) -> Result<()>;  // 写在变更调用之前
    pub async fn renew_lease(&mut self, id: &ActionId, until: Timestamp) -> Result<()>;
    pub async fn complete(&mut self, id: &ActionId, github_ref: GithubRef) -> Result<()>;   // succeeded + outbox.succeeded，同事务
    /// Terminal：置 failed（终态）并追加 outbox.failed（data.code 取 forge-actions §2.6）。
    /// Retry：临时错误，行**留在 queued** 并设 not_before 退避，last_error 更新，**不**追加 outbox.failed（P-53）。
    /// 已 failed 的行不会回到 queued；人工重试由调用方以 epoch + 1 入队一个新动作。
    pub async fn fail(&mut self, id: &ActionId, err: OutboxError, then: AfterFailure) -> Result<()>;
    pub async fn requeue(&mut self, id: &ActionId, not_before: Timestamp, refund_attempt: bool) -> Result<()>; // 限流退避：不消耗尝试次数
    pub async fn cancel(&mut self, id: &ActionId, reason: CancelReason) -> Result<()>;
    /// 依赖已 failed / cancelled 的 queued 行置 cancelled（reason = dependency_failed）。claim 前自动执行一次。
    pub async fn cancel_blocked(&mut self) -> Result<Vec<ActionId>>;
}
pub enum AfterFailure { Terminal, Retry { not_before: Timestamp } }
// 读取：ProjectionRead 之外，另有
//   outbox_action / outbox_by_key / outbox_list(OutboxQuery, Page) /
//   count_succeeded_since(delivery_id, since)
```

本节涉及的 `outbox.enqueued` / `outbox.executing` / `outbox.cancelled` data 送审字段见 [event-data.md](../interfaces/event-data.md)（尚未批准）。契约批准前运行时仍按 fail-closed 拒绝，本节其余语义不变。

### 6.4 CAS

```rust
pub enum CasKind { ResultBundle, Artifact, Progress, Quarantine }
impl Cas {
    pub async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<Sha256Digest>;
    pub async fn put_file(&self, path: &Path, kind: CasKind) -> Result<Sha256Digest>;     // 流式：边读边算摘要，写临时文件，rename
    pub async fn open(&self, d: &Sha256Digest) -> Result<Option<CasReader>>;              // AsyncRead
    pub async fn get_bytes(&self, d: &Sha256Digest, max: usize) -> Result<Option<Vec<u8>>>;
    pub async fn has(&self, d: &Sha256Digest) -> Result<bool>;
    pub async fn stat(&self, d: &Sha256Digest) -> Result<Option<CasStat>>;               // {bytes, kind, created_at, quarantined}
    /// 结果包清单：JCS 规范化后存入，使"重复 collect 得到相同的 result_ref"（worker-protocol §3.5）。
    pub async fn put_manifest(&self, manifest: &ResultManifest) -> Result<Sha256Digest>;
    pub async fn gc(&self, policy: &RetentionPolicy) -> Result<GcReport>;                // 默认策略 = 不删除（OQ-5）
}
```

### 6.5 错误

```rust
pub enum StoreError {
    InvalidEvent(SchemaError),                // data 形状不符 / 未知事件类型 / 超过 max_event_data_bytes
    IllegalTransition(TransitionError),       // core 状态机拒绝（含审批不可消费、非法阶段变更）
    PreconditionFailed(PreconditionKind),     // 工作树锁冲突、SubjectMoved（append_with 的守卫）、Outbox 行不在预期状态、游标回退…
    NotFound, InvalidCursor, CursorAhead { head: Seq },
    TxTimeout, Busy, SnapshotExpired, DiskFull,
    SchemaTooNew { found: u32, supported: u32 }, AlreadyRunning, Corrupt(String),
    Io(std::io::Error), Sqlite(String),
}
```

## 7. 内部设计

### 7.1 数据目录与 SQLite 配置

```
<store.dir>/
  gantry.db  gantry.db-wal  gantry.db-shm      # 单个 SQLite 文件（预写日志 WAL 模式）
  gantry.lock                                  # flock 排他锁：一个数据目录只能有一个 gantryd
  cas/sha256/ab/cdef…                          # CAS 对象，路径由摘要决定
  cas/tmp/                                     # 写入中的临时文件
  backup/                                      # backup_to 的默认目标（可由 deploy 改）
```

| PRAGMA | 值 | 理由 |
| --- | --- | --- |
| `journal_mode` | `WAL` | 读者不阻塞写者；快照读 |
| `synchronous` | `FULL`（默认，OQ-8） | 事件日志是事实源，提交返回即已落盘 |
| `foreign_keys` | `ON` | 投影表的引用完整性 |
| `busy_timeout` | `store.busy_timeout_ms` | 读连接与备份偶发竞争 |
| `wal_autocheckpoint` | `store.wal_checkpoint_pages` | 控制 WAL 增长 |
| `trusted_schema` | `OFF` | 防御：库内视图 / 触发器不得调用不受信函数 |

**连接模型（单写者多读者）**：一个写连接，由 `tokio::sync::Mutex`（FIFO 公平）保护，事务一律 `BEGIN IMMEDIATE`；`read_connections` 个读连接组成池，每个读事务是一个 WAL 快照。写事务持有期间读者照常读旧快照。

### 7.2 表结构草案

SQLite `STRICT` 表。主键、唯一约束、触发器是正确性的一部分；其余索引随查询调整。

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
-- 键：schema_version、projector.<module>.version（内置投影器的 module 为 core）、module.<name>.version、
--     log_id（建库时生成的随机 ULID）、log_epoch（P-67）、projected_seq

CREATE TABLE events (
  seq            INTEGER PRIMARY KEY AUTOINCREMENT,         -- 全局单调；AUTOINCREMENT 保证不复用
  id             TEXT    NOT NULL UNIQUE,                   -- evt_<ULID>
  ts             INTEGER NOT NULL,                          -- epoch 毫秒，seq 序下非递减
  type           TEXT    NOT NULL,                          -- 'delivery.stage_changed'、'forge.pr_merged'…
  subject        TEXT    NOT NULL,                          -- 实体 ID 或字面量 'settings'
  project_id     TEXT,                                      -- NULL = 全局事件
  actor_kind     TEXT    NOT NULL,
  actor_id       TEXT    NOT NULL,
  causation_id   TEXT,
  correlation_id TEXT,
  dedup_key      TEXT UNIQUE,                               -- 可选；forge.<kind> 必带（forge-actions G1），其他事件按需（如 message.posted）
  data           TEXT    NOT NULL CHECK (json_valid(data))  -- 含不可信文本，JSON 转义形式，NUL 以 \u0000 存储
) STRICT;
CREATE INDEX events_project_seq ON events (project_id, seq);
CREATE INDEX events_type_seq    ON events (type, seq);
CREATE INDEX events_subject_seq ON events (subject, seq);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;

CREATE TABLE consumer_cursors (consumer TEXT PRIMARY KEY, seq INTEGER NOT NULL, updated_at INTEGER NOT NULL) STRICT;

-- Outbox 是主表（不是投影）：租约、expected_effects 的更新不逐条产生事件，重建投影时不清空它
CREATE TABLE outbox (
  id               TEXT PRIMARY KEY,                        -- act_<ULID>
  idempotency_key  TEXT NOT NULL UNIQUE,                    -- gk1_…
  project_id       TEXT NOT NULL,
  delivery_id      TEXT NOT NULL,
  kind             TEXT NOT NULL,
  payload          TEXT NOT NULL CHECK (json_valid(payload)),
  requires_approval TEXT,
  depends_on       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(depends_on)),
  priority         TEXT NOT NULL CHECK (priority IN ('high','normal','low')),
  not_before       INTEGER,
  epoch            INTEGER NOT NULL DEFAULT 0,
  coalesce_key     TEXT,
  status           TEXT NOT NULL CHECK (status IN ('queued','executing','succeeded','failed','cancelled')),
  attempts         INTEGER NOT NULL DEFAULT 0,
  lease_until      INTEGER,
  last_error       TEXT,
  expected_effects TEXT NOT NULL DEFAULT '[]',
  github_ref       TEXT,
  enqueued_by      TEXT NOT NULL,
  enqueued_seq     INTEGER NOT NULL,                        -- 出队顺序：(priority, enqueued_seq)
  updated_seq      INTEGER NOT NULL
) STRICT;
CREATE INDEX outbox_ready    ON outbox (status, priority, enqueued_seq) WHERE status IN ('queued','executing');
CREATE INDEX outbox_coalesce ON outbox (coalesce_key) WHERE coalesce_key IS NOT NULL AND status = 'queued';
CREATE INDEX outbox_delivery ON outbox (delivery_id, status, updated_seq);

CREATE TABLE cas_objects (digest TEXT PRIMARY KEY, bytes INTEGER NOT NULL, kind TEXT NOT NULL,
                          created_at INTEGER NOT NULL, quarantined INTEGER NOT NULL DEFAULT 0) STRICT;

CREATE TABLE settings_docs (                                -- 投影：settings.updated 的每个 rev
  scope TEXT NOT NULL CHECK (scope IN ('global','project')),
  project_id TEXT NOT NULL DEFAULT '',                      -- global 为空串
  resource TEXT NOT NULL,
  rev INTEGER NOT NULL,                                     -- = 事件 seq
  value TEXT,                                               -- 稀疏文档；项目 DELETE 为 NULL；不含 secret 值
  PRIMARY KEY (scope, project_id, resource, rev)
) STRICT;
```

**投影表一览**（列 = data-model §3 字段一一对应；枚举列加 `CHECK`；含不可信文本的列存 JSON 文本）：

| 表 | 实体 | 驱动事件 | 关键索引 / 约束 |
| --- | --- | --- | --- |
| `projects` | Project | `project.added` / `updated` | `UNIQUE(repo)`；不含 `settings_override`（它在 `settings_docs`） |
| `conversations` | Conversation | `conversation.created` | `UNIQUE(project_id)` |
| `messages` | Message | `message.posted` | `(conversation_id, id)` |
| `deliveries` | Delivery | `delivery.created` / `stage_changed` / `level_changed` / `paused` / `resumed` / `abandoned`；`worktree.created`（挂 `worktree_id`） | `(project_id, stage, id)`；`(parent_id)` |
| `issue_snapshots`、`pr_snapshots`、`delivery_issues` | IssueRef / PullRequestRef 与绑定 | `delivery.created`（绑定）、`forge.synced`、`forge.<kind>`；只有 `actor.kind = forge` 的事件可更新快照列 [提议] | `PK(repo, number)`；`(delivery_id)` |
| `workflows`、`nodes` | Workflow、Node | `workflow.created` / `revised` / `finished`、`node.*`（含 `node.retry_requested`） | `(delivery_id, stage)`；`(conversation_id)`（`delivery_id` 可空时必有 `conversation_id`，P-58）；`(workflow_id)` |
| `attempts` | Attempt | `attempt.started` / `finished` | `UNIQUE(node_id, n)`；终态后拒绝任何修改 |
| `worktrees` | Worktree + 锁 | `worktree.*` | `UNIQUE(project_id, branch)` |
| `sandboxes` | Sandbox | `sandbox.created` / `destroyed` | `(attempt_id)` |
| `inbox_items` | InboxItem | `inbox.raised` / `resolved` / `dismissed` | `(status, id)`；`(project_id, status)`（`inbox_summary`）；`(delivery_id)`；`dedup_key` 在 `open` 行上唯一（部分唯一索引，P-63） |
| `approvals` | Approval | `approval.*` | `(delivery_id)`；`(status, expires_at)` |
| `budget_ledgers` | BudgetLedger | `delivery.created`（建账）、`budget.charged` / `exhausted` | 交付台账 `UNIQUE(delivery_id)`；会话台账（`subject = conversation`，P-58）按 `(conversation_id, UTC 日)` 唯一，`budget.charged` 无 `delivery_id` 而带 `conversation_id` 时首笔即建账 |
| `devices` | Device（事件可得的列） | `device.paired` / `updated` / `revoked` | `UNIQUE(cert_serial)`；`last_seen` 等非事件列归 `pki` 的表（OQ-4） |
| `settings_secret_changes` | `(ref, version) → seq` 反查 | `settings.updated` 的 `secret_changes[]` | `PK(ref, version)`；只存名字与版本，无值无哈希 |

### 7.3 写路径

`WriteTx::append(ev)` 依次执行，任一步失败则回滚本次 SAVEPOINT 并返回错误：

1. **形状**：`data` 序列化后 ≤ `max_event_data_bytes`；`core::validate_event_data(ev.ty, &ev.data)`；事件类型必须在 `core` 目录内（拼写错误的类型被拒，读取侧则容忍未知类型字符串以便降级）。
2. **去重**：带 `dedup_key` 时先 `SELECT seq FROM events WHERE dedup_key = ?`，命中则返回 `Duplicate { existing_seq }`，**不执行后续步骤、无任何副作用**（单写者下"先查后插"没有竞态；唯一索引是兜底）。
3. **分配**：`id = ids.next::<Evt>()`；`ts = max(clock.now(), last_ts)`（时钟回拨时 `ts` 不倒退）；`INSERT … RETURNING seq`。
4. **折叠**：先运行内置投影器（被某个模块 `owns` 接管的实体除外），再按 `ModuleSpec` 的注册顺序运行各模块投影器（只收到 `handles()` 命中的事件）。`Projector::apply`——只用事件自身字段（含 `event.ts`）计算，**不读时钟、不读环境**，使重放结果确定。内置投影器折叠中调用 `core` 状态机，例如：
   - `delivery.stage_changed` → `core::machines::delivery::is_legal_change(from, to, level_effective)`，且 `from` 必须等于投影里的当前阶段；
   - `approval.consumed` → `core::machines::approval::can_consume(&approval, &data.subject_digest, event.ts)`；
   - `worktree.locked` → `worktree_lock::acquire(current, req, event.ts)`（租约是否到期以 `event.ts` 判定）；
   - `attempt.finished` → `attempt::check_terminal`；终态后的任何修改被拒。
5. **提交后**：`COMMIT` 成功才更新进程内 `head` 的 `watch`、`config_rev` 缓存；`COMMIT` 失败则这些都不动。

`settings.updated` 追加时 `rev = seq`；`value` 必须通过 `core` 的 schema（SecretRef 在存储形态里没有 `value` 属性，所以带 secret 值的事件在第 1 步就被拒）。`store` **不**对事件内容做"形似密钥"的启发式扫描：GitHub 评论、CI 日志这类外部文本可能合法地含有形似密钥的串，拒绝存储会丢事件；防线是类型与 schema（I6）。

**到期是被动的**：租约、审批、challenge 的"到期"没有后台线程改状态；`store` 只提供到期查询（`due_approvals`、`claim` 对过期租约的重认领），由 `policy` / `forge` 的扫描任务据此追加 `approval.expired` 等事件。

### 7.4 读路径与快照

`snapshot()` 从读池取一个连接，`BEGIN` 后第一条语句读取 `head`（`SELECT seq FROM events ORDER BY seq DESC LIMIT 1`，空表为 0），这一步固定了 WAL 快照，之后该快照上的所有读取都与 `head_seq` 一致。因为投影与事件在同一个写事务里提交，**任何快照里的投影恰好反映到 `head_seq`**。快照最长存活 `snapshot_max_age_s`（避免拖住 WAL 检查点），超时后续调用返回 `SnapshotExpired`，调用方重新取快照。

键集分页：`ORDER BY id`（ULID 与文本序、时间序一致）；`PageCursor` 编码 `(资源标签, 排序键, 方向)`，`next_cursor` 在多取一行时生成；`attempts` 以 `(node_id, n)` 为键。游标解码失败、资源标签不符、方向不符一律 `InvalidCursor`。

### 7.5 事件流

`scan(after, filter, limit)` 在一个读快照里执行：取命中过滤的 `seq > after` 的前 `limit` 条；`scanned_to` 的定义——返回满 `limit` 条时为最后一条的 `seq`；不足 `limit` 时为该快照的 `head`（说明 `(after, head]` 内已无更多命中，被过滤掉的事件也算已扫描）。这保证 `api` 的 `heartbeat.id`（"本连接已扫描到的最大 seq"，api.md §5.1）在过滤流上也能前进，且绝不越过任何一条未返回的命中事件。

`tail` 的循环：

```
cursor = after
loop:
    rx.borrow_and_update()                     // 先登记已见的 head 版本
    page = scan(cursor, filter, tail_batch)
    if page.events 非空 or page.scanned_to > cursor:
        yield TailBatch{ page.events, page.scanned_to }; cursor = page.scanned_to; continue
    select { rx.changed() , sleep(tail_rescan_s) }   // 兜底定时重扫，防御任何漏通知
```

订阅者没有每连接的内存队列，所以不存在"缓冲溢出丢事件"；`api` 的 `lagging` 关闭只是发送缓冲的事。

### 7.6 Outbox 实现要点

**认领**（`BEGIN IMMEDIATE` 内，先 `cancel_blocked`，再选一行）：

```sql
SELECT o.id FROM outbox o
WHERE ( (o.status = 'queued'    AND (o.not_before IS NULL OR o.not_before <= :now))
     OR (o.status = 'executing' AND o.lease_until <= :now) )                    -- 租约过期 = 崩溃后重放
  AND NOT EXISTS (SELECT 1 FROM json_each(o.depends_on) d
                  JOIN outbox p ON p.id = d.value WHERE p.status <> 'succeeded')
ORDER BY CASE o.priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END, o.enqueued_seq
LIMIT 1;
-- 然后 UPDATE … SET status='executing', lease_until=:now+:lease, attempts=attempts+(重放?0:1), updated_seq=…
-- 并在同一事务追加 outbox.executing
```

- 单写者 + 同一事务内"选 + 更新"，天然保证同一行不会被两个认领者拿到；若将来多进程（OQ-9），改为 `UPDATE … WHERE status='queued' … RETURNING` 的原子抢占（forge-actions §2.3 的预留说明）。
- `record_effects` 在变更调用**之前**单独提交（write-ahead），`complete` 把 `github_ref`、`succeeded` 与 `outbox.succeeded` 放在同一事务。
- `requeue(…, refund_attempt = true)` 用于限流退避，不消耗尝试次数（forge-actions §2.6）；`fail(…, Retry)` 把行从 `executing` 置回 `queued`、设 `not_before` 与 `last_error`，不追加 `outbox.failed`；`Terminal` 置 `failed`（终态，无出边）并追加 `outbox.failed`。`failed → queued` 不存在（P-53）：人工重试由入队方以新 `epoch` 创建新动作。
- `coalesce_key`：新行创建成功后，同键的其他 `queued` 行置 `cancelled`（reason `superseded`），各发 `outbox.cancelled`。
- `count_succeeded_since(delivery_id, since)` 让 `forge` 用滑动窗口实现每小时令牌桶，**不在 `store` 里持久化令牌桶状态**。

**崩溃点与 `store` 的保证**（对应 [forge-actions §2.3 崩溃点矩阵](../interfaces/forge-actions.md#23-执行协议先查后做)）：

| 崩溃点 | 落盘状态（`store` 保证） | 重启后 |
| --- | --- | --- |
| `claim` 提交前 | `queued` | 正常认领 |
| `claim` 之后、变更调用之前 | `executing`，租约会过期 | 租约过期后 `claim` 返回 `replay = true`；`attempts` 不加 |
| `record_effects` 之后、变更调用成功之前或之后、`complete` 之前 | `executing` + `expected_effects` 已持久 | 同上；`forge` 靠 probe 发现对象已存在，补记 `github_ref` 后 `complete`，**不再创建** |
| `complete` 之后 | `succeeded` + `github_ref` + `outbox.succeeded` 同事务 | 无事 |

本节涉及的 `outbox.enqueued` / `outbox.executing` / `outbox.cancelled` data 送审字段见 [event-data.md](../interfaces/event-data.md)（尚未批准）。契约批准前运行时仍按 fail-closed 拒绝，本节其余语义不变。

### 7.7 内容寻址存储

- 路径 `cas/sha256/<前两位>/<其余 62 位>`；对象内容不可变，同摘要同内容天然去重。
- 写入：流式读入同时计算 SHA-256，写入 `cas/tmp/<随机>`，`fsync`，`rename` 到目标（目标已存在则删除临时文件），最后在一个小写事务里 `INSERT OR IGNORE INTO cas_objects`。文件先于行：崩溃后可能出现"有文件无行"的孤儿，`verify` 会补行；不会出现"有行无文件"。
- 读取只接受 `Sha256Digest` 类型，路径由摘要拼出，**没有调用方可控的路径片段**，不存在目录穿越。`get_bytes` 带 `max` 上限；小对象读取后重算摘要校验（`verify_on_read`）。
- 文件权限 `0640`、目录 `0750`，属主为服务进程用户；不设置可执行位，且 `store` 之外没有任何代码把这个目录当静态文件服务。
- 隔离区（被拒产物）用 `kind = Quarantine`，`quarantined = 1`，不进入常规引用；保留期未定（OQ-3、OQ-5），`gc` 默认不删。
- 清单（`ResultManifest`）用 `core::jcs` 规范化后存入，所以对同一已停止沙盒的 `out/` 重复 `collect` 得到同一个 `result_ref`。

### 7.8 备份、恢复、完整性

- `backup_to(dir)`：`VACUUM INTO '<dir>/gantry-<ts>.db'`（一致性快照，写者可继续写），随后对副本执行 `PRAGMA integrity_check`，写一份清单（`head_seq`、`log_id`、各表行数摘要、CAS 对象数）。CAS 目录是内容寻址的，`deploy` 可用增量复制；`store` 提供 `cas().verify_sample()` 校验副本。**不备份 `secrets/`**（它有自己的加密备份）。
- 恢复是离线操作（`Store::restore_from(dir)`，由运维命令调用）：校验副本 → 替换数据库文件 → `log_epoch += 1`（P-67），`log_id` 保持。恢复后 `head` 可能小于恢复前客户端见过的 `seq`：`api` 在事件流与快照响应中携带 `log_epoch`，客户端发现变化就全量重同步。随后由 `pki` 重放 `revocations.journal` 中晚于备份的吊销（`deploy` 的恢复命令按此顺序调用）。
- `verify(Quick)`：`PRAGMA quick_check`；`meta.projected_seq == head`；`outbox` 与 `events` 的 ID 前缀 / 枚举合法。`verify(Full)`：在临时库里全量折叠事件，与现有投影表逐表比较哈希；CAS 抽样重算摘要。
- 发现 `SQLITE_CORRUPT`：拒绝启动并输出"从备份恢复"的指引；**不自动修复**。

### 7.9 迁移与投影重建

- 前向迁移：`migrations/NNNN_name.sql` 编号递增，编译进二进制；`meta.schema_version` 记录；数据库版本高于二进制 → `SchemaTooNew`，拒绝启动；迁移前先 `VACUUM INTO` 一份 `backup/pre-migrate-N.db`（`pre_migrate_backup`）。
- 事件表只做**加法**迁移；投影表可以直接 `DROP` 后靠重建恢复。每个投影器（内置的与模块注册的）的行为变化时递增自己的 `version`，与 `meta.projector.<module>.version` 不符则启动时自动重建：在一个事务里对相关投影器调用 `reset`，按 `seq` 顺序重放全部事件（所有受影响的投影器一次遍历），更新 `projected_seq` 与各 `version`；`outbox`、`consumer_cursors`、`cas_objects`、模块的非投影表**不动**。
- 事件 `data` 的向后兼容：只增字段（`#[serde(default)]`），不删不改义（data-model §1）；重建必须能折叠历史上写入过的所有形状。

### 7.10 模块私有表与投影器注册 **[提议，OQ-2、OQ-4]**

其他模块的规格书已经提出这类需求：`forge` 的 `forge_webhook_inbox` 与 `forge_effect_index` 必须能与事件追加、Outbox 写入放进同一事务；`policy` 有 `policy_challenges`、`policy_cursor`；`notify` 有 `notify_*`；`pki` 有设备与证书表。机制如下：

- `Store::open` 接收各模块的 `ModuleSpec { module, migrations, projector }`。迁移在核心迁移之后依次应用，版本记在 `meta` 的 `module.<name>.version`；表名必须以 `<module>_` 开头，启动时扫描迁移 SQL 里的 `CREATE TABLE` 名称做前缀检查。
- 模块通过 `WriteTx::module_conn(module)` 在**同一事务**里读写自己的表（例如 `device.paired` 事件与 `pki_devices` 行一起提交），事务外读取经 `ReadSnapshot::module_conn`。`ModuleConn` 只暴露参数化查询接口，不暴露原始连接字符串。
- 模块可以注册投影器，在事件追加的同一事务里维护**自己的**派生表（例如 `notify_*`）。内置投影器负责 data-model 全部实体；某个模块若通过 `owns()` 声明接管某个实体（`policy` 的 `approvals`、`notify` 的 `inbox_items`、`pki` 的设备表，它们的规格书各自这样假定），`store` 对该实体停用内置投影器，并去掉对应的 `ProjectionRead` 方法由该模块提供读取。是否接管是 OQ-2，未决前默认由内置投影器负责。
- 事务内的"先检查后写入"（如 `policy` 对审批的比较并交换）在单写者下天然成立：`tx.read()` 读到的状态在本事务提交前不会被他人改变。

## 8. 不变式与安全要求

| 不变式 | `store` 如何保证 |
| --- | --- |
| I2 GitHub 写入唯一路径 | Outbox 表是唯一的待执行队列；`enqueue` 以派生幂等键唯一约束去重，重复入队返回同一行；认领在单写者事务里原子完成，`complete` 与 `outbox.succeeded` 同事务；`store` 不提供执行 GitHub 写的接口 |
| I3 放行唯一决策点 | `approval.consumed` 追加时由 `core` 状态机核对 `granted`、未过期、`subject_digest` 与当前主体一致，同一审批并发消费只有一个成功；审批的"检查 + 消费 + 业务写入"可在同一 `WriteTx` 中完成（api.md §6.6 第 4 步） |
| I4 状态各有事实源 | 事件日志只追加、触发器拒绝 UPDATE / DELETE；快照列只由 `actor.kind = forge` 的事件更新 [提议]，其他模块无法在本地"改写" GitHub 事实；`forge.drift_detected` 作为普通事件保存差异 |
| I5 外部文本不可信 | `UntrustedText` 连同 `source` / `truncated` 原样往返；`store` 不渲染、不拼接进 SQL（全部参数化）、不写入日志（日志只记类型 / `seq` / `subject`）；CAS 内容当不透明字节 |
| I6 客户端不持凭据 | 事件与投影里只有 `SecretRef`；`settings.updated` 的 schema 使带 `value` 的 SecretRef 无法通过校验；`store` 不连接 `secrets` |
| I7 配置与工作分离 | settings 的唯一写入口是 `settings.updated` 事件；`Project.settings_override` 不是 `projects` 表的列，只能从 `settings_docs` 读 |
| 审计 | 审计日志 = 事件日志（architecture §6）：追加式、触发器保护、`ts` 非递减、`seq` 无空洞（见 §12） |
| 数据库层 | 所有 SQL 参数化；数据库文件与 CAS 目录权限仅服务进程用户可读写；不加载 SQLite 扩展；`trusted_schema = OFF` |

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| `data` 形状不符 / 未知事件类型 / 过大 | `InvalidEvent`，本次 `append` 的 SAVEPOINT 回滚；调用方当作缺陷处理，不重试 |
| 非法状态转移 | `IllegalTransition`，同上。**这是契约缺口或调用方缺陷的信号**（见 core.md OQ-6），不要靠改目标状态绕过 |
| `dedup_key` 已存在 | 不是错误：`Appended::Duplicate`，无副作用 |
| 工作树锁冲突、Outbox 行不在预期状态 | `PreconditionFailed(kind)`；`api` 映射 `409 worktree_locked` / `409 conflict` |
| 磁盘写满（`SQLITE_FULL`） | `DiskFull`；事务整体回滚；`store` 进入只读降级（读照常，写一律 `DiskFull`），`api` 映射 `503 unavailable`；空间恢复后自动解除 |
| `SQLITE_BUSY`（读连接竞争备份 / 检查点） | 在 `busy_timeout` 内重试，仍失败返回 `Busy`，`api` 映射 `503` 并带 `Retry-After` |
| 写事务持有过久 | 强制回滚，`TxTimeout`，记 `error` 日志（含持有者的调用点标签） |
| 读快照超龄 | `SnapshotExpired`；调用方重新 `snapshot()` 后重做读取 |
| 进程崩溃 | WAL 恢复；启动时校验 `projected_seq == head`，不等则重建投影；Outbox 的 `executing` 行靠租约过期重放（§7.6） |
| `SQLITE_CORRUPT` / 完整性检查失败 | `Corrupt`，拒绝启动，不自动修复；运维从最近备份恢复 |
| 数据库版本高于二进制 | `SchemaTooNew`，拒绝启动（防止旧二进制写坏新库） |
| 第二个实例打开同一目录 | `AlreadyRunning`（flock） |
| 游标超前 `head` | `check_cursor` 返回 `CursorAhead { head }`，`api` 映射 `409 cursor_ahead` |
| CAS 临时文件残留 / 孤儿文件 | 启动时清理 `cas/tmp/`；`verify` 补行或报告 |

## 10. 配置

`store` 的配置是**引导配置**（`deploy` 拥有，重启生效；[config.md §1.1](../interfaces/config.md#11-不属于本文的内容)），不是 settings，不进事件日志。以下数值默认全部是 **[建议值，待调优]**。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `store.dir` | 无默认（`deploy` 给出） | 数据目录 |
| `store.synchronous` | `FULL` | `FULL` / `NORMAL`；`NORMAL` 掉电可能丢最近提交但不损坏，OQ-8 |
| `store.read_connections` | `4` | 读连接数 |
| `store.busy_timeout_ms` | `5000` | |
| `store.max_event_data_bytes` | `1048576` | 单个事件 `data` 上限；大载荷（review 评论、diff 片段）由发出方截断，OQ-10 |
| `store.max_events_per_tx` | `1000` | |
| `store.write_tx_max_hold_ms` | `5000` | 超时强制回滚 |
| `store.snapshot_max_age_s` | `30` | |
| `store.tail_batch` / `store.tail_rescan_s` | `200` / `5` | `tail` 的批大小与兜底重扫间隔 |
| `store.wal_checkpoint_pages` | `1000` | |
| `store.pre_migrate_backup` | `true` | |
| `store.cas.dir` | `<store.dir>/cas` | |
| `store.cas.max_object_bytes` | `67108864` | 单个 CAS 对象上限；与 `sandbox.io.*` 的产物上限（config.md §7）对齐的上限由 OQ-3 一并确定 |
| `store.cas.verify_on_read_below_bytes` | `1048576` | 小于此值的对象读取时重算摘要 |

## 11. 可观测性

`store` **自己不发出领域事件**（事件由调用方追加）。

- 日志：`info`——打开 / 迁移 / 重建投影 / 备份 / 关闭；`warn`——事务持有超过 100 ms、`Busy`、投影重建触发；`error`——`Corrupt`、`DiskFull`、`TxTimeout`。日志字段只含事件类型、`seq`、`subject`、`correlation_id`，**不含 `data`**（可能含不可信文本，I5）。
- 指标（名称为建议）：`store_head_seq`、`store_append_total{type_family,result}`、`store_append_duration_seconds`、`store_write_tx_duration_seconds`、`store_write_queue_depth`、`store_read_snapshot_age_seconds`、`store_projection_lag_seq`（恒为 0，非 0 即缺陷）、`store_projection_rebuild_seconds`、`store_outbox_depth{status,priority}`、`store_outbox_oldest_queued_seconds`、`store_db_bytes`、`store_wal_bytes`、`store_cas_bytes`、`store_cas_objects`。
- 健康：`Store::verify(Quick)` 供 `gantry doctor` 调用。`store` 不在 `/v1/settings/status` 的模块清单内（[config.md §2.8](../interfaces/config.md#28-catalog-与-status-的响应结构)），不可用时 `api` 直接返回 `503 unavailable`。

## 12. 测试与验收

所有测试用 `Store::open_temp` + `FakeClock` + `SeqEntropy`，不依赖网络。

### 12.1 M1 退出标准中 `store` 负责的部分

- [ ] **游标续传、界面状态一致**（"任务进行中关闭桌面端，重新打开后事件流从游标续上"）：并发写入 5 万个事件的同时，N 个订阅者各自随机断开、用最后已应用的 `seq` 重连；每个订阅者最终收到的 `seq` 序列严格递增、无重复、无遗漏，且把"快照 + 之后的事件"折叠的结果等于最新快照。
- [ ] **webhook 重复投递不产生重复动作**：同一 `dedup_key` 追加两次，日志只有一条，第二次返回 `Duplicate`，投影与 Outbox 均无变化；并发 16 路追加同一 `dedup_key`，恰好一条成功。
- [ ] **崩溃后不重复创建 PR**：崩溃注入测试（子进程 + `SIGKILL`，注入点：`claim` 提交后、`record_effects` 提交后、变更调用后 `complete` 前）。重启后该行为 `executing` 且租约过期，`claim` 返回 `replay = true`；用 `FakeForge`（probe 能发现已存在的 PR）完成后，`FakeForge` 里**恰好一个 PR**，Outbox 行 `succeeded`。
- [ ] **幂等入队**：同一 `(project, delivery, kind, target, intent, epoch)` 入队 N 次（含并发）只有一行，`created` 仅第一次为 `true`；`epoch + 1` 得到新行；已 `failed` / `cancelled` 的行重复入队原样返回。

### 12.2 事件日志与读一致性

- [ ] `seq` 从 1 起，严格 +1、无空洞：并发 16 个写者各提交含多个事件的事务，随机回滚一部分；最终 `SELECT seq` 连续，且读者任何时刻看到的最大 `seq` 之前没有缺口。
- [ ] `events` 的 UPDATE / DELETE（直接 SQL）被触发器拒绝；`dedup_key` 唯一。
- [ ] `ts` 在 `seq` 序下非递减：`FakeClock` 回拨后继续追加，`ts` 不倒退。
- [ ] `scan`：过滤时 `scanned_to` 越过被过滤事件前进；满 `limit` 时等于最后一条命中事件的 `seq`；绝不越过任何一条未返回的命中事件（属性测试，随机事件流与随机过滤器）。
- [ ] 全局事件（`project_id` 为 NULL）在任何 `project_ids` 过滤下都命中；`include` / `exclude` 的前缀与精确匹配正确；`message.*` 不匹配 `messages.x`。
- [ ] `snapshot()`：数据与 `head_seq` 同快照——对随机事件流，在随机时刻取快照，再追加后续事件并折叠，结果与最新快照一致（属性测试）；超过 `snapshot_max_age_s` 后返回 `SnapshotExpired`。
- [ ] `check_cursor(after > head)` 返回 `CursorAhead { head }`；`after == head` 通过。
- [ ] `PageCursor`：跨资源复用、篡改、方向不符都得到 `InvalidCursor`（含模糊测试）；翻页期间插入新行不重不漏（键集语义）。
- [ ] `Cursors::advance` 只增不减。
- [ ] 不可信文本往返：含 NUL、双向控制符、4 字节 Unicode、1 MiB 字符串的 `UntrustedText` 存取后字节相同。

### 12.3 投影

- [ ] **重建等价**：对随机生成的合法事件序列，增量维护的投影表与 `rebuild_projections()` 之后的投影表逐表哈希相等（属性测试，覆盖 §7.2 全部投影表）。
- [ ] 任一投影器（内置或模块注册）的 `version` 变化后 `open` 自动重建其表；`outbox`、`consumer_cursors`、`cas_objects`、模块的非投影表在重建前后不变。
- [ ] 模块投影器：注册一个测试投影器（带 `owns` 与不带各一个），同事务内与事件一起提交 / 回滚；`owns` 某实体后内置投影器对该实体不再写表；`reset` 后重放得到相同的表内容；投影器 `apply` 返回错误时整次 `append` 回滚。
- [ ] `append_with(expect_subject_seq)`：守卫不符返回 `PreconditionFailed(SubjectMoved)` 且无副作用；两个任务基于同一 `expect` 并发追加，恰好一个成功；`Some(None)` 只对没有事件的 subject 成功。
- [ ] `advance_cursor` 与事件写入原子：事务回滚时游标不前进，提交时同时前进；`dedup_key` 命中的 `Duplicate` 不影响同事务内游标的推进（`conversation` 的"追加消息 + 推进游标"场景）。
- [ ] `dedup_key` 对任意事件类型生效（不限 `forge.*`）。
- [ ] 非法状态转移被拒且不留痕：`delivery.stage_changed` 的 `from` 与当前阶段不符、`attempt` 终态后再改、`approval.consumed` 对非 `granted` 审批、`worktree.locked` 他人未过期的锁——均返回对应错误，事件与投影无变化，同事务内其他 `append` 不受影响（SAVEPOINT）。
- [ ] 审批并发消费：两个任务同时 `approval.consumed`，恰好一个成功。
- [ ] 工作树锁：并发 `worktree.locked` 恰好一个成功；租约按 `event.ts` 判定过期，`FakeClock` 推进后可被他人取得。
- [ ] settings：`rev == 事件 seq`；`settings_current` 对从未写过的返回 `rev 0`；`settings_at_rev` / `settings_as_of` / `config_rev(project)` 在随机交错的全局 / 项目写入下正确；项目 `DELETE` 后 `value` 为 `None`。
- [ ] 带 `credential.value` 的 `settings.updated` 追加返回 `InvalidEvent`；正常的 `secret_changes[]` 不含值与哈希；`by_secret_change(ref, version)` 能反查到对应 `seq`，未写入的返回 `None`。
- [ ] 快照列只接受 `actor.kind = forge` 的事件 [提议]。
- [ ] 会话台账：`delivery_id` 为空、带 `conversation_id` 的 `budget.charged` 计入该会话当日台账；跨 UTC 日新建台账；`delivery_id` 与 `conversation_id` 都为空的 `workflow.created` 被拒（P-58）。
- [ ] 收件箱：同一 `dedup_key` 的第二个 `open` 项被唯一索引拒绝；`inbox_summary` 的按项目计数与逐项统计一致（P-63）。

### 12.4 Outbox

- [ ] 认领互斥：N 个并发认领者争抢 M 个就绪行，每行恰好被一个认领者拿到；`exclude_deliveries` 里的交付单元的行不被认领且保持 `queued`。
- [ ] 出队顺序 `(priority, enqueued_seq)`；`not_before` 未到不出队；依赖未 `succeeded` 不出队；依赖 `failed` / `cancelled` 时 `cancel_blocked` 级联取消并发 `outbox.cancelled`。
- [ ] `requeue(refund_attempt = true)` 不增加 `attempts`；`fail(Retry)` 回到 `queued`（带 `not_before`）且不产生 `outbox.failed`；`fail(Terminal)` 置 `failed` 并产生恰好一条 `outbox.failed`，再次 `enqueue` 同键不复活，`epoch + 1` 得到新行；任何路径都不出现 `failed → queued`（P-53）。
- [ ] `coalesce_key`（M2）：新行创建后同键 `queued` 旧行被取消，已 `executing` 的不受影响。
- [ ] `count_succeeded_since` 的窗口边界正确。
- [ ] `outbox.*` 事件与 `outbox` 表状态一致：对随机操作序列，从事件重放得到的状态流转与表终态相容（`verify(Full)` 校验）。

### 12.5 CAS、备份、迁移、故障

- [ ] CAS：`put` 去重（同内容同摘要、只存一份）；并发 `put` 同内容安全；`get_bytes` 超过 `max` 被拒；构造的目录穿越摘要字符串无法通过 `Sha256Digest` 解析；文件写入后崩溃（有文件无行）经 `verify` 补行；`put_manifest` 对键序不同但内容相同的清单得到同一摘要。
- [ ] `backup_to` 在写者持续写入时产出的副本：`integrity_check` 通过，`head_seq` 对应一个已提交前缀；副本可被 `restore_from` 恢复，`log_epoch` 加一，`log_identity()` 返回新值（P-67）。
- [ ] 迁移：从 `schema_version = n` 升到 n+1 成功；更高版本 → `SchemaTooNew`；模块迁移表名不带前缀被拒。
- [ ] 第二个 `open` 同目录得到 `AlreadyRunning`。
- [ ] 小容量 tmpfs 写满：写返回 `DiskFull`、事务无半成品，读正常，释放空间后恢复。
- [ ] `write_tx_max_hold_ms` 到期强制回滚并返回 `TxTimeout`。
- [ ] 提交返回后 `SIGKILL`：重启后该事务的事件与投影都在（`synchronous = FULL`）；提交前 `SIGKILL`：都不在，且 `projected_seq == head`。
- [ ] 性能冒烟（不作为阻断项）：单事务批量追加 ≥ 1000 事件 / 秒；单事件提交 p99 < 50 ms [建议值]；`deliveries` 列表（万级行）p99 < 50 ms。

## 13. 开放问题

骨架没定、需要上报的点。"临时处理"只让本文自洽，不构成决定。

| # | 问题 | 本文临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | **已决：见裁决 C5 / P-51**（`Store::open_in_memory` 为 M0 替身，签名在 M0 冻结）。原问题：**M0 替身缺口**：milestones 的 M0 替身表没有 `store`，而 `forge`、`pki`、`policy`、`notify`、`repo`、`models`、`conversation`、`api` 在 M1 与 `store` 同期并行开发，都依赖它；`engine`、`policy`、`api` 的规格书已经假定一个 `InMemoryStore` 存在（engine 称其由 `contract` 提供）。是否把 §6 的签名冻结与 `InMemoryStore` 作为 M0 交付，或让 `store` 的最小可用版本先行？ | 按"M0 冻结签名 + 提供 `open_in_memory` / `open_temp`"写；`store` 实现仍从 M1 起 | milestones、全部 M1 泳道 |
| OQ-2 | **投影归属与可折叠性**：(a) 投影能否**纯由事件折叠**重建，取决于每个事件的 `data` 是否自带足够状态（"胖事件"）；data-model 只给了少数事件的 `data`，api.md §5.3 还允许客户端在 `data` 不足时重新 `GET`。(b) 谁拥有 `Approval` / `InboxItem` / `Device` 的投影表：`policy`、`notify`、`pki` 的规格书假定由本模块注册投影器、自己维护（`policy` 还用 `UPDATE approvals … WHERE status IN (…)` 做比较并交换）；`engine`、`conversation` 假定读 `store` 的投影且"引擎没有自己的表" | (a) 按"事件自带足够状态"设计，core.md §7.5 列出缺口，M0 须补；(b) 机制两种都支持（内置投影器 + 模块注册投影器 + `owns` 接管），**默认由内置投影器负责**，待决定 | `core`、全部事件发出方、`policy`、`notify`、`pki` |
| OQ-3 | **CAS 的归属与保留**：architecture §3 拓扑图把"结果库 CAS"画在 STORE 内，§4 模块地图的 `store` 行却没列；worker-protocol OQ-12 的保留期、隔离区保留期未定；单对象上限未定 | 放在 `store`；`gc` 默认不删；单对象上限 64 MiB | `store`、`sandbox`、`engine`、`deploy` |
| OQ-4 | **模块私有表放哪**：`forge` 的 `forge_webhook_inbox` / `forge_effect_index`、`pki` 表（architecture §6）、设备 `last_seen`（节流更新，非事件）、`api` 的 `Idempotency-Key` 记录（24 h 保留）、审批 challenge / nonce（`policy_challenges`，api.md OQ-6）、`policy_cursor`（可改用 `Cursors`）、通知推送令牌（M4） | 同一个 SQLite 文件，模块注册迁移、表名前缀、`module_conn` 同事务读写（§7.10） | `forge`、`pki`、`api`、`notify`、`policy` |
| OQ-5 | 事件、CAS、幂等记录的**保留与清理**：api.md OQ-13 暂定"保留不限"，`410 cursor_expired` 预留 | 事件永不删除；CAS 默认不删；不实现过期 | 存储增长、`api` |
| OQ-6 | **已决：见裁决 C21 / P-67**（`log_epoch` 每次恢复加一，事件流与快照响应携带，客户端变化即全量重同步；吊销另记 `revocations.journal`）。原问题：**备份恢复后的 `seq` 回退**：api.md 只处理"游标超前"（`409 cursor_ahead`）。恢复到旧备份后，`seq` 会被重新发放；一个游标已越过旧 head、但此后日志又长过它的客户端，会静默错过 / 错位事件 | `store` 暴露 `log_id` 与 `log_epoch`；`api` 在事件流与快照响应中携带 `log_epoch` | `api`、`desktop-shell`、`mobile` |
| OQ-7 | `GET …?rev=N`（config.md §2.3）当 `N` 不是该 `(scope, resource)` 的某个确切 rev 时：返回"≤ N 的最近一次"还是 `404` | 提供两种读取（`settings_at_rev` 精确、`settings_as_of` 取最近），`api` 择一 | `api` |
| OQ-8 | `synchronous` 默认 `FULL` 的成本是否接受（掉电可靠 vs 提交延迟） | 默认 `FULL`，可配 `NORMAL` | 性能、可靠性 |
| OQ-9 | (a) **已决：见裁决 C7 / P-53**（临时错误留在 `queued` 以 `not_before` 退避；`failed` 为终态，只在终态发 `outbox.failed`；人工重试 = 新 `epoch` 的新动作）。(b) 未决。原问题：Outbox：(a) `failed` 是终态还是可重试的中间态（data-model §4.3 写 `executing → failed（可重试）→ queued`，forge-actions §2.2 写 `failed`（终态）要 `epoch + 1`）；`outbox.failed` 是逐次失败都发（forge-actions §2.6 把可重试码也列在该事件的 `data.code` 下）还是仅终态（forge.md 写"仅终态"）；(b) 将来 `forge` 拆成独立进程时认领与通知如何做 | (a) 已按 P-53 落实到 §6.3、§7.6；(b) 现为单进程 + flock | `forge`、`core` |
| OQ-10 | 事件 `data` 体积：forge 事件含评论正文、`diff_hunk`、check 输出，可能很大；超限时由谁截断 | `store` 只拒绝超限事件；`forge` 须在发出前截断并置 `truncated` | `forge`、`store` |
| OQ-11 | `forge.<kind>`、`policy.denied`、`budget.*` 事件的 `subject` 取值未定，而 `by_subject` 与索引依赖它 | 暂不对这几类做 `by_subject` 保证，只保证 `by_type` / `audit` | `core`、`api` |
| OQ-12 | ~~事件 `ts` 与 forge 事件里 `occurred_at` 的精度~~ | 已决：forge-actions §6.4 规定 `occurred_at` / `observed_at` 按 data-model §1 输出三位毫秒（GitHub 秒级时间补 `.000`） | — |
