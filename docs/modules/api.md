# API 网关（`api`）

> 状态：设计 v0.1 · 里程碑：M1（鉴权、路由骨架、事件流、统一签名原语、settings·模型）→ M2 → M3（即时断流）→ M4（手机档案验收）· 泳道：`sec`（双向 TLS 即 mTLS、鉴权、签名、事件流）+ `be-engine`（资源路由的业务对接，[interfaces/api.md](../interfaces/api.md) 头部分工）· crate / 目录：`crates/gantry-api`

`api` 是服务端对客户端的唯一网络面：一个强制客户端证书的 HTTPS 监听器（mTLS API + 服务端发送事件（Server-Sent Events, SSE）事件流）和一个只含一条路由的单向 TLS 配对监听器。它负责把"握手通过的证书"变成"带角色与档案的设备"，把 48 条契约路由接到各模块，并保证客户端看到的错误、分页、幂等、并发控制、事件游标、审批签名流程都与 [interfaces/api.md](../interfaces/api.md) 逐字一致。它不做业务判断：业务归各模块，放行归 `policy`，证书归 `pki`。

## 1. 职责

1. **mTLS 终止**：用 `pki` 提供的客户端证书校验器与服务端证书解析器构造 `rustls::ServerConfig`；仅 TLS 1.3，强制客户端证书，握手失败不产生任何 HTTP 响应（[api.md §1.2](../interfaces/api.md)）；关闭会话票据（session ticket）/ 会话缓存与零往返数据（0-RTT）（§7.2）。
2. **配对监听器**：独立端口、单向 TLS、只路由 `POST /v1/pair`，其余路径一律 404；只在 `pki` 给出的配对窗口内监听；业务委托 `pki::PairingService::redeem`（请求同时携带 CSR 与审批签名公钥 `approval_pubkey`，P-47；响应链 `[叶子, 中间 CA, 根]`，P-64；api.md §1.3）。
3. **设备鉴权与角色**：每个请求按 api.md §2.1 的判定链——握手 → `cert_serial` 映射 Device 且 `active` → 角色（**实时查库，不缓存**）→ 客户端档案（`desktop` / `mobile`）→ 限流 → 处理；写操作的事件 `actor` 记为 `{kind: "device", id}`。
4. **路由实现**：契约 §4.1 的全部 48 条路由（47 条 mTLS + 1 条配对；含控制路由 #43–#45、收件箱计数 #46、证书续期 #47、推送令牌注册 #48（M4，P-77））；读类走 `store` 投影，写类委托所属模块；请求体严格校验（未知字段 → `422 unknown_field`，I7）。
5. **统一错误**：`application/problem+json`（RFC 9457），30 个稳定 `code`（api.md §3.1），`X-Request-Id`。
6. **分页、幂等键、实体标签（ETag）/ If-Match 条件请求、限流、读一致性**：键集（keyset）游标分页；`Idempotency-Key` 去重与重放；settings 的 ETag 条件请求；按设备令牌桶（token bucket）与 `RateLimit-*` 头；每个成功响应带 `X-Event-Seq`；**每个响应**（含错误与事件流响应头）带 `X-Log-Epoch`（P-67）（api.md §3.2–§3.6）。
7. **SSE 事件流**：先回放 `(游标, head]` 再切到实时，不丢不重；过滤（`project_id`、`types`、手机白名单、`settings.*` 门禁）；15 s 心跳推进游标并携带 `log_epoch`（P-67）；心跳内检查设备吊销与证书到期；`close` 帧；慢消费者断开；每设备并发 ≤ 3（api.md §5）。
8. **统一签名原语的服务端部分**：审批资源路由（列表 / 详情 / 挑战 challenge / grant / deny）、内部动作的发起审批与"带 `Gantry-Approval` 头重发"流程（`settings_update`、`device_role_change`、`device_revoke`）。验签（用设备登记的审批签名公钥 `approval_pubkey`，P-47）、状态机、摘要一致性在 `policy`，本模块负责 HTTP 外壳、前置对账、事务编排与错误映射（api.md §6）。
9. **settings 路由编排**：合并 / 校验（调 `core` 校验器并提供上下文）、`dry_run`、角色与签名判定、secret `value` 的剥离与转存、`settings.updated` 事件写入、`catalog` 与 `status` 聚合（api.md §4.9，config.md §2）。
10. **证书续期**：`POST /v1/devices/self/renew`（#47，mTLS，所有档案）委托 `pki::DeviceAdmin::renew`，把本次握手出示的证书序列号一并传入（P-64）。
11. **I6 / I7 的落点**：没有任何接收代码、命令、补丁的路由；响应、事件、日志里没有任何凭据；工作路由拒绝配置字段（api.md §8）。

### 1.1 各里程碑交付

"起"列是本规格对**路由上线里程碑**的建议（milestones 只给了泳道范围，没有逐路由排期）；未启用的路由与未注册路由一样返回标准 `404 not_found`，由注册表的 `since` 字段控制（§6.2）。

| 里程碑 | 交付 |
| --- | --- |
| M0 | 不交付实现。依赖 M0：`schemas/openapi.yaml`、`schemas/events/*.json`、Mock API 服务、`TestCA`。**契约一致性测试套件**（§12）在 M0 以 Mock API 为被测方先跑通，之后同一套件对真实 `api` 跑 |
| M1 | 两个监听器与 TLS 加固；设备鉴权链、角色、档案表（`ios` → `mobile` 子集一并落地，失败即关闭）；统一错误、分页、幂等键、限流、`X-Event-Seq`、`X-Log-Epoch`；SSE 事件流（含游标回放、心跳内吊销检查、心跳 `log_epoch`、`cursor_ahead`）；审批全流程路由 #24–#29 与内部动作签名写入；settings #30 #31（**写入只开放 `models`**）#35 #36；设备 #37–#40 与证书续期 #47（P-64）；配对 #1（CSR + `approval_pubkey`）；业务路由 #2–#6、#10–#12、#17–#22 对接投影 / `conversation`；收件箱计数 #46（P-63）；控制路由 #43–#45（暂停 / 继续 / 重试，引擎 M1 交付，P-56） |
| M2 | 工作树 #13–#16；交付单元控制 #7 #8 与预算只读 #9；收件箱 #23 #42（M1 的上报项还没有可选项可选，`choose` 随 `spec_needed` / `escalation` 的选项上线）；项目范围 settings #32–#34；settings 的 `levels` / `policies` / `guardian` 等 resource 开放写入（随各设置页，config.md §13）；`settings.*` 事件门禁的完整验收；审批的重新请求路径（与 `policy`、`engine` 配合） |
| M3 | 进程内即时断流（订阅 `pki::RevocationNotifier`，心跳检查保留为兜底）；预算面板所需的读取验收；密钥轮换对 settings secret 流程的联动验收（同名 `secret_ref` 写入新 `value` 的轮换路径） |
| M4 | 手机档案的端到端验收（13 条路由、事件白名单、审批路由 `403 forbidden_profile`、`choose` 的手机限制（只允许 `decision_needed` 与 `budget_exhausted` 的非 `raise_cap` 选项，P-79）、发消息时忽略 `level_cap`）；`ios` 配对联调（`approval_pubkey` 可省略、永不成为 approver，P-76）；#48 推送令牌注册（P-77） |

## 2. 不负责

| 事 | 归谁 |
| --- | --- |
| 证书签发与续期、令牌、证书签名请求（CSR）与 `approval_pubkey` 校验、设备注册表、握手时的吊销检查、`revocations.journal`、`gantry pair` / `gantry device recover` | `pki` |
| 放行判定、审批状态机、验签、挑战 / 一次性随机数（nonce）、`subject_digest` 一致性、`stale_head` 的状态转移 | `policy`；本模块只转发并映射错误 |
| 业务状态机：交付单元阶段（含暂停 / 继续 / 重试节点）、对话意图解析、收件箱语义、工作树锁语义 | `engine` / `conversation` / `notify` / `repo` |
| 事件日志的追加与投影本身、事件游标的分配、`log_epoch` 的维护 | `store` |
| GitHub 任何读写、PR head 的对账 | `forge`（本模块只在 `grant` 前请求一次强制对账） |
| Webhook 入口（独立监听、HMAC、隧道） | `forge` / `deploy`；API 监听器对 `/webhook` 返回 404 |
| 沙盒面向的任何端点 | 不存在（P-15） |
| 模型 / 包仓库凭据的保存 | `egress`（模型 key、私有包仓库凭据，经控制面 `EgressControl` 写入，只写不读，P-50）与 `secrets/`（App 私钥、webhook secret）；本模块只"转交、不留存" |
| 设置键的定义、合并 / clamp / 校验规则、JSON Schema | `core` + [config.md](../interfaces/config.md) |
| 客户端的重连、游标持久化、签名界面一致性（api.md §6.5） | `desktop-shell` / `mobile` |
| 限流参数的修改（静态配置，**不经 API**，不是 settings） | `deploy` |
| 审计日志的读取路由 | 契约没有（`policy` OQ-P8） |

## 3. 依赖

本文中不带路径的 `api.md`、`config.md`、`forge-actions`、`worker-protocol` 均指 `docs/interfaces/` 下的契约；模块规格写作 `modules/<id>.md`，本规格自身称"本文"。

