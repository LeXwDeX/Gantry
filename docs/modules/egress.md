# 出网代理（`egress`）

> 状态：设计 v0.1 · 里程碑：M1 起（M2 / M3 增量） · 泳道：`exec`（见 [milestones.md](../milestones.md)） · crate / 目录：`crates/gantry-egress/`（独立二进制 `gantry-egress`，独立系统用户与 systemd 单元，由 `deploy` 安装）

`egress` 是唯一持有模型 key 与私有包仓库凭据、也是沙盒唯一的出网路径的独立进程：模型 API 走反向代理（注入 key、按每个尝试一枚的能力令牌 `job_token` 或服务令牌 `svc_token` 计量 token，P-49、P-50），公共包仓库走 HTTPS `CONNECT` 白名单（不注入凭据），私有包仓库走反向代理（注入凭据），GitHub 域名族一律拒绝，所有请求都留审计（decisions P-16）。

> 标注 **[提议]** 的是本文为了能写出接口与测试而给的默认，骨架没定；标注 **需核实** 的是作者没有确认的外部行为；标注 OQ-E*n* 的点见 §13，实现者不得自行拍板。

## 1. 职责

| # | 职责 | 依据 |
| --- | --- | --- |
| G1 | **模型 API 反向代理**：调用方（沙盒内的 agent CLI，和服务端的 `models`）把模型 base URL 指向 Egress——沙盒为 `http://<egress>/j/<job_token>/<provider>/…`，`models` 走回环 HTTP 监听并带 `svc_token`（P-49、P-50）；Egress 丢弃调用方自带的认证头，向固定的上游 provider 主机注入真实 key，流式透传响应 | P-16、P-49、P-50；[worker-protocol §2.3 E3](../interfaces/worker-protocol.md#23-对-egress-的要求) |
| G2 | **按调用方计量 token 并封顶**：沙盒调用方以 URL 路径中（`CONNECT` 为 `Proxy-Authorization` 中，P-72）的 **`job_token`**（服务端为每个 Attempt 签发的能力令牌，只对该尝试有效、尝试结束即作废；"只能从沙盒网段使用"的强制方式需核实，P-73）映射到 `job_id`，不信任沙盒自报的其他任何标识；沙盒即使读到令牌也只能给自己的尝试记账。`models` 以进程级 `svc_token` 调用、以可信请求头 `X-Gantry-Ledger` 声明台账（P-74），同一口径，计入会话台账或交付台账。四类 token（input / output / cache_read / cache_write）与请求数分别记录，按 `levels.token_weights` 加权；达到 `tokens_total` 时切断后续模型请求 | P-16、P-44、P-49、P-50、P-70；E4；data-model [Attempt.usage](../data-model.md#workflow--node--attempt) |
| G3 | **包仓库 `CONNECT` 白名单**：按 job 的出网档案（分组集合）放行 `host:443`，不注入凭据、不解 TLS | P-16；E2、E5；[config §8](../interfaces/config.md#8-egress) |
| G4 | **私有包仓库反向代理**（`egress.injection`）：沙盒内包管理器指向 Egress 的路由，Egress 在出站时向列出的主机注入凭据 | P-16；E5；config §8.1 |
| G5 | **硬性拒绝**：GitHub 域名族、IP 字面量、解析到回环 / 私网 / 链路本地 / 元数据地址的连接、非 443 端口、裸 `*` / 多层通配——这些不是配置，配置与运行时两层都执行 | I1；config §5.5、§8.3 |
| G6 | **出网审计**：每个允许与拒绝都有带 `job_id` 的记录，**不可关闭**；M3 提供回放 | E6；config §8.3；milestones M3 |
| G7 | **控制面**（`/run/gantry/egress.sock`，契约 [interfaces/egress-control.md](../interfaces/egress-control.md)，P-50）：签发 / 吊销 `job_token`、写入 secret（只写不读）、查询 secret 是否存在（`present`）、拉取用量；另有应用配置视图、报告状态（`applied_rev`、`health`、`unresolved_secret_refs`） | E7；P-50；config §2.4、§2.6、§2.8 |
| G8 | **密钥保管**：模型 key 与私有包仓库凭据只存在于本进程的密钥存储；轮换（M3）热切换 | architecture §5–§6；P-38 |
| G9 | **失败闭合**：无配置、配置非法、凭据缺失、审计写失败时拒绝而不是放行 | config §2.6 |

### 里程碑交付

| 里程碑 | 本模块交付 |
| --- | --- |
| M0 | 无实现。冻结 `core::ports::EgressControl`（§6.2，P-51）与 `models` 调用约定（§6.3），二者以 `interfaces/egress-control.md`（P-50）为准；`contract` 泳道交付的 `EgressStub`（实现 `EgressControl`，P-51）；`MockUpstream` / `TestSandboxClient` 等测试替身的约定 |
| M1 | G1、G2、G3、G5–G9：模型反向代理（沙盒的 `job_token` 路径与 `models` 的 `svc_token` 回环监听两种调用方）、key 注入、按令牌计量与封顶（`levels.token_weights` 加权）、`CONNECT` 白名单（机制就绪，默认档案可只含 `model_api`）、GitHub 等硬性拒绝、出网审计（文件）、控制面、登记与用量持久化、配置视图应用、密钥转交与存储、进程加固。验收退出标准的两条："沙盒内访问 `github.com` 和 `api.github.com` 失败"（拒绝侧）、"沙盒内读不到任何模型 key"（密钥侧） |
| M2 | G4 私有包仓库反向代理（M2 `exec` 泳道，C28）；按 job 的限速、连接数与字节上限；默认语言包仓库分组落地；项目收窄档案（有效集合 = 快照 ∩ 当前）的验证 |
| M3 | 审计回放接口与哈希链；密钥轮换热加载（模型 key）；为"沙盒逃逸检测告警"提供信号（无效 / 已吊销 / 非沙盒网段使用的 `job_token`、伪造头，OQ-E12） |
| M4 | 容量与公平：Ultimate 多沙盒并发下的连接 / 带宽 / 审计吞吐压测 |

## 2. 不负责

| 事项 | 归属 |
| --- | --- |
| 创建沙盒网络、把 `job_token` 写进沙盒环境 | `sandbox`（经控制面向本模块申请 `job_token`，P-49）；本模块只签发、校验与吊销令牌 |
| 模型选择、重试、降级（fallback）、推理深度 | `models`（直接调用）/ agent CLI（沙盒内）；`egress` 不重试、不换 provider |
| 预算预留 / 回收、`BudgetLedger`（交付台账与会话台账）、`budget.charged` / `budget.exhausted` | `engine` / `models`（从 `egress` 取计量与封顶标志） |
| 失败分类（`exec.model_api`、`budget.tokens`） | `engine`（`egress` 只给事实：封顶标志、上游错误摘要） |
| 密钥文件的加密与备份实现 | `secrets`（`egress` 使用其库的**自己的实例**） |
| 设置的校验与存储、角色与签名 | `api` + `core` 校验器（`egress` 收到视图后只做防御性复核） |
| 写事件日志 | 不写：`egress` 与 `store` 之间没有写路径；`gantryd` 经控制面**拉取**用量与状态再记事件 |
| 与 GitHub 的任何通信 | 无：GitHub 域名族被硬性拒绝（G5）；`forge` 在 `gantryd` 里用自己的 token 联网，不经 `egress` |
| TLS 中间人解密 `CONNECT` 流量 | 不做（既不信任也不需要代理 CA，E3） |
| 依赖指向 `github.com` 的 git 依赖供给 | 第一版默认拒绝（decisions Q-08）；服务端缓存方案待决 |

## 3. 依赖

规格书与本文同批编写，对方文件若尚不存在，以 `interfaces/` 契约为准。

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `secrets`（库，本进程独立实例） | 读取模型 key 与私有包仓库凭据；写入（由 `api` 转交）、版本、轮换；仅 `egress` 的使用方标识可读这些 ref | architecture §4、§6；decisions P-38；config §2.4 |
| `core` | 端口 `core::ports::EgressControl`（`gantryd` 侧的客户端实现它，P-51）；`AttemptId` `SandboxId` `TokenUsage` `TokenWeights` `Tier` `ProfileName`、配置视图类型与防御性校验函数 | data-model；`schemas/config/`；[interfaces/egress-control.md](../interfaces/egress-control.md) |

依赖按裁决 C13（P-59）：`egress` 依赖 `core` 与 `secrets`。`egress` **不**依赖 `store`、`engine`、`models`、`sandbox`（反过来是它们经控制面调用 `egress`）。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 沙盒数据面请求（模型 API 明文 HTTP；`CONNECT`；私有仓库路由） | 沙盒内进程，经沙盒网络 | HTTP/1.1；归属由 `job_token`（模型 API 与私有仓库在路径 `/j/<job_token>/…`，`CONNECT` 在 `Proxy-Authorization`）确定（P-49、P-72）；"来源属于沙盒网段"的检查需核实（P-73） | **不可信**：请求头、路径、体、目标主机、声称的标识全部不可信；令牌只决定"记到哪个尝试"，不提升权限 |
| `models` 数据面请求 | `gantryd` 内的 `models`，经回环 HTTP 反向代理（P-50） | HTTP/1.1；路径中的 `svc_token` 与 `X-Gantry-Ledger` / `x-gantry-tier` / `x-gantry-call-id` 头（§6.3，P-74） | 调用方可信（`svc_token` + 回环来源），载荷里可含不可信文本 |
| 控制面调用 | `gantryd`（`sandbox`、`engine`、`api`、`models`） | §6.2；Unix socket `/run/gantry/egress.sock`，校验对端凭据 | 可信 |
| 配置视图 | `gantryd` 经 `apply_config` | `egress.*` + `models.providers`（只含 `secret_ref`，无值） | 可信；仍做防御性复核（V10、V11、V13） |
| 密钥值 | `api` 经 `EgressControl::stage_secret` 转交（[api.md](api.md)） | 一次性写入（只写不读），之后不再出现在任何接口 | 可信，高敏 |
| 上游响应（模型 API、包仓库） | 外部服务 | HTTP | **不可信数据**（响应头 / 体可能被篡改或含恶意内容）；只提取用量字段，其余原样透传 |
| DNS 解析结果 | 本进程解析器 | A / AAAA | 不可信：每次连接前校验落点 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| 代理后的响应 | 沙盒 / `models` | 透传，去掉逐跳头与可能回显凭据的头；拒绝时返回固定形状的错误体（§6.4） |
| `JobTokenGrant`（`job_token` 与模型 base URL） | `sandbox`（写进沙盒环境） | §6.2 |
| `JobUsage` / `FinalUsage`（含 `capped`、`incomplete`、上游错误摘要） | `sandbox` → `engine` | §6.2；字段与 data-model `Attempt.usage` 一致 |
| `CallUsage` / 用量游标页 | `models` | §6.2 |
| `EgressStatus`（`applied_rev`、`health`、`unresolved_secret_refs`、有效令牌数） | `api`（`GET /v1/settings/status` 的 `modules.egress`） | config §2.8 |
| 审计记录（JSON Lines 文件） | 运维 / M3 回放 | §6.5 |
| 指标与日志 | 运维 | §11 |

## 6. 对外接口

### 6.1 数据面监听器

| 监听器 | 面向 | 协议 | 作用 | 归属判定 |
| --- | --- | --- | --- | --- |
| `sandbox_model` | 沙盒 | HTTP/1.1（明文，到 Egress；Egress 再以 TLS 连上游，E3） | 模型 API 反向代理，路径 `/j/<job_token>/<provider_id>/…`；默认 `<EGRESS_ADDR>:8081`（deploy.md §7.8） | 路径中的 `job_token` → 有效令牌表（P-49）；来源网段检查需核实（P-73） |
| `sandbox_connect` | 沙盒 | HTTP/1.1 `CONNECT` | 公共包仓库白名单隧道（`HTTPS_PROXY=http://job:<job_token>@<egress>:3128`）；默认 `<EGRESS_ADDR>:3128` | `Proxy-Authorization`（Basic，用户名 `job`）中的 `job_token`（P-72）；网段检查同上 |
| `sandbox_registry`（M2） | 沙盒 | HTTP/1.1 | 私有包仓库反向代理，路径 `/r/<job_token>/<injection_id>/…` **[提议，egress-control.md OQ-1]**；端口由 `deploy` 分配（OQ-E7） | 同 `sandbox_model` |
| `svc_model` | `gantryd` 内的 `models` | HTTP/1.1，**只监听回环地址** | 服务端模型调用的反向代理，路径 `/s/<svc_token>/<provider_id>/…`（P-50，egress-control §3.2） | `svc_token`（每个 `gantryd` 进程一枚，P-74）+ 回环来源；台账取自 `X-Gantry-Ledger` |
| `egress.sock` | `gantryd` | HTTP/1.1 over Unix socket，只有控制面路由 `/control/…`（JSON 请求 / 响应） | `/run/gantry/egress.sock`（P-50；目录与套接字权限见 deploy.md §6.3） | 套接字对端凭据（`SO_PEERCRED` 的 uid 必须是 `gantry`） |

`egress.sock` 是 `gantryd` ↔ `egress` 控制面的唯一套接字（P-50）；它**不会**被挂进任何沙盒（I1，P-15），也不监听在沙盒网络可达的地址上。`models` 的模型调用不再走 Unix socket，而走 `svc_model` 回环监听（P-50）。

### 6.2 控制面（`gantryd` → `egress`，`core::ports::EgressControl`）

`EgressControl` 定义在 `core::ports`（P-51），由 `gantryd` 侧经 `egress.sock` 的客户端实现，`sandbox`、`models`、`api`、`engine` 经它调用；线上形状以 [interfaces/egress-control.md](../interfaces/egress-control.md) 为准（P-50），下面是签名草案。M0 的 `EgressStub` 实现同一个 trait。

```rust
#[async_trait]
pub trait EgressControl: Send + Sync {
    /// 静态端点：沙盒侧监听地址与 CONNECT 代理地址（不含任何令牌）。
    async fn endpoints(&self) -> Result<EgressEndpoints, EgressError>;

    // ---- job_token（P-49）----
    /// 为一个 Attempt 签发 job_token。建沙盒容器之前调用（环境变量在创建时设定）。重放：同 job_id 且绑定相同 →
    /// 签发新令牌、旧令牌作废、已计用量保留（reissued = true）；绑定不同 → binding_mismatch（egress-control §4.1）。
    async fn issue_job_token(&self, req: IssueJobToken) -> Result<JobTokenGrant, EgressError>;
    /// 当前计量快照（不吊销）。展示 / 预警用。
    async fn job_usage(&self, job_id: &AttemptId) -> Result<JobUsage, EgressError>;
    /// 原子地"取最终计量并吊销令牌"。幂等：重复调用返回同一份 FinalUsage，直到 forget_job。
    async fn revoke_job_token(&self, job_id: &AttemptId) -> Result<FinalUsage, EgressError>;
    /// engine 在 attempt.finished 落库后经 SandboxService::release 触发。TTL（默认 24 h）到期也会自动遗忘。
    async fn forget_job(&self, job_id: &AttemptId) -> Result<(), EgressError>;

    // ---- svc_token（P-50、P-74：每个 gantryd 进程一枚）----
    /// 本进程的服务令牌与回环 base URL。客户端实现在首次调用时签发（POST /control/v1/svc-tokens，
    /// 同时吊销此前进程留下的令牌），之后缓存复用；到期前 1 h 自动轮换。
    async fn service_token(&self) -> Result<SvcTokenGrant, EgressError>;
    /// 收到 token_invalid 后强制轮换：签发新令牌并吊销旧令牌；grant.revoked_previous 供对账。
    async fn rotate_service_token(&self) -> Result<SvcTokenGrant, EgressError>;

    // ---- 配置 ----
    /// 配置视图（带 config rev）。非法视图被拒绝并保留旧配置。
    async fn apply_config(&self, view: EgressConfigView) -> Result<AppliedConfig, EgressError>;

    // ---- secret：只写不读（P-50）----
    /// 查询 secret 是否存在（present）；只回答布尔值。
    async fn secret_present(&self, refs: &[SecretName]) -> Result<BTreeMap<SecretName, bool>, EgressError>;
    /// 两段式写入，满足 config §2.4 的"secret 写入与 settings.updated 原子"；底层是 secrets.md 的 stage / commit / abort。
    async fn stage_secret(&self, req: StageSecret) -> Result<StagedSecret, EgressError>;   // {secret_ref, value}；待生效版本，对数据面不可见
    async fn activate_secret(&self, staged: &StagedSecret) -> Result<SecretVersion, EgressError>;  // 在 settings.updated 提交之后
    async fn discard_secret(&self, staged: &StagedSecret) -> Result<(), EgressError>;
    /// 启动对账用：列出暂存项及其年龄（超过 10 分钟仍无事件引用的暂存项被丢弃）。
    async fn staged_secrets(&self) -> Result<Vec<StagedInfo>, EgressError>;

    // ---- 用量拉取 ----
    /// models 的直接调用计量（按 x-gantry-call-id）与对账游标。
    async fn call_usage(&self, call_id: &CallId) -> Result<CallUsage, EgressError>;
    async fn usage_since(&self, cursor: UsageCursor, limit: u32) -> Result<UsagePage, EgressError>;
    /// 某枚 svc_token 的全部用量记录（吊销 / 轮换后对账，P-74）。
    async fn svc_token_usage(&self, id: &SvcTokenId, cursor: Option<UsageCursor>) -> Result<UsagePage, EgressError>;

    // ---- 调用级终结（B-01，interfaces/egress-call-terminal.md）----
    /// 按全局 call_id 关闭一次直接调用：置 draining、等全部 inflight 结束、计量落盘后返回 terminal 证明。
    /// 幂等；路径 token 须对该调用有已接受请求（零接收除外），否则 Conflict。
    async fn close_call(&self, svc_token_id: &SvcTokenId, call_id: &CallId, ctx: &ActorCtx) -> Result<CallClosed, EgressError>;
    /// 只读：该调用的全部记录与 terminal（未关闭为 None）；可重复读取。
    async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError>;

    async fn status(&self) -> Result<EgressStatus, EgressError>;
}

pub struct IssueJobToken {
    pub job_id: AttemptId, pub sandbox_id: SandboxId, pub project_id: ProjectId,
    pub delivery_id: Option<DeliveryId>, pub conversation_id: Option<ConversationId>,  // 二者恰有一个：交付台账或 None 档会话台账（P-58）
    pub tier: Tier,                       // planner | worker，用于按 tier 归集
    pub provider: ProviderId,             // 本 job 使用的 provider；沙盒内不支持 fallback（config §3.4）
    pub profile: ProfileName,
    pub groups: Vec<GroupName>,           // 创建时固化的分组集合；有效集合 = 此快照 ∩ 当前配置（config §2.6）
    pub tokens_total: u64,                // 封顶（加权口径）
    pub weights: TokenWeights,            // levels.token_weights 的生效值（P-70）：{input, output, cache_read, cache_write}
    pub wall_time_s: u64,                 // 令牌硬上限 = 签发时刻 + wall_time_s + 600 s（egress-control §3.1）
    pub config_rev: Seq,                  // weights 所属配置修订，写入该尝试用量记录的 weight_rev（egress-control §4.5）
}                                         // 会话台账的 day 取签发当天（UTC，P-58）
pub struct JobTokenGrant {
    pub job_token: JobToken,              // 不透明随机串（≥ 128 位熵）；Debug 脱敏；只写进沙盒环境，不进事件、日志
    pub model_base_urls: BTreeMap<ProviderId, Url>,   // http://<egress>/j/<job_token>/<provider_id>
    pub connect_proxy_url: Url,           // http://job:<job_token>@<egress>:3128（P-72）
    pub registry_routes: BTreeMap<InjectionId, Url>,  // M2
    pub effective_groups: Vec<GroupName>, pub issued_at: Timestamp, pub expires_at: Timestamp, pub reissued: bool,
}
pub struct SvcTokenGrant { pub svc_token_id: SvcTokenId, pub svc_token: SvcToken,
                           pub model_base_urls: BTreeMap<ProviderId, Url>, /* http://127.0.0.1:<port>/s/<svc_token>/<provider_id> */
                           pub expires_at: Timestamp, pub revoked_previous: Vec<SvcTokenId> }
pub struct EgressEndpoints { pub connect_proxy_host: Url, pub add_host: Option<(Hostname, IpAddr)> }

pub struct JobUsage { pub usage: TokenUsage /* input, output, cache_read, cache_write, requests */,
                      pub weighted_tokens: u64,          // 按 weights 加权（P-70）
                      pub capped: bool,                 // 因 tokens_total 切断过后续请求
                      pub incomplete: bool,             // 有请求在途时进程崩溃 / 用量不可解析 → 用量是估计或下界
                      pub upstream_errors: UpstreamErrorSummary, // {total_5xx, total_429, total_conn_errors, max_consecutive, last_at}
                      pub denied: DeniedSummary }       // 按规则分类的拒绝计数，供 engine 判断异常
pub struct FinalUsage { pub job_id: AttemptId, pub usage: TokenUsage, pub weighted_tokens: u64, pub capped: bool, pub incomplete: bool,
                        pub upstream_errors: UpstreamErrorSummary, pub revoked_at: Timestamp }
pub struct EgressStatus { pub applied_rev: Seq, pub health: Health /* ok | degraded | unhealthy */, pub detail: Option<String>,
                          pub unresolved_secret_refs: Vec<String>, pub active_job_tokens: u32 }
```

`status()` 的形状对应 [config §2.8](../interfaces/config.md#28-catalog-与-status-的响应结构)；设置状态字段统一为 `applied_rev`（C26）。

**调用顺序**（与 worker-protocol §3.2 一致）：`issue_job_token` → 建容器（令牌经 `ExecSpec.env` 写入沙盒）→ `stage` → 启动 → …… → `collect` → `revoke_job_token` → 销毁；`attempt.finished` 落库后 `forget_job`。

### 6.3 `models` 的直接调用约定（P-50、P-74）

`models` 经 `svc_model` 回环监听发普通的 Anthropic 风格 HTTP 请求，base URL 取自 `service_token()` 的 `model_base_urls`，即路径 `/s/<svc_token>/<provider_id>/v1/messages` 等（令牌在路径中，与 egress-control §3.2 一致），并带：

| 头 | 含义 |
| --- | --- |
| `X-Gantry-Ledger: <台账 ID>` | 归属的台账（必填）：交付台账 `dlv_…`，会话台账 `cnv_…:YYYY-MM-DD`（data-model 台账 ID）；无项目的全局调用为 `unattributed` |
| `x-gantry-tier: utility\|planner` | 计量归属的 tier（必填） |
| `x-gantry-call-id: <ULID>` | 调用 ID（必填）；`call_usage` 与 `usage_since` 以它为键 |

`svc_token` 与 `job_token` 同一计量口径（同一 `UsageParser`、同一加权），区别只在归属：前者按 `call_id` 与上述头归集到交付台账或会话台账（头来自服务端，视为可信，P-74），后者按尝试归集。缺任一必填头或格式不符 → `400 ledger_header_invalid`，不转发、不计量。这些头在转发给上游之前**全部剥离**。沙盒侧监听器收到的任何 `x-gantry-*` 头一律忽略并剥离；`svc_token` 在沙盒侧监听器上无效（只认回环监听），`job_token` 在 `svc_model` 上无效。Egress 在响应里回显 `x-gantry-call-id`。计量交接：Egress 每个请求完成即写用量记录（带 `weight_rev`）；`models` 在调用结束后先 `close_call` 关闭该 `call_id`（之后同一 `call_id` 的请求一律 `403 token_invalid`），再用 `call_terminal(call_id)` 取终结证明，校验通过且 `incomplete = false` 才记账 `budget.charged`（interfaces/egress-call-terminal.md），并周期性 `usage_since(cursor)` 对账；`svc_token` 被吊销（进程重启或轮换）后，`gantryd` 用 `svc_token_usage` 取该令牌的全部记录对账、补记遗漏——用量的事实源只有 Egress（P-22、P-74）。

### 6.4 沙盒侧可见的约定

- 模型 base URL：`http://<egress-host>:<port>/j/<job_token>/<provider_id>`（由 `issue_job_token` 给出，经 `ExecSpec.env` 设置，如 `ANTHROPIC_BASE_URL`；P-49）。Egress 校验令牌后去掉该前缀，拼到 `provider.base_url` 之后转发。令牌只对该尝试有效、尝试结束即作废（"只能从沙盒网段使用"的强制方式需核实，P-73）；沙盒即使读到也只能给自己的尝试记账。
- `HTTPS_PROXY` / `https_proxy`：`http://job:<job_token>@<egress-host>:<port>`（`CONNECT` 端点；`job_token` 经 `Proxy-Authorization` 携带，P-72；取自 `JobTokenGrant.connect_proxy_url`），值里不含任何上游凭据；`NO_PROXY=localhost,127.0.0.1`。
- 拒绝响应：HTTP 状态 + `application/json` 体 `{"type":"error","error":{"type":"<gantry_error_code>","message":"…"}}`（与模型 API 的错误体同形，便于 agent CLI 当作 API 错误处理）。`gantry_error_code` ∈ `token_invalid`、`not_allowed_host`、`hard_denied`、`path_not_allowed`、`method_not_allowed`、`budget_exhausted`、`rate_limited`、`credential_missing`、`upstream_unavailable`、`request_too_large`。拒绝类用 403（`budget_exhausted` 也用 403 而不是 429，避免 CLI 对 429 的自动重试放大请求量——对 CLI 的实际行为 **需核实**，OQ-E4）。
- `CONNECT` 不带 `Proxy-Authorization`：`407`，`Proxy-Authenticate: Basic realm="gantry-egress"` **[提议]**；被拒：`403`，`Connection: close`，体同上（egress-control §5）。

### 6.5 审计记录（JSON Lines，追加）

```json
{"seq":18234,"ts":"2026-10-24T12:41:03.120Z","kind":"model","caller":{"type":"job","job_id":"att_01JB…","tier":"worker"},
 "decision":"allow","rule":null,"method":"POST","host":"api.anthropic.com","port":443,"path":"/v1/messages",
 "status":200,"bytes_in":18211,"bytes_out":4102,"duration_ms":2310,"provider":"anthropic",
 "usage":{"input_tokens":3201,"output_tokens":410,"cache_read_tokens":2800,"cache_write_tokens":0},
 "src":"10.90.0.5","resolved_ip":"160.79.104.10","prev_hash":"sha256:…","error":null}
```

| 字段 | 说明 |
| --- | --- |
| `kind` | `model` / `connect` / `registry` / `control` |
| `caller` | `{type: "job", job_id, tier}` / `{type: "svc", tier, call_id}` / `{type: "unknown", src}`（令牌无效时，不记令牌本身） |
| `decision` / `rule` | `allow` / `deny`；规则 id：`hard.github`、`hard.ip_literal`、`hard.private_ip`、`hard.port`、`token.invalid`、`token.revoked`、`token.source_not_allowed`、`profile.not_in_groups`、`path.not_allowed`、`method.not_allowed`、`cap.tokens_exhausted`、`rate.limited`、`credential.missing`…… |
| 路径 | 只记路径，**不记查询串**；`CONNECT` 只记 `host:port` |
| 不记录 | 任何请求 / 响应头的值、请求 / 响应体、密钥、`Authorization`、`Cookie` |
| `prev_hash` | M3 的哈希链，`sha256(prev_record_bytes)`；M1 可留空 |

## 7. 内部设计

### 7.1 进程结构与并发

单进程、`tokio` 多线程运行时。每个监听器一个 accept 循环；每个连接一个任务；`Arc<JobState>` 保存每个 job 的原子计数与信号量。全局连接上限与**每令牌**连接上限（默认 1024 / 64 [建议值，待调优]），防止一个沙盒耗尽别人的容量。所有外部 I/O 都有超时；无阻塞调用进入运行时（DNS 用异步解析器）。

### 7.2 登记表与持久化

```rust
struct JobState {
    job_id: AttemptId, tier: Tier, provider: ProviderId, token_hash: Sha256Digest,   // 只存令牌哈希
    groups_snapshot: Vec<GroupName>, tokens_total: u64, weights: TokenWeights,
    used: AtomicUsage, capped: AtomicBool, incomplete: AtomicBool,
    inflight: Semaphore, errors: Mutex<UpstreamErrorSummary>, state: Active | Revoked,
}
```

- 令牌校验（P-49）：`job_token` 由本进程以 CSPRNG 生成（≥ 128 位熵），表里只存 `sha256(token)`，常量时间比较；令牌只在 `Active` 状态有效，`revoke_job_token` 后立即失效；请求来源地址必须属于沙盒网段（引导配置 `egress.sandbox_cidrs`），否则 `token.source_not_allowed`——这一项的强制方式**需核实**（P-73），不能可靠判定源地址时不做此检查，`status.detail` 标 `source_check: unverified`。`svc_token` 同样只存哈希，任一时刻至多一枚 `Active`（P-74）。一枚令牌只对应一个 job。
- 存储：`<data_dir>/egress.db`（SQLite，WAL）：`job_tokens`（令牌哈希与 job 元数据）、`svc_tokens`（令牌哈希、`issued_to`、状态）、`usage_jobs`、`usage_calls`、`final_usage`、`inflight`、`secret_versions`（元数据，值在 `secrets`）、`applied_config`（最后一次已应用的视图，**不含任何值**）。
- 用量在每个请求**完成时**同步提交（一次 WAL 提交）；请求**开始**时先在 `inflight` 写一行，完成后删除。重启时遗留的 `inflight` 行让对应 job 置 `incomplete = true`——用量是下界，**绝不静默丢失**。
- 重启后：载入 `job_tokens`、`svc_tokens` 与最后一次 `applied_config`，继续服务（两种令牌都不因 Egress 重启而失效，egress-control §8）；运行中的沙盒不受影响，归属不变。`gantryd` 重连后若自己的 config rev 更新则重发 `apply_config`。
- `final_usage` 在 `revoke_job_token` 时写入，`forget_job` 或 TTL 后删除，保证 `collect` 与 `attempt.finished` 之间崩溃的重放得到同一份数字。

### 7.3 模型 API 反向代理流水线

```
1  识别调用方   sandbox_model：路径 /j/<job_token>/ → JobState（Active）且来源属于沙盒网段（可强制时，P-73），否则 403 token_invalid（审计，rule=token.*，不记令牌）
              svc_model：回环来源 + 路径 /s/<svc_token>/ → 有效令牌，否则 403 token_invalid；必填头（X-Gantry-Ledger、x-gantry-tier、x-gantry-call-id）齐全，否则 400 ledger_header_invalid
2  路由         去掉令牌前缀后的 /<provider_id>/；provider 必须存在、凭据可解析（否则 503 credential_missing）；
              沙盒调用方：provider 必须等于登记的 provider，且其 base_url 主机属于有效分组 model_api（V11 的运行时再检）
3  方法 / 路径   按 provider kind 的白名单：Anthropic 为 POST /v1/messages、POST /v1/messages/count_tokens、GET /v1/models [提议，需核实当前端点]；
              规范化后拒绝 `..`、`//`、编码的斜杠与点、绝对形式 URI、非白名单路径（如 /v1/files、组织管理类端点）→ 403 path_not_allowed
              —— 白名单的原因：注入的 key 权限可能大于"调模型"，不能让沙盒借它访问文件上传 / 管理端点
4  限量         请求头 ≤ 16 KiB；请求体 ≤ 32 MiB [建议值，待调优]；读超时；CL/TE 冲突、重复 Content-Length → 400
5  封顶 / 限速   job：加权用量（levels.token_weights，P-70）≥ tokens_total → 403 budget_exhausted，置 capped；单 job 在途请求 ≤ 8、每分钟 ≤ 120 [建议值，待调优]，否则 429 rate_limited
6  构造上游请求  URL = provider.base_url + 去前缀后的路径；只复制白名单头（content-type、accept、anthropic-version、anthropic-beta [需核实头名]），
              User-Agent 设为固定值；丢弃 authorization、x-api-key、cookie、proxy-*、x-gantry-*、host 等；注入 provider 的鉴权头（Anthropic：x-api-key）
7  发送         rustls，证书与主机名验证（系统根证书），SNI = provider 主机，不跟随重定向，不降级到明文；连接 / 首字节 / 空闲超时
8  响应         流式透传（不缓冲整个响应），同时旁路解析用量；响应头去逐跳头与 set-cookie 及任何可能回显凭据的头
9  收尾         完成或中断：把解析到的用量累加进 JobState 并提交（§7.2）；写审计；上游 429 / 5xx / 连接错误累加到 upstream_errors
```

### 7.4 计量

按 provider kind 的 `UsageParser`（第一版只有 Anthropic；字段名 **需核实当前 API**）：

| 来源 | 提取 |
| --- | --- |
| 非流式 JSON | 顶层 `usage`：`input_tokens`、`output_tokens`、`cache_creation_input_tokens` → `cache_write_tokens`、`cache_read_input_tokens` → `cache_read_tokens` |
| SSE 流 | `message_start` 里的 `message.usage`（输入与缓存类，及初始输出数）；`message_delta` 里的 `usage.output_tokens`（累计值，取最后一条）；`message_stop` 结束 |
| `count_tokens` | 不计 token，计入请求数 |

- 解析有界：单行 / 单个 JSON 上限 1 MiB；解析不了或流被截断时，按保守估计计入（`ceil(请求字节/3) + ceil(已转发响应字节/3)` **[提议]**）并置 `incomplete = true`，使"让用量解析失败"不能成为绕过封顶的手段（OQ-E4）。
- `requests`：返回了 2xx 并有用量的请求数 **[提议]**；失败请求只进 `upstream_errors` 与审计。
- 封顶：新请求到达时若 `Σ weight × tokens ≥ tokens_total` 则拒绝（E4："切断后续模型请求"）；在途请求允许完成，越界量最多是一个请求的上限（硬切断是否包括在途流，OQ-E4）。`capped` 一经置位不再清除。
- 权重取 config `levels.token_weights`（`input`、`output`、`cache_read`、`cache_write`，默认值为建议值，待按价格调优；P-70），经 `IssueJobToken.weights` 传入（沙盒）或由 `apply_config` 的视图带入（`svc_token` 调用）；`JobUsage` 同时给出原始四类计数与 `weighted_tokens`。
- 自报用量（沙盒的头或 stdout）完全不参与计量；与计量显著偏离由 `sandbox` / `engine` 记审计，不是本模块的事。

### 7.5 `CONNECT` 流水线

```
1  读请求行与头（≤ 8 KiB，10 s 超时）。方法必须是 CONNECT，否则 405（含绝对 URI 的明文代理请求）
2  识别调用方：`Proxy-Authorization`（Basic，用户名 `job`）中的 `job_token`（P-72），校验同 §7.3 步 1；缺失 → 407，无效 → 403 `token_invalid`
3  解析 authority 严格：拒绝 userinfo、方括号 IPv6 字面量、IP 字面量（含十进制 / 八进制 / 十六进制 / IPv4 映射写法）；端口必须是 443
4  规范化主机：小写、去尾部点、IDNA → punycode（非法 → 拒绝）；规范化后只允许 [a-z0-9.-]
5  硬性拒绝表（§7.7）：命中 → 403（rule=hard.*）
6  档案：主机必须匹配 job 的有效分组（快照 ∩ 当前配置）中的某个模式，否则 403 profile.not_in_groups
7  若主机属于某个 egress.injection 的 hosts（私有仓库）：拒绝 CONNECT，必须走反向代理路由 [提议]（避免绕过注入后泄漏行为不一致）
8  自己解析主机的全部 A / AAAA：任一落点在拒绝网段 → 403 hard.private_ip；否则选定一个落点，用该 IP 建连（钉住，不二次解析，防 DNS 重绑定）
9  回 200，双向拼接；空闲超时 60 s、总时长 30 min、单隧道字节上限与 job 累计字节上限 [建议值，待调优，OQ-E6]
10 关闭时写审计（字节数、时长）；拒绝同样写审计
```

### 7.6 私有包仓库反向代理（M2）

`/r/<job_token>/<injection_id>/<rest>`：令牌校验同 §7.3 步 1；只允许 `GET` / `HEAD`（拒绝发布类方法，防止借私有仓库外泄 [提议]）；上游主机固定为 `injection.hosts` 的主机，且必须在有效分组内（V13）；丢弃调用方的认证头，按 `kind`（`bearer` / `header`）注入凭据；**不跟随重定向**，重定向响应原样返回（调用方若跟随，会经 `CONNECT` 走白名单、且不带凭据）；响应不回显凭据；审计记录路径（不含查询串）。包管理器如何被指向这些路由（npm / pip / cargo 的配置注入点）见 OQ-E7。

### 7.7 硬性拒绝表与主机匹配

**GitHub 域名族**（I1；config §5.5 把完整硬编码表交给本规格书）**[提议，完整性 需核实]**。拒绝模式对**任意层级子域**生效（与允许模式的单层通配不同）：

`github.com`、`*.github.com`（含 `api.`、`gist.`、`codeload.`、`uploads.`、`pkg.`、`npm.pkg.`、`maven.pkg.`）、`githubusercontent.com`、`*.githubusercontent.com`（含 `raw.`、`objects.`、`avatars.`）、`githubassets.com`、`*.githubassets.com`、`github.io`、`*.github.io`、`ghcr.io`、`*.ghcr.io`、`githubapp.com`、`*.githubapp.com`、`github.dev`、`*.github.dev`、`githubcopilot.com`、`*.githubcopilot.com`、`git.io`。

**网段**（对 DNS 解析结果与 IP 字面量均适用）：`0.0.0.0/8`、`10.0.0.0/8`、`100.64.0.0/10`、`127.0.0.0/8`、`169.254.0.0/16`（含云元数据 `169.254.169.254`）、`172.16.0.0/12`、`192.0.0.0/24`、`192.168.0.0/16`、`198.18.0.0/15`、`224.0.0.0/4`、`240.0.0.0/4`；`::/128`、`::1/128`、`::ffff:0:0/96`（IPv4 映射，按内嵌的 IPv4 再判）、`64:ff9b::/96`（NAT64，按内嵌 IPv4 再判）、`fc00::/7`、`fe80::/10`、`ff00::/8`；以及 Egress 自己的监听地址与沙盒网络网段。

**其他**：端口只允许 443；裸 `*` 与多层通配不是合法分组条目（配置层 V10 + 运行时再检）。

**允许模式匹配**：`example.com` 精确匹配；`*.example.com` 只匹配**恰好多一层标签**的子域，不匹配 `example.com` 本身、`evilexample.com`、`example.com.evil.com`，也不匹配 `a.b.example.com` **[提议，"一层前缀通配"的解释待确认，OQ-E8]**。匹配在规范化之后（§7.5 步 4）进行。

### 7.8 审计日志

- 追加写入 `<data_dir>/audit/YYYY-MM-DD.jsonl`（按日轮转），权限仅 `gantry-egress`；`seq` 单调；M3 加 `prev_hash` 哈希链并提供只读回放接口（按 `job_id` / 时间窗取出有序记录）。
- **不可关闭**（config §8.3）：没有任何设置或开关能停掉它。审计写入失败（磁盘满等）→ 失败闭合：拒绝新请求并置 `health = unhealthy` **[提议，OQ-E10]**。
- 保留期未定（config OQ-21）。不记录头值、请求 / 响应体、密钥；路径不含查询串。

### 7.9 配置应用与热加载

`EgressConfigView = {rev, groups, profiles, default_profile, injection(含 secret_ref，无值), providers{id: {kind, base_url, secret_ref}}}`，来自 `gantryd` 的 `apply_config`。流程：防御性复核（V10：分组无 GitHub 域名族 / IP 字面量 / 裸 `*`；V11：provider 主机属于 `model_api`；V13：注入主机属于某分组、`(host, header)` 唯一、`header` 不是逐跳 / 框架头）→ 通过则原子替换内存视图并持久化，`applied_rev = rev`；失败则保留旧视图并 `health = degraded`、`detail` 写原因。

- 无任何已应用视图时：**拒绝一切**（失败闭合），`health = unhealthy`。
- 有效白名单 = **登记时快照 ∩ 当前配置**（config §2.6）：收紧对存量沙盒的下一次请求 / 下一个 `CONNECT` 立即生效，放宽只对之后登记的沙盒生效。
- 模型 API 的反向代理路由由 `models.providers` 自动派生（config §8.1），不在 `egress.injection` 里重复。
- 凭据缺失（`secret_ref` 在密钥库里找不到）：不崩溃，该 provider / 路由置不可用（503 `credential_missing`），`unresolved_secret_refs` 列出。

### 7.10 密钥处理

- 值只存在于 `secrets` 的本进程实例（加密落盘，仅 `gantry-egress` 用户可读）与进程内存；用 `zeroize` 包装类型，`Debug` / `Display` 不输出值；从不经环境变量、命令行或配置文件传入。
- 写入路径（只写不读，P-50）：`api` 在 `PUT /v1/settings/{resource}` 里把 `SecretRef.value` 经 `stage_secret` 转交（得到 `StagedSecret`，对读者不可见）→ `api` 追加 `settings.updated` → `activate_secret`（失败则 `discard_secret`）。未激活的暂存值由超时回收，`api` 启动时用 `staged_secrets()` 对账。`api` 不留存值。路由为 `egress.sock` 上的 `/control/secrets/*`（以 egress-control.md 为准）；控制面没有任何读取 secret 值的路由，只有 `secret_present`。
- 轮换（M3）：同名 ref 写入新值，`version` 递增；新请求用新版本，在途请求用旧版本完成；旧版本短暂保留后删除。
- 注入只发往：`models.providers` 的上游主机（模型 key）与 `injection.<id>.hosts`（私有仓库凭据）；凭据**从不**出现在发往沙盒 / `models` 的任何响应（含错误体）、审计、日志、指标、崩溃转储中。

### 7.11 进程加固（`deploy` 落地，本文列要求）

专用系统用户 `gantry-egress`，与 `gantry`、`gantry-sbx` 不共享文件；`NoNewPrivileges`、`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、`PrivateDevices`、`CapabilityBoundingSet=`（空，监听端口 ≥ 1024）、`RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`、`SystemCallFilter=@system-service`、`MemoryDenyWriteExecute`、`LimitCORE=0`，仅 `ReadWritePaths` 自己的数据 / 运行目录；进程内 `prctl(PR_SET_DUMPABLE, 0)`；日志层对 `authorization`、`x-api-key`、`cookie` 等头名做强制脱敏（类型层面不提供打印头值的路径）。

### 7.12 `job_token` 的安全性质（P-49）

- 取代 P-16 中"沙盒网络身份"的说法：归属只来自令牌，不再要求 Egress 观察到各沙盒唯一的源地址；"来源属于沙盒网段"是附加条件，防止令牌被拿到沙盒外使用。
- 令牌只对一个尝试有效：沙盒读到自己的令牌（它就在自己的环境变量里）也只能给自己的尝试记账，不能冒充别的尝试、不能读取任何 key、不能访问控制面。
- 尝试结束（`revoke_job_token`）即作废；令牌不进事件日志、审计、日志与指标（审计只记 `job_id`）。
- 测试用不同令牌扮演不同"沙盒"，环回地址扮演沙盒网段即可在不起容器的情况下覆盖归属逻辑（§12.A）。

## 8. 不变式与安全要求

| 不变式 | 本模块如何保证 |
| --- | --- |
| **I1** 沙盒只计算 | 沙盒网络只通 Egress；`github.com` / `api.github.com` 等被硬性拒绝（配置层 V10 与运行时双层）；沙盒内无任何 key——模型 key 只在本进程，请求头里的认证信息被丢弃并替换；沙盒里的 `job_token` 只是本尝试的记账凭证；沙盒到服务端没有 socket：控制面 socket 不挂进沙盒，`svc_model` 只监听回环 |
| **P-16** 反向代理 / CONNECT 两种模式 | 模型 API 与私有包仓库反向代理（注入、计量）；公共包仓库 `CONNECT`（不注入）；所有模型调用（沙盒与 `models`）都经此，计量只有一个事实源 |
| **I5** 外部文本不可信 | 沙盒请求的任何头 / 路径 / 体都当不可信：白名单化转发、规范化后匹配、不据此提升权限；上游响应只提取用量字段，其余透传；错误体与审计里的文本去控制符 |
| **I6** 客户端不持凭据 | 凭据只经 `api` 转交一次、存于本进程；任何读路由、事件、日志都不含值（config §2.4） |
| **I7** 配置与工作分离 | 只接收 `gantryd` 下发的设置视图，不接受其他来源的规则变更；没有从沙盒或聊天修改白名单的路径 |
| **E4** 不信任沙盒自报 | 归属只来自服务端签发的 `job_token`（P-49），不来自头；`x-gantry-*` 在沙盒侧监听器上被忽略并剥离；自报用量不进计量 |
| 失败闭合（W8） | 无配置、非法配置、凭据缺失、审计写失败、令牌无效 / 已吊销 / 非沙盒网段、路径不在白名单——一律拒绝 |

附加要求：

- 注入的凭据只发往对应的固定上游主机，且仅在 TLS 连接（证书与主机名验证通过）上发送；不跟随重定向，因此凭据不会被重定向带到别的主机。
- 上游主机永远来自配置（provider / injection），**不**来自请求的 `Host`、绝对 URI 或任何头。
- 沙盒不能借 `CONNECT` 到达 `egress` 自己的控制面 / 监听地址或宿主内网（G5 的网段与自身地址拒绝）。
- 响应里不得出现 key：错误体、调试输出、`Via` / `X-Forwarded-*` 头都不含凭据；崩溃 / panic 信息经脱敏。
- `CONNECT` 白名单本身是低带宽外泄通道（URL 路径、查询串、发布操作；worker-protocol OQ-15）；本模块用每 job 字节上限、审计、私有仓库只读缓解，不能消除（OQ-E6）。

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 无配置 / 配置非法 | 拒绝一切（无配置）或保留旧视图并 `degraded`（非法更新）；`status` 如实报告 |
| 凭据缺失 / 解密失败 | 该 provider / 路由 503 `credential_missing`；其余不受影响；`unresolved_secret_refs` 列出 |
| 上游 TLS 失败 / 超时 / 连接错误 | 502 / 504，计入 `upstream_errors`；不重试、不换 provider |
| 上游 429 / 5xx | 原样透传，计入 `upstream_errors`；沙盒内 CLI 自己退避；`engine` 据摘要决定 `exec.model_api` |
| 无效 / 已吊销 / 非沙盒网段的 `job_token` | 403 `token_invalid`，审计 `token.*`（M3 作为逃逸检测信号） |
| 用量解析失败 | 保守估计计入，`incomplete = true` |
| 进程崩溃 | 重启后载入令牌表与用量；`job_token` 与 `svc_token` 继续有效（登记表持久化）；遗留 `inflight` 使对应 job `incomplete = true`；沙盒内在途连接断开，由 CLI 重试 |
| `gantryd` 不可达 | 数据面照常服务已签发令牌的 job；新签发不可能发生；控制面请求在其恢复后补做；`status` 不变 |
| 审计写失败 | 拒绝新请求并 `unhealthy`（OQ-E10） |
| `issue_job_token` 失败 | `sandbox` 归 `infra.egress`，新沙盒重试 |
| `revoke_job_token` 对未知 job | 若 `final_usage` 仍在则返回之；否则 `NotFound`（`sandbox` 视为 `infra.egress` 并用零用量标 `incomplete`） |
| 磁盘 / 内存不足 | 拒绝新连接，`unhealthy`；不降低任何校验 |

## 10. 配置

设置项（`egress` 只读这一份视图，不读其他键；[config §8](../interfaces/config.md#8-egress) 与 `models.providers`）：

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `egress.groups.<name>.hosts[]` | 内置建议分组见 config §8.2（`model_api` `npm` `pypi` `crates` `go` `maven`） | 白名单分组；只放行 443 |
| `egress.profiles.<name>.groups[]` | `default` | 档案 = 分组集合；沙盒创建时固化 |
| `egress.default_profile` | `default` | M1 的 `default` 可只含 `model_api` |
| `egress.injection.<id>` | 无 | 私有包仓库反向代理（M2）：`hosts[]`、`kind`、`header?`、`credential` |
| `models.providers.<id>` | 无 | 自动派生模型 API 路由：`kind`、`base_url`、`credential` |

引导配置（`deploy` 拥有，文件、重启生效；config §1.1）**[提议]**：

| 项 | 默认 | 说明 |
| --- | --- | --- |
| `egress.data_dir` | `/var/lib/gantry-egress` | `egress.db`、`audit/`、`secrets/` |
| `egress.control_socket` | `/run/gantry/egress.sock` | 控制面套接字（P-50；目录 / 套接字权限见 deploy.md §6.3，组 `gantry-link`） |
| `egress.listen.svc_model` | `127.0.0.1:8082`（egress-control §3.2） | `models` 用的回环反向代理（P-50） |
| `egress.sandbox_cidrs` | 由 `deploy` 按沙盒网络给出 | `job_token` 只接受来自这些网段的请求（P-49） |
| `egress.listen.sandbox_model` / `sandbox_connect` / `sandbox_registry` | `<EGRESS_ADDR>:8081` / `<EGRESS_ADDR>:3128` / 由 `deploy` 分配（M2） | 沙盒侧监听地址（取自 deploy.md §7.8 的 `nft` 规则） |
| `egress.limits.max_connections` / `per_token_connections` | 1024 / 64 | [建议值，待调优] |
| `egress.limits.request_body_bytes` / `request_header_bytes` | 32 MiB / 16 KiB | [建议值，待调优] |
| `egress.limits.job_inflight` / `job_requests_per_min` | 8 / 120 | [建议值，待调优] |
| `egress.limits.tunnel_idle_s` / `tunnel_max_s` / `tunnel_bytes` / `job_tunnel_bytes` | 60 / 1800 / 1 GiB / 4 GiB | [建议值，待调优]，OQ-E6 |
| `egress.final_usage_ttl_s` | 86400 | `forget_job` 之前最终用量的保留期 |
| `egress.audit.retention_days` | 未定 | 对应 config OQ-21 |

## 11. 可观测性

`egress` **不写事件日志**；`gantryd` 经控制面拉取：`sandbox` 在 `collect` 后取 `FinalUsage`（`revoke_job_token`），`engine` 据此写 `attempt.finished.usage` 与 `budget.charged` / `budget.exhausted`；`models` 经 `close_call` + `call_terminal` 记自己的 `budget.charged`（交付台账或会话台账）；`api` 经 `status()` 提供 `GET /v1/settings/status` 的 `modules.egress`。数据模型事件目录无需新增。

日志（结构化，字段 `job_id`、`kind`、`rule`、`status`）：不含头值与体；拒绝与异常带规则 id；密钥操作只记 `ref` 与 `version`。

指标：`egress_requests_total{kind,decision,rule}`、`egress_request_duration_seconds{kind}`、`egress_tokens_total{tier,type}`、`egress_capped_total`、`egress_upstream_errors_total{class}`、`egress_connect_tunnels_active`、`egress_tunnel_bytes_total{direction}`、`egress_job_tokens_active`、`egress_token_invalid_total{reason}`、`egress_config_applied_rev`、`egress_unresolved_secret_refs`、`egress_audit_write_failures_total`、`egress_inflight_recovered_total`。

## 12. 测试与验收

测试替身：

- `MockUpstream`：本地 TLS 服务（测试 CA 注入 Egress 的信任库），可脚本化：JSON 响应与 SSE 流（带 `usage` 的各种组合）、429 / 5xx、慢速流、中途断连、重定向、畸形用量；**带外**记录收到的请求头集合（绝不在响应里回显，以免哨兵泄露被测试自己造成）。
- `TestSandboxClient`：带指定 `job_token`、从指定源地址发起连接的原始 TCP 客户端。用不同令牌扮演不同沙盒、用环回别名扮演沙盒网段与非沙盒网段，使归属逻辑**不需要容器**就能完整测试。
- `FakeResolver`（可注入 DNS 应答，含"先公网后私网"的重绑定序列）、`MemorySecrets`（内存密钥库，M0 替身，P-81）、`FakeClock`（`tokio::time::pause`）、`FakeControlClient`（`gantryd` 一侧的调用方）。`EgressStub`（M0，`contract` 泳道交付，P-51）是**给其他模块用的**替身，本模块的一致性用例约束它与真实控制面的行为一致。
- 集成：`[集成:podman]`（与 `sandbox` 泳道共用，真实沙盒访问真实 `egress`）。

### A. `job_token` 与归属（M1，P-49）

- [ ] 无令牌、伪造令牌、已吊销令牌访问三个沙盒侧入口（模型 / `CONNECT` / 私有仓库）都得到 403 `token_invalid`，且各产生一条审计记录（审计里不出现令牌本身）。
- [ ] 签发后带该令牌的请求归属该 job；两个 job 并发（不同令牌）各自发请求，用量互不串、互不影响封顶。
- [ ] 有效令牌从非沙盒网段使用 → 403（`token.source_not_allowed`）；`svc_token` 用在沙盒侧监听器、`job_token` 用在 `svc_model` → 403。
- [ ] 沙盒侧请求带 `x-gantry-caller`、`x-gantry-job`、`x-gantry-call-id` 等头：被忽略、被剥离，归属不变（上游收到的头集合里没有它们）。
- [ ] `issue_job_token` 对同一 `job_id` 幂等；`egress.db` 里只有令牌哈希，没有令牌明文。
- [ ] `revoke_job_token` 后同一令牌立即 403；`revoke_job_token` 重复调用返回逐字段相同的 `FinalUsage`；`forget_job` 后再 `revoke_job_token` → `NotFound`；TTL 到期自动遗忘（假时钟）。
- [ ] 重启 `egress` 进程：令牌、用量、封顶状态恢复，运行中的 job 继续被正确归属；重启时在途的请求使该 job `incomplete = true`；`svc_token` 继续有效（登记表持久化）；`gantryd` 重启后签发新 `svc_token` → 旧令牌 403，`revoked_previous` 列出它（P-74）。
- [ ] 签发时的 provider 与路径中的 provider 不一致 → 403。

### B. 模型 API 反向代理（M1）

- [ ] 调用方带的 `Authorization`、`x-api-key`、`Cookie` 被丢弃，`MockUpstream` 带外记录到的是**注入的真实 key**，且上游收到的头集合恰等于"白名单头 + 注入的鉴权头"。
- [ ] **M1 退出标准（密钥侧）**：密钥库里的哨兵 `sk-gantry-sentinel-…` 从不出现在——发往沙盒 / `models` 的任何响应（头、体、错误体）、审计文件、日志、指标输出、`egress.db`（明文）、`/proc/<pid>/environ`、命令行；控制面没有任何返回 secret 值的路由（只有 `secret_present`）；对 `egress` 数据目录（除加密的密钥库外）与进程内存转储做哨兵扫描。
- [ ] 路径 / 方法白名单：`/v1/files`、组织管理类路径、`/v1/../x`、`//`、`%2e%2e`、`%2f`、绝对形式 URI、错误方法、非 provider 前缀——全部 403 / 404 且**不转发**（`MockUpstream` 无记录）。
- [ ] 请求头 / 体大小上限、慢速发送（slowloris）超时、`Content-Length` 与 `Transfer-Encoding` 冲突 / 重复 → 4xx 并断开。
- [ ] SSE 流式透传：首字节延迟不依赖整个响应完成（对慢速流断言）；客户端中途断开 → 上游连接被取消；上游中途断开 → 调用方得到截断的流并计入 `incomplete` 或保守估计。
- [ ] 响应头清洗：逐跳头、`set-cookie` 以及任何回显凭据的头不会到达调用方。
- [ ] 上游证书错误 / 主机名不符 → 502 且**不降级**；上游重定向不被跟随（原样返回，且不带凭据）。
- [ ] provider 的密钥缺失 → 503 `credential_missing`，进程不崩溃，`status.unresolved_secret_refs` 列出该 ref，其他 provider 正常。
- [ ] 上游 429 / 5xx 原样透传，`upstream_errors` 的计数与 `max_consecutive` 正确。
- [ ] 单 job 在途请求 / 每分钟请求超限 → 429 `rate_limited`，其他 job 不受影响。

### C. 计量与封顶（M1）

- [ ] JSON 与 SSE 两种响应的用量与 `MockUpstream` 给出的数字**逐项精确相等**：四类 token 分别记录，`cache_creation` 映射为 `cache_write`；多请求累加；`count_tokens` 不计 token。
- [ ] 封顶：累计恰为 `tokens_total - 1` 时下一个请求放行，达到 `tokens_total` 后的**后续**请求 403 `budget_exhausted`、`capped = true`，在途请求完成；按 `levels.token_weights` 的非 1 权重时按加权值封顶，`weighted_tokens` 与手算一致（P-70）。
- [ ] 用量不可解析 / 流被截断 → 按保守估计计入并 `incomplete = true`（不会因此绕过封顶）。
- [ ] 沙盒伪造 `x-gantry-usage` 之类的头或 stdout 事件，不影响计量（计量只来自上游响应）。
- [ ] `svc_model` 回环监听：路径无有效 `svc_token` → 403 `token_invalid`；缺 `X-Gantry-Ledger` / `x-gantry-tier` / `x-gantry-call-id` → 400 `ledger_header_invalid` 且无用量记录；非回环来源不可达；`X-Gantry-Ledger`（`dlv_…`、`cnv_…:YYYY-MM-DD`、`unattributed`）原样进入 `CallUsage` 归属（P-74）；签发第二枚 `svc_token` 后第一枚 403，`svc_token_usage` 返回第一枚的全部记录；`call_usage(call_id)` 与 mock 数字一致；`close_call` 之后同一 `call_id` 的请求 403、无新记录，重复关闭返回同一 `terminal`，`call_terminal` 可重复读取且 `terminal.verify(call_id, &items)` 通过；控制面 `egress.sock` 对端 uid 不是 `gantry` → 拒绝；`usage_since(cursor)` 翻页无遗漏、无重复；同一 `call_id` 重复调用不重复计量（幂等）。
- [ ] 崩溃一致：在"响应完成"与"用量提交"之间 `kill -9`，重启后该请求要么已计入、要么使 job `incomplete`，**绝不静默丢失**。

### D. `CONNECT` 白名单与硬性拒绝（M1）

- [ ] 档案内主机（`MockTLS` 服务）成功建立隧道并转发字节；审计含 `job_id`、`host`、`bytes_in/out`、时长。
- [ ] **M1 退出标准（拒绝侧）**：对 `github.com`、`GitHub.COM`、`github.com.`（尾部点）、`api.github.com`、`x.y.github.com`、`raw.githubusercontent.com`、`objects.githubusercontent.com`、`ghcr.io`、`foo.github.io`、punycode / IDN 混淆写法、`a@github.com:443`（userinfo）——全部 403 `hard.github`，并写带 `job_id` 的审计。
- [ ] IP 字面量（IPv4、IPv6、十进制 / 八进制 / 十六进制写法、IPv4 映射 IPv6）→ 403 `hard.ip_literal`；端口 22 / 80 / 8443 → 403 `hard.port`。
- [ ] DNS 重绑定：`FakeResolver` 第一次返回公网地址、之后返回私网地址——实际建连使用的是第一次校验过的 IP（钉住）；解析结果含回环 / 私网 / 链路本地 / `169.254.169.254` / `egress` 自身地址 / 沙盒网段中的任一个 → 403 `hard.private_ip`；IPv4 映射与 NAT64 内嵌私网地址同样被拒。
- [ ] 通配符：`*.example.com` 不匹配 `example.com`、`evilexample.com`、`example.com.evil.com`；对 `a.b.example.com` 的结果符合 OQ-E8 的最终决定。
- [ ] 不在有效分组内的主机 → 403 `profile.not_in_groups`；对项目收窄后的档案：有效集合 = 快照 ∩ 当前配置。
- [ ] 非 `CONNECT` 方法、绝对 URI 的明文代理请求 → 405；注入主机的 `CONNECT` → 403。
- [ ] 隧道空闲 / 总时长 / 字节上限生效；单源连接数上限使一个沙盒无法耗尽别人的容量。
- [ ] 解析与建连过程中 `egress` 不向任何 GitHub 地址发起连接（对 `FakeResolver` 的查询记录与连接记录断言）。

### E. 私有包仓库反向代理（M2）

- [ ] 仅 `GET` / `HEAD`；`PUT` / `POST` / `DELETE` → 405；凭据只注入列出的主机，调用方带的认证头被丢弃。
- [ ] 上游重定向不被跟随且重定向响应里不含凭据；响应不回显凭据；审计只记路径不记查询串。
- [ ] `injection.hosts` 不在任何分组 → 配置被拒（V13）；同一 `(host, header)` 重复 → 被拒。

### F. 配置与热加载（M1）

- [ ] 无已应用视图启动 → 拒绝一切，`health = unhealthy`；`apply_config` 后恢复。
- [ ] 非法视图（分组含 `api.github.com`、IP 字面量、裸 `*`、provider 主机不在 `model_api`、注入主机不在分组）→ 拒绝，旧视图保留，`degraded` 与 `detail` 准确。
- [ ] 收紧立即作用于已登记的沙盒（下一次请求 / `CONNECT`）；放宽只作用于之后登记的沙盒。
- [ ] `status()` 的 `applied_rev` / `health` / `unresolved_secret_refs` 形状符合 config §2.8（C26）。
- [ ] 进程重启后用持久化的最后视图继续服务，`applied_rev` 不回退。

### G. 密钥（M1；轮换 M3）

- [ ] `stage_secret` → `activate_secret` 之前新值不可见；`discard_secret` 后无残留；`secret_present` 与 `staged_secrets` 的结果与实际一致；超时未激活的暂存值被回收。
- [ ] 密钥库目录与文件权限：仅 `gantry-egress` 可读；`gantry` 用户读取失败（文件模式断言）。
- [ ] panic / 错误路径 / 调试输出不含密钥（注入故障后扫描日志）；核心转储被禁用。
- [ ] （M3）轮换：新版本对新请求生效，在途请求用旧版本完成，旧版本随后被清除；`secret_changes` 对应的 `version` 递增。

### H. 审计（M1；回放 M3）

- [ ] 每个允许与拒绝都有恰好一条记录，含 `job_id`（或 `models`）、规则 id、方法 / 主机 / 路径（无查询串）、字节数、耗时、状态；记录里没有任何头值、体、密钥。
- [ ] 审计不可关闭：不存在会停止写审计的设置；审计写失败 → 拒绝新请求并 `unhealthy`。
- [ ] （M3）回放接口按 `job_id` / 时间窗取出有序记录；哈希链校验通过，篡改任一记录即检出。

### I. 进程与加固（M1）

- [ ] 以非特权用户运行，无任何 capability；`deploy` 的 systemd 单元包含 §7.11 列出的加固项（单元文件测试）。
- [ ] 沙盒侧监听器上的令牌只接受沙盒网段来源；`/run/gantry/egress.sock` 为 `0660`、组 `gantry-link`，`gantry-sbx` 用户无法连接；套接字不在任何沙盒挂载内（`sandbox` 的 `validate_invariants` 也断言）。
- [ ] 无法读取 `gantryd` 的状态目录与 `secrets/`。

### J. 与退出标准的对应

- [ ] M1 "沙盒内访问 `github.com` 和 `api.github.com` 失败"：§12.D 的拒绝用例 + 与 `sandbox` 的 `[集成:podman]` 用例联跑（真实沙盒中直连与经代理均失败，且 `egress` 审计里能按 `job_id` 查到被拒的 `CONNECT`）。
- [ ] M1 "沙盒内读不到任何模型 key"：§12.B 的哨兵用例 + `sandbox` 的 `[集成:podman]` 全文件系统搜索用例联跑。
- [ ] M1 "出网日志"：§12.H。
- [ ] M3 "出网审计回放"：§12.H 的回放项。

## 13. 开放问题

| # | 问题 | 影响 | 本文暂用 |
| --- | --- | --- | --- |
| OQ-E1 | **已决：见裁决 C3 / P-49**（每个尝试一枚 `job_token`，放在模型 API base URL 路径中，只对该尝试有效、尝试结束即作废、只能从沙盒网段使用；取代"沙盒网络身份"）。原问题：**沙盒网络身份**：契约要求按"登记时绑定的源地址"映射 job（E4），但 rootless Podman 下 `egress` 未必能看到每个沙盒的唯一源地址；deploy.md §7.8 的主机防火墙按发起**用户**过滤，只能保证"沙盒只能到 `8081` / `3128`"，区分不了沙盒（它自己也标"需核实"，OPS-11）。候选方案见 sandbox.md §7.4（特权辅助建网 / `egress` 入驻 rootless netns / 每 job 端口 / 每 job Unix socket / 每 job 能力令牌 / 按发起进程识别）；其中"能力令牌"放在占位 key 与代理凭据里，对本模块最省事，但偏离 P-16 / E4 的"网络身份"字面，需用户确认。M1 退出标准的前置依赖 | `egress`、`sandbox`、`deploy` | 已落实到 §6.1、§7.2、§7.12；`CONNECT` 的令牌携带方式已决（P-72，`Proxy-Authorization`）；网段限制的强制方式标"需核实"（P-73） |
| OQ-E2 | **已决：见裁决 C4 / P-50**（新增 `interfaces/egress-control.md`；控制面 `/run/gantry/egress.sock`；`models` 走回环 HTTP 反向代理并用 `svc_token`；不再使用 `models_uds`）。原问题：**控制面与 `models` 通道仍缺契约**：interfaces/ 里没有 `gantryd` ↔ `egress` 的契约。几份同批规格各自留了缺口——deploy.md OPS-10（套接字 `egress.sock`、settings 视图怎么给 Egress）、models.md OQ-2（`models` 的身份与 `call_id`、`UsageMeter`）、secrets.md OQ-1 与 api.md OQ-A6（跨进程转交与两阶段激活）。本文 §6.1–§6.3 给出一个把三者收拢到 `egress.sock` 的提议；需要一份契约（建议 `interfaces/egress-control.md` + `schemas/egress/`）。`egress` 是独立进程，不写事件日志 | `egress`、`sandbox`、`models`、`api`、`secrets`、M0 冻结 | 已落实到 §6.1–§6.3；`svc_token` 的粒度与归属已决（P-74：每进程一枚，`X-Gantry-Ledger` 头） |
| OQ-E3 | `egress` 如何拿到设置视图并回报 `applied_rev`：由 `gantryd` 推送（本文，对应 deploy.md OPS-10 的第二问）还是 `egress` 自己读事件日志；architecture §4 的依赖列为 "—" 而依赖图有 `secrets → egress`；`egress.db` 不在 deploy.md §6.3 的目录表里（需补，并决定是否备份——登记与用量对运行中的 job 有意义，对已结束的可重建） | 依赖与可部署性 | 推送；`/var/lib/gantry-egress/egress.db`；字段名以 config §2.8 为准（api.md OQ-A4 已采用） |
| OQ-E4 | （① **已决：见裁决 C24 / P-70**——权重取 `levels.token_weights`；②–⑥ 未决）**计量口径**：① P-44 说预算按加权 token、权重是配置项，config.md 无该键且 OQ-7 仍写"不加权"，权重怎么来；② `requests` 的口径；③ 用量不可解析时的保守估计；④ "切断后续请求"（E4）与 "硬切断"（worker-protocol OQ-14）是否包括在途流；⑤ 封顶后返回 403 还是 429 对 agent CLI 重试行为的影响（**需核实**）；⑥ 在途时崩溃的 `incomplete` 如何影响 `engine` 的记账 | `egress`、`engine`、`models` | §7.4 的 **[提议]** |
| OQ-E5 | `exec.model_api`（"持续上游错误超阈值"）的阈值与定义；每 job 的限速参数 | 失败分类、重试风暴 | `egress` 只报告 `upstream_errors` 摘要，阈值由 `engine` 定；限速用内部常量 |
| OQ-E6 | GitHub 域名族硬编码清单是否完整、是否还要覆盖其他 GitHub 托管的内容分发域；Q-08（git 依赖的服务端缓存）；worker-protocol OQ-15（包仓库白名单作为外泄通道，是否改成只读缓存代理）；`CONNECT` 的每 job 字节上限取值 | I1 的实际强度 | §7.7 清单为 **[提议]**；字节上限为内部常量 |
| OQ-E7 | （里程碑**已决：见裁决 C28**——归 M2 `exec`；其余未决）私有包仓库反向代理的里程碑（milestones 没有点名，config §8.2 只说 M1 不要求）；沙盒内的 npm / pip / cargo 如何被指向 `/registry/<id>/` 路由（配置注入点归 `worker`、`sandbox` 还是 devcontainer）；仅 `GET` / `HEAD` 的限制是否可接受 | M2 | M2；只读 |
| OQ-E8 | `*.example.com`（"一层前缀通配"）是否匹配多层子域 | 白名单宽度 | 只匹配恰好多一层标签（更严） |
| OQ-E9 | 各 provider kind 的上游路径 / 方法白名单、可转发头名单、鉴权头方案，以及 Anthropic 用量字段名（均 **需核实当前 API**）；第二个 provider kind 的接入方式 | G1、G2 | Anthropic 的 §7.3 / §7.4 列表 |
| OQ-E10 | 审计：保留期（config OQ-21）、哈希链的范围与起始里程碑、写失败时是否真的应该拒绝新请求（可用性 vs 审计完整性） | G6、G9 | 保留期未定；M3 哈希链；写失败即拒绝 |
| OQ-E11 | 最终用量 `final_usage` 的保留期与 `forget_job` 的触发责任（本文：`sandbox.release` + TTL 24 h）；登记表在 `egress` 重启后的有效期 | 恢复语义 | 如 §7.2 |
| OQ-E12 | M3 "沙盒逃逸检测告警"中 `egress` 提供的信号（无效或跨网段使用的 `job_token`、伪造头、对控制面地址的探测）如何上报——`egress` 不写事件，是经 `status()` / 拉取接口，还是别的通道 | M3 | 仅审计 + 指标 |
| OQ-E13 | `sandbox_ro` 角色是否需要包仓库（config OQ-16）：是否按角色区分出网档案 | 白名单最小化 | 单一档案 |
