# 领域核心（`core`）

> 状态：设计 v0.1 · 里程碑：M0（类型与契约冻结），M1–M4 随契约增量 · 泳道：`contract` · crate / 目录：`crates/gantry-core`

`core` 是所有模块共用的领域词汇：ID、值类型、实体、状态机、事件、Forge 动作、Worker 协议类型和配置类型的唯一 Rust 定义，外加把它们落成 `schemas/` 的生成与校验。它没有 IO，任何模块都能依赖它而不引入运行时耦合；契约的每一次变化因此集中在一个 crate 里评审。

读者：实现 `core` 的 agent；其他模块的实现者只需读 §6（接口）和 §7.4–§7.6（状态机、事件、settings 规则表）。

本文遵循的优先级见 [AGENTS.md](../../AGENTS.md)：与 `decisions.md`、[architecture.md §5](../architecture.md#5-信任边界与不变式)、`interfaces/` 冲突时以后者为准，并**停下来提问**，不要在实现 PR 里改契约。标注 **[提议]** 的是本文为了能实现而给的默认，骨架没定；标注 **OQ-n** 的见 §13，不要自行拍板。

## 1. 职责

1. **标识符**：15 种带前缀的可按时间排序的唯一标识符（Universally Unique Lexicographically Sortable Identifier, ULID）ID（[data-model §1、§2](../data-model.md#1-约定)）：类型安全的 `Id<K>`、严格解析与格式化、单调生成器（时钟与熵源由调用方注入）。
2. **基础值类型**：`Timestamp`、`Level`、`Sha256Digest`、`GitOid`、`SecretName`、`UntrustedText`、`Ref`、`Card`、`CheckSummary`（[data-model §1.1](../data-model.md#11-共用值类型)）。
3. **实体**：[data-model §3](../data-model.md#3-实体字段) 的全部实体及其枚举。字段名与枚举值与 data-model 逐字一致（映射表见 §6.3）。
4. **状态机**：[data-model §4](../data-model.md#4-状态机) 的交付单元阶段、节点、尝试、审批、Outbox 动作，以及工作流、沙盒、收件箱、工作树锁。纯函数 + 常量转移表，非法转移返回错误；**谁来触发转移不归 `core`**（见 §2）。
5. **事件**：信封、事件类型目录（[data-model §5](../data-model.md#5-事件)）、每个类型 `data` 的 Rust 类型；校验函数 `validate_event_data`；`schemas/events/<type>.json` 的生成。
6. **Forge 契约类型**：`ForgeActionKind`（14 个）、`PolicyActionId`（20 个，[config.md §5.1](../interfaces/config.md#51-policiesactions)）、`ApprovalAction`、`ForgeEventKind`（23 个）、Outbox 幂等键派生 `IdempotencyKey::derive`（[forge-actions §2.2](../interfaces/forge-actions.md#22-幂等键)）、14 个动作的 payload 类型；对应 `schemas/forge/`。
7. **Worker 协议类型** **[提议，OQ-1]**：`JobSpec`、`ResultJson`、`ProgressEvent`、`Proposals`、各 role 的 `StructuredOutput`、`ResultManifest`、`ValidatedPatch`（[worker-protocol §4–§6、§5.2、§8.3](../interfaces/worker-protocol.md)）；对应 `schemas/worker/`。`gantry-worker`（沙盒内二进制）与服务端共用同一份定义。
8. **预算台账算术**：`BudgetLimits` / `BudgetUsed` / `BudgetDelta` 与按 [data-model BudgetLedger 口径](../data-model.md#device--budgetledger) 的纯函数（累计、是否超限）。
9. **设置（settings）**：八个 resource 的类型、内置默认、RFC 7386 合并、"仅收紧"的 clamp、`relaxes[]` 差分、所需角色与签名判定、校验器 V01–V22（[config.md §1.3、§2、§11](../interfaces/config.md)，该文标注为 **[提议]**）。
10. **规范化与摘要** **[提议，OQ-3]**：RFC 8785 JSON 规范化方案（JSON Canonicalization Scheme, JCS）、"拒绝重复键 + 深度上限"的严格 JSON 解析、`sha256:` 摘要串、审批签名载荷的构造。`forge` 幂等键、`api` 请求体摘要、`policy` 验签、`desktop-shell` 签名共用同一份实现，避免字节级分歧。
11. **不可信文本** **[提议，OQ-4]**：`UntrustedText` 的类型约束，以及 [worker-protocol §4.4](../interfaces/worker-protocol.md#44-输入材料打包与隔离标记) 步 2–3 的规范化函数。
12. **测试支撑**（cargo feature `testing`）：`FakeClock`、`SeqEntropy`、实体工厂（builders）、契约示例夹具（fixtures）。
13. **`schemas/` 生成与一致性**：由类型生成 JSON Schema，CI 校验提交的 schema 与生成结果一致，并以契约文档中的示例作为正例 / 反例测试。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 本文 §1 全部**类型**（含全部里程碑用到的事件类型、实体、枚举）；ID、`Timestamp`、`Level`；全部状态机的转移表与测试；JCS、严格 JSON；事件 `data` 类型与 `schemas/events/*.json`；`schemas/forge/*.json`、`schemas/worker/*.json`、`schemas/config/*.json`；settings 的合并 / clamp / `relaxes` / 角色判定与通用校验 V01–V03；`IdempotencyKey::derive`；`testing` feature。退出标准见 §12.1 |
| M1 | 随 M1 模块启用的校验器与守卫：`models`（V04–V06、V21–V22）、`egress`（V10、V13）、`models` ↔ `egress`（V11）、`github`、`sandbox`（V12）、`policies.actions / paths / approval / patch`（V14–V15）[提议，按 config.md §13 的设置页里程碑推断]；M1 用到的 `ApprovalPayload` 构造与测试向量 |
| M2 | `levels`（V07–V09）、`policies.escalation`（V16）、`guardian`（V17）、`notify`（V18）、项目覆盖规则（V19）的校验器；`delivery` 状态机中 `merge → review` 的完整触发器；审批失效的守卫 |
| M3 | 图改写相关（`workflow.revised`、`node.superseded`）的不变式校验；`arbiter` / `reasoner` 的结构化输出一致性规则（类型 M0 已有，规则此时启用） |
| M4 | `parent_id` / `depends_on` 的无环与父子汇总校验；手机档案的事件白名单常量（见 [api.md §5.4](../interfaces/api.md#54-过滤)） |

类型在 M0 一次冻结；之后只经"契约 PR"追加（data-model §1："枚举只追加"）。

## 2. 不负责

| 事 | 归谁 |
| --- | --- |
| 任何 IO：数据库、文件、网络、系统时钟、操作系统随机源 | `store` / `api` / `forge` / 部署组装；`core` 只定义 `Clock` / `Entropy` trait（OQ-5） |
| 阶段推进的**决策**（何时准入通过、何时进入下一阶段）、图模板选择、调度、预算预留 | `engine`；`core` 只回答"这条转移合法吗" |
| 投影到表、事件落盘与游标 | `store` |
| 危险动作是否放行、验签（ECDSA）、审批创建 | `policy`、`api`；`core` 只提供 `Approval` 状态机与签名字节的构造 |
| settings 的存储、角色检查、`If-Match`、secret 值转存、HTTP 错误体 | `api`、`store`、`secrets` |
| 客户端 API 的 `problem+json` 错误码映射 | `api`；`core` 只产出 config.md §11 的语义码 |
| 真实的 `Forge` / `SandboxDriver` / `ModelProvider` 等扩展点 trait | 各自模块；定义位置见 OQ-1 |
| 每个 Outbox `kind` 的 `target` / `intent` 推导（含 `render_digest`） | `forge`（OQ-9）；`core` 只提供 `IdempotencyKey::derive` |
| GitHub 域名族完整硬编码拒绝表 | `egress` 规格书（[config.md §5.5](../interfaces/config.md#55-不变式规定不可配置为放宽的项)）；`core` 的 V10 只带最小集（OQ-11） |
| 密钥值的存储 | `secrets`；`core` 的类型**不携带**密钥值（§8） |

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| —（叶子 crate，不依赖任何 Gantry 模块） | — | architecture §4 |

外部库约束（`Cargo.toml` 用 `cargo deny` 或等价检查强制）：

- **允许**：`serde`、`serde_json`、`schemars`、`thiserror`、`sha2`、`zeroize` / `secrecy`、`regex`（或等价的纯 Rust 实现）、纯计算的时间历法库。
- **禁止**：`tokio`、`sqlx`、`reqwest`、`std::fs` / `std::net` / `std::process` 的任何使用、`SystemTime::now`、`getrandom`（`testing` feature 之外）、`unsafe`。
- 必须能在 Linux、macOS、Windows 上编译（architecture §4 把 `core` 的位置写成"共享 crate"，`desktop-shell` 可能依赖它，OQ-2）。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 待解析的字符串：ID、时间戳、摘要、`SecretName` | `api`（客户端请求）、`store`（行）、`forge`（GitHub） | §6.1 的严格语法 | 客户端 / GitHub 来源不可信；`store` 来源可信但仍校验 |
| JSON 文本：settings 文档、事件 `data`、worker 的 `result.json` 等 | `api` / `store` / `sandbox` | 严格 JSON（拒绝重复键，深度 ≤ 32） | 沙盒、GitHub、客户端来源**不可信** |
| 当前状态 + 触发器 | `engine` / `store` / `policy` | §6.4 | 可信；外部事实先经 `forge` 对账（I4） |
| `Clock`、`Entropy` | 调用方注入 | §6.1 | 可信 |
| `ValidationContext` | `api` 在写入前组装 | §6.7 | 可信（服务端自身数据：模型能力表、模板登记、密钥存在性、其他 resource 的当前值） |
| 契约文档 | 人 | data-model、`interfaces/` | — |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| Rust 类型与纯函数 | 所有 Rust 模块 | §6 |
| `schemas/events/`、`schemas/forge/`、`schemas/worker/`、`schemas/config/` | `ops`（CI 校验）、`desktop-ui`（生成 TypeScript 类型）、`mobile`（Swift 手写 / 生成）、Mock API 服务 | JSON Schema（方言见 OQ-12） |
| 黄金夹具（契约文档中的示例原样落盘） | 所有泳道的测试 | `crates/gantry-core/fixtures/` |
| `testing` feature | 各模块的单元测试 | §6.9 |
| 校验报告 `ValidationReport` | `api`（拼 `422 validation_failed` 的 `errors[]`） | `{pointer, code, message, severity}`，码取 config.md §11 |

## 6. 对外接口

只写签名与语义。模块路径是建议，公开名称以本节为准。

### 6.1 标识符、时间、基础值

```rust
// ids
pub trait IdKind: Copy + 'static { const PREFIX: &'static str; }
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id<K: IdKind> { /* 128 位 ULID + PhantomData<K> */ }

pub type ProjectId = Id<Prj>;       // prj_
pub type DeliveryId = Id<Dlv>;      // dlv_
pub type WorkflowId = Id<Wfl>;      // wfl_
pub type NodeId = Id<Nod>;          // nod_
pub type AttemptId = Id<Att>;       // att_
pub type WorktreeId = Id<Wtr>;      // wtr_
pub type SandboxId = Id<Sbx>;       // sbx_
pub type ConversationId = Id<Cnv>;  // cnv_
pub type MessageId = Id<Msg>;       // msg_
pub type InboxId = Id<Inb>;         // inb_
pub type ApprovalId = Id<Apr>;      // apr_
pub type ActionId = Id<Act>;        // act_ （OutboxAction）
pub type DeviceId = Id<Dev>;        // dev_
pub type EventId = Id<Evt>;         // evt_
pub type BudgetId = Id<Bdg>;        // bdg_

impl<K: IdKind> Id<K> {
    pub fn from_parts(timestamp_ms: u64, random80: u128) -> Result<Self, IdError>; // timestamp_ms < 2^48，random80 < 2^80
    pub fn timestamp_ms(&self) -> u64;
}
// FromStr 严格；Display / Serialize 输出规范形；Deserialize 走 FromStr。
// Ord 即字符串序即时间序（同毫秒内由 IdGen 保证单调）。

pub struct Ulid(u128); // 无前缀，仅用于 api 的 request_id

pub trait Clock: Send + Sync { fn now(&self) -> Timestamp; }
pub trait Entropy: Send + Sync { fn fill(&self, buf: &mut [u8]); }
pub struct IdGen { /* Arc<dyn Clock>, Arc<dyn Entropy>, Mutex<(last_ms, last_random)> */ }
impl IdGen {
    pub fn new(clock: Arc<dyn Clock>, entropy: Arc<dyn Entropy>) -> Self;
    /// 单调：同一毫秒内随机部分 +1；时钟回拨时沿用上一次的毫秒数；随机部分溢出返回 IdError::Overflow。
    pub fn next<K: IdKind>(&self) -> Result<Id<K>, IdError>;
    pub fn next_ulid(&self) -> Result<Ulid, IdError>;
}

// time
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64); // Unix epoch 毫秒（UTC）
impl Timestamp {
    pub fn from_millis(ms: i64) -> Self;
    pub fn as_millis(self) -> i64;
    pub fn parse_rfc3339(s: &str) -> Result<Self, TimeError>;   // 宽松：0–9 位小数、Z 或偏移；归一为 UTC 毫秒，多余精度截断
    pub fn parse_canonical(s: &str) -> Result<Self, TimeError>; // 严格：恰好三位毫秒 + Z（审批签名载荷）
    pub fn to_canonical_string(self) -> String;                 // 2026-10-24T12:41:03.120Z；Serialize 一律输出此形式
    pub fn checked_add_secs(self, secs: u64) -> Option<Self>;
}

// 档位
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level { None, Low, Medium, High, Max, Ultimate } // serde："None"…"Ultimate"
pub enum LevelKey { Low, Medium, High, Max, Ultimate }    // 配置对象键 low…ultimate；没有 none（V07）
impl Level { pub fn capped_by(self, cap: Level) -> Level; pub fn key(self) -> Option<LevelKey>; }

// 摘要与引用
pub struct Sha256Digest([u8; 32]);   // 文本形式 "sha256:" + 64 位小写十六进制
pub struct GitOid(/* 40 位小写十六进制 */);
pub struct RepoSlug(/* owner/repo */);
pub struct SecretName(String);       // ^[a-z0-9][a-z0-9._-]{0,63}$
```

不可信文本与密钥写入：

```rust
pub struct UntrustedText { /* text, source, truncated */ }
// 刻意没有 Deref<Target=str> / Display / AsRef<str>；取内容必须调用 as_untrusted_str()，便于 grep 与评审。
// Debug 只输出 source、长度、truncated，不输出内容。
impl UntrustedText {
    pub fn new(text: String, source: String, truncated: bool) -> Self;
    pub fn as_untrusted_str(&self) -> &str;
    pub fn source(&self) -> &str;           // 如 "github:issue.title"、"sandbox:report"
    pub fn truncated(&self) -> bool;
}
pub struct Normalized { pub text: String, pub stats: NormalizeStats }
/// worker-protocol §4.4 步 2–3：UTF-8 宽松解码、去 NUL / C0（保留 \n \t）/ ANSI / bidi / Tag 字符，按上限取头尾截断。
pub fn normalize_untrusted(input: &[u8], limits: NormalizeLimits) -> Normalized;

// 密钥：core 只有"引用"，没有值的读取路径
pub struct SecretRefView { pub secret_ref: SecretName, pub present: bool }
pub enum SecretWrite { Keep, Set(SecretString) }
// Deserialize：字段缺省 = Keep；字符串 = Set；null = Err(code "secret_clear_in_use")。不实现 Serialize / Clone；Debug 不输出值。
pub enum Consumer { Forge, Pki, Egress }  // 密钥的使用方（architecture §4 secrets 行）
```

### 6.2 严格 JSON、规范化、摘要

```rust
/// 拒绝重复键，深度上限（默认 32，worker-protocol §8.2），拒绝 NaN / Infinity；整数超出 2^53−1 的按调用方策略拒绝。
pub fn parse_json_strict(bytes: &[u8], opts: JsonLimits) -> Result<serde_json::Value, JsonError>;
/// RFC 8785 全量实现，含 IEEE-754 双精度数字的 ES6 序列化（settings 文档里有非整数，如 sandbox.limits.cpu_cores）。
pub fn jcs(value: &serde_json::Value) -> Result<Vec<u8>, CanonError>;
pub fn sha256(bytes: &[u8]) -> Sha256Digest;
pub fn jcs_digest(value: &serde_json::Value) -> Result<Sha256Digest, CanonError>; // sha256(jcs(value))

/// 审批签名载荷（api.md §6.2–§6.3）。待签字节 = "gantry/approval/v1\n" ‖ JCS(payload)。
pub struct ApprovalPayload {
    pub action: ApprovalAction, pub approval_id: ApprovalId, pub delivery_id: Option<DeliveryId>,
    pub device_id: DeviceId, pub expires_at: Timestamp, pub nonce: Nonce,
    pub subject_digest: SubjectDigest, pub target: ApprovalTarget, pub v: u32,
}
impl ApprovalPayload {
    pub fn signing_bytes(&self) -> Result<Vec<u8>, CanonError>; // 只含字符串、整数、对象；时间用 to_canonical_string
}
```

### 6.3 实体与枚举对照

字段与枚举值的权威是 data-model；下表只记 Rust 类型名与线上形态上需要实现者注意的点。省略的 `id`、`created_at`、`updated_at` 在所有实体上都存在（api.md §0 约定）。

| Rust 类型 | data-model | 线上形态要点 |
| --- | --- | --- |
| `Project` | §3 Project | 不含 `settings_override`（api.md §4.3、§8.2：它只经 settings 路由读写，由 `SettingsDoc` 承载） |
| `Delivery`、`Stage`、`StageStatus`、`Origin` | §3 Delivery、§4.1 | `Stage`：`spec` / `plan` / `implement` / `propose` / `review` / `merge` / `closed` / `abandoned`；`StageStatus`：`running` / `waiting_human` / `blocked` / `done` |
| `IssueRef`、`PullRequestRef`、`CheckSummary` | §3、§1.1 | `title` 是 `UntrustedText`；`mergeable_state` 保存 GitHub 原值字符串，不枚举；`PullRequestRef.state` 为 `open` / `closed` / `merged` |
| `Workflow`、`WorkflowStatus` | §3 | `status`：`running` / `paused` / `succeeded` / `failed` / `cancelled`（api.md §4.7 示例的 `finished` 不在此集合，见 OQ-6） |
| `Node`、`Role`、`Tier`、`Exec`、`Verdict` | §3、architecture §9 | `Role::Review(String)` 序列化为 `review:<视角>`，视角匹配 `[a-z][a-z0-9_-]*`；`Verdict` 大写 `ACCEPT` / `REVISE` / `REJECT` / `BLOCKED`；`Tier`：`utility` / `planner` / `worker`；`Exec`：`sandbox` / `sandbox_ro` / `direct`；`role`、`tier`、`exec` 的固定搭配由 `fn role_profile(Role) -> (Tier, Exec)` 给出，构造 `Node` 时校验 |
| `Attempt`、`AttemptStatus`、`FailureClass`、`TokenUsage` | §3 | `FailureClass`：`timeout` / `exec_failed` / `verdict_fail` / `policy_denied` / `cancelled` / `budget_exhausted` / `infra_error`；`failure_detail` 为 `FailureDetail(String)`，形如 `timeout.wall`，已知子码常量取自 worker-protocol §9，但不做封闭枚举（协议会增长）；`TokenUsage = {input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, requests}` |
| `Worktree`、`WorktreeLock`、`LockHolder` | §3 | `holder`：`engine` / `guardian` / `human` |
| `Sandbox`、`SandboxStatus`、`SandboxTrust`、`SandboxLimits`、`WorktreeSnapshot` | §3 | `status`：`created` / `running` / `stopped` / `destroyed` / `lost` |
| `Conversation`、`Message`、`MessageRole`、`AgentKind`、`Card`、`Ref` | §3、§1.1 | `Card` 未知 `kind` 反序列化为 `Card::Unknown`（保留原 JSON），不报错 |
| `InboxItem`、`InboxKind`、`Severity`、`InboxPayload` | §3 | `InboxPayload` 是按 `kind` 分派的枚举，形状见 data-model 的 payload 表 |
| `Approval`、`ApprovalStatus`、`ApprovalTarget`、`SubjectDigest` | §3、§4.4、api.md §6.1 | `SubjectDigest`：PR 类动作为 40 位 head SHA（`GitOid`），补丁与内部动作为 `sha256:…`；构造时按 `action` 校验形式 |
| `OutboxAction`、`OutboxStatus`、`Priority`、`ExpectedEffect`、`GithubRef` | §3、forge-actions §2、§4.2 | `status`：`queued` / `executing` / `succeeded` / `failed` / `cancelled` |
| `Device`、`DevicePlatform`、`DeviceRole`、`DeviceStatus` | §3 | `role`：`viewer` / `operator` / `approver`，实现 `Ord`（`approver > operator > viewer`） |
| `BudgetLedger`、`BudgetLimits`、`BudgetUsed` | §3、BudgetLedger 口径 | 台账另需记 `config_rev`（config.md §2.6），data-model 未列此字段，见 OQ-8 |
| `Event`、`Actor`、`ActorKind`、`Seq` | §5.1 | `ActorKind`：`engine` / `guardian` / `user` / `device` / `forge` / `system`；`Seq` 为 ≥ 1 的整数，构造时检查 ≤ 2^53−1 |

### 6.4 状态机

每个状态机提供：状态枚举、**常量转移表**（可导出为 mermaid 与 data-model 比对）、纯校验函数。校验函数不读时钟：时间由调用方传入。

```rust
pub mod machines {
    pub mod delivery {
        /// 触发器名与 data-model §4.1 状态图的边标签一一对应。
        pub enum StageTrigger {
            AdmissionAccept, AdmissionRevise, LowSkipAdmission, PlanProduced, PatchValidated,
            PlanInfeasible, PrCreated, ReviewRevise, CiFailureNeedsCode, ReviewAcceptCiGreen,
            ApprovalInvalidated, BaseAdvanced, MergedAndIssuesClosed,
            UserAbandoned, IssueNotPlanned, PrClosedUnmerged,
        }
        pub enum StatusTrigger { GateRejected, GateBlocked, HumanResolved } // 阶段内 stage_status 的变化，见 OQ-6
        pub const EDGES: &[Edge];   // (from, trigger, to, level_guard)
        pub fn next_stage(from: Stage, t: StageTrigger, level_effective: Level) -> Result<Stage, TransitionError>;
        pub fn is_legal_change(from: Stage, to: Stage, level_effective: Level) -> bool; // 供 store 校验 delivery.stage_changed
        pub fn next_status(cur: StageStatus, t: StatusTrigger) -> Result<StageStatus, TransitionError>;
    }
    pub mod node     { pub fn check(from: NodeStatus, to: NodeStatus) -> Result<(), TransitionError>; }
    pub mod attempt  {
        pub fn check(from: AttemptStatus, to: AttemptStatus) -> Result<(), TransitionError>;
        pub fn check_terminal(a: &Attempt) -> Result<(), InvariantError>; // 终态不可变；failure_class 与 status 一致
    }
    pub mod workflow { pub fn check(from: WorkflowStatus, to: WorkflowStatus) -> Result<(), TransitionError>; }
    pub mod sandbox  { pub fn check(from: SandboxStatus, to: SandboxStatus) -> Result<(), TransitionError>; }
    pub mod inbox    { pub fn check(from: InboxStatus, to: InboxStatus) -> Result<(), TransitionError>; }
    pub mod approval {
        pub fn check(from: ApprovalStatus, to: ApprovalStatus) -> Result<(), TransitionError>;
        /// granted 且未过期且 subject_digest 仍等于当前主体，才可消费（一次）。
        pub fn can_consume(a: &Approval, current_subject: &SubjectDigest, now: Timestamp) -> Result<(), ApprovalError>;
        /// 主体变化（PR head 变了）时应转入的状态：pending / granted → invalidated；其余 None。
        pub fn on_subject_changed(a: &Approval, new_subject: &SubjectDigest) -> Option<ApprovalStatus>;
    }
    pub mod outbox   { pub fn check(from: OutboxStatus, to: OutboxStatus, cause: OutboxCause) -> Result<(), TransitionError>; }
    pub mod worktree_lock {
        pub fn acquire(cur: Option<&WorktreeLock>, req: &LockRequest, now: Timestamp) -> Result<WorktreeLock, LockError>;
        pub fn renew  (cur: &WorktreeLock, holder: LockHolder, until: Timestamp, now: Timestamp) -> Result<WorktreeLock, LockError>;
        pub fn release(cur: &WorktreeLock, holder: LockHolder) -> Result<(), LockError>; // 只能释放自己持有的锁
        pub fn is_expired(cur: &WorktreeLock, now: Timestamp) -> bool;
    }
}
```

转移表的内容见 §7.4。

### 6.5 事件

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)] pub struct Seq(u64);

pub struct Actor { pub kind: ActorKind, pub id: String }
pub struct Event<D = serde_json::Value> {            // data-model §5.1 信封，字段名逐字一致
    pub id: EventId, pub seq: Seq, pub ts: Timestamp,
    #[serde(rename = "type")] pub ty: EventType,
    pub subject: Subject,                             // 实体 ID，或字面量 "settings"
    pub project_id: Option<ProjectId>,                // 全局事件为 None（序列化为 null）
    pub actor: Actor, pub causation_id: Option<EventId>, pub correlation_id: Option<String>,
    pub data: D,
}
pub struct NewEvent {                                 // store 追加时补 id / seq / ts
    pub ty: EventType, pub subject: Subject, pub project_id: Option<ProjectId>, pub actor: Actor,
    pub causation_id: Option<EventId>, pub correlation_id: Option<String>,
    pub data: serde_json::Value, pub dedup_key: Option<String>,   // 可选；forge.<kind> 必带（forge-actions G1），其他事件按需（如 message.posted）
}

/// 闭集；as_str / FromStr 与 data-model §5.2 逐字一致；"forge.<kind>" 用 Forge(ForgeEventKind)。
pub enum EventType { /* 49 个固定类型 + Forge(ForgeEventKind) */ }
pub struct EventTypeInfo { pub name: &'static str, pub emitter: &'static [&'static str], pub subject: SubjectKind }
pub const EVENT_TYPES: &[EventTypeInfo];

/// 形状校验：能反序列化为该类型的 data 结构，且满足一致性规则；失败返回带 JSON Pointer 的错误。
pub fn validate_event_data(ty: &EventType, data: &serde_json::Value) -> Result<(), SchemaError>;
pub fn decode_event_data<D: EventData>(ev: &Event) -> Result<D, SchemaError>; // D::TYPE 必须与 ev.ty 相符
```

### 6.6 Forge 契约类型

```rust
pub enum ForgeActionKind { PushBranch, CreatePr, UpdatePr, UpsertStatusComment, CreateComment,
    ReplyReviewThread, ResolveReviewThread, CreateIssue, UpdateIssue, CloseIssue, AddLabels,
    RequestReviewers, MergePr, RerunFailedJobs }                 // serde snake_case，共 14 个
pub enum PolicyActionId { /* config.md §5.1 的 20 个 id；ForgeActionKind 的 14 个 + force_push、
    push_outside_prefix、resolve_review_thread_human + 预留 delete_branch、publish_release、change_repo_settings */ }
pub enum InternalAction { SettingsUpdate, DeviceRoleChange, DeviceRevoke }
pub enum ApprovalAction { Policy(PolicyActionId), Internal(InternalAction) }   // 线上是同一个字符串空间，见 OQ-7
impl PolicyActionId { pub fn is_dangerous(&self) -> bool; pub fn allows_auto(&self) -> bool; } // 前 7 个不允许 auto（P-42）

pub struct IdempotencyKey(String);                               // "gk1_" + 26 位小写 base32（无填充）
impl IdempotencyKey {
    /// base32(first16(sha256(JCS(["gantry/outbox/v1", project_id, delivery_id, kind, target, hex(sha256(JCS(intent))), epoch]))))
    pub fn derive(project: &ProjectId, delivery: &DeliveryId, kind: ForgeActionKind,
                  target: &str, intent: &serde_json::Value, epoch: u32) -> Result<Self, CanonError>;
}
pub enum ForgeEventKind { /* forge-actions §6.3 的 23 个 kind：pr_opened … repo_changed */ }
pub struct ForgeEventEnvelope { /* forge-actions §6.2 信封；写入事件日志时作为 forge.<kind> 的 data */ }
```

14 个动作的 payload 类型（`PushBranchPayload`、`CreatePrPayload`…）与 [forge-actions §3.3](../interfaces/forge-actions.md#33-payload-示例) 的示例一一对应，`deny_unknown_fields`。

### 6.7 预算与设置

```rust
// 预算（纯函数；谁在何时调用归 engine / models）
pub struct BudgetDelta { pub tokens: TokenUsage, pub tier: Tier, pub nodes: u32, pub attempts: u32,
                         pub rounds: u32, pub wall_time_s: u64, pub forge_writes: u32 }
impl BudgetLedger {
    pub fn apply(&mut self, d: &BudgetDelta, w: &TokenWeights) -> Vec<BudgetDimension>; // 返回本次新变为"已耗尽"的维度
    pub fn would_exceed(&self, d: &BudgetDelta, w: &TokenWeights) -> Vec<BudgetDimension>; // used + delta > limit
    pub fn is_exhausted(&self, dim: BudgetDimension, w: &TokenWeights) -> bool;           // used >= limit
}
pub struct TokenWeights { /* input, output, cache_read, cache_write；默认全 1，见 OQ-10 */ }

// 设置
pub enum Resource { Models, Levels, Policies, Guardian, Sandbox, Egress, Notify, Github }
pub enum Scope { Global, Project(ProjectId) }
pub const SETTINGS_SCHEMA_VERSION: u32;

pub fn builtin_defaults(r: Resource) -> serde_json::Value;                  // 随二进制；models.*.model 等无默认的键保持缺失
pub fn merge_patch(base: &Value, patch: &Value) -> Value;                   // RFC 7386；数组整体替换；存储文档不含 null
pub struct Effective { pub value: Value, pub clamped: Vec<Clamp> }
pub fn effective(r: Resource, builtin: &Value, global: &Value, project: Option<&Value>) -> Effective;
pub fn diff_relaxes(r: Resource, scope: &Scope, old: &Value, new: &Value) -> Vec<KeyPath>;  // config.md §2.5 的 relaxes[]
pub struct Authority { pub required_role: DeviceRole, pub requires_signature: bool }
pub fn required_authority(r: Resource, relaxes: &[KeyPath], new_doc: &Value, writes: &[SecretWriteIntent]) -> Authority;

pub struct SecretWriteIntent { pub name: SecretName, pub consumer: Consumer, pub write: SecretWriteKind } // 不含值
/// 把请求体里的 SecretRef.value 剥出来：返回（可入事件的稀疏文档，待转存的值）。值只出现在第二个返回值。
pub fn split_secret_values(r: Resource, request_value: Value) -> Result<(Value, Vec<(SecretName, Consumer, SecretString)>), ValidationReport>;
pub fn find_secret_refs(r: Resource, doc: &Value) -> Vec<(JsonPointer, SecretName, Consumer)>;

pub trait ValidationContext {
    fn model_capability(&self, provider: &str, model: &str) -> Option<ModelCapability>;  // V04 / V05
    fn adapter(&self, name: &str) -> Option<AdapterCapability>;                          // V06
    fn graph_templates(&self, level: LevelKey) -> &[String];                             // V08
    fn secret_exists(&self, consumer: Consumer, name: &SecretName) -> bool;              // V03
    fn resource(&self, r: Resource, scope: &Scope) -> Option<&Value>;                    // 跨 resource 的 V09 / V11 / V13 / V19
    fn host_memory_mb(&self) -> Option<u64>;                                             // V12 提示
}
pub struct ValidationIssue { pub pointer: String, pub code: &'static str, pub message: String, pub severity: Severity }
pub struct ValidationReport { pub errors: Vec<ValidationIssue>, pub warnings: Vec<ValidationIssue> }
pub fn validate(r: Resource, scope: &Scope, doc: &Value, ctx: &dyn ValidationContext) -> ValidationReport;
```

### 6.8 `schemas/` 生成

`cargo xtask schemas`（或等价命令）：把 §6 的类型经 `schemars` 生成 JSON Schema 写入 `schemas/{events,forge,worker,config}/`；`cargo xtask schemas --check` 在 CI 中失败于"提交的文件与生成结果不一致"。`schemas/openapi.yaml` 不由 `core` 生成（归 `api` 契约），但其中引用的实体 schema 须与 `core` 一致（M0 退出标准"所有 schema 通过校验并与 `docs/interfaces/` 一致"）。

### 6.9 测试支撑（feature `testing`）

```rust
pub struct FakeClock { /* 可手动推进 */ }  impl Clock for FakeClock {}
pub struct SeqEntropy { /* 确定性序列 */ } impl Entropy for SeqEntropy {}
pub mod builders { /* delivery() / node() / attempt() / approval() / event() ... 带合法默认值 */ }
pub mod fixtures { pub fn load(name: &str) -> serde_json::Value; } // 取自契约文档示例的原样 JSON
```

## 7. 内部设计

### 7.1 crate 布局

```
crates/gantry-core/
  src/
    lib.rs
    ids.rs  time.rs  level.rs  text.rs  secret.rs  canon.rs  json_strict.rs
    entities/      project.rs delivery.rs workflow.rs attempt.rs worktree.rs sandbox.rs
                   conversation.rs inbox.rs approval.rs outbox.rs device.rs budget.rs
    machines/      delivery.rs node.rs attempt.rs workflow.rs sandbox.rs inbox.rs approval.rs outbox.rs worktree_lock.rs
    events/        envelope.rs catalog.rs data/*.rs      # 每个事件类型一个 data 结构
    forge/         kinds.rs actions.rs events.rs key.rs payloads/*.rs
    worker/        job.rs result.rs progress.rs proposals.rs output/*.rs manifest.rs patch.rs
    settings/      resource.rs defaults/*.json rules.rs merge.rs clamp.rs authority.rs validate/*.rs
    testing/       (feature = "testing")
  fixtures/        # 契约文档示例的原样拷贝 + 来源注释 + 文档块哈希
  xtask/           # schemas 生成与检查
```

### 7.2 serde 与线上形态约定

| 约定 | 做法 |
| --- | --- |
| 枚举字符串 | 小写 `snake_case`；`Level` 为首字母大写（`"Medium"`）；`Verdict` 为大写；`Role::Review` 为 `review:<视角>`（自定义 `Serialize` / `Deserialize`） |
| 输出的确定性 | 对象一律用 `BTreeMap` / 字段声明顺序，不用 `HashMap`；同一值多次序列化字节相同 |
| 整数 | 对外 JSON 整数不超过 2^53−1（api.md §1.5）：`Seq`、计数、字节数的构造函数检查 |
| 未知字段 | 请求形状与 settings 文档（V01）、动作 payload：`deny_unknown_fields`。实体的输出形状：容忍未知字段（前向兼容） |
| 未知枚举值 | 服务端解码**严格**（未知即错）；客户端要求容错解码（api.md §1.4）。若 `desktop-shell` 依赖 `core`，用 `Tolerant<T>`（`Known(T)` / `Unknown(String)`）包装客户端侧类型，不改变服务端类型（OQ-2） |
| `Option` | `null` 表示"没有值"，键始终出现在输出中（与 api.md 示例一致）；审批签名载荷是例外：无值的键**省略**，且不允许 `null` |
| 时间 | 一律 `Timestamp`，输出三位毫秒 + `Z` |
| 增量演进 | 新字段加 `#[serde(default)]`；枚举新增值只追加；删除或改义需经契约 PR（data-model §1） |

### 7.3 ID 编码

- 文本形式 `<prefix>_<ULID>`，`ULID` 为 26 位 Crockford base32，字母表 `0123456789ABCDEFGHJKMNPQRSTVWXYZ`，**只接受大写**，首字符 ≤ `7`（48 位毫秒时间戳 + 80 位随机，共 128 位）。
- 解析失败的原因分别报告：前缀不符、长度、非法字符、小写、溢出。
- 内部存 `u128`；`Ord` 与文本序一致，所以 `store` 可以用 `ORDER BY id` 做键集分页。
- `IdGen` 的状态在一把互斥锁里；同毫秒内随机部分 +1，保证先生成的 ID 排在前面。
- `act_`（OutboxAction 的 ID）与幂等键 `gk1_…` 是两种东西：后者是 `IdempotencyKey`，不是 `Id<K>`。

### 7.4 状态机转移表

**交付单元阶段**（data-model §4.1 状态图的逐边实现）。`level_effective` 守卫只有两处：Low 走 `LowSkipAdmission`，其余档位走 `AdmissionAccept`。

| from | 触发器 | to | 备注 |
| --- | --- | --- | --- |
| `spec` | `AdmissionAccept` | `plan` | 非 Low |
| `spec` | `AdmissionRevise` | `spec` | 追问；自环 |
| `spec` | `LowSkipAdmission` | `implement` | 仅 Low |
| `plan` | `PlanProduced` | `implement` | |
| `implement` | `PatchValidated` | `propose` | |
| `implement` | `PlanInfeasible` | `plan` | |
| `propose` | `PrCreated` | `review` | |
| `review` | `ReviewRevise` / `CiFailureNeedsCode` | `implement` | 两个触发器同一条边 |
| `review` | `ReviewAcceptCiGreen` | `merge` | |
| `merge` | `ApprovalInvalidated` / `BaseAdvanced` | `review` | 同一条边 |
| `merge` | `MergedAndIssuesClosed` | `closed` | |
| `spec` | `UserAbandoned` / `IssueNotPlanned` | `abandoned` | 状态图只画了这一个起点 |
| `review` | `PrClosedUnmerged` | `abandoned` | 状态图只画了这一个起点；`UserAbandoned` 是否也可从 `review` 及其他阶段发起见 OQ-6 |

`closed`、`abandoned` 是终态，没有出边。**转移表按状态图实现，不自行补边**；状态图没覆盖的现实情形（例如 `merge` 阶段 PR 被关闭、`implement` 阶段用户放弃）会让调用方得到 `TransitionError`，这是有意的：它暴露契约缺口，由 OQ-6 推动契约 PR 补边。

**其他状态机**（data-model §4.2–§4.4 及各契约）：

| 状态机 | 合法转移 | 出处 / 备注 |
| --- | --- | --- |
| 节点 | `pending → queued → running → succeeded / failed / skipped / cancelled`；`superseded` 为被图改写替换 | §4.2。进入 `skipped` / `cancelled` / `superseded` 的合法起点、`failed` 后重试时节点状态：OQ-6，**暂只放行 §4.2 字面写出的链**，并额外放行 `pending / queued → cancelled / superseded`（worker-protocol §3.4 的"节点被图改写替换而取消"）[提议] |
| 尝试 | `preparing → running → succeeded / failed / cancelled`；`preparing → failed / cancelled` | worker-protocol §3.2–§3.4（创建 / 注入阶段失败或被取消）。终态不可变。`check_terminal`：`failed` / `cancelled` 必有 `failure_class`；`cancelled` 的 `failure_class` 恒为 `cancelled`；`succeeded` 且 gate 判定非 `ACCEPT` 时 `verdict_fail` 对应的 `Attempt.status`：OQ-6 |
| 工作流 | `running ⇄ paused`；`running / paused → succeeded / failed / cancelled` | §3 Workflow 枚举；转移为 [提议]（骨架只给了枚举） |
| 沙盒 | `created → running → stopped → destroyed`；任意非 `destroyed` → `lost`；`created / running / stopped → destroyed` | §3 Sandbox 枚举 + worker-protocol §3.2、§3.5；转移为 [提议] |
| 收件箱 | `open → resolved / dismissed` | api.md §4.8：`dismiss` 对 `dismissed` 幂等，对 `resolved` 为 `conflict`；`choose` 仅 `open` |
| 审批 | `pending → granted → consumed / invalidated / expired`；`pending → denied / invalidated / expired` | data-model §4.4、P-20。`consumed` 只能一次 |
| Outbox | `queued → executing → succeeded`；`executing → failed`；`failed → queued`（可重试）；`executing → queued`（限流退避、租约过期重放）；`queued / executing / failed → cancelled` | data-model §4.3。`failed` 是终态还是可重试的中间态，data-model 与 forge-actions §2.2 的措辞不同（OQ-6）；转移表放行 `failed → queued`，`OutboxCause`（`Retry` / `RateLimited` / `LeaseExpired` / `DependencyFailed` / `Superseded` / `Manual`）区分原因 |
| 工作树锁 | `acquire`：无锁、或已过期、或同持有者 → 成功；否则 `LockConflict`。`release`：只能释放自己的锁。`human` 锁租约默认 1800 s、最长 14400 s（api.md §3.7）由调用方传入上限 | data-model §3 Worktree；租约到期**由调用时的 `now` 判定**，不依赖后台任务 |

### 7.5 事件目录实现清单

`EVENT_TYPES` 必须覆盖 data-model §5.2 的全部类型（49 个固定类型 + 23 个 `forge.<kind>`）。下表按事件族列出**已知**的 `data` 内容及其出处，以及 M0 必须补齐的缺口。凡写"缺口"的，`contract` 泳道在 M0 通过契约 PR 补全并回写 data-model，不要靠实现者猜。

| 事件族 | 发出方 | 已知的 `data` 内容（出处） | 缺口 |
| --- | --- | --- | --- |
| `settings.updated` | `api` | `scope`、`resource`、`schema_version`、`base_rev`、`value`（已剥离 secret）、`relaxes[]`、`secret_changes[]`（`{ref, op, version}`，无值无哈希）；`subject` 为 `settings` 或 `prj_…`（config.md §2.3） | 无 |
| `forge.<kind>` | `forge` | `data` 即 `ForgeEventEnvelope`（forge-actions §6.2–§6.3）；`dedup_key` 进信封外的唯一索引列 | `subject` 取值（交付单元 / PR / 项目）未定；`occurred_at` 示例无毫秒（宽松解析兜住） |
| `forge.synced` / `forge.drift_detected` | `forge` | drift 形状见 forge-actions §7.4 | `forge.synced` 的 data（快照差异）未定 |
| `outbox.*` | `forge`、`store` | `outbox.failed.data.code`（forge-actions §2.6）；`outbox.succeeded` 带 `github_ref` | `enqueued` 需带完整动作（payload、依赖、优先级）才能让投影可重建 |
| `delivery.*` | `engine`、`api` | `stage_changed` 示意 `{from, to}`（api.md §5.1）；`abandoned` 带可选 `reason`（≤ 500 字符，api.md §4.4） | `created` 需带完整交付单元与预算台账（模板名、limits、`config_rev`）；`stage_changed` 建议带 `trigger`；`level_changed` 未定 |
| `workflow.*` / `node.*` | `engine` | `superseded_by`（data-model）；`workflow.revised` 带新 `graph_rev` | 节点的创建随 `workflow.created` / `revised` 带入的假设需确认；`Workflow.template` 取值（族名还是阶段模板名，OQ-6） |
| `attempt.started` / `finished` | `engine` | `started`：实际使用的 provider / model / effort 与 `config_rev`（config.md §2.6）；`finished`：`result_ref`、`failure_class`、`failure_detail`、`usage`、`result_tree`（worker-protocol §3.2、§3.5） | `Attempt` 从 `preparing` 转 `running` 没有对应事件 |
| `attempt.progress` | `sandbox` / `engine` | 采样后的进度（worker-protocol §7），仅展示 | 形状未定 |
| `patch.validated` / `rejected` | `repo` | `ValidatedPatch`（`sha256`、`result_tree`、`files[]`、`verdict`、`reasons[]`，worker-protocol §8.3） | 无 |
| `sandbox.*` | `sandbox` | — | `Sandbox.status` 的 `running` / `stopped` / `lost` 没有对应事件（OQ-6） |
| `worktree.*` | `repo`、`api`（人工锁） | 锁字段 `{holder, delivery_id, acquired_at, lease_until}` | 租约到期自动释放是否产生 `worktree.unlocked`（原因 `expired`）未定 |
| `message.posted` | `api`、`conversation` | 完整 `Message`（api.md §4.5） | 无（建议带全文，使投影可重建） |
| `inbox.*` | `engine`、`guardian`、`api` | `resolved.data.option_id`（api.md §4.8）；payload 形状见 data-model | 无 |
| `approval.*` | `policy`、`api` | `requested` 带 `config_rev`（config.md §2.6）；`consumed` 需带消费时核对的 `subject_digest` | 其余未定 |
| `budget.charged` / `exhausted` | `engine`、`models` | 按 tier 的用量、实际 provider / model（config.md §3.4） | `charged` 的粒度（逐次调用 / 逐 Attempt）未定；`exhausted.data` = `{dimension, limit, used}`（data-model InboxItem payload 可参照） |
| `policy.denied` | `policy` | 原因码（worker-protocol §9 的 `policy.*`）、`config_rev` | 其余未定 |
| `device.*` / `project.*` / `conversation.created` | `pki`、`api`、`forge` | 对应实体的字段（data-model §3） | `project.added` 的触发者（api.md OQ-8） |
| 未入目录 | — | — | `artifact.unexpected`（worker-protocol §5.1、§8.2 称"记审计"）、`gantry device recover` 的审计事件（P-39）、自报用量与计量显著偏离的审计（worker-protocol §7.4）都没有事件类型（OQ-6） |

`EventData` trait：每个 data 结构实现 `const TYPE: EventTypeRef`，使 `decode_event_data` 能在类型不符时报错。

### 7.6 settings 规则表

config.md §2.2（项目覆盖矩阵）与 §2.5（"仅 approver 键"）用**一张常量规则表**实现，`effective`、`diff_relaxes`、`required_authority`、`validate`（V19）共用，避免四处各写一遍。

```rust
pub struct KeyRule {
    pub path: &'static str,        // 如 "levels.*.budget.*"、"policies.paths.*"
    pub project: ProjectOverride,  // Free / TightenOnly / NarrowOnly / GlobalOnly
    pub relax: RelaxCmp,           // 比较"放宽方向"的方式
    pub approver_only: bool,       // config.md §2.5 的"仅 approver 键"
}
pub enum RelaxCmp {
    NumGreaterLoosens,             // 预算、限额、ttl、阈值：变大 = 放宽
    EnumRank(&'static [&'static str]), // policies.actions：auto < approve < deny 的反向，见规则表
    GlobsFewerLoosens,             // policies.paths.*.globs：变少 = 放宽
    SetSupersetLoosens,            // egress.profiles.*.groups：加分组 = 放宽（项目只能收窄）
    BoolFalseLoosens, BoolTrueLoosens,
    Custom(fn(&Value, &Value) -> bool), // escalation 规则（关闭 / 删除 / 缩小 when / 降低 raise_to）等
    Incomparable,                  // 无方向：视同敏感键
}
```

- 规则表按 config.md §2.2 的行逐条录入，**每一行一个测试**（§12.2）。
- `required_authority`：任一 `relaxes` 非空，或写入了 `approver_only` 键，或带 secret `value` → `approver` + 需要签名；否则 `operator`、无签名（config.md §2.5）。
- `split_secret_values` 依据 schema 上的 `x-gantry-secret` 注解找 `SecretRef` 字段，`Consumer` 由字段路径决定：`models.providers.*.credential`、`egress.injection.*.credential` → `Egress`；`github.app.*` → `Forge`；`pki` 的中间 CA 私钥不经 settings（`pki` 直接用 `secrets`）。
- 内置默认放在 `src/settings/defaults/<resource>.json`，随二进制编译；config.md §4.3、§5、§6、§7 的 **[建议值，待调优]** 原样录入并保留该标注（注释字段不进 JSON，进同目录的 `.md` 说明）。

### 7.7 `schemas/` 生成流水线

1. `schemars` 派生，枚举 / `deny_unknown_fields` / `x-gantry-secret` 注解映射为 schema。
2. 事件：每个 `EventType` 一份 `schemas/events/<type>.json`（`forge.<kind>` 共用信封 schema + 按 kind 的 `data` 子 schema）。
3. 契约示例夹具：把 api.md、forge-actions.md、worker-protocol.md、config.md 中的示例块**原样**提取到 `fixtures/`，文件头记录来源文件与块的哈希；文档块变化而夹具未更新时 CI 失败，提醒同步。
4. 反例：每条校验规则至少一个反例夹具（config.md §12.3 的"会被拒绝的写入"表逐行录入）。
5. 已知与 data-model 不符的示例（如 api.md §4.7 `Workflow.status: "finished"`）登记为 `known_failure`，附 OQ 编号；契约 PR 修正后移除。

## 8. 不变式与安全要求

| 不变式 | `core` 如何保证 |
| --- | --- |
| I2 GitHub 写入唯一路径 | `ForgeActionKind` 是封闭枚举，payload `deny_unknown_fields`；幂等键只有 `IdempotencyKey::derive` 一个构造入口，输入含 `project_id`、`delivery_id`、`kind`、`target`、`intent_digest`、`epoch`，且 `intent` 不接受时间戳 / 重试计数这类易变字段的约定写进文档注释与测试向量 |
| I3 放行唯一决策点 | `Approval` 状态机让 `consumed` 只能从 `granted` 进入，`can_consume` 同时核对未过期与 `subject_digest`；`SubjectDigest` 按动作校验形式（PR 类 40 位 SHA，补丁 / 内部动作 `sha256:`）；审批签名字节只有 `ApprovalPayload::signing_bytes` 一个构造点，`api`（验签）与 `desktop-shell`（签名）若共用它就不会字节分歧 |
| I4 状态各有事实源 | `IssueRef` / `PullRequestRef` 是带 `synced_at` 的快照类型，与本地编排状态（`Delivery`）分开；`ForgeEventEnvelope` 带 `origin` / `stale` / `source`，消费方无法忽略这些字段而不改类型 |
| I5 外部文本不可信 | `UntrustedText` 无 `Deref` / `Display`，`Debug` 不输出内容；所有来自 GitHub / 沙盒的文本字段类型都是它（IssueRef.title、review 评论、CI 输出、`result.json` 的 `summary` 等）；JobSpec 的 `task.instructions` 是普通 `String`，类型上与 `UntrustedText` 不可互换 |
| I6 客户端不持凭据 | 实体与事件类型里没有任何承载密钥值的字段；`SecretRefView` 只有 `present`；`SecretWrite` 不实现 `Serialize` / `Clone`，`Debug` 脱敏，所以密钥值无法被序列化进响应、事件或日志 |
| I7 配置与工作分离 | settings 类型与"工作请求"类型分开；消息请求体 `deny_unknown_fields`，`model` / 预算字段被拒（api.md §7）；`levels` 的键集合在类型上排除 `none`；"仅收紧"规则表是 settings 唯一的放宽判据 |
| I8 先验后推 | `ValidatedPatch` 是 `repo` 校验结论的唯一类型，`push_branch` payload 的 `validation_ref` / `expected_tree_oid` 与它对应；`core` 不提供"绕过校验"的构造路径 |
| 其他 | 无 `unsafe`；无全局可变状态；所有解析器对畸形输入返回错误而不是 panic（模糊测试覆盖，§12.3）；`jcs` 对重复键 / 非法数字 fail-closed |

## 9. 错误与恢复

`core` 没有运行期状态，也就没有"恢复"；所有失败都是返回给调用方的结构化错误，**不 panic**。

| 失败 | 处理 |
| --- | --- |
| ID / 时间 / 摘要 / `SecretName` 解析失败 | `IdError` / `TimeError` / `DigestError` / `SecretNameError`，带原因分类；调用方（`api`）映射为 `400` / `422` |
| `IdGen` 随机部分溢出 | `IdError::Overflow`，调用方稍后重试（实际上 80 位随机几乎不会触发） |
| 时钟回拨 | `IdGen` 沿用上一次毫秒数，不报错；`Timestamp` 本身不检查单调性（由 `store` 保证事件 `ts` 非递减） |
| 非法状态转移 | `TransitionError { machine, from, to_or_trigger }`；调用方必须当作缺陷处理（回滚事务、上报），**不得吞掉或改写目标状态** |
| 事件 `data` 形状不符 | `SchemaError`，带 JSON Pointer；`store` 拒绝追加 |
| JSON 含重复键 / 超深 / 非法数字 | `JsonError`；`jcs` 返回 `CanonError`；审批与摘要路径一律 fail-closed |
| 校验器遇到未知键 | V01：`unknown_field` 错误，不静默忽略 |
| 校验上下文缺数据（如模型能力表里没有该模型） | config.md OQ-3 未定未知模型的处理；暂按 fail-closed：V04 / V05 在上下文返回 `None` 时报 error，不放行 |

## 10. 配置

`core` 不读取任何运行期配置，也不读 settings——它定义 settings 的类型与规则。下表是编译期常量，修改它们是契约变更。

| 常量 | 值 | 来源 |
| --- | --- | --- |
| ID 前缀表 | `prj dlv wfl nod att wtr sbx cnv msg inb apr act dev evt bdg` | data-model §2 |
| `SETTINGS_SCHEMA_VERSION` | `1` | config.md §2.8 `catalog.schema_version` 示例；升级规则见 OQ-13 |
| 审批签名前缀 / 版本 | `"gantry/approval/v1\n"` / `v = 1` | api.md §6.2–§6.3 |
| 幂等键前缀 / 版本串 | `"gk1_"` / `"gantry/outbox/v1"` | forge-actions §2.2 |
| JSON 深度上限 | `32` | worker-protocol §8.2 |
| 整数安全上限 | `2^53 − 1` | api.md §1.5 |
| `approval.ttl_s` 范围 | `60 ..= 604800` | config.md §5.4 |
| `policies.patch.*` 的硬上限 | **未给值** | config.md §5.5"大小阈值只能在 `core` 定义的硬上限内调"；OQ-8 |
| 手机档案事件白名单 | `project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked` | api.md §5.4 [提议]（M4 才启用） |

## 11. 可观测性

- `core` **不发出事件，不写日志，不产生指标**（无 IO）。它是事件类型的定义方：发出方与 `data` 缺口见 §7.5。
- 错误类型携带稳定的机器码（`code`），由调用方记录；`Debug` / `Display` 对 `UntrustedText`、`SecretWrite`、`SecretString` 脱敏（只给长度或 `***`）。
- 提供 `fn describe(&self) -> &'static str` 之类的低基数标签（状态机名、事件类型名、校验规则码），供调用方做指标标签，避免各模块自己拼字符串。

## 12. 测试与验收

### 12.1 M0 退出标准（`contract` 泳道，涉及 `core` 的部分）

- [ ] `schemas/events/`、`forge/`、`worker/`、`config/` 全部 schema 通过元校验，且与 `docs/interfaces/` 一致：§7.7 第 3 步提取的每个示例夹具都通过对应 schema 与对应 Rust 类型的反序列化，再序列化后与原文 JSON 语义相等（键序无关）。
- [ ] `cargo xtask schemas --check` 在干净检出上通过；任意手改 `schemas/` 下的生成文件会使其失败。
- [ ] Mock API 服务能用 `core` 的 `Event` 类型回放录制数据，按 `seq` 游标续传（与 `api` / `store` 的契约测试共用夹具）。
- [ ] `testing` feature 提供的 `FakeClock`、`SeqEntropy`、builders 各有至少一个使用示例测试。
- [ ] `core` 在 Linux、macOS、Windows 上 `cargo test` 通过，且 `cargo deny`（或等价检查）确认依赖符合 §3。

### 12.2 逐项验收

**标识符与时间**

- [ ] 15 种 ID 的往返测试；错误前缀、小写、长度 25 / 27、非法字母（`I L O U`）、首字符 `8`、`Id<Dlv>` 解析 `prj_…` 都被拒绝，且错误原因可区分。
- [ ] 属性测试：`IdGen` 在同一 `FakeClock` 毫秒内连续生成 10^5 个 ID，严格递增、无重复；时钟回拨时仍单调。
- [ ] `Timestamp::parse_canonical` 只接受恰好三位毫秒 + `Z`；`parse_rfc3339` 接受 `2026-10-24T12:41:02Z`（forge-actions §6.4 示例）和带偏移的输入；`to_canonical_string` 对 `parse_*` 的结果往返稳定。

**状态机**

- [ ] 交付单元：`EDGES` 导出的 mermaid 与 data-model §4.1 的边集合逐边比对（测试内置期望边集合，文档变化时需同步修改）；Low 档 `spec → plan` 被拒、非 Low 档 `LowSkipAdmission` 被拒。
- [ ] 对每个状态机做穷举测试：全部 `(from, to)` 组合，只有表内转移通过，其余返回 `TransitionError`（终态无出边）。
- [ ] 审批：`consumed` 只能从 `granted` 进入；同一审批第二次 `can_consume` 失败；`subject_digest` 变化后 `can_consume` 失败；过期后失败。
- [ ] 工作树锁：他人持有且未过期 → `LockConflict`；已过期 → 可被取得；`release` 他人的锁失败；`human` 租约超过调用方传入的上限被拒。
- [ ] `check_terminal`：`cancelled` 缺 `failure_class` 或 `failure_class != cancelled` 被拒；终态后再改任意字段被拒。

**事件**

- [ ] `EventType::from_str(as_str(x)) == x` 对全部 49 + 23 个类型成立；未知类型字符串返回错误；`EVENT_TYPES` 与 data-model §5.2 的类型清单逐项比对。
- [ ] 每个事件类型至少一个正例与一个反例 `data`；`validate_event_data` 对缺必填字段、多余字段、错误枚举值给出带 JSON Pointer 的错误。
- [ ] `settings.updated` 的 `data` 里，任何 `SecretRef` 位置带 `value` 键 → `SchemaError`（存储形态的 `SecretRef` 只有 `secret_ref` 等非值字段）；`secret_changes[]` 带 `value` 或哈希字段同样被拒。`store` 依赖这条 schema 作为"事件日志里没有密钥值"的防线（store.md §7.3）。

**Forge 与摘要**

- [ ] 测试向量：forge-actions §2.1 的 `create_pr` 示例经 `IdempotencyKey::derive` 得到 `gk1_anbfwcip7r36is6wsqcuxxfd34`（已在设计阶段用独立实现复现过）。
- [ ] 测试向量：api.md §6.3 的 `canonical(payload)` 字节与其 SHA-256 `be72c31c…1d7a` 逐字节一致（已复现）；`ApprovalPayload::signing_bytes` 对同一输入产出同样的字节。
- [ ] RFC 8785 官方测试向量（含 Appendix B 的数字序列化）全部通过；重复键、`NaN`、超深嵌套被拒。
- [ ] `PolicyActionId` 恰为 20 个，`ForgeActionKind` 恰为 14 个，`ForgeEventKind` 恰为 23 个；`allows_auto` 对前 7 个返回 `false`。

**预算**

- [ ] `apply` 按 data-model 口径：被图改写替换的节点仍计入 `nodes`，`attempts` 单独计，`wall_time` 为秒，`rounds` 为评审轮与守护自愈轮之和，`forge_writes` 为累计数；`concurrency` 不进 `used`。
- [ ] `would_exceed` 与 `is_exhausted` 的边界（`used + delta == limit` 不超限但已耗尽）有测试。

**settings**

- [ ] `merge_patch` 通过 RFC 7386 官方用例；数组整体替换；存储文档含 `null` 被拒。
- [ ] 规则表每行一个 `effective` / `diff_relaxes` 用例，覆盖 config.md §2.2 全部行；config.md §12.3 的"会被拒绝的写入"表逐行成为 `validate` 的反例，错误码一致。
- [ ] `required_authority`：`deny → approve`（放宽）需要 `approver` + 签名，`approve → deny`（收紧）只需 `operator`；带 secret `value` 的写入需要签名；"仅 approver 键"变更需要签名。
- [ ] V01–V22 各有正例与反例；警告级规则（V09 前半、V12 后半、V18 后半、V21、V22 后半）不产生 error。
- [ ] `split_secret_values`：`value` 缺省 → `Keep`；字符串 → 转存；`null` → `secret_clear_in_use`；返回的稀疏文档不含任何 `value`；形似密钥的内联字符串 → `inline_secret_detected`。
- [ ] `effective` 对"全局后来被收紧、既有项目覆盖变宽松"的情形输出 `clamped[]`，值取全局。

**类型约束（编译期）**

- [ ] `trybuild` 失败用例：`UntrustedText` 不能直接当 `&str` 用、不能 `format!("{}")`；`SecretWrite` 不能 `Clone` / `Serialize`；`Id<Dlv>` 不能传给要 `Id<Prj>` 的函数。

### 12.3 健壮性

- [ ] 对 ID / 时间 / 摘要 / JSON / settings 文档 / 事件 `data` 的解析器做模糊测试（`cargo fuzz` 或 `proptest` 的畸形输入生成），不 panic、不无限循环、不分配超过输入量常数倍的内存。
- [ ] 同一值多次序列化字节相同（确定性），含 `BTreeMap` 与浮点。

## 13. 开放问题

骨架没定、需要上报的点。"临时处理"只让本文自洽，不构成决定。

| # | 问题 | 本文临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | **契约类型与扩展点 trait 的归属**。M0 要交付 `FakeForge` / `FakeSandboxDriver` / `FakeRepo` / `FakeModelProvider`，它们要实现的 `Forge` / `SandboxDriver` / `Repo` / `ModelProvider` / `Notifier` / `WorkerAdapter` trait 定义在哪（`core`，还是各模块 crate 的 M0 骨架）？Worker 协议与 Forge payload 类型放 `core` 是否接受？ | Worker / Forge 的**数据类型**放 `core`；**trait** 不放（`core` 无 IO、无 async） | `contract` 泳道的 M0 范围；crate 依赖图 |
| OQ-2 | `desktop-shell`（Rust）是否依赖 `core`（architecture §4 把位置写成"共享 crate"）？若是，需要客户端容错解码（api.md §1.4）与 Windows / macOS 编译 | 服务端严格解码；客户端用 `Tolerant<T>` 包装，不改服务端类型；crate 保持跨平台 | `core` 依赖约束、`desktop-shell` 设计 |
| OQ-3 | JCS、严格 JSON、审批载荷构造放 `core` 不在 architecture §4 对 `core` 的一句话职责里 | 放 `core`（单一实现，避免字节分歧） | `policy`、`api`、`forge`、`desktop-shell` |
| OQ-4 | `UntrustedText` 的规范化（worker-protocol §4.4 步 2–3）与隔离包装归 `core` 还是 `sandbox`？ | 规范化放 `core`；包装（nonce、标记行）留给 `sandbox` | `sandbox`、`engine` |
| OQ-5 | `Clock` / `Entropy` 的真实实现放哪（`core` 禁止 IO） | 只定义 trait；生产实现由 `gantryd` 组装时提供 | 组装、测试 |
| OQ-6 | **状态机缺口**：(a) `abandoned` 的起点——状态图只有 `spec` / `review`，而 api.md `abandon` 路由与 P-37 对任意非终态可用，`merge` 阶段 PR 被关闭也没有边；(b) "重新规划"（`REJECT` 后）的边；(c) `stage_status` 各值的进入 / 退出条件（`waiting_human` / `blocked` / `done`；`AdmissionRevise` 自环期间的状态）；(d) architecture §8 表对 Medium 写"全部"、High 写"全部，含准入"，而 data-model §4.1 只豁免 Low——Medium 是否走准入；(e) 节点进入 `skipped` / `cancelled` / `superseded` 的起点，`failed` 后重试时的节点状态；(f) gate 判定非 `ACCEPT` 时 `Attempt.status`；(g) Outbox `failed` 是终态还是可重试中间态（data-model §4.3 与 forge-actions §2.2）；(h) `Attempt` 的 `preparing → running`、`Sandbox` 的 `running` / `stopped` / `lost` 无对应事件；(i) `Workflow.status` 示例 `finished`（api.md §4.7）不在 data-model 枚举，`Workflow.template` 示例 `medium.implement` 与 `levels.*.graph_template`（`medium.plan_build_verify`）口径不同；(j) `artifact.unexpected`、`gantry device recover` 等审计事件无类型 | 转移表只实现状态图 / 字面写出的边，其余按 §7.4 标注 [提议] 或拒绝 | `engine`、`store` 的校验会让缺口显形，需契约 PR |
| OQ-7 | `Approval.action` 的词表：data-model 写"`ForgeActionKind` 或内部动作"，但 `force_push` 不是 `ForgeActionKind`；config.md §5.1 有 `push_outside_prefix`、`resolve_review_thread_human` 两个需审批的伪动作，forge-actions §2.5 没有对应行 | `ApprovalAction = PolicyActionId ∪ InternalAction`（20 + 3），`target` / `subject_digest` 形态按已有三行，另两个伪动作的形态待定 | `policy`、`forge`、`api` |
| OQ-8 | 实体字段缺口：`BudgetLedger.config_rev`（config.md §2.6）data-model 未列；config.md §5.5 提到的 `core` 硬上限（`policies.patch.*`）没给数值；事件信封 `causation_id` / `correlation_id` 的可空性未定 | 前者加为可选字段；硬上限未定前 `patch.*` 只校验为正整数；信封两字段按 `Option` | `engine`、`policy`、`store` |
| OQ-9 | 每个 `ForgeActionKind` 的 `target` / `intent` 推导放哪：`upsert_status_comment` 的 `render_digest`、`update_issue` 的 `spec_block_digest` 依赖 `forge` 的渲染 | `core` 只给通用 `derive`；推导归 `forge` | `forge`、`engine`、`guardian` |
| OQ-10 | token 是否加权：decisions P-44 写"按加权 token 计，权重是配置项（默认按价格比例）"，config.md OQ-7 仍写"不加权"且没有权重键 | `TokenWeights` 默认全 1，不引入配置键 | `engine`、`models`、预算口径 |
| OQ-11 | V10 的 GitHub 域名族清单：`core` 校验器与 `egress` 的完整硬编码表可能漂移 | `core` 只内置 config.md §5.5 点名的最小集（`github.com`、`api.github.com` 及其子域），并要求与 `egress` 共用一份常量 | `egress`、`sandbox` |
| OQ-12 | JSON Schema 方言与 OpenAPI 3.1 对齐的方式 | 2020-12 | `ops` 的校验工具选型 |
| OQ-13 | settings 稀疏存储下，内置默认随版本变化会静默改变未显式设置的键（config.md OQ-20）；`schema_version` 迁移策略 | 暂无迁移；`SETTINGS_SCHEMA_VERSION` 变化视为破坏性变更 | `core`、`deploy` |
| OQ-14 | 跨使用方共用同一 `secret_ref`：config.md §2.4 允许"多个字段引用同一个 ref"，而 `secrets` 按使用方隔离 | `split_secret_values` 对"同一名字出现在不同 `Consumer` 的字段里"报错（新增语义码，需契约 PR 登记） | `secrets`、`api` |