`api` 依赖"全部服务端模块 + `pki`"（architecture §4）。下表只列用到什么；各模块的 trait 以其自己的规格书为准。`engine` / `conversation` / `notify` / `repo` / `forge` 的规格书已经成稿，`DeliveryControl`、`ConversationService`、`WorktreeLocks`、`PrRefresher` 与 `InboxPort` 都在 `core::ports`（P-51，core.md §6.10），写方法一律带 `ActorCtx`（P-78）。

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `pki` | `TlsMaterial`（客户端证书校验器、服务端证书解析器、根证书）；`DeviceDirectory`（`by_cert_serial`、`get`、`list`、`verifying_key`（返回审批签名公钥 `approval_pubkey`，P-47）、`touch_last_seen`）；`DeviceAdmin`（`rename`、`set_role`、`revoke`、`renew`）；`PairingService`（`window`、`redeem`）；`RevocationNotifier`（M3） | [modules/pki.md §6](pki.md)；api.md §1.3、§2、§4.10 |
| `policy` | `ApprovalService`（`get`、`list`、`create_internal`、`challenge`、`grant`、`deny`、`consume_internal`）与 `signing::request_digest`；`DeviceKey.verifying_key` 填审批签名公钥 | [modules/policy.md §6.4、§6.7](policy.md)；api.md §6 |
| `store` | `Store::snapshot()` → `ReadSnapshot`（`ProjectionRead` 全部读取，含 `inbox_summary`；`settings_current` / `settings_at_rev` / `config_rev`、`head_seq`）；`Store::log_identity()` → `LogIdentity { log_id, log_epoch }`（P-67）；`Store::begin_write()` → `WriteTx`（`append` 返回 `Appended::{New, Duplicate}`、`module_conn("api")`）；`EventLog::{head, check_cursor, tail, scan}` 与 `EventFilter`；`ModuleSpec { module: "api" }` 注册 `api_idempotency` | [store.md §6、§7.4、§7.5、§7.10](store.md)；data-model §5；config.md §2.3 |
| `core` | 领域类型、`Device`、`DeviceRole`、`DevicePlatform`、`Level`、`Ref`、`Card`、`UntrustedText`、`Actor`、`Clock`、`Entropy`、`Ulid`（`request_id`）；settings 的 `effective` / `merge_patch` / `diff_relaxes` / `required_authority` / `split_secret_values` / `find_secret_refs` 与校验器；`jcs` / `jcs_digest` | [core.md §6.1–§6.7](core.md)；config.md §2.1、§2.5、§11 |
| `forge` | `PrRefresher::refresh_pr(repo, number)`（= `ForgeReader::reconcile_now(Pr)` 后读快照）：`grant` 前对 PR 强制对账一次 | [forge.md §6](forge.md)；api.md §6.4 第 5 步 |
| `engine` | `DeliveryControl`：`set_level_cap`、`abandon`、`pause`、`resume`、`retry_node`（P-56） | [engine.md §6](engine.md)；api.md §4.4、§4.7 |
| `conversation` | `post_user_message`（持久化用户消息并写 `message.posted`） | [conversation.md §6](conversation.md)；api.md §4.5 |
| `repo` | `WorktreeLocks`：`acquire_human` / `release_human` | [repo.md](repo.md)；api.md §4.6 |
| `notify` | `core::ports::InboxPort`：`dismiss`、`choose`、`summary`（P-51、P-63） | [notify.md §6](notify.md)；api.md §4.8；decisions P-40、P-63 |
| `secrets` / `egress` | `Consumer::Forge`：进程内 `ScopedSecrets`（`SecretSink`：`present` / `stage` / `commit` / `abort` / `staged`）；`Consumer::Egress`：`core::ports::EgressControl` 的 `secret_present` / `stage_secret` / `activate_secret` / `discard_secret` / `staged_secrets`，经控制面 `/run/gantry/egress.sock`（P-50）；各模块的 `ModuleStatus` 与 `catalog` 供给 | [secrets.md §6](secrets.md)；[egress.md §6.2](egress.md)；config.md §2.4、§2.8 |
| 外部 crate（建议） | `tokio`、`axum` + `hyper` + `hyper-util`（HTTP/1.1 与 h2）、`tokio-rustls`、`rustls`、`serde` + `serde_json`、`jsonschema`（一致性测试）、`subtle`、`zeroize`、`tracing`、`arc-swap`、`dashmap` | architecture §10 |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| mTLS 请求（握手 + HTTP） | 桌面端 `desktop-shell` / `mobile` | api.md 全文 | 握手由 `pki` 校验后，身份可信；**请求内容一律不可信**（严格校验） |
| 配对请求 `POST /v1/pair`（`token`、`csr`、`approval_pubkey`、`device`） | 未配对设备（单向 TLS） | api.md §1.3 | **完全不可信**；只做最小解析后委托 `pki` |
| 事件日志 | `store` | data-model §5.1 信封 | 可信（服务端自产）；其中的 GitHub 文本字段原样转发，由客户端按不可信渲染（I5） |
| 投影读 | `store` | data-model §3 | 可信 |
| 各模块的返回值 | `pki`、`policy`、`engine` 等 | §6.3 | 可信 |
| settings 请求体中的 secret `value` | 客户端（`approver` + 签名） | config.md §2.4 | 高敏感：只在内存、不落日志、转交后清零 |
| 静态配置（监听地址、限流常量） | `deploy` 的引导配置 | §10 | 可信 |
| 配对窗口 | `pki::PairingService::window()` | — | 可信 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| HTTP 响应（JSON / problem+json） | 客户端 | api.md 全文；响应头 `X-Request-Id`、`X-Event-Seq`、`X-Log-Epoch`、`RateLimit-*`、`ETag`（settings）、`Idempotent-Replayed` |
| SSE 帧 | 客户端 | api.md §5.1 |
| 事件 `settings.updated`（`secret_changes[]` 只含 `{ref, op, version}`，无值无哈希） | `store` | data-model §5.2；config.md §2.3。**这是本模块唯一自己追加的事件**，其余事件由所属模块追加（`message.posted` 归 `conversation`、`device.*` 归 `pki`、`approval.*` 归 `policy`、`worktree.*` 归 `repo`、`inbox.*` 归 `notify`、`delivery.*` 归 `engine`） |
| 对下游模块的调用 | 各模块 | §6.3；写调用携带 `dedup_key = (device_id, Idempotency-Key, 序号)` |
| 秘密转交 | `egress` / `secrets` | `EgressControl` 的 secret 写入（egress.md §6.2）/ `SecretSink`（secrets.md §6） |
| 指标、结构化日志 | 运维 | §11 |

## 6. 对外接口

### 6.1 装配

```rust
pub struct ApiServer { /* 内部 */ }

pub struct Services {
    pub clock: Arc<dyn Clock>, pub entropy: Arc<dyn Entropy>,
    pub store: Store,                                // store.md §6.1：写事务 + 一致读 + 事件日志
    pub pki: PkiHandles,                             // directory / admin / pairing / tls / revocations
    pub policy: Arc<dyn ApprovalService>,
    pub forge: Arc<dyn PrRefresher>,
    pub engine: Arc<dyn DeliveryControl>,
    pub conversation: Arc<dyn ConversationService>,
    pub repo: Arc<dyn WorktreeLocks>,
    pub notify: Arc<dyn InboxPort>,                  // core::ports（P-51）：dismiss / choose / summary
    pub settings_ctx: Arc<dyn SettingsContext>,      // 校验上下文 + catalog + status 聚合（§6.3）
    pub secrets: SecretTargets,                      // Forge → ScopedSecrets；Egress → EgressControl（P-50，§6.3）
}

impl ApiServer {
    pub fn new(cfg: ApiConfig, svc: Services) -> Result<Self, ApiError>;
    /// 启动 mTLS 监听器、配对监听器（按窗口开关）、SSE 中心、幂等清理任务；shutdown 触发时
    /// 向所有事件流尽力发送 close(server_shutdown)，等待在途请求完成后返回。
    pub async fn serve(self, shutdown: CancellationToken) -> Result<(), ApiError>;
    /// 测试入口：不含 TLS，调用方直接注入 AuthContext，用于路由级单元测试
    pub fn router_for_test(&self) -> axum::Router;
}
```

### 6.2 路由注册表

注册表是**数据**，不是散落在各 handler 里的 if：一份静态表驱动鉴权链、限流分类、幂等策略、体积上限与手机子集，并与 `schemas/openapi.yaml` 做双向对账（§12）。表中序号、路径、最低角色、手机列**逐字取自** [api.md §4.1](../interfaces/api.md)；"限流类"来自 §3.7；"委托"与"起"是本规格新增。

```rust
pub struct RouteSpec {
    pub id: u8,                      // 契约 §4.1 序号
    pub method: Method, pub pattern: &'static str,
    pub min_role: RoleReq,           // Fixed(DeviceRole) | SettingsDynamic（按内容判定，§7.8）
    pub mobile: MobileAccess,        // No | Yes | YesChooseLimited
    pub rate: RateClass,             // Read | Write | Message | Sign | PairIp
    pub idem: IdemPolicy,            // Required | Optional | NotApplicable
    pub body_limit: usize,           // 64 KiB；settings PUT 256 KiB
    pub since: Milestone,
}
```

| # | 路由 | 最低角色 | 手机 | 限流类 | 幂等键 | 委托 | 起 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `POST /v1/pair`（配对监听器；CSR + `approval_pubkey`） | —（令牌） | — | 源 IP 5/min | 不适用 | `pki.pairing.redeem` | M1 |
| 2 | `GET /v1/me` | viewer | ✓ | 读 | — | `pki.directory` + 构建信息 | M1 |
| 3 | `GET /v1/projects` | viewer | ✓ | 读 | — | 投影 | M1 |
| 4 | `GET /v1/projects/{project_id}` | viewer | ✓ | 读 | — | 投影 | M1 |
| 5 | `GET /v1/deliveries` | viewer | — | 读 | — | 投影 | M1 |
| 6 | `GET /v1/deliveries/{delivery_id}` | viewer | — | 读 | — | 投影 | M1 |
| 7 | `PATCH /v1/deliveries/{delivery_id}` | operator | — | 写 | 不适用 | `engine.set_level_cap` | M2 |
| 8 | `POST /v1/deliveries/{delivery_id}/abandon` | operator | — | 写 | 可带 | `engine.abandon` | M1 |
| 9 | `GET /v1/deliveries/{delivery_id}/budget` | viewer | — | 读 | — | 投影 | M2 |
| 10 | `GET /v1/projects/{project_id}/conversations` | viewer | ✓ | 读 | — | 投影 | M1 |
| 11 | `GET /v1/conversations/{conversation_id}/messages` | viewer | ✓ | 读 | — | 投影 | M1 |
| 12 | `POST /v1/conversations/{conversation_id}/messages` | operator | ✓ | 发消息 30/min | **必** | `conversation.post_user_message` | M1 |
| 13 | `GET /v1/worktrees` | viewer | — | 读 | — | 投影 | M2 |
| 14 | `GET /v1/worktrees/{worktree_id}` | viewer | — | 读 | — | 投影 | M2 |
| 15 | `PUT /v1/worktrees/{worktree_id}/lock` | operator | — | 写 | 不适用 | `repo.acquire_human` | M2 |
| 16 | `DELETE /v1/worktrees/{worktree_id}/lock` | operator | — | 写 | 不适用 | `repo.release_human` | M2 |
| 17 | `GET /v1/deliveries/{delivery_id}/workflows` | viewer | — | 读 | — | 投影 | M1 |
| 18 | `GET /v1/workflows/{workflow_id}` | viewer | — | 读 | — | 投影 | M1 |
| 19 | `GET /v1/nodes/{node_id}/attempts` | viewer | — | 读 | — | 投影 | M1 |
| 20 | `GET /v1/attempts/{attempt_id}` | viewer | — | 读 | — | 投影 | M1 |
| 21 | `GET /v1/inbox` | viewer | ✓ | 读 | — | 投影 | M1 |
| 22 | `GET /v1/inbox/{inbox_id}` | viewer | ✓ | 读 | — | 投影 | M1 |
| 23 | `POST /v1/inbox/{inbox_id}/dismiss` | operator | — | 写 | 可带 | `InboxPort::dismiss` | M2 |
| 24 | `GET /v1/approvals` | viewer | — | 读 | — | `policy.list` | M1 |
| 25 | `GET /v1/approvals/{approval_id}` | viewer | — | 读 | — | `policy.get` | M1 |
| 26 | `POST /v1/approvals` | operator | — | 签名类 30/min | **必** | `policy.create_internal` | M1 |
| 27 | `GET /v1/approvals/{approval_id}/challenge` | approver | — | 签名类 30/min | — | `policy.challenge` | M1 |
| 28 | `POST /v1/approvals/{approval_id}/grant` | approver | — | 写 | **必** | `forge.refresh_pr`（PR 类）→ `policy.grant` | M1 |
| 29 | `POST /v1/approvals/{approval_id}/deny` | approver | — | 写 | **必** | `policy.deny` | M1 |
| 30 | `GET /v1/settings/{resource}` | operator | — | 读 | — | settings 编排 | M1 |
| 31 | `PUT /v1/settings/{resource}` | operator* | — | 写 | 不适用 | settings 编排 + `policy.consume_internal` + `SecretSink` | M1（仅 `models`） |
| 32 | `GET /v1/projects/{project_id}/settings/{resource}` | operator | — | 读 | — | settings 编排 | M2 |
| 33 | `PUT /v1/projects/{project_id}/settings/{resource}` | operator* | — | 写 | 不适用 | 同 #31 | M2 |
| 34 | `DELETE /v1/projects/{project_id}/settings/{resource}` | operator* | — | 写 | 不适用 | 同 #31（`value: null`） | M2 |
| 35 | `GET /v1/settings/catalog` | operator | — | 读 | — | `settings_ctx.catalog` | M1 |
| 36 | `GET /v1/settings/status` | operator | — | 读 | — | `settings_ctx.status` | M1 |
| 37 | `GET /v1/devices` | viewer | — | 读 | — | `pki.directory.list` | M1 |
| 38 | `PATCH /v1/devices/{device_id}` | operator | — | 写 | 不适用 | `pki.admin.rename` | M1 |
| 39 | `PUT /v1/devices/{device_id}/role` | approver | — | 写 | 不适用 | `policy.consume_internal` + `pki.admin.set_role`（同一事务） | M1 |
| 40 | `POST /v1/devices/{device_id}/revoke` | approver | — | 写 | 可带 | `policy.consume_internal` + `pki.admin.revoke`（同一事务） | M1 |
| 41 | `GET /v1/events` | viewer | ✓ | 读 + 并发 ≤ 3 | — | `EventHub` | M1 |
| 42 | `POST /v1/inbox/{inbox_id}/choose` | operator | ✓* | 写 | **必** | `InboxPort::choose` | M2 |
| 43 | `POST /v1/deliveries/{delivery_id}/pause` | operator | — | 写 | **必** | `engine.pause` | M1 |
| 44 | `POST /v1/deliveries/{delivery_id}/resume` | operator | — | 写 | **必** | `engine.resume` | M1 |
| 45 | `POST /v1/nodes/{node_id}/retry` | operator | — | 写 | **必** | `engine.retry_node` | M1 |
| 46 | `GET /v1/inbox/summary` | viewer | ✓ | 读 | — | 投影（`inbox_summary`） | M1 |
| 47 | `POST /v1/devices/self/renew` | viewer | ✓ | 写 | 可带 | `pki.admin.renew`（传入握手出示的 `cert_serial`） | M1 |
| 48 | `PUT /v1/devices/self/push-token` | operator | ✓ | 写 | 不适用 | `InboxPort::register_push_token`（`notify` 私有表，不进事件，P-77） | M4 |

`*`：#31 #33 #34 的最低角色是路由下限，实际角色与是否需要签名按改动内容判定（§7.8）；#42 的手机限制（由 `notify` 判定，P-79）：只能对 `decision_needed` 与 `budget_exhausted` 选择，`budget_exhausted` 的 `raise_cap`、`escalation`（升档一律走 `escalation`，P-63）与 `approval_needed` → `403 forbidden_profile`（P-35）；`spec_needed` 没有选项，任何档案 `choose` → `422`。`/v1/inbox/summary`、`/v1/devices/self/renew`、`/v1/devices/self/push-token` 是字面路径，优先于 `/v1/inbox/{inbox_id}`、`/v1/devices/{device_id}` 匹配。合计 48 条：mTLS 47 条 + 配对 1 条；手机档案 13 条。其余格式遵循契约；方法错误 → `405 method_not_allowed` + `Allow`。

### 6.3 对其他模块的需求（facade，占位签名）

读类端点不经 facade，直接用 `store` 的一致读（同一读事务里读投影与最大 `seq`，才能给出正确的 `X-Event-Seq`，api.md §3.6）。写类 facade 的统一约定：入参一律是 `core::ports::ActorCtx`（P-78，字段见 [core.md §6.10](core.md#610-跨模块端口coreportsp-51)），由本模块在鉴权后构造：`actor = {device, dev_…}`、`device`、`role`、`client_profile`、`request_id`、`idempotency_key`（请求头原文）、`dedup_key`（由 `(device_id, Idempotency-Key)` 派生）；模块用 `dedup_key` 作为所追加事件的 `dedup_key`，使重放同一 `Idempotency-Key` 在 `store` 层也至多追加一次（见 §7.6）。下列 trait 定义在 `core::ports`（跨泳道，P-51），这里只列本模块用到的方法。

```rust
#[async_trait] pub trait ConversationService { async fn post_user_message(&self, ctx: ActorCtx, conv: ConversationId, msg: NewUserMessage) -> Result<Message, ModuleError>; }
#[async_trait] pub trait DeliveryControl     { async fn set_level_cap(&self, ctx: ActorCtx, id: DeliveryId, cap: Level) -> Result<Delivery, ModuleError>;
                                               async fn abandon(&self, ctx: ActorCtx, id: DeliveryId, reason: Option<String>) -> Result<Delivery, ModuleError>;
                                               // P-56：控制路由 #43–#45；引擎按目标状态幂等，API 层另有必带的 Idempotency-Key
                                               async fn pause(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError>;
                                               async fn resume(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError>;
                                               async fn retry_node(&self, ctx: ActorCtx, id: NodeId) -> Result<Node, ModuleError>; }
#[async_trait] pub trait WorktreeLocks       { async fn acquire_human(&self, ctx: ActorCtx, id: WorktreeId, lease: Duration) -> Result<Worktree, LockError>;
                                               async fn release_human(&self, ctx: ActorCtx, id: WorktreeId) -> Result<Worktree, LockError>; }
// 收件箱不再有 api 专用 facade：直接用 core::ports::InboxPort（notify.md §6）的 dismiss / choose / summary；
// notify 的 dismiss / choose / register_push_token 同样接收 ActorCtx（P-78）。
#[async_trait] pub trait PrRefresher         { async fn refresh_pr(&self, repo: &RepoSlug, number: u64) -> Result<PrHead, ModuleError>; }  // 失败 → 退回投影快照

/// settings 编排需要的上下文（来源见 config.md §11 开头）与聚合
#[async_trait] pub trait SettingsContext: Send + Sync {
    fn validation_ctx(&self) -> ValidationCtx;                    // models 能力表、engine 模板登记、密钥存在性、他 resource 当前值
    async fn catalog(&self) -> Result<serde_json::Value, ModuleError>;   // config.md §2.8
    async fn status(&self) -> Result<SettingsStatus, ModuleError>;       // latest_seq + modules.<name>.{applied_rev, health, detail?, unresolved_secret_refs}
}

/// 凭据转交：两阶段写入（stage → commit / abort），保证
/// "settings.updated 与 secret 写入要么都成功要么都不成功"（config.md §2.4）。按 core::Consumer 分两条通道：
///   Consumer::Forge  → 进程内 ScopedSecrets（secrets.md §6 的 SecretSink：present / stage / commit / abort / staged）
///   Consumer::Egress → core::ports::EgressControl（egress.md §6.2，经 /run/gantry/egress.sock，只写不读，P-50）：
///                      secret_present / stage_secret / activate_secret / discard_secret / staged_secrets
/// 两条通道对 api 呈现同一组语义（present、stage、commit、abort、staged），由 SecretTargets 适配。
pub struct SecretTargets { pub forge: Arc<dyn SecretSink>, pub egress: Arc<dyn EgressControl> }
```

下游错误（`ModuleError` 等）统一映射表见 §9。

### 6.4 请求上下文与错误类型

```rust
pub struct AuthContext {
    pub device: Device, pub profile: ClientProfile,           // desktop | mobile，由 Device.platform 推出
    pub cert_serial: CertSerial, pub cert_not_after: Timestamp,
    pub request_id: RequestId,
}
pub struct Problem {                                          // RFC 9457 + 扩展成员
    pub status: u16, pub code: &'static str,                  // api.md §3.1 的 30 个之一，只增不改
    pub detail: String,                                       // 英文、面向日志，不展示给用户
    pub ext: serde_json::Map<String, serde_json::Value>,      // required_role / errors / current_head_sha / approval_status / reason / signing / lock …
    pub headers: HeaderMap,                                   // Allow / Retry-After / ETag
}
```

## 7. 内部设计

### 7.1 请求处理管线（顺序固定）

每条 mTLS 请求按下表顺序处理，**前一步失败立即返回，不进入下一步**。顺序里属于契约的（步骤 0、2、4、5、6、13，即握手 → 映射 → 角色 → 档案 → 限流 → 处理）逐字取自 api.md §2.1；其余是本规格补全的实现细则，须由测试固化。

| 步 | 名称 | 失败 → 响应 |
| --- | --- | --- |
| 0 | TLS 握手（`pki` 校验器：链、有效期、`clientAuth`、吊销、在册） | 握手失败，**无 HTTP 响应** |
| 1 | 生成 `request_id`（ULID）、开日志 span；取握手叶子证书的 `cert_serial` 与 `notAfter` | — |
| 2 | 映射设备：`pki.directory.by_cert_serial` 实时读库；不在册或 `status = revoked` | `401 device_revoked` |
| 3 | 路由匹配：路径 + 方法对注册表（含未启用路由）；`/webhook` 与一切未知路径 | `404 not_found`；路径存在方法不对 `405 method_not_allowed` + `Allow` |
| 4 | 角色：`device.role ≥ route.min_role`（实时，不缓存）；settings 另按内容（§7.8） | `403 forbidden_role` + `required_role` |
| 5 | 档案：`profile = mobile` 时路由须在手机子集（#42 对 `escalation` / `approval_needed` 的限制要读取收件箱项，在步骤 13 开头判定，结果同为 `403 forbidden_profile`） | `403 forbidden_profile` |
| 6 | 限流：`(device, class)` 令牌桶，消耗一个令牌并刷新 `RateLimit-*` 头状态 | `429 rate_limited` + `Retry-After` |
| 7 | `Content-Type` 必须是 `application/json`（带 body 的路由） | `415 unsupported_media_type` |
| 8 | 请求体大小（流式计数，超限即停读）：默认 64 KiB，settings PUT 256 KiB | `413 payload_too_large` |
| 9 | 查询参数：未知参数、类型错误 | `400 invalid_request` |
| 10 | 解析 JSON（语法错）；`Idempotency-Key` 格式（16–64 位 `[A-Za-z0-9_-]`）与必带性 | `400 invalid_request` / `400 idempotency_key_required` |
| 11 | 幂等登记（§7.6）：重放 / 冲突 / 进行中 | 原样重放 + `Idempotent-Replayed: true` / `422 idempotency_key_reuse` / `409 idempotency_in_progress` |
| 12 | 请求体 schema 校验（`deny_unknown_fields`、类型、枚举、范围）；结果 `4xx` 也被幂等登记（只有 5xx 不登记） | `422 validation_failed` + `errors[]` |
| 13 | 处理：调用所属模块 / 投影读 | 业务错误按 §9 映射 |
| 14 | 响应：`X-Request-Id`、`X-Log-Epoch`（**所有**响应，含错误与事件流响应头，取自 `Store::log_identity()`，P-67）、`X-Event-Seq`（成功响应）、`RateLimit-*`（步骤 2 之后的所有响应；步骤 6 之前的错误响应取该设备读类桶的当前状态，只读不扣）、`Cache-Control: no-store`（settings 例外）、`X-Content-Type-Options: nosniff`；不带 `Server` 版本、不带任何 CORS 头 | — |

`touch_last_seen` 在步骤 2 成功后异步调用（节流在 `pki`）。`OPTIONS` 一律 `405`（无 CORS，api.md §1.2）。

### 7.2 TLS 终止加固（构造 `ServerConfig` 时的硬性要求）

| 项 | 要求 | 理由 |
| --- | --- | --- |
| 协议 | 仅 TLS 1.3（`builder_with_protocol_versions(&[&TLS13])`） | api.md §1.2 |
| 客户端证书 | `pki` 的校验器；`client_auth_mandatory = true` | 失败即关闭（fail-closed）：未配对者在握手层被挡住，应用层不存在"忘了写鉴权" |
| 会话恢复 | **关闭**：`send_tls13_tickets = 0`，`session_storage = NoServerSessionStorage`，不设 `ticketer` | 恢复会话不再重新校验客户端证书，会让已吊销设备继续连上（吊销只在握手时检查） |
| 0-RTT | `max_early_data_size = 0` | 早期数据可重放，非幂等 POST 会被放大 |
| ALPN | `h2`、`http/1.1` | api.md §1.2 |
| 证书 | `pki` 的 `ResolvesServerCert`（`ArcSwap`，续签后热替换） | 不断开现有连接 |
| HTTP/2 | `max_concurrent_streams` 64 [建议]；头部大小上限 16 KiB；读头超时 10 s | 抑制慢速攻击；SSE 长连接不受读头超时影响 |
| 配对监听器 | 单向 TLS（`with_no_client_auth`），同一服务端证书链（含根，便于客户端核对指纹）；仅 HTTP/1.1；仅路由 `POST /v1/pair`；请求体 ≤ 16 KiB；连接并发上限 8 [建议]；读头 / 总请求超时 5 s / 10 s | 暴露在未认证面上的最小面 |

测试要断言这些 `ServerConfig` 属性（§12），不能只靠代码评审。

### 7.3 设备与档案

- `ClientProfile`：`platform ∈ {windows, macos}` → `desktop`；`ios` → `mobile`；**其他或未知取值 → 视为 `mobile` 子集的最小集合并告警**（失败即关闭）。
- 手机子集由注册表的 `mobile` 列决定，不得在 handler 里另写判断。`#41` 对手机档案再取事件类型白名单的交集（§7.9）。
- 角色与档案相互独立：`ios` 设备即便是 `approver`，审批路由仍是 `403 forbidden_profile`（api.md §2.3）；`policy.grant` 对此也有防御性复查。
- 步骤 4 与 5 的顺序按契约：先角色后档案。

### 7.4 统一错误

- 所有非 2xx（含 404、405、429、5xx、路由不存在）都是 `application/problem+json`：`type = "urn:gantry:problem:<code>"`、`title`、`status`、`code`、`detail`、`instance`（请求路径）、`request_id`，外加按 `code` 的扩展成员（api.md §3.1 表）。
- `detail` 为英文、只进日志语义，**不得拼入外部文本**（分支名、用户输入原样回显会变成注入通道，I5）；需要给客户端看的变量放扩展成员。
- 500 一律 `internal`，不泄露内部错误链；详细错误只记日志并带 `request_id`。
- `validation_failed.errors[].pointer` 为相对请求体（settings 相对 `value`）的 JSON Pointer；`code ∈ {required, type, enum, range, unknown_field}` ∪ config.md §11 的语义码。

### 7.5 请求校验（I7 的执行点）

- 每个请求体类型 `#[serde(deny_unknown_fields)]`；`POST …/messages` 只允许 `text`（必填，≤ 16 KiB）、`level_cap`、`refs`（≤ 20 条）。**手机档案**带的 `level_cap` 被接受但忽略：转交 `conversation` 前置为 `None`，由其使用项目默认上限（C26、P-35）。控制路由 #43–#45 的请求体只允许 `{}`。**带 `model`、预算、策略等字段 → `422`，`errors[].code = unknown_field`**，绝不静默忽略。
- `refs` 只接受最小字段（`issue` / `pr`: `repo` + `number`；`worktree` / `delivery`: `id`）；服务端解析不到 → `422`。
- 整数 ≤ 2^53−1；时间 RFC 3339 毫秒 `Z`。
- 校验失败的 `errors` 按字段指针逐条列出，最多 50 条 [建议]。

### 7.6 幂等键

| 项 | 实现 |
| --- | --- |
| 存储 | 表 `api_idempotency(device_id, key, fingerprint, state, status, headers_json, body, created_at, lease_until, expires_at)`，主键 `(device_id, key)`；保留 24 h，后台每 10 分钟清理 |
| 指纹 | `SHA-256(method ‖ path ‖ JCS(body))`；无 body 按空串。键相同指纹不同 → `422 idempotency_key_reuse` |
| 流程 | 步骤 11：原子 `INSERT … ON CONFLICT`：不存在则写 `state = in_progress`（`lease_until = now + 30 s` [建议]）；已 `done` 且指纹相同 → 原样返回首次状态码 + 正文，加 `Idempotent-Replayed: true`；`in_progress` 且租约未到期 → `409 idempotency_in_progress`；租约已过期（上次处理崩溃）→ 接管 |
| 完成 | handler 返回后把 `status` / `headers` / `body` 写回并置 `done`；**首次响应是 5xx 不登记**（删除该行，允许同 key 重试）；`4xx` 登记并重放 |
| 业务层去重 | 所有幂等 POST 追加的业务事件带 `dedup_key = "api:<device_id>:<key>:<n>"`（`n` 为该请求内第几条事件）；`store` 的唯一索引保证：即使 `api_idempotency` 的完成记录因崩溃丢失，同 key 重试也不会重复追加事件——`append` 返回 `Appended::Duplicate { existing_seq }`，handler 据此读取已存在的事件重建响应（`EventLog::by_dedup_key`） |
| 作用域 | `(device_id, key)`：不同设备的相同 key 互不影响 |
| 不适用 | `GET` / `PUT` / `PATCH` / `DELETE` 与 `POST /v1/pair` |
| 必带 | #12 #26 #28 #29 #42 #43 #44 #45（契约 §3.3，C26、P-56） |
| 可带 | 其余 `POST`（#8 #23 #40 #47），按目标状态幂等 |

### 7.7 分页、ETag、读一致性

- **游标**：不透明字符串 `base64url(v ‖ resource_tag ‖ order ‖ last_id)`，keyset 基于 ULID `id`；解析失败、资源标签不符、`order` 不符 → `400 invalid_cursor`。默认 `limit = 50`（1–200）、`order = desc`（attempts 默认 `asc` 按 `n`）；`messages` 默认 `desc`。响应 `{items, next_cursor}`，`next_cursor = null` 表示末页。
- **读一致性**：每个读响应在同一读事务内取投影与最大 `seq`，写入 `X-Event-Seq`；写响应的 `X-Event-Seq` 等于本次写入产生的最后一个事件的 `seq`（`dedup` 重放时取已存在事件的 `seq`）。
- **ETag**（仅 settings）：`ETag: "<rev>"`，`rev` = 该 `(scope, resource)` 最近一次 `settings.updated` 的 `seq`，从未写过为 `0`；`GET` 支持 `If-None-Match` → `304`；`PUT` / `DELETE` 缺 `If-Match` → `428 precondition_required`，不符 → `412 precondition_failed` 且响应头带当前 `ETag`。`?view=effective` 的 ETag 不可用于 `If-Match`。其他资源不用 ETag。

### 7.8 settings 编排

**读**：`GET /v1/settings/{resource}[?rev=N]`——`{resource}` 必须是八个之一（`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`、`notify`、`github`），`catalog` / `status` 是字面路径优先匹配；读取需 operator；`?rev=N` 从 `store` 按 rev 取历史文档；项目范围另有 `view=effective`（`core` 的 merge + clamp，附 `clamped[]`）。响应里的 `SecretRef` 只含 `secret_ref` 与 `present`，**永不含 `value`**。

**写**（#31 #33 #34）的固定顺序（契约只规定了部分，其余为本规格补全）：

1. 范围与 resource 校验：项目不存在 → `404`；项目范围写 `notify` / `github` → `422 not_overridable`；M1 里非 `models` 的 resource 写入 → `404`（路由未启用）。
2. `If-Match`：缺失 `428 precondition_required`；与当前 rev 不符 `412 precondition_failed`（**优先于**其后的一切，api.md §6.6 第 4 步 #5）。
3. （流水线步骤 7–12 已保证：`Content-Type`、体积 ≤ 256 KiB、JSON 语法、`{ "value": … }` 外壳与 `deny_unknown_fields`。）
4. `core` 校验器（V01–V22）：先 schema 再语义，上下文取 `settings_ctx.validation_ctx()`；失败 → `422 validation_failed`。算出 `relaxes[]`、`required_role`、`requires_signature`、`signing`。
5. `?dry_run=true`：返回 `{valid, errors[], warnings[], relaxes[], required_role, requires_signature, signing?}`，不落库、不发事件、不消费审批，**即使缺签名头**也照常返回（`signing` 供发起审批用）。
6. 角色：`device.role < required_role` → `403 forbidden_role`（附 `required_role`）。
7. 签名：`requires_signature` 且缺 `Gantry-Approval` → `403 signature_required`（附 `signing`）。
8. **同一写事务**内：`policy.consume_internal(tx, {approval_id, expected_action: settings_update, expected_target: {kind:"settings", scope, project_id?, resource, base_rev}, body_digest: signing::request_digest(body), rev_check})`（不通过 → `409 approval_mismatch` + `reason`，整事务回滚）→ 追加 `settings.updated`（`value` 已剥离 secret）。
9. 提交成功后对每个暂存项 `SecretSink::commit`；响应 `200`：新文档 + `warnings[]`，头 `ETag`（新 rev）与 `X-Event-Seq`。

**secret `value` 的处理**（config.md §2.4）：在步骤 4 之前，调用 `core::split_secret_values(resource, request_value)`——它把请求体里的 `SecretRef.value`（字符串 → 写入或轮换；缺省 → 保持不变；`null` → `422 secret_clear_in_use`；`present` 接受并忽略）剥出来，返回"可入事件的稀疏文档"与"待转存的值"两部分；值只出现在第二部分，且是 `SecretString`（`zeroize`）。待转存项带 `core::Consumer`（`models.providers.*.credential`、`egress.injection.*.credential` → `Egress`；`github.app.*` → `Forge`）。之后的校验、事件都只见 `secret_ref`；`core::find_secret_refs` 用于校验 `secret_ref_missing`（经 `SecretSink::exists`）。`signing::request_digest` 按**含 `value` 的原始请求体**计算（api.md §6.6：摘要覆盖整个请求体，服务端不保存请求体）。写入流程：对每个待转存项 `SecretSink::stage`（版本待生效，对消费者不可见）→ 追加 `settings.updated`（`secret_changes[]` 记 `{ref, op, version}`，无值无哈希）并提交事务 → `commit`；事务失败则 `abort`。**崩溃恢复**：启动时对账——`SecretSink::staged()` 报告的暂存项用 `EventLog::by_secret_change(name, version)` 反查：事件已提交的补做 `commit`，超过 10 分钟仍无事件引用的 `abort`（store.md §6.2、secrets.md §7.4）。

**写入后的广播**：`settings.updated`（`subject = "settings"` 或 `prj_…`）由事件流按 §7.9 的门禁下发。

**`catalog` / `status`**：字段结构以 config.md §2.8 为准（`status` 为 `latest_seq` + `modules.<name>.{applied_rev, health, detail?, unresolved_secret_refs}`；字段名统一为 `applied_rev`，C26）。聚合不到的模块报 `health: unhealthy`，不报错。

### 7.9 SSE 事件流

**入口检查**（在返回 `200` 之前，失败按普通 problem 响应）：并发上限（每设备 3，超出 `429` + `Retry-After`）→ 参数解析（`project_id` 可重复；`types` 逗号分隔，精确类型或 `前缀.*`；未知参数 / 格式错 `400 invalid_request`）→ 游标：`Last-Event-ID` 头优先于 `after=<seq>`，必须是十进制 `u64`，否则 `400 invalid_cursor`；`EventLog::check_cursor(after)`：游标 > head → `CursorAhead` → `409 cursor_ahead`；`游标 < 保留下界`（预留，当前下界为 0）→ `410 cursor_expired`。

**流的生命周期**（一个连接一个任务；回放与实时的无缝衔接由 `store` 的 `EventLog::tail` 提供，store.md §7.5，本模块不自己做"先订阅后回放"）：

1. 响应头 `200`、`Content-Type: text/event-stream; charset=utf-8`、`Cache-Control: no-store`、`X-Accel-Buffering: no`；首帧 `retry: 3000`。
2. 起点：有游标则 `after = 游标`；无游标则 `after = head()`，不回放，首帧是带当前 head 的 `heartbeat`。`stream = events.tail(after, filter)`，逐个取 `TailBatch { events, scanned_to }`，`scanned = scanned_to`。
3. **过滤**：`EventFilter { project_ids, include, exclude }`——`project_ids`（`project_id = null` 的全局事件由 `store` 始终放行）；`include` = 请求的 `types`；`mobile` 档案把 `include` 与白名单 `project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked` 求交（请求了白名单外的类型不报错，只是收不到；请求为空时 `include` = 白名单）；`exclude` 含 `settings.*`，除非**桌面档案且角色 ≥ operator**（store.md §6.2 注明这两个用法）。因为角色可能在流的生命周期中变化，发送每个事件前再以**发送时刻**的设备角色与档案做一次最终判定（`settings.*` 只对桌面档案且角色 ≥ operator 的流发送）；每次心跳重新读取角色，越过 operator 边界时从当前 `scanned` 起用新的 `EventFilter` 重建 `tail`。通过的事件发 `id: <seq>` / `event: <type>` / `data: <信封单行 JSON，原样>`。
4. **心跳**：每 15 s 一帧 `event: heartbeat`、`id: <scanned>`、`data: {"seq":<scanned>,"ts":…,"log_epoch":<n>}`（`scanned` 含被过滤掉的事件，让过滤后的流也推进客户端游标；`log_epoch` 取自 `Store::log_identity()`，客户端发现变化就断开并全量重同步，P-67）。**每次心跳同时检查**：设备 `status`（读 `pki.directory.get`；已吊销 → 发 `close{"reason":"device_revoked"}` 后关闭，M1 的吊销断流机制，通常 15 s 内、上限 60 s）、证书 `notAfter`（到期 → `close{"reason":"cert_expired"}`；并为 `notAfter` 单设定时器，不必等心跳）、关停信号（`server_shutdown`）。
5. **慢消费者**：`tail` 没有每订阅者的内存队列，慢消费者只表现为"写得慢"（store.md §7.5）。本模块给发送端设有界缓冲（字节数 [建议] 4 MiB）和写超时（30 s）；缓冲写满且超时、或 `head - scanned` 超过落后上限 [建议] 100000 → `close{"reason":"lagging"}` 后关闭（客户端可从日志续传，不丢事件）。
6. **M3**：订阅 `pki::RevocationNotifier`，收到本设备 id 立即 `close{"reason":"device_revoked"}`；`Lagged` 时对本进程所有流做一次全量设备状态检查；心跳检查保留。
7. 连接结束（含错误）释放并发许可；`close` 帧发送失败不阻塞关闭。

保证：同一连接内 `seq` 严格递增不重复；过滤会产生空洞（正常）；跨重连至少一次，客户端按 `seq` 幂等；游标取"最后已应用事件 `seq`"与"最后收到心跳 `id`"中较大者由客户端负责。

### 7.10 统一签名原语的服务端部分

**审批读与拒绝**：#24 #25 viewer 起，直接返回 `policy` 的 `Approval`；#29 `deny` approver 起、**不需要签名**、幂等键必带 → `policy.deny`，非 `pending` → `409 approval_not_pending`（带 `approval_status`）。

**challenge**（#27）：approver、桌面档案；`Cache-Control: no-store`；`policy.challenge(id, device_key)`；同一设备有效期内重复请求得到同一份。

**grant**（#28）：幂等键必带；请求体只有 `{nonce, signature}`（`deny_unknown_fields`——**客户端不能提交 payload**）；步骤：

1. 鉴权链已保证 approver + 桌面档案（`policy.grant` 仍会复查）。
2. `device_key = DeviceKey { device_id, role, profile, status, approval_pubkey_fpr, verifying_key }`，其中 `verifying_key` 是配对时登记的审批签名公钥 `approval_pubkey`，来自 `pki.directory.verifying_key`（实时读，P-47）；mTLS 证书里的公钥不参与验签。
3. 审批是 PR 类（`merge_pr` 等）时：`forge.refresh_pr(repo, number)` 强制对账一次，得到的 head 作为 `CurrentSubject` 传入；`forge` 不可用 / 被限流则不传（`policy` 退回投影快照，权威检查在消费点）。
4. `policy.grant(...)`；错误映射见 §9：`StaleHead` → `409 stale_head`（审批**已**落盘为 `invalidated`，响应带 `current_head_sha` 与 `approval_status`）。
5. `200 Approval`（`granted`）。`200` 不代表已合并；合并由 Outbox 异步执行，进展经 `outbox.*` / `forge.*` 事件到达。

**内部动作**（api.md §6.6）：

| 步 | 本模块做什么 |
| --- | --- |
| 发起（#26） | 请求体 = `signing`（`{action, target, subject_digest}`）；`action` 必须是 `settings_update` / `device_role_change` / `device_revoke`（Forge 动作、条件伪动作 `force_push` / `push_outside_prefix` / `resolve_review_thread_human`、补丁闸门 `patch_apply_protected` → `422`，客户端不能创建，P-54）；`target` 形状与 `action` 对应、设备目标须存在（否则 `404`）；`subject_digest` 须是 `sha256:` + 64 位小写十六进制 → `policy.create_internal`（有效期 `policies.approval.ttl_s`，默认 24 小时，C27）→ `201 Approval`（`delivery_id: null`） |
| 签名 | 同 challenge / grant（第 5 步主体一致性不适用） |
| 重发（#31/#33/#34/#39/#40） | 带 `Gantry-Approval: apr_…`；先做 §7.8 的前置检查，再在**同一写事务**里 `policy.consume_internal` + 业务写（`settings.updated` / `pki.admin.set_role` / `pki.admin.revoke`）+ 业务事件；任一失败整体回滚，审批保持 `granted` |

- #39（改角色）：`role ∈ {viewer, operator, approver}`；**不能改自己**（`409 conflict`，`reason: self_role_change`，在消费审批之前判断）；目标是 `ios` 设备且 `role = approver` → `409 conflict`（`reason: mobile_cannot_approve`，同样在消费审批之前判断，P-76）；设备不存在 `404`；新角色对该设备的下一个请求立即生效（无缓存）。
- #40（吊销）：请求体 `{}`；可吊销自己；设备**已吊销则原样 `200`**——目标态已达成时先于签名校验返回，无副作用（本规格的约定，OQ-A3）；吊销提交后：M1 靠心跳断流，M3 即时断流。
- `signing::request_digest` 对 #39 的请求体 `{ "role": "operator" }`、对 #40 的 `{}` 计算，空白与键序不影响。

### 7.11 限流

- 内存令牌桶 `(DeviceId, RateClass)`；桶参数取 §3.7：读 600/min、写 120/min、发消息 30/min、challenge 与发起审批 30/min；配对 5/min/源 IP（IPv6 按 /64 聚合 [建议]）。容量 = 每分钟额度，匀速补充；空闲桶 15 分钟后淘汰，进程重启后重置（可接受）。
- 响应头（步骤 2 之后的所有响应，见 §7.1 步骤 14）：`RateLimit-Limit`（该类额度）、`RateLimit-Remaining`、`RateLimit-Reset`（秒）；超限 `429` + `Retry-After`（秒，向上取整）。`GET /v1/events` 每次建连消耗一个"读"令牌，另有每设备并发 3 的硬上限。
- 限流只作用于已认证设备与配对源 IP；握手失败的连接不进入应用层，由 TCP 层与 `pki` 的指标观察。

### 7.12 并发模型与持久化

- tokio 多线程运行时；每连接一个 task，HTTP/2 多路复用下每个请求独立 task；`Services` 全部 `Arc<dyn …>`，无全局可变状态，除：限流桶（`DashMap`）、SSE 连接注册表（并发许可）、配对窗口的 `watch` 接收端。
- 鉴权每请求读库（本地 SQLite 点查）。握手与设备查询用只读连接池，写事务走 `store` 的单写者通道，互不阻塞。
- `api_idempotency` 是本模块唯一自有表；其余状态全在 `store`。
- 优雅关停：停止接受新连接 → 向 SSE 发 `close(server_shutdown)` → 等待在途请求（上限 30 s）→ 退出。

## 8. 不变式与安全要求

本模块要保证 [architecture.md §5](../architecture.md#5-信任边界与不变式) 中的 **I6**（客户端不执行、不持凭据）与 **I7**（配置与工作分离），并支撑 I3、I4、I5，遵守 I1、I2。契约侧的落点见 [interfaces/api.md §8](../interfaces/api.md)，下表是实现层面的保证手段。

| 不变式 / 要求 | 本模块如何保证 |
| --- | --- |
| **I6 客户端不执行、不持凭据**（核心） | ① 路由集合只有六类写入（发消息、settings、审批签名 / 拒绝、状态流转、工作树人工锁、设备管理）；注册表中没有任何接收代码、命令、补丁、脚本的路由，也没有"让服务端替客户端运行某物"的路由——一致性测试对注册表做白名单断言，新增路由必须同步契约。② 响应、事件、日志、错误 `detail` 中不出现 installation token、App 私钥、webhook secret、模型 key；`SecretRef` 读回只有 `secret_ref` 与 `present`；请求体里的 `value` 立即进入 `SecretString`、不入日志、转交后清零。③ 客户端不调用 GitHub：PR / Issue 信息来自服务端快照，跳转链接由 `repo` + `number` 拼出。④ 客户端只持有两把设备私钥（mTLS 客户端密钥、审批签名密钥，均不可导出，P-47）、离线根 CA 指纹、设备证书；签名内容里没有任何凭据 |
| **I7 配置与工作分离**（核心） | 配置读写只有 `/v1/settings/{resource}` 与 `/v1/projects/{id}/settings/{resource}`（及只读 `catalog`、`status`），operator 起、手机档案不可用、viewer 不可读；所有工作路由请求体 `deny_unknown_fields`，消息里带 `model` / 预算 / 策略字段 → `422 unknown_field`；`Project.settings_override` 不在 `Project` 表示里返回；`Message.level_cap`、`PATCH /deliveries` 的 `level_cap` 是任务级上限，不是配置 |
| **I3 放行唯一决策点** | 本模块不判定放行：审批只经 `policy`；`grant` 请求不带 payload，服务端用自存 challenge 重建；内部动作的"消费 + 业务写"同事务；Forge 动作的审批客户端不能创建 |
| **I4 事实源** | 交付类快照带 `synced_at` 原样返回，不在 API 层缓存或修正；`grant` 前强制对账只是提前失败 |
| **I5 外部文本不可信** | 错误 `detail` 不回显外部文本；事件信封原样转发，由客户端渲染规则（api.md §1.5）处理；本模块不做 HTML 渲染、不拼接外部文本进响应头 |
| **I2** | 本模块不写 GitHub，不持有 token |
| **I1** | 沙盒面没有任何端点；`/webhook` 在 API 监听器上是 404 |
| 威胁：未认证访问 | API 监听器握手即拒；配对监听器仅一条路由、窗口外关闭、限流 + 最小体积 + 连接上限 |
| 威胁：吊销被绕过 | 无会话恢复、无 0-RTT；每请求复查设备状态；事件流心跳复查；M3 即时断流；备份恢复后由 `pki` 重放 `revocations.journal`（P-67） |
| 威胁：后台 mTLS 密钥被盗用来审批 | 审批只认 `approval_pubkey` 签名（必须用户在场的独立密钥，P-47）；`grant` 用 mTLS 密钥签名 → `403 signature_invalid` |
| 威胁：角色缓存导致提权残留 | 角色 / 状态每请求实时读库，进程内不缓存 |
| 威胁：重放 / 重复提交 | `Idempotency-Key` + 业务事件 `dedup_key`；nonce 单次；`If-Match` 防覆盖 |
| 威胁：信息泄露 | 401/403/404 的 `detail` 不区分内部原因（`pair_token_invalid` 统一）；握手失败原因只记日志；`X-Request-Id` 供支持排查；无 `Server` 版本头 |
| 威胁：资源耗尽 | 体积上限、流式读取、每设备并发流 3、令牌桶、HTTP/2 流上限、慢消费者断开、`limit ≤ 200` |
| 不防的 | 服务端主机失陷；持有有效 approver 设备且用户确认的恶意审批 |

## 9. 错误与恢复

下游错误到 problem 的映射（`code` 全部取自 api.md §3.1）：

| 下游 | HTTP / `code` | 扩展成员 |
| --- | --- | --- |
| `GrantError::Forbidden` | `403 forbidden_role`（档案不符为 `forbidden_profile`） | `required_role` |
| `GrantError::NotPending` / `deny` 非 pending | `409 approval_not_pending` | `approval_status` |
| `GrantError::ChallengeInvalid` | `409 challenge_invalid` | `reason`：`expired` / `used` / `mismatch` / `none` |
| `GrantError::SignatureInvalid` | `403 signature_invalid` | — |
| `GrantError::StaleHead` | `409 stale_head` | `current_head_sha`、`approval_status: "invalidated"` |
| `ConsumeError::Mismatch` | `409 approval_mismatch` | `reason`：`not_granted` / `consumed` / `expired` / `action_mismatch` / `target_mismatch` / `digest_mismatch` / `base_rev_mismatch` |
| 缺 `Gantry-Approval` 头 | `403 signature_required` | `signing`：`{action, target, subject_digest}` |
| `DeviceError::SelfRoleChange` | `409 conflict` | `reason: "self_role_change"` |
| `DeviceError::MobileCannotApprove` | `409 conflict` | `reason: "mobile_cannot_approve"`（P-76） |
| 状态冲突（放弃已关闭的交付等） | `409 conflict` | `reason: "invalid_state"` |
| `LockError::Held` | `409 worktree_locked` | `lock` |
| `PairError::TokenInvalid` | `401 pair_token_invalid` | — |
| `PairError::Validation` | `422 validation_failed` | `errors[]`（含 `/approval_pubkey`） |
| `RenewError::Validation` | `422 validation_failed` | `errors[]`（`/csr`） |
| `RenewError::Revoked` | `401 device_revoked` | — |
| `DeliveryControl` 目标不在可转移状态（暂停终态交付、重试非 `failed` 节点等） | `409 conflict` | `reason: "invalid_state"` |
| `pki::DeviceAdmin::revoke` 写 `revocations.journal` 失败 | `503 unavailable` + `Retry-After` | —（审批保持 `granted`） |
| settings 校验 | `422 validation_failed` | `errors[]`（含 config.md §11 语义码） |
| `store` 忙 / 不可用 | `503 unavailable` + `Retry-After` | — |
| 其余 | `500 internal` | — |

| 失败 | 处理 |
| --- | --- |
| 处理中途崩溃（写事务已提交 / 未提交） | 事务原子：已提交则业务事件在、幂等完成记录可能丢——同 key 重试时 `dedup_key` 冲突，读取已存在事件重建响应；未提交则 `in_progress` 租约过期后接管重做 |
| secret 已 `stage`、事件提交失败 | `abort`；启动对账兜底回收孤儿暂存 |
| 事件已提交、`commit` 失败 | 响应 `500`（客户端用同一 key 重试 `PUT` 会因 `If-Match` 不符得 `412`；重新 `GET` 看 `present`）；启动对账与后台重试补做 `commit`；`status` 里该模块 `unresolved_secret_refs` 可见 |
| `forge.refresh_pr` 失败 / 限流 | 不阻塞 `grant`，退回投影快照；权威检查在消费点 |
| 事件日志回放读失败 | 关闭该流（`close` 尽力）；客户端按退避重连 |
| 数据库从备份恢复（head 变小） | `log_epoch` 已加一：所有响应的 `X-Log-Epoch` 与心跳的 `log_epoch` 变化，客户端丢缓存全量重同步（P-67）；游标越过 head 的仍得 `409 cursor_ahead`（api.md §5.3） |
| 配对窗口监听器启动失败（端口占用） | `gantry pair` 的 CLI 报错；API 监听器不受影响 |
| TLS 证书热替换失败 | 保留旧证书、告警（`pki` 负责续签结果） |
| 未知路由 / 未启用路由 | `404 not_found`（不泄露"该路由在后续里程碑才有"） |
| 时钟偏差 | 以 `/v1/me` 的 `server.time` 供客户端校正；服务端自身的时间只用于有效期判断，不用客户端时间 |

## 10. 配置

`api` 的参数是**引导配置**（`deploy` 写入，重启生效），**不是 settings**（限流等"不经 API 修改"，api.md §3.5）。数值全是契约 §3.7 的 **[提议]** 默认值（OQ-14），实现为 `consts` 模块里的集中常量，禁止散落。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `api.listen` / `api.pair_listen` | `deploy` 定 | 端口不由契约固定；配对端口只在窗口内监听 |
| `api.sse.heartbeat_interval` / `api.sse.client_idle_timeout` | 15 s / 45 s | 后者是契约给客户端的断线判据（45 s 无字节），服务端不使用 |
| `api.sse.max_streams_per_device` | 3 | — |
| `api.sse.buffer_bytes` / `api.sse.write_timeout` / `api.sse.max_lag_events` | 4 MiB / 30 s / 100000 [建议] | 慢消费者判据（`tail` 本身无每订阅者队列） |
| `api.sse.tail_batch` | 沿用 `store` 的 `tail_batch` | 回放分批由 `store` 决定 |
| `api.page.default_limit` / `api.page.max_limit` | 50 / 200 | — |
| `api.idempotency.retention` / `api.idempotency.lease` | 24 h / 30 s [建议] | — |
| `api.approval.challenge_ttl` | 120 s | 不超过 `Approval.expires_at`（C27） |
| `api.worktree_lock.default` / `api.worktree_lock.max` | 1800 s / 14400 s | 交给 `repo` 校验 |
| `api.body.max` / `api.body.max_settings` | 64 KiB / 256 KiB | — |
| `api.message.max_text` / `api.message.max_refs` | 16 KiB / 20 | — |
| `api.rate.read` / `.write` / `.message` / `.sign` / `.pair_per_ip` | 600 / 120 / 30 / 30 / 5（每分钟） | — |
| `api.clock_skew_warn` | 60 s | 客户端判据，服务端仅在 `/v1/me` 提供 `server.time` |
| `api.revoke_stream_close_max` | 60 s | 上限；M1 由心跳实现（P-30） |
| `api.pair.max_body` / `api.pair.max_conns` / `api.pair.header_timeout` / `api.pair.request_timeout` | 16 KiB / 8 / 5 s / 10 s [建议] | 未认证面的最小化 |
| `api.h2.max_concurrent_streams` | 64 [建议] | — |
| `api.validation.max_errors` | 50 [建议] | — |
| `api.tls.min_version`（强制） | 1.3 | 不可配置 |
| `api.tls.session_tickets`、`api.tls.early_data`（强制） | 关闭 | 不可配置 |

## 11. 可观测性

发出的事件见 [data-model §5](../data-model.md#5-事件)。

- **事件**：本模块只追加 `settings.updated`（data-model §5.2；`data` 含 `scope`、`resource`、`schema_version`、`base_rev`、已剥离 secret 的 `value`、`relaxes[]`、`secret_changes[]`；`actor = {device, dev_…}`；全局写入 `subject = "settings"`、`project_id = null`，项目写入 `subject = prj_…`）。事件流本身不产生事件。
- **访问日志**（结构化）：`request_id`、`device_id`、`route_id`（契约序号）、方法、状态码、耗时、响应字节数、`rate_class`；**不记请求体和响应体**（settings、grant 尤其）；`Gantry-Approval` 头只记审批 id。
- **指标**：`api_requests_total{route_id,status}`、`api_request_seconds{route_id}`（直方图）、`api_rate_limited_total{class}`、`api_idempotent_replays_total`、`api_idempotency_conflicts_total{kind}`、`api_sse_streams`（仪表）、`api_sse_events_total{type}`、`api_sse_closes_total{reason}`、`api_sse_replay_events_total`、`api_sse_lag_seconds`、`api_signature_failures_total{code}`（`signature_invalid` / `challenge_invalid` / `stale_head` / `approval_mismatch`，突增是安全信号）、`api_pair_requests_total{result}`、`api_handshake_failures_total{reason}`（来自 `pki` 校验器回调）、`api_auth_failures_total{code}`。
- **追踪**：每请求一个 span，贯穿到下游模块调用；`request_id` 写入响应头与 problem 体。
- **健康**：`api` 自身不是 settings 的消费方，不出现在 `status.modules` 里；运维健康检查端点由 `deploy` 另定（不属于 48 条客户端路由）。

## 12. 测试与验收

### 12.1 测试替身与夹具

| 替身 / 夹具 | 用途 |
| --- | --- |
| `TestCA`（M0）+ `TestDevice` | 签发有效 / 过期 / 尚未生效 / 外部 CA / 已吊销 / 未知序列号的设备证书；`TestDevice` 持两把密钥（mTLS 客户端密钥、审批签名密钥，P-47），用 `rustls` 客户端 + `hyper` 发**真实 TLS** 请求，并能用审批签名密钥对 challenge 签名（`sign_approval`）。测试一律走真实握手，不绕过 TLS（`router_for_test` 只用于不涉及鉴权的路由级单元测试） |
| 真实 `pki` + `policy` + `Store::open_in_memory` | 组件测试用真实实现（二者是本泳道产物）；需要隔离时用 `FakePolicy`（`gantry-fakes`，P-51） |
| `FakeForge`（M0） | 提供 PR 快照、`refresh_pr`、注入 `forge.pr_synchronized` 以触发 `stale_head` |
| `FakeEngine`（含 `pause` / `resume` / `retry_node`）、`FakeConversation`、`FakeInbox`（`gantry-fakes`，P-51）、`FakeRepo`（locks）、`FakeSettingsContext`、`MemorySecrets`（secrets.md §6，带 `stage` 后崩溃 / 写失败注入）、`EgressStub`（`EgressControl` 的 secret 通道，P-51） | 实现 §6.3 的 facade 与端口，可脚本化返回与注入故障（含 `commit` / `activate_secret` 失败、`stage` 后崩溃） |
| `FakeClock`、`SeededEntropy` | 心跳、存活时间（TTL）、限流窗口、证书到期 |
| **Mock API 服务**（M0，由 `openapi.yaml` 生成） | 契约一致性套件的另一个被测方：同一套测试对 Mock 与真实 `api` 各跑一遍 |
| `schemas/openapi.yaml`、`schemas/events/*.json` | 响应与事件的 Schema 校验（`jsonschema`） |
| api.md §6.3 测试向量 | 与 `policy` 共用，验证 grant 端到端 |

### 12.2 验收项

**契约一致性（M0 起，M1 对真实实现）**

- [ ] 注册表的 `(id, method, pattern, min_role, mobile)` 与 `schemas/openapi.yaml`、api.md §4.1 **逐行相等**（46 + 1 条）；OpenAPI 里有而注册表没有、反之亦然，均失败。
- [ ] 手机子集恰好是 api.md §2.3 / §4.1 列出的 13 条路由（#2 #3 #4 #10 #11 #12 #21 #22 #41 #42 #46 #47 #48，C26、P-77）。
- [ ] （M4）#48：`ios` 设备 `PUT /v1/devices/self/push-token` → `204`，`notify` 的 `push_tokens` 有该设备一行；之后 `GET /v1/devices`、`/v1/me`、事件流里都没有令牌；设备吊销后该行被清除（P-77）。
- [ ] 把 `ios` 设备改为 `approver`（#39）→ `409 conflict`（`reason: mobile_cannot_approve`），在消费审批之前判断，审批不被消费（P-76）。
- [ ] `X-Log-Epoch`：每个响应（含 `404`、`429`、`5xx`、事件流响应头）都带，值等于 `Store::log_identity().log_epoch`；`restore_from` 后重启，值加一（P-67）。
- [ ] 每个成功响应都通过 `openapi.yaml` 的 Schema 校验；每个非 2xx 响应都是 `application/problem+json`，`type = urn:gantry:problem:<code>`，`code` ∈ 契约的 30 个；`X-Request-Id` 与 body 的 `request_id` 相同。
- [ ] 路由白名单：注册表里没有任何接收代码 / 命令 / 补丁 / 脚本的路由；新增路由而未更新白名单测试 → 失败（I6）。
- [ ] 所有 api.md §7 "路由与错误对照"表中的 19 行各转成一个测试，全部通过（下列用例是其展开）。

**TLS 与监听器（M1）**

- [ ] 无证书 / 过期 / 尚未生效 / 外部 CA / 已吊销 / 未在册证书连 API 端口 → 握手失败，**零 HTTP 响应字节**。
- [ ] `ServerConfig` 属性断言：仅 TLS 1.3；`client_auth_mandatory`；`send_tls13_tickets = 0`；无会话存储；`max_early_data_size = 0`；ALPN = `h2`、`http/1.1`。
- [ ] 吊销后用旧会话信息尝试恢复 / 0-RTT 重连被拒（rustls 客户端实测）。
- [ ] 在 API 端口请求 `/v1/pair`、在配对端口请求 `/v1/me`、API 端口请求 `/webhook` → `404 not_found`（problem+json）。
- [ ] 配对端口：窗口外拒绝连接；窗口内只接受 `POST /v1/pair`；超过连接并发上限 / 超大 body / 慢速读头被断开；窗口随 `open_session`、兑换、作废、过期在 1 s 内开关。
- [ ] 配对端到端：`TestDevice` 用令牌 + CSR + `approval_pubkey` 配对 → 取得链 `[叶子, 中间 CA, 根]`（根指纹 = 配对串固定值）→ 对 mTLS 端口 `GET /v1/me` 成功，`device.approval_pubkey_fpr` 正确；缺 `approval_pubkey` 或与 CSR 同钥 → `422`；第一台设备 `role = approver`，第二台 `operator`；令牌无效 / 已用 / 过期 / 作废的 `401` 响应逐字节相同。

**鉴权链（M1）**

- [ ] `viewer` 调 `POST …/messages` → `403 forbidden_role`，`required_role: "operator"`。
- [ ] `ios` 设备（含 `approver`）调 `GET /v1/deliveries`、`GET /v1/settings/models`、任一审批路由 → `403 forbidden_profile`；`ios` 的 `viewer` 调 `POST …/messages` → `403 forbidden_role`（先角色后档案）。
- [ ] 角色立即生效：`PUT /v1/devices/{id}/role` 成功后该设备的**下一个请求**即按新角色判定（同一 TLS 连接，不重连）。
- [ ] 设备在长连接上被吊销：同一连接的下一个请求 → `401 device_revoked`。
- [ ] 步骤顺序用例：未知路径 `404`；已知路径错方法 `405` + `Allow`；限流先于 body 解析（超限请求不读 body）；`415` → `413` → `400` → `422` 的顺序各一例。
- [ ] 角色 / 状态不缓存：用包装了 `DeviceDirectory` 的计数替身断言每个请求恰好读一次 `by_cert_serial`。

**证书续期（M1，P-64）**

- [ ] `POST /v1/devices/self/renew`：桌面与手机档案、viewer 起均可用；响应链根指纹不变；新证书立即可握手，旧证书到期前仍可握手；`approval_pubkey_fpr` 不变；非法 CSR → `422`；已吊销设备 → 握手失败；请求按写类限流。

**控制路由与收件箱计数（M1，P-56、P-63）**

- [ ] #43 #44 #45 缺 `Idempotency-Key` → `400 idempotency_key_required`；viewer → `403 forbidden_role`；手机档案 → `403 forbidden_profile`；暂停后再暂停 → `200`（目标状态幂等）；终态交付 / 非 `failed` 节点 → `409 conflict`；成功响应的 `X-Event-Seq` 指向 `delivery.paused` / `delivery.resumed` / `node.retry_requested`。
- [ ] #46 `GET /v1/inbox/summary` 只数 `open`，按 `project_id` 分组（全局项为 `null`）；手机档案可用；`/v1/inbox/summary` 不被 `/v1/inbox/{inbox_id}` 吞掉。

**统一错误 / 校验（M1）**

- [ ] 30 个 `code` 各至少一个触发用例，扩展成员（`required_role`、`errors[]`、`current_head_sha`、`approval_status`、`reason`、`signing`、`lock`、`Allow`、`Retry-After`、`ETag`）齐全。
- [ ] 请求体带 `model` 或预算字段（`POST …/messages`）→ `422`，`errors[].code = unknown_field`，指针指向该字段；JSON 语法错 → `400 invalid_request`；未知查询参数 → `400 invalid_request`；非 JSON `Content-Type` → `415`；超 64 KiB（settings 超 256 KiB）→ `413`。
- [ ] `errors[].pointer`：settings 相对 `value`；跨 resource 校验错误指向本次写入的 resource。
- [ ] 错误 `detail` 不含请求里的外部文本（注入探针字符串不出现在任何响应体中）。

**分页 / 幂等 / 并发控制（M1）**

- [ ] 分页：`limit` 默认 50、最大 200（201 → `400`）；游标稳定（翻页期间插入新项不重复不遗漏）；篡改 / 跨资源游标 → `400 invalid_cursor`；`order` 默认值各路由正确（`messages` desc、attempts asc）。
- [ ] 幂等：同 key 重放 `POST …/messages` 只产生 1 条 `message.posted`，第二次响应与首次逐字节相同且带 `Idempotent-Replayed: true`；同 key 不同内容 → `422 idempotency_key_reuse`；首次处理中重试 → `409 idempotency_in_progress`；首次 5xx 不登记，同 key 可重试；key 不足 16 位 / 含非法字符 → `400`；必带路由缺头 → `400 idempotency_key_required`；不同设备同 key 互不影响；24 h 后（`FakeClock`）失效。
- [ ] 崩溃：在"业务事务已提交、幂等完成记录未写"处注入崩溃，重启后同 key 重试**不重复追加事件**（`dedup_key` 冲突，响应由已有事件重建）。
- [ ] settings：缺 `If-Match` → `428 precondition_required`；旧 `If-Match` → `412 precondition_failed` 且头带当前 `ETag`；`GET` 的 `If-None-Match` 命中 → `304`；写响应的 `X-Event-Seq` 等于新 `rev` 等于 `ETag`；两个客户端并发保存同一 resource，后到者 `412`。
- [ ] `X-Event-Seq`：每个成功响应都有；读响应的值不小于该快照反映的最大事件（并发写入下用"读—写—读"断言单调）。
- [ ] 限流：各类额度耗尽 → `429` + `Retry-After`；`RateLimit-*` 头在所有响应上出现；`FakeClock` 推进后恢复；不同设备互不影响；配对按源 IP；事件流建连消耗读令牌。

**settings（M1：models；M2：其余）**

- [ ] `dry_run` 返回 `{valid, errors, warnings, relaxes, required_role, requires_signature, signing?}`，不落库、不发事件、不消费审批。
- [ ] 带 secret `value` 的 `PUT /v1/settings/models` 不带 `Gantry-Approval` → `403 signature_required`（附 `signing`）；operator 设备即使带有效审批也 `403 forbidden_role`；approver + 有效审批 → `200`。
- [ ] 为内容 A 批准的审批提交内容 B → `409 approval_mismatch`（`digest_mismatch`）且**未写入**（库、密钥存储、事件日志均无变化）；同一审批第二次使用 → `consumed`；`base_rev` 与 `If-Match` 不符时 `412` 优先于 `approval_mismatch`。
- [ ] 写入凭据后：`GET` 同一 resource、`settings.updated` 事件、访问日志、错误响应、`/status` 中**都没有 `value`**（用唯一探针字符串全量扫描响应、事件日志、日志输出）；`null` → `422 secret_clear_in_use`；`secret_ref` 不存在且未带 `value` → `422 secret_ref_missing`；形似密钥的内联字符串 → `422 inline_secret_detected`。
- [ ] 两阶段：`stage` 后事务失败 → `abort` 被调用且配置未变；事件已提交而 `commit` 失败 → 启动对账（`SecretSink::staged()` + `EventLog::by_secret_change`）补做；消费者在 `commit` 前读不到新值。
- [ ] M1 里对非 `models` 的 resource `PUT` → `404`；读取八个 resource 都 `200`；`catalog` / `status` 结构符合 config.md §2.8；`{resource}` 不会吞掉 `catalog` / `status`。
- [ ] M2：项目范围 `PUT` 越界键 → `422`（`override_not_allowed` / `override_loosens_global`）；`notify` / `github` → `422 not_overridable`；`view=effective` 附 `clamped[]`；`DELETE` 回退全局；`?rev=N` 取历史。

**审批与签名（M1）**

- [ ] 全链路：`TestDevice`（approver）`GET challenge` → 按 api.md §6.3 签名 → `POST grant` → `200 granted`；同一设备重复 `GET challenge` 得到同一份；用掉后换新 nonce。
- [ ] `GET challenge` → 篡改 nonce → `POST grant` → `409 challenge_invalid`；payload 被改一个字节 → `403 signature_invalid`；换 key 重发已用 nonce → `409 challenge_invalid`（`used`）；同 key 重试 → 重放首次 `200`。
- [ ] `grant` 请求体带 `payload` 字段 → `422 unknown_field`（客户端不能提交待签内容）。
- [ ] 签名后 PR 有新推送（`FakeForge` 注入）再 `grant` → `409 stale_head`，`current_head_sha` 正确，之后 `GET /v1/approvals/{id}` 为 `invalidated`，事件流出现 `approval.invalidated`。
- [ ] 两个 approver 设备并发 `grant` → 一个 `200`，一个 `409 approval_not_pending`。
- [ ] `deny`：approver、不需要签名、幂等键必带；非 pending → `409 approval_not_pending`。
- [ ] 内部动作：operator `POST /v1/approvals`（`device_revoke` 等）→ `201`、`delivery_id: null`、有效期 = `policies.approval.ttl_s`（C27）；`action` 为 `merge_pr` 等 Forge 动作、条件伪动作或 `patch_apply_protected` → `422`；approver 设备批准后，operator 设备带 `Gantry-Approval` 重发吊销 → 仍须 `required_role` 满足，否则 `403`。
- [ ] 改角色 / 吊销不带 `Gantry-Approval` → `403 signature_required`；改自己的角色 → `409 conflict`（`self_role_change`）且**审批未被消费**；吊销已吊销设备 → 原样 `200`；吊销提交失败时审批仍 `granted`（同事务回滚）。
- [ ] 测试向量（api.md §6.3）：用文中审批签名公钥与签名走 `grant` 的验签路径（`policy` 夹具设备），通过；canonical 字节的 SHA-256 吻合。
- [ ] 用 mTLS 客户端私钥（而非审批签名私钥）对 challenge 签名 → `403 signature_invalid`（P-47）。
- [ ] 手机档案设备（即使 approver）调 challenge / grant / deny → `403 forbidden_profile`。

**事件流（M1；M3 即时断流）**

- [ ] 断线续传：建立流 → 收到若干事件 → 断开 → 带 `Last-Event-ID` 重连 → 不丢、不重（`seq` 连续）；`after=` 与头同时给时头优先。
- [ ] 回放与实时无缝：在回放进行中并发追加事件，客户端收到的序列无缺口无重复（用 `Store::open_in_memory` 加测试钩子控制交错）。
- [ ] 无游标：首帧是带当前 head 的 `heartbeat`，不回放历史。
- [ ] 过滤：`project_id` 过滤时全局事件（`project_id = null`）仍到达；`types` 精确与 `前缀.*`；过滤产生的 `seq` 空洞由 `heartbeat.id` 推进。
- [ ] 手机档案：只收到 `project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked`；请求白名单外类型不报错但收不到。
- [ ] `settings.*`：viewer 与手机档案的流里没有；operator 桌面设备有；运行中把设备降到 viewer，之后的 `settings.*` 不再下发（发送时刻判定）。
- [ ] `Last-Event-ID` 大于 head → `409 cursor_ahead`（普通 problem，不开流）；非数字 → `400 invalid_cursor`。
- [ ] 每设备第 4 条并发流 → `429`；关闭一条后可再开。
- [ ] 心跳：`FakeClock` 每 15 s 一帧；`id` 为已扫描最大 seq（含被过滤）；`data.log_epoch` 等于当前纪元（P-67）。
- [ ] **吊销断流（M1 退出标准）**：`pki.revoke` 后，该设备已建立的事件流在下一次心跳检查时收到 `close{"reason":"device_revoked"}` 并关闭，用 `FakeClock` 断言 ≤ 60 s（通常 ≤ 15 s）；新握手被拒；其他设备的流不受影响。
- [ ] 证书到期：到 `cert_not_after` 时流关闭并带 `cert_expired`。
- [ ] 慢消费者：不读取的客户端在缓冲超限后收到 / 触发 `lagging` 关闭，服务端内存不无限增长；重连续传无丢失。
- [ ] 关停：`shutdown` 时所有流收到 `server_shutdown`。
- [ ] **M3**：订阅 `RevocationNotifier` 后吊销在 1 s 内断流；`Lagged` 退回全量检查；心跳兜底仍有效。
- [ ] **M3 密钥轮换**：对同名 `secret_ref` 写入新 `value`（approver + 签名）→ `settings.updated.data.secret_changes[]` 为 `{ref, op: "rotate", version: n+1}`；`commit` 之后消费者读到新版本、之前读到旧版本；全流程不出现明文 `value`；轮换失败（`stage` / `commit` 故障）时旧版本保持有效。
- [ ] 帧格式：`id` = `seq`、`event` = 类型、`data` = 信封单行 JSON 且与 `store` 里的信封**字节一致**；`retry: 3000` 在连接开头。

**M1 退出标准中的 `sec` 项（集成）**

- [ ] 任务进行中关闭桌面端，任务继续；重新打开后事件流从游标续上，界面状态一致（与 `fe-desktop` 联调）。
- [ ] 写入模型凭据需要 approver 设备签名；没有签名的写入被拒绝。
- [ ] 吊销设备后，其已建立的事件流在心跳间隔内断开（上限 60 秒，建议值）。
- [ ] 端到端：对话发起 Low 任务 → PR → CI 通过 → 桌面端经 `challenge` / `grant` 审批 merge → Issue 关闭（与 `policy`、`forge`、`pki` 联调）。

**M4（手机档案）**

- [ ] 手机端能配对（`ios`）、发消息（带 `level_cap` 被忽略，交付单元的上限是项目默认值，C26）、读收件箱与 `GET /v1/inbox/summary`、续期证书、`choose` 一个 `decision_needed` 选项与 `budget_exhausted` 的 `abandon`；对 `escalation` / `approval_needed` 的 `choose` 与 `raise_cap` → `403 forbidden_profile`；对 `spec_needed` 用发消息答复（`choose` → `422`，P-79）；注册推送令牌（#48）；设置、审批、设备管理、交付单元、工作树、工作流、预算路由全部 `403 forbidden_profile`。

**模糊测试（fuzzing）/ 属性测试（property-based testing）（建议）**

- [ ] 对请求解析（游标、`Idempotency-Key`、`Last-Event-ID`、`types` 过滤、JSON body）做 `cargo fuzz`：无 panic、无超时。
- [ ] 属性：任意交错的 `POST` 重试序列下，业务事件数 = 不同 `(device, key)` 数；SSE 任意断线重连序列下，最终应用的 `seq` 序列等于日志中通过过滤的子序列。

## 13. 开放问题

| # | 问题 | 本文的临时处理（不构成决定） | 影响 |
| --- | --- | --- | --- |
| OQ-A1 | **已决：见裁决 C26**（契约 §2.3 的手机路由数改为实际数；加入 #46 #47 后为 12 条，P-77 加入 #48 后为 13 条）。原问题：手机路由数文字与表格不一致 | 已落实到 §6.2、§12.2 | — |
| OQ-A2 | **已决：见裁决 C26 / P-56**（必带清单加入 `choose` 与控制路由 #43–#45）。原问题：§3.3 必带清单缺 `choose` | 已落实到 §7.6 | — |
| OQ-A3 | 目标态已达成的内部动作（吊销已吊销的设备、角色未变）是否仍要求签名 / 消费审批 | 吊销已吊销设备原样 `200`，先于签名校验；改成相同角色按普通写入走完整校验并消费审批，不发事件 | api.md §4.10、`pki` |
| OQ-A4 | **已决：见裁决 C26**（设置状态字段统一为 `applied_rev`，结构以 config.md §2.8 为准）。原问题：契约示例与 config 的状态字段不一致 | 已落实到 §7.8 | — |
| OQ-A5 | **已决：见裁决 C26**（手机档案发消息时服务端忽略 `level_cap`，使用项目默认上限，P-35）。原问题：手机能否带 `level_cap` | 已落实到 §7.5 | — |
| OQ-A6 | **已决：见裁决 C4 / P-50**（模型 key 经 Egress 控制面 `/run/gantry/egress.sock` 两段式写入，只写不读）与**裁决 C20 / P-66**（第一台设备配对前由主机命令写入并记审计事件）。原问题：密钥转交的跨进程通道与引导期写入入口 | 已落实到 §3、§6.3（`SecretTargets`） | — |
| OQ-A7 | ~~邻接规格与 §6.3 的写上下文对齐（`WriteCtx` / `ActorCtx` / `DeviceActor`）~~ | 已决：见 decisions.md P-78（统一 `core::ports::ActorCtx`）；内部动作的"消费 + 业务写"仍由本模块开 `WriteTx` 后传给 `policy` / `pki` | — |
| OQ-A8 | 事件保留期与 `410 cursor_expired`（api.md OQ-13）；`close` 帧 `reason` 的完整集合是否还需要 `revoked_other` 等 | 保留不限；`reason` 取契约的四个值 | `store` |
| OQ-A9 | **已决：见裁决 C18 / P-64**（`POST /v1/devices/self/renew`，M1，mTLS + 新 CSR，可换 mTLS 密钥，审批密钥不变）。原问题：证书续期路由缺失 | 已落实到 §6.2 #47、§12.2 | — |
| OQ-A10 | 未启用 / 未实现路由的响应：本规格用 `404 not_found`；是否应改为 `503 unavailable` 以便客户端区分"尚未上线" | `404` | 客户端行为 |
| OQ-A11 | 审计日志的读取路由（契约无）；`policy` 的 `AuditLog` 如何向客户端暴露 | 不实现；事件流中的 `policy.denied` / `approval.*` 即客户端可见的审计 | api.md、`desktop-ui` |
| OQ-A12 | 配对监听器的源 IP 在内网 / 反向代理 / 隧道后的可信取法（`X-Forwarded-For` 是否可信） | 只用 TCP 对端地址；配对端口不经隧道（反向代理 / 隧道只转发 `/webhook` 与 `/setup/github/callback`，P-65） | 限流有效性、`deploy` |
| OQ-A13 | 工作路由的 `deny_unknown_fields` 在 v1 内新增可选请求字段时的兼容策略（客户端新于服务端时会被 `422`） | 严格拒绝；新字段随服务端先发布 | api.md §1.4 |
| OQ-A14 | M1 settings 的写入范围：契约列八个 resource，M1 设置页只有"模型"（config.md §13）；M1 是否开放其余 resource 的 `PUT`（尤其 `policies`，policy v0 在 M1 不读取它，见 [policy.md OQ-P11](policy.md)） | M1 只开放 `models` 写入（其余 `PUT` 返回 `404`），读取全开；M2 起随各设置页开放 | api.md §4.9、config.md §13、`desktop-ui` |
