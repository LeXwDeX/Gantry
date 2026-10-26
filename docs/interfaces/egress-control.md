# Egress 控制面契约（`gantryd` ↔ Egress）

> 状态：设计 v0.1（2026-10-24）· 契约 · 协议版本 `gantry-egress-control/1` · 机器可读版本：`schemas/egress/*.json`（M0 由 `contract` 泳道据本文生成；冲突以本文为准）· 决策：decisions P-49（`job_token`）、P-50（控制面与 `svc_token`）、P-72（`CONNECT` 携带 `job_token`）、P-73（网段限制需核实）、P-74（`svc_token` 每进程一枚与台账请求头）
> 使用者：`sandbox`（签发 / 吊销 `job_token`、取尝试用量）、`models`（签发 `svc_token`、取调用用量）、`api`（转交 secret、查询 `present`、读状态）、`gantryd` 的设置应用（下发配置视图）、`egress`（实现方）。端口 trait `EgressControl` 定义在 `core::ports`（P-51），M0 替身是 `EgressStub`（[milestones](../milestones.md#替身让泳道互不阻塞)）。
> 标注 **[提议]** 的取值是本文给出的默认值，骨架文档没有定；标注 OQ-n 的点见 §9 开放问题，不要自行拍板。

本文规定服务端进程 `gantryd` 怎样调用独立进程出网代理（Egress）的**控制面**，以及 Egress 用来识别调用方并计量的两种**能力令牌**（`job_token`、`svc_token`）的语义。它是 [architecture.md §5](../architecture.md#5-信任边界与不变式) 中 I1（沙盒只计算）、I6（不持凭据）与 decisions P-16 / P-49 / P-50 的落地契约。数据面的代理流水线（反向代理、CONNECT、硬性拒绝表、审计格式）归 [modules/egress.md](../modules/egress.md)；本文只写数据面上与令牌有关、调用方必须知道的约定（§5）。

## 0. 一页纸

| 通道 | 端点 | 调用方 | 鉴权 | 用途 |
| --- | --- | --- | --- | --- |
| 控制面 | Unix socket `/run/gantry/egress.sock`，HTTP/1.1 + JSON，路径前缀 `/control/v1/` | `gantryd`（`sandbox`、`models`、`api`、设置应用） | 仅主机本地；按对端进程用户（`SO_PEERCRED`） | 令牌签发 / 吊销、secret 只写转交与 `present` 查询、用量拉取、配置视图、状态 |
| 沙盒数据面 · 模型 | `http://<egress-sbx>/j/<job_token>/<provider_id>/…` | 沙盒内的 agent CLI | `job_token`（源地址网段限制的强制方式需核实，P-73） | 模型 API 反向代理，按尝试计量（P-49） |
| 沙盒数据面 · 包仓库 | `HTTPS_PROXY=http://job:<job_token>@<egress-sbx>:3128`（CONNECT 白名单） | 沙盒内的包管理器 | `Proxy-Authorization`（Basic，用户名 `job`）携带 `job_token`（P-72） | 公共包仓库，不注入凭据；按尝试的出网档案放行、计量（字节与连接数）与审计 |
| 沙盒数据面 · 私有包仓库（M2） | `http://<egress-sbx>/r/<job_token>/<injection_id>/…` **[提议]**（OQ-1） | 沙盒内的包管理器 | `job_token` | 私有包仓库反向代理，注入凭据，只读 |
| 回环数据面 · 模型 | `http://127.0.0.1:<port>/s/<svc_token>/<provider_id>/…`，请求头 `X-Gantry-Ledger`、`x-gantry-tier`、`x-gantry-call-id` | `gantryd` 的 `models` | `svc_token`（每个 `gantryd` 进程一枚，P-74）+ 只在回环监听器上接受 | 服务端直接调用模型；按请求头声明的台账（交付台账或会话台账）实时计量（P-50、P-74） |

不存在：控制面上的任何 TCP 端点；从沙盒到控制面或回环监听器的路由；任何读出 secret 值的操作；Egress 对 Gantry 事件日志的写入。控制面统一称 `egress.sock`，**不再使用** `models_uds` 等别名（P-50）。

## 1. 原则

| # | 原则 | 依据 |
| --- | --- | --- |
| EC1 | 控制面只在主机本地：Unix socket，不监听 TCP，不挂进任何沙盒，沙盒网络不可达 | I1、P-15、P-50 |
| EC2 | 令牌是**能力**，不是身份声明：持有 `job_token` 只意味着"可以给这个尝试记账"，持有 `svc_token` 只意味着"可以以服务端名义经回环监听器调用模型"（归属由服务端的可信请求头声明，P-74）；令牌不授予任何控制面权限 | P-49、P-74 |
| EC3 | 密钥只写不读：控制面没有返回 secret 值的操作，响应、错误、日志、审计里都不出现值 | I6、config §2.4 |
| EC4 | 计量只有一个事实源（Egress），两种令牌同一口径（同一用量解析、同一组 `levels.token_weights`）；记账（`budget.charged`、`Attempt.usage`）由 `gantryd` 写事件日志 | P-22、P-50、P-70 |
| EC5 | 失败闭合：无配置、配置非法、令牌未知 / 已吊销 / 过期、对端用户不符、审计写失败——一律拒绝 | W8 |
| EC6 | Egress 不保存 Gantry 的业务状态，只保存令牌登记（哈希）、用量记录、配置视图与密钥；`gantryd` 可在 Egress 重启后对账恢复 | — |
| EC7 | 每个写操作都幂等或有明确的重放语义（§4 各节） | — |

## 2. 传输与鉴权

### 2.1 传输

| 项 | 规则 |
| --- | --- |
| 地址 | Unix socket `/run/gantry/egress.sock`（P-50）。**[提议]** 由 systemd socket 激活创建：`ListenStream=/run/gantry/egress.sock`、`SocketUser=gantry-egress`、`SocketGroup=gantry-link`、`SocketMode=0660`；`gantry-link` 组只含 `gantry` 与 `gantry-egress` 两个系统用户。与 deploy 规格现有路径的差异见 OQ-5 |
| 协议 | HTTP/1.1；请求与响应 `application/json; charset=utf-8`；错误 `application/problem+json`（形状同 [api.md §3.1](api.md#31-错误applicationproblemjsonrfc-9457)：`type`、`title`、`status`、`code`、`detail`、`request_id`） |
| 版本 | 路径前缀 `/control/v1/`；`GET /control/v1/status` 返回 `protocol: "gantry-egress-control/1"`。v1 内只追加字段与操作；调用方忽略未知响应字段，Egress 对请求体严格校验（未知字段 → `422 validation_failed`） |
| 关联 | 请求头 `x-gantry-request-id`（ULID，可选），Egress 在审计与日志里原样记录 |
| 超时 | 调用方对每个控制面调用设超时：令牌与 secret 操作 5 s，用量拉取 10 s **[提议]**；超时按"结果未知"处理，依靠 §4 的幂等语义重试 |
| 体积 | 请求体 ≤ 4 MiB（配置视图可能较大），其余操作 ≤ 64 KiB **[提议]** |
| 数据约定 | 时间 UTC、RFC 3339、三位毫秒、`Z` 结尾；ID 为 data-model §2 的带前缀 ULID；用量字段名与 data-model `Attempt.usage` 相同 |

### 2.2 鉴权

| 规则 | 说明 |
| --- | --- |
| 仅主机本地 | 只接受 Unix socket 连接；Egress 不在任何 TCP 地址上提供 `/control/*`（数据面监听器收到 `/control/*` 一律 404） |
| 按进程用户 | 每个连接 accept 时读取对端凭据（`SO_PEERCRED`），`uid` 必须等于引导配置 `egress.control.allowed_uid`（默认 `gantryd` 的系统用户 `gantry`）**[提议]**；不符 → 立即关闭连接，不返回任何 HTTP 响应，写审计（`kind = control`、`decision = deny`、`rule = control.peer_not_allowed`） |
| 不用口令或令牌 | 控制面的身份就是进程用户；不区分 `gantryd` 内的模块（同一进程、同一信任级别） |
| 沙盒不可达 | socket 不挂进任何沙盒；沙盒用户（`gantry-sbx`）不在 `gantry-link` 组；`sandbox` 模块的不变式检查断言这一点（I1） |
| 数据面令牌无效 | `job_token` / `svc_token` 在控制面上不被识别，带了也不改变任何判定 |

## 3. 令牌

### 3.1 `job_token`（每个尝试一枚，P-49）

| 属性 | 规定 |
| --- | --- |
| 形状 | `gjt1_` + 32 字节随机数的 base64url（无填充，43 字符）**[提议]**；调用方视为不透明字符串 |
| 签发 | `sandbox` 在创建容器之前经 §4.1 签发；一个 Attempt 一枚，以 `job_id`（= `Attempt.id`，`att_…`）为键 |
| 绑定 | `job_id`、`sandbox_id`、`project_id`、台账（`{subject: "delivery", id: dlv_…}`，或 None 档临时图的 `{subject: "conversation", id: cnv_…, day}`）、`tier`、`provider`、出网档案与分组快照、`tokens_total` 封顶、`token_weights` 快照、`expires_at` |
| 用法 | 沙盒内模型 base URL：`http://<egress-sbx>/j/<job_token>/<provider_id>`（由签发响应直接给出）。Egress 去掉 `/j/<job_token>/<provider_id>` 前缀后拼到该 provider 的真实 `base_url` 之后转发。包仓库：`HTTPS_PROXY=http://job:<job_token>@<egress-sbx>:3128`（签发响应的 `connect_proxy_url` 已含令牌），Egress 从 `Proxy-Authorization` 取令牌（P-72）。私有包仓库（M2）：`/r/<job_token>/<injection_id>/…` **[提议]** |
| 有效范围 | 只在**沙盒侧监听器**（模型 / `CONNECT` / 私有仓库三个入口）上接受；`provider_id` 必须等于绑定的 provider。"只能从沙盒网段使用"的强制方式**需核实**（P-73）：rootless Podman 下 Egress 能可靠看到沙盒源地址时，按引导配置 `egress.sandbox_cidrs` 拒绝网段外使用；不能时不做此项检查，`GET /control/v1/status` 的 `detail` 标明 `source_check: unverified`。安全底线不依赖网段：令牌每尝试一枚、短期、只能给本尝试记账、不授予控制面权限 |
| 有效期 | 从签发到吊销；另有硬上限 `expires_at` = 签发时刻 + `wall_time_s` + 600 s **[提议]**，到期自动作废，防止吊销调用丢失时令牌长期有效 |
| 吊销 | 尝试结束、`collect` 之后由 `sandbox` 调 §4.2；吊销即时生效，之后该令牌的任何请求都被拒绝，在途请求允许完成并计入 |
| 存储 | Egress 只存 `sha256(job_token)`；原文只在签发响应中出现一次，由 `sandbox` 写进容器的环境变量。**不进**事件日志、`job.json`、`inputs/`、Gantry 日志、Egress 审计（审计记 `job_id`） |
| 泄露后果 | 沙盒内任何进程都可能读到它。它只能在本尝试存续期间（网段限制可强制时还须从沙盒网段，P-73）、对绑定的 provider 与本尝试的出网档案发请求，用量记在本尝试上并受 `tokens_total` 封顶；不能读配置、不能取用量、不能影响其他尝试 |

### 3.2 `svc_token`（服务端直接调用，P-50、P-74）

| 属性 | 规定 |
| --- | --- |
| 形状 | `gst1_` + 43 字符随机 base64url **[提议]**；控制面上用 `svc_token_id`（`svt_` + ULID，Egress 内部标识，不是 data-model 实体）指代它 |
| 签发 | **每个 `gantryd` 进程一枚**（P-74）：`models` 在进程启动时经 §4.3 签发，进程内所有服务端模型调用共用。签发同时吊销此前所有未吊销的 `svc_token`（上一个进程留下的，或本进程轮换前的），任一时刻至多一枚有效 |
| 绑定 | 不绑定台账与项目：只记 `issued_to`（`gantryd` 进程标识，仅用于审计）与 `expires_at` |
| 用法 | `http://127.0.0.1:<port>/s/<svc_token>/<provider_id>/…`，每次请求**必须**带三个头：`X-Gantry-Ledger: <台账 ID>`（交付台账 `dlv_…`；会话台账 `cnv_…:YYYY-MM-DD`，即 [data-model](../data-model.md#device--budgetledger) 的台账 ID）、`x-gantry-tier: utility\|planner`、`x-gantry-call-id: <ULID>`。这些头来自服务端，Egress 视为可信（`svc_token` 只在回环监听器上有效，沙盒不可达），据此记录每条用量的归属，不校验台账是否存在。无项目的全局调用（modules/models.md OQ-3）用 `X-Gantry-Ledger: unattributed`，用量记录的 `ledger` 为 `null` |
| 有效范围 | 只在**回环监听器**（引导配置 `egress.listen.svc_model`，默认 `127.0.0.1:8082`，与 deploy.md §6.4 的端口表一致）上接受；该监听器只绑回环地址，沙盒网段不可达。沙盒侧监听器拒绝 `/s/…`，回环监听器拒绝 `/j/…` 与 `/r/…` |
| 有效期 | 从签发到吊销；硬上限 `expires_at` = 签发时刻 + 24 h **[提议]**。`models` 在到期前 1 h 轮换（签发新令牌，旧令牌随之吊销；旧令牌上的在途请求允许完成并计入） |
| 吊销 | 签发新令牌时自动吊销旧令牌；`gantryd` 正常退出时显式吊销（§4.3）；进程崩溃留下的令牌由下一个进程签发时吊销，或到期作废 |
| 对账 | 令牌吊销（进程重启或轮换）后，`gantryd` 按该 `svc_token_id` 拉取其全部用量记录（§4.5），与已记的 `budget.charged` 按 `call_id` 对账、补记遗漏（P-74） |
| 存储 | 与 `job_token` 相同：Egress 只存哈希；原文只在 `gantryd` 进程内存 |
| 泄露后果 | 主机上的其他本地进程能连回环端口，但没有令牌只能得到拒绝。拿到令牌的本地进程可以以服务端名义调模型、把用量记到任意台账，直到轮换或进程重启；这要求已能读 `gantryd` 进程内存，即已越过服务端信任边界（[architecture §5](../architecture.md#5-信任边界与不变式)） |

## 4. 操作

| § | 操作 | 方法与路径 | 调用方 | 幂等 | 里程碑 |
| --- | --- | --- | --- | --- | --- |
| 4.1 | 签发 `job_token` | `POST /control/v1/jobs` | `sandbox` | 以 `job_id` 为键（见下） | M1 |
| 4.2 | 吊销 `job_token` 并取最终用量 | `POST /control/v1/jobs/{job_id}/revoke` | `sandbox` | 是 | M1 |
| 4.2 | 查询尝试的当前用量 | `GET /control/v1/jobs/{job_id}/usage` | `sandbox` | 读 | M1 |
| 4.2 | 遗忘尝试（`attempt.finished` 落库之后） | `DELETE /control/v1/jobs/{job_id}` | `sandbox` | 是 | M1 |
| 4.3 | 签发 `svc_token`（并吊销此前的） | `POST /control/v1/svc-tokens` | `models`（进程启动、轮换） | 否（每次新令牌） | M1 |
| 4.3 | 吊销 `svc_token` | `POST /control/v1/svc-tokens/{svc_token_id}/revoke` | `models` | 是 | M1 |
| 4.4 | 写入 secret：暂存 | `POST /control/v1/secrets/stage` | `api` | 以 `request_id` 为键 | M1 |
| 4.4 | 写入 secret：激活 / 丢弃 | `POST /control/v1/secrets/staged/{staged_id}/activate`、`…/discard` | `api` | 是 | M1 |
| 4.4 | 列出暂存项（启动对账） | `GET /control/v1/secrets/staged` | `api` | 读 | M1 |
| 4.4 | 查询 secret 是否存在（`present`） | `POST /control/v1/secrets/present` | `api` | 读 | M1 |
| 4.4 | 回收不再被引用的 secret | `DELETE /control/v1/secrets/{secret_ref}` | `api` | 是 | M1 |
| 4.5 | 拉取用量 | `GET /control/v1/usage` | `models`、`engine` 的对账任务 | 读（游标） | M1 |
| 4.6 | 下发配置视图 | `PUT /control/v1/config` | `gantryd` 的设置应用 | 以 `rev` 为键 | M1 |
| 4.7 | 状态 | `GET /control/v1/status` | `api`（`GET /v1/settings/status`）、`sandbox` | 读 | M1 |
| 4.7 | 端点 | `GET /control/v1/endpoints` | `sandbox`、`models` | 读 | M1 |

### 4.1 签发 `job_token`

请求：

```json
{
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "sandbox_id": "sbx_01JBX4R2M6P8Q1S3T5V7W9Y0ZA",
  "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
  "ledger": { "subject": "delivery", "id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD" },
  "tier": "worker",
  "provider": "anthropic",
  "profile": "default",
  "groups": ["model_api", "npm", "crates"],
  "tokens_total": 800000,
  "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 },
  "wall_time_s": 1800,
  "config_rev": 18240
}
```

响应 `201`：

```json
{
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "job_token": "gjt1_q3Vb7nT1xK9wL2mP5sR8uY0aC4eG6iJ9kM1oQ3sU5wX",
  "model_base_urls": {
    "anthropic": "http://10.89.0.1:8081/j/gjt1_q3Vb7nT1xK9wL2mP5sR8uY0aC4eG6iJ9kM1oQ3sU5wX/anthropic"
  },
  "connect_proxy_url": "http://job:gjt1_q3Vb7nT1xK9wL2mP5sR8uY0aC4eG6iJ9kM1oQ3sU5wX@10.89.0.1:3128",
  "effective_groups": ["model_api", "npm", "crates"],
  "issued_at": "2026-10-24T12:40:58.120Z",
  "expires_at": "2026-10-24T13:20:58.120Z",
  "reissued": false
}
```

| 规则 | 说明 |
| --- | --- |
| 字段 | `ledger.subject` ∈ `delivery` / `conversation`；`conversation` 时另带 `day`（UTC 日期，P-58）。`tier` ∈ `planner` / `worker`（沙盒内只用这两类）。`groups` 是创建时固化的分组集合，有效集合 = 此快照 ∩ 当前配置（config §2.6），`effective_groups` 是签发时的有效集合。`tokens_total` 与 `token_weights` 一起决定封顶：加权用量达到 `tokens_total` 即切断后续请求（worker-protocol §2.3 E4）。`connect_proxy_url` 已含 `job:<job_token>@`，`sandbox` 原样写进容器的 `HTTPS_PROXY`（P-72） |
| 重放 | 同一 `job_id` 且绑定完全相同、令牌仍有效 → `200`，**签发新令牌**、旧令牌立即作废、已计用量保留，`reissued: true`（用于 `sandbox` 在签发响应丢失后重试；只应发生在容器启动之前） |
| 冲突 | 同一 `job_id` 绑定不同 → `409 binding_mismatch`；该 `job_id` 已吊销 → `409 job_revoked`（新尝试必须用新的 `job_id`） |
| 校验 | `groups` 必须是 `profile` 在当前配置中的分组子集；`provider` 必须存在且凭据可解析，否则 `422 validation_failed`（凭据缺失为 `credential_missing`）；没有任何已应用配置 → `503 not_configured` |

### 4.2 吊销 `job_token`、查询与遗忘

`POST /control/v1/jobs/{job_id}/revoke`，请求体 `{}`。响应 `200`（`FinalUsage`）：

```json
{
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "state": "revoked",
  "revoked_at": "2026-10-24T12:58:49.310Z",
  "usage": {
    "input_tokens": 415200,
    "output_tokens": 18100,
    "cache_read_tokens": 351000,
    "cache_write_tokens": 21000,
    "requests": 26
  },
  "weighted_tokens": 567050,
  "capped": false,
  "incomplete": false,
  "upstream_errors": { "total_5xx": 0, "total_429": 1, "total_conn_errors": 0, "max_consecutive": 1, "last_at": "2026-10-24T12:47:12.004Z" },
  "denied": { "not_allowed_host": 2, "path_not_allowed": 0, "budget_exhausted": 0 }
}
```

| 规则 | 说明 |
| --- | --- |
| 原子 | 吊销与"取最终用量"是同一步：返回的数字之后不会再变 |
| 幂等 | 已吊销则返回同一份 `FinalUsage`，直到 `DELETE`；`job_id` 不存在 → `404 not_found` |
| `capped` / `incomplete` | `capped`：曾因 `tokens_total` 切断请求（对应 worker-protocol §9 的 `budget.tokens`）。`incomplete`：有请求在途时 Egress 崩溃，或上游用量不可解析而按保守估计计入，数字是下界或估计 |
| `GET …/usage` | 同形状，`state: "active"`、无 `revoked_at`，是当前快照，供进度展示与预警 |
| `DELETE …` | `attempt.finished` 落库之后由 `sandbox` 调用（`SandboxService::release`）；之后 `revoke` / `usage` 返回 `404`。未调用时 Egress 在吊销后 24 h **[提议]** 自动遗忘 |

`sandbox` 把 `FinalUsage.usage` 交给 `engine`，写入 `Attempt.usage` 与 `attempt.finished`，并据 `weighted_tokens` 记一次 `budget.charged`（交付台账或会话台账）。§4.5 里 `caller.kind = job` 的记录只用于进度与预警，**不重复记账** **[提议]**（OQ-4）。

### 4.3 签发与吊销 `svc_token`

请求（`gantryd` 启动时）：

```json
{
  "issued_to": { "process": "gantryd", "pid": 4182, "started_at": "2026-10-24T08:00:01.004Z" },
  "ttl_s": 86400
}
```

响应 `201`：

```json
{
  "svc_token_id": "svt_01JBX5A2C4E6G8J1L3N5Q7S9UW",
  "svc_token": "gst1_Zt4Kp9Qm2Wx7Rb5Nc8Hv1Lj3Fd6Gs0Ay2Eu4Io6Ok8P",
  "model_base_urls": {
    "anthropic": "http://127.0.0.1:8082/s/gst1_Zt4Kp9Qm2Wx7Rb5Nc8Hv1Lj3Fd6Gs0Ay2Eu4Io6Ok8P/anthropic"
  },
  "expires_at": "2026-10-25T08:00:01.120Z",
  "revoked_previous": ["svt_01JBWZ8M2P4R6T8V0X2Z4B6D8F"]
}
```

| 规则 | 说明 |
| --- | --- |
| 字段 | `issued_to` 只进审计；`ttl_s` 可选，默认 86400，范围 3600–604800 **[提议]** |
| 签发即轮换 | 签发成功即吊销此前所有未吊销的 `svc_token`，`revoked_previous` 列出它们（P-74）；`models` 对其中每一枚做一次对账（下行） |
| 吊销 | `POST /control/v1/svc-tokens/{svc_token_id}/revoke`，请求体 `{}`，响应 `200 {"svc_token_id": "…", "state": "revoked", "revoked_at": "…"}`；已吊销原样返回 |
| 记账 | Egress 每个请求完成即写一条用量记录（实时计量，归属取自请求头）。`models` 在每次调用结束后按 `x-gantry-call-id` 取该调用的用量（§4.5），以 `weighted_tokens` 记 `budget.charged` 到 `X-Gantry-Ledger` 指明的台账 |
| 对账 | 令牌吊销后（`revoked_previous` 或显式吊销），`models` 用 `GET /control/v1/usage?svc_token_id=<id>` 取该令牌的全部记录，与已记账的 `call_id` 比对，补记遗漏（P-74）；平时也按游标定期对账 |
| 封顶 | Egress 不对 `svc_token` 封顶（令牌不绑定台账）；会话台账的每日上限与交付台账的余额由 `models` 在派发前检查 |

### 4.4 密钥：只写不读

模型 key 与私有包仓库凭据只存在 Egress（P-16）。`api` 在 `PUT /v1/settings/{resource}` 里收到 `SecretRef.value` 后，经下列两段式转交，满足"secret 写入与 `settings.updated` 原子"（config §2.4）：**暂存 → 追加 `settings.updated`（`secret_changes[]`）→ 激活**；事件追加失败则**丢弃**。控制面**没有**任何返回 secret 值的操作。

暂存，`POST /control/v1/secrets/stage`：

```json
{
  "request_id": "01JBX5C7E9G2J4K6N8Q0S3T5WY",
  "secret_ref": "model.anthropic.main",
  "value": "example-value-not-a-real-key"
}
```

响应 `201`（不回显值）：

```json
{
  "staged_id": "stg_01JBX5C8F0H3K5M7P9R1T4V6XZ",
  "secret_ref": "model.anthropic.main",
  "version": 3,
  "staged_at": "2026-10-24T12:41:00.004Z",
  "expires_at": "2026-10-24T12:51:00.004Z"
}
```

激活，`POST /control/v1/secrets/staged/{staged_id}/activate`，请求体 `{}` → `200`：

```json
{ "secret_ref": "model.anthropic.main", "version": 3, "activated_at": "2026-10-24T12:41:00.213Z" }
```

查询是否存在，`POST /control/v1/secrets/present`：

```json
{ "refs": ["model.anthropic.main", "registry.npm.corp"] }
```

```json
{ "present": { "model.anthropic.main": true, "registry.npm.corp": false } }
```

| 规则 | 说明 |
| --- | --- |
| 暂存 | 暂存值对数据面不可见，直到激活；同一 `request_id` 重放返回同一 `staged_id`（值不同 → `409 conflict`）。`secret_ref` 匹配 `^[a-z0-9][a-z0-9._-]{0,63}$`，`value` 非空、≤ 16 KiB，否则 `422 validation_failed` |
| 激活 | 已激活的再激活原样 `200`；暂存已过期（默认 10 分钟 **[提议]**）→ `410 staged_expired`，`api` 须重新暂存；同名 ref 的新版本激活后，新请求用新版本，在途请求用旧版本完成（轮换，M3） |
| 丢弃 | `POST …/discard`，请求体 `{}` → `200 {}`；已丢弃或已过期原样 `200` |
| 启动对账 | `GET /control/v1/secrets/staged` → `{"items": [{"staged_id", "secret_ref", "version", "staged_at", "age_s"}]}`（无值）。`api` 启动时用它找出"暂存了但没有对应 `settings.updated`"的项并丢弃 |
| `present` | 只回答"该 ref 当前是否有已激活的值"，供 settings 响应的 `present` 字段；不存在的 ref 返回 `false`，不报错 |
| 回收 | `DELETE /control/v1/secrets/{secret_ref}` → `200 {}`：不再被任何 `(scope, resource)` 引用的 ref 由 `api` 回收（config §2.4 `op: delete`）；被当前配置视图引用的 ref 拒绝回收（`409 conflict`） |
| 不留痕 | 请求体中的 `value` 不写 Egress 日志、审计与崩溃转储；`api` 发送后立即清除内存 |

### 4.5 拉取用量

`GET /control/v1/usage?after=<cursor>&limit=<n>`（`limit` 默认 200，最大 1000），或 `GET /control/v1/usage?call_id=<ULID>`（取某次 `svc_token` 调用的记录），或 `GET /control/v1/usage?svc_token_id=<svt_…>&after=<cursor>`（取某枚 `svc_token` 的全部记录，吊销后对账用，P-74）。每一条记录对应一次已完成（或中断）的上游请求：

```json
{
  "items": [
    {
      "seq": 90211,
      "at": "2026-10-24T12:41:03.120Z",
      "caller": { "kind": "svc", "svc_token_id": "svt_01JBX5A2C4E6G8J1L3N5Q7S9UW", "call_id": "01JBX5D2F4H6K8M0P2R4T6V8XZ" },
      "project_id": null,
      "ledger": { "subject": "conversation", "id": "cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ", "day": "2026-10-24" },
      "tier": "utility",
      "provider": "anthropic",
      "model": "example-utility-model",
      "usage": { "input_tokens": 3201, "output_tokens": 410, "cache_read_tokens": 2800, "cache_write_tokens": 0, "requests": 1 },
      "weighted_tokens": 5531,
      "incomplete": false
    },
    {
      "seq": 90212,
      "at": "2026-10-24T12:41:04.877Z",
      "caller": { "kind": "job", "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD" },
      "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
      "ledger": { "subject": "delivery", "id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD" },
      "tier": "worker",
      "provider": "anthropic",
      "model": "example-worker-model",
      "usage": { "input_tokens": 18200, "output_tokens": 2400, "cache_read_tokens": 9000, "cache_write_tokens": 1200, "requests": 1 },
      "weighted_tokens": 32600,
      "incomplete": false
    }
  ],
  "next_cursor": "90212"
}
```

| 规则 | 说明 |
| --- | --- |
| 游标 | `seq` 在 Egress 内单调递增；`after` 取上一页的 `next_cursor`（不透明字符串）；没有更多时 `next_cursor` 为当前最后一条的游标，调用方下次带它轮询 |
| 交付语义 | 至少一次：调用方以 `seq` 去重，把游标与它写下的 `budget.charged` 放在同一个 `store` 事务里推进 |
| 归属 | `caller.kind = job` 的 `project_id`、`ledger`、`tier` 取自令牌绑定；`caller.kind = svc` 的 `ledger`、`tier` 由请求头 `X-Gantry-Ledger`、`x-gantry-tier` 解析而来（P-74），`project_id` 为 `null`（Egress 不知道台账属于哪个项目，由 `models` 记账时补全） |
| 加权 | `weighted_tokens` = `Σ token_weights.<类别> × 该类别 token 数`，权重取签发令牌时的快照（`job_token`）或请求时的当前配置（`svc_token`），与台账口径一致（P-70） |
| 记账分工 | `caller.kind = svc` 的记录由 `models` 逐条记账，令牌吊销时按 `svc_token_id` 全量对账（P-74）；`caller.kind = job` 的记录不记账，尝试用量以 §4.2 的 `FinalUsage` 为准（OQ-4） |
| 保留 | 记录至少保留 7 天 **[提议]**；游标早于保留窗口 → `410 cursor_expired`，调用方从当前位置继续，并对仍未记账的 `svc` 调用逐个按 `call_id` 补查 |
| 不含 | 请求 / 响应体、头、令牌原文、上游 URL 的查询串 |

### 4.6 下发配置视图

`PUT /control/v1/config`：`gantryd` 在 `egress`、`models`（`providers`）、`levels`（`token_weights`）任一 resource 的 `settings.updated` 之后、以及与 Egress 重新建立连接时下发。视图只含引用，不含任何 secret 值：

```json
{
  "rev": 18240,
  "providers": {
    "anthropic": { "kind": "anthropic", "base_url": "https://api.anthropic.com", "secret_ref": "model.anthropic.main" }
  },
  "groups": {
    "model_api": { "hosts": ["api.anthropic.com"] },
    "npm": { "hosts": ["registry.npmjs.org"] },
    "crates": { "hosts": ["crates.io", "index.crates.io", "static.crates.io"] }
  },
  "profiles": { "default": { "groups": ["model_api", "npm", "crates"] } },
  "default_profile": "default",
  "injection": {},
  "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 }
}
```

响应 `200 {"applied_rev": 18240}`。

| 规则 | 说明 |
| --- | --- |
| 复核 | Egress 对视图做防御性复核（config V10、V11、V13）；不通过 → `422 config_invalid`（附 `errors[]`，`pointer` 相对视图），保留旧视图并把 `health` 置 `degraded` |
| 顺序 | `rev` 小于已应用的 → `409 stale_rev`，不改变任何东西；等于 → 原样 `200`（幂等） |
| 生效 | 原子替换；对存量令牌的有效分组 = 签发时快照 ∩ 新配置（收紧立即生效，放宽只对之后签发的令牌生效，config §2.6） |
| 无配置 | 从未应用过视图时，签发令牌返回 `503 not_configured`，数据面拒绝一切（失败闭合） |

### 4.7 状态与端点

`GET /control/v1/status`，供 `api` 组装 `GET /v1/settings/status` 的 `modules.egress`（config §2.8）：

```json
{
  "protocol": "gantry-egress-control/1",
  "applied_rev": 18240,
  "health": "ok",
  "detail": null,
  "unresolved_secret_refs": [],
  "active_jobs": 3,
  "active_svc_tokens": 1,
  "usage_head": "90212"
}
```

`GET /control/v1/endpoints`（静态，不含任何令牌）：

```json
{
  "sandbox_model_base": "http://10.89.0.1:8081",
  "connect_proxy_url": "http://10.89.0.1:3128",
  "loopback_model_base": "http://127.0.0.1:8082",
  "registry_base": null
}
```

`health` ∈ `ok` / `degraded` / `unhealthy`：审计写失败、无已应用配置为 `unhealthy`；最近一次配置被拒、有未解析的 `secret_ref` 为 `degraded`。`registry_base` 是私有包仓库反向代理（M2）的基址，M1 为 `null`。`connect_proxy_url` 不含令牌；尝试专用的带令牌地址只在 §4.1 的签发响应里出现。`detail` 在网段检查无法强制时含 `source_check: unverified`（P-73）。`active_svc_tokens` 正常为 1（P-74）。

## 5. 数据面上的令牌约定

数据面的完整流水线见 [modules/egress.md](../modules/egress.md)；下表是调用方（agent CLI、`models`）与实现方都必须遵守的令牌相关部分。

| 情形 | Egress 的处理 | 响应 |
| --- | --- | --- |
| 路径不是 `/j/<令牌>/<provider_id>/…` / `/r/<令牌>/<injection_id>/…`（沙盒侧）或 `/s/<令牌>/<provider_id>/…`（回环） | 拒绝 | `404`，错误体同模型 API 错误形状（modules/egress.md §6.4） |
| 令牌格式不符、未知、已吊销、已过期 | 拒绝；四种情况**同一响应**，不给探测线索；审计记 `rule`（`token.unknown` / `token.revoked` / `token.expired`），不记令牌原文 | `403`，`error.type = token_invalid` |
| 沙盒侧 `CONNECT` 不带 `Proxy-Authorization` | 要求认证 | `407`，`Proxy-Authenticate: Basic realm="gantry-egress"` **[提议]**（部分包管理器只在收到质询后才发送代理凭据） |
| `CONNECT` 的 `Proxy-Authorization` 不是 Basic、用户名不是 `job`，或其中的令牌无效 | 拒绝（与上一行的令牌无效同一响应） | `403`，`Connection: close`（审计记 `token.*`，P-72） |
| `CONNECT` 令牌有效，但目标 `host:port` 不在该尝试的有效分组 | 拒绝 | `403`（审计记 `job_id` 与目标主机） |
| `job_token` 来自沙盒网段以外的源地址（仅在网段检查可强制时，P-73）；`svc_token` 出现在沙盒侧监听器；`job_token` 出现在回环监听器 | 拒绝 | `403 token_invalid`（同上，审计记 `token.wrong_listener` / `token.source_not_allowed`） |
| `provider_id` 不是令牌绑定的（`job_token`）或不存在 | 拒绝 | `403`，`error.type = provider_not_allowed` |
| 加权用量达到 `tokens_total` | 拒绝新请求，置 `capped`；在途请求允许完成 | `403`，`error.type = budget_exhausted` |
| 回环请求缺 `X-Gantry-Ledger` / `x-gantry-tier` / `x-gantry-call-id` 之一，或格式不符 | 拒绝，不转发、不计量 | `400`，`error.type = ledger_header_invalid` **[提议]** |
| 请求带 `authorization`、`x-api-key`、`x-gantry-*` 等头 | 沙盒侧一律丢弃（身份只来自令牌）；回环侧读取 `X-Gantry-Ledger`、`x-gantry-tier`、`x-gantry-call-id` 作归属（P-74），转发上游前全部剥离 | — |
| 正常 | 去掉前缀，注入 provider 的真实 key，转发；按令牌归属计量 | 透传上游响应，不回显令牌与 key |

令牌原文只出现在请求路径或 `Proxy-Authorization` 里：Egress 的访问日志与审计在记录前先把令牌替换为 `job_id` / `svc_token_id`（`Proxy-Authorization` 整个不记）；调用方（适配层、`models`）同样不得记录完整 base URL。

## 6. 错误

控制面错误一律 `application/problem+json`，`code` 稳定、只增不改：

| HTTP | `code` | 触发 | 调用方动作 |
| --- | --- | --- | --- |
| —（连接被关闭） | — | 对端进程用户不符（§2.2） | 部署错误，不重试；`gantryd` 健康检查报 `egress` 不可用 |
| 400 | `invalid_request` | JSON 语法错误、查询参数错误 | 调用方缺陷，不重试 |
| 404 | `not_found` | `job_id`、`svc_token_id`、`staged_id` 不存在，或路由不存在 | `revoke` 遇到它说明已遗忘，按"已吊销"处理 |
| 409 | `binding_mismatch` | 同一 `job_id` 以不同绑定签发 | 引擎缺陷，不重试，`infra.egress` |
| 409 | `job_revoked` | 对已吊销的 `job_id` 签发 | 新尝试用新 `job_id` |
| 409 | `stale_rev` | 下发的配置 `rev` 小于已应用的 | 忽略（已有更新的视图） |
| 409 | `conflict` | `request_id` 重放但内容不同；回收仍被引用的 secret | 调用方缺陷 |
| 410 | `staged_expired` | 激活过期的暂存项 | 重新暂存 |
| 410 | `cursor_expired` | 用量游标早于保留窗口 | 见 §4.5 |
| 422 | `validation_failed` | 字段缺失 / 越界 / 未知字段；`groups` 不在档案内；`secret_ref` 不合规；附 `errors[]` | 调用方缺陷或配置未同步 |
| 422 | `credential_missing` | 签发令牌时 provider 的凭据未激活 | `sandbox` 记 `infra.egress` 并上报；设置页 `status` 已显示 `unresolved_secret_refs` |
| 422 | `config_invalid` | 配置视图未通过复核 | `gantryd` 保留旧 rev，设置页显示"Egress 尚未应用 rev N" |
| 503 | `not_configured` | 从未应用过配置视图 | `gantryd` 先下发配置再重试 |
| 503 | `unavailable` | 启动中、存储不可用、审计写失败 | 退避重试；`sandbox` 超过阈值记 `infra.egress` |
| 500 | `internal` | 未预期错误 | 写操作按各节的幂等语义重试 |

调用方的映射：签发 / 吊销失败对应 worker-protocol §9 的 `infra.egress`；`models` 的直接调用遇到控制面或回环不可用时返回 `unavailable`，不降级为直连上游（模型 key 只在 Egress）；`api` 的 secret 暂存失败 → 该 settings 写入失败，不追加 `settings.updated`。

## 7. 与其他契约的关系

| 文档 | 关系 |
| --- | --- |
| [worker-protocol.md](worker-protocol.md) | §2.2 的模型 base URL 与 `HTTPS_PROXY`（含 `job_token`，P-72）来自 §4.1 的响应；§2.3 E1–E7 由本文与 modules/egress.md 实现；§3.2 阶段 1（签发）、6（吊销并取最终计量）、7（确认已吊销）调用本文 §4.1–§4.2；§7.4 的计量口径即本文 §4.2 / §4.5 |
| [config.md](config.md) | §2.4 的 secret 转交与 `present` 即本文 §4.4；§2.6 的 `egress.*` 热加载即本文 §4.6；§2.8 `status` 的 `modules.egress` 来自本文 §4.7；§4 的 `levels.token_weights` 随 §4.6 下发、随令牌快照；§8 的 `egress` resource 是视图的来源 |
| [api.md](api.md) | §4.9 settings 写入中的凭据字段经本文 §4.4 转交；客户端从不直接接触本文的任何端点 |
| [data-model.md](../data-model.md) | `Attempt.usage` 取自 §4.2 的 `FinalUsage`；`BudgetLedger.subject`（`delivery` / `conversation`）即本文的 `ledger`；`Workflow.conversation_id`（None 档临时图）对应 `ledger.subject = conversation` |
| [architecture.md](../architecture.md) | §3 部署拓扑、§5 信任边界（Egress 行、沙盒行）、§6 状态归属（令牌只存哈希）、§9 模型调用 |
| [modules/egress.md](../modules/egress.md)、[modules/sandbox.md](../modules/sandbox.md)、[modules/models.md](../modules/models.md)、[modules/deploy.md](../modules/deploy.md) | 实现细节；与本文冲突时以本文为准（AGENTS.md 的优先级） |

## 8. 测试与验收

`EgressStub`（M0）实现本文全部控制面操作，并提供一个能接收 `/j/…` 与 `/s/…` 的假数据面（返回脚本化的模型响应与用量）。一致性测试对 `EgressStub` 与真实 Egress 同样运行：

- [ ] 非 `gantry` 用户连接 `egress.sock` → 连接被关闭、无 HTTP 响应、审计有 `control.peer_not_allowed`；`gantry-sbx` 用户无法打开该 socket。
- [ ] 签发 → 用 `model_base_urls` 调模型成功并计量 → 吊销 → 同一令牌再请求得到 `403 token_invalid`；吊销重复调用返回同一份 `FinalUsage`。
- [ ] 在沙盒侧使用 `svc_token`、在回环使用 `job_token`、使用伪造 / 过期令牌 → 全部 `403 token_invalid`，响应完全相同，不计入任何账本。从沙盒网段以外使用 `job_token` → 同样拒绝：**需核实**（P-73），在目标主机的 rootless Podman 网络下验证；不能强制时断言 `status.detail` 含 `source_check: unverified`。
- [ ] `CONNECT`：`HTTPS_PROXY=http://job:<job_token>@…` 到白名单主机 → 隧道建立，审计按 `job_id` 记录目标与字节数；不带 `Proxy-Authorization` → `407`；令牌无效或用户名不是 `job` → `403`；目标不在有效分组 → `403`；尝试吊销后同一 `HTTPS_PROXY` → `403`（P-72）。
- [ ] `svc_token`：签发第二枚 → 第一枚立即 `403`，`revoked_previous` 列出它；回环请求缺 `X-Gantry-Ledger` → `400 ledger_header_invalid` 且无用量记录；用量记录的 `ledger`、`tier` 与请求头一致；`usage?svc_token_id=` 返回该令牌的全部记录（P-74）。
- [ ] 同一 `job_id` 以相同绑定重复签发 → 新令牌可用、旧令牌失效、用量不清零；以不同绑定签发 → `409 binding_mismatch`。
- [ ] 加权用量达到 `tokens_total` → 后续请求 `403 budget_exhausted`，`FinalUsage.capped = true`。
- [ ] secret：暂存后数据面仍用旧值；激活后新请求用新值；丢弃后 `present` 不变；任何响应、日志、审计里都搜不到值；暂存过期后激活 → `410 staged_expired`。
- [ ] 用量拉取：重复拉取同一游标得到同一批记录；`svc` 记录按 `call_id` 可查；`job` 记录的累计与 `FinalUsage` 一致。
- [ ] 配置视图：`rev` 回退 → `409 stale_rev`；非法视图（含 `api.github.com`）→ `422 config_invalid`，旧视图继续生效、`health = degraded`。
- [ ] Egress 重启：已签发未吊销的令牌继续有效（登记表持久化，只存哈希）；有在途请求的尝试 `incomplete = true`。
- [ ] 访问日志与审计里没有任何令牌原文（对日志做令牌前缀 `gjt1_` / `gst1_` 的全文搜索）。

## 9. 开放问题

骨架没有定的点。"本文暂用"只是让契约能写出 schema 的临时处理，**不是决定**。

| # | 问题 | 本文暂用 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | ~~`CONNECT` 怎样识别尝试~~；私有包仓库反向代理的路由形状 | `CONNECT` 已决：见 decisions.md P-72（`Proxy-Authorization` 携带 `job_token`）。私有包仓库路由仍为 **[提议]** `/r/<job_token>/<injection_id>/…`（M2，modules/egress.md 对齐） | `egress`、`sandbox`、`worker` |
| OQ-2 | "只能从沙盒网段使用"如何强制：rootless Podman 下 Egress 能否看到沙盒的真实源地址 | 已转为 decisions.md P-73：标"需核实"，M1 在目标主机上核实（`sandbox`、`deploy` 的 `[集成:podman]` 用例）；核实前不承诺此项，`status.detail` 标明；安全底线是能力令牌本身 | `sandbox`、`egress`、`deploy` |
| OQ-3 | ~~`svc_token` 的粒度、有效期与封顶~~ | 已决：见 decisions.md P-74（每个 `gantryd` 进程一枚，台账由请求头 `X-Gantry-Ledger` 声明；Egress 不对它封顶，`models` 派发前检查台账余额） | — |
| OQ-4 | 尝试用量的记账时点：吊销时一次记账，还是按 §4.5 的记录实时增量记账（影响运行中的预算预警与并发尝试争用，worker-protocol OQ-14） | 吊销时一次记账；实时记录只用于展示与预警 | `engine`、`sandbox` |
| OQ-5 | socket 位置与属主：modules/deploy.md 目前写的是 `/run/gantry-egress/egress.sock`（目录 `gantry-egress:gantry-link 0750`），本文按 P-50 定为 `/run/gantry/egress.sock`，并提议由 systemd socket 激活创建 | 以本文为准，deploy 规格对齐 | `deploy`、`egress` |
| OQ-6 | Egress 数据库（令牌登记、用量记录）丢失或损坏时的恢复：`gantryd` 如何发现未吊销的令牌、如何补齐用量 | 全部现存令牌视为作废（运行中的尝试会因模型请求失败而终止，记 `infra.egress`），用量按 `incomplete` 处理 | `egress`、`engine` |
| OQ-7 | 长时尝试（Ultimate 子交付单元）的令牌是否需要中途轮换；令牌长度与前缀是否需要版本化之外的校验位 | 不轮换；`gjt1_` / `gst1_` 前缀即版本 | `egress` |
| OQ-8 | ~~控制面与 `models` 通道缺契约（modules/egress.md OQ-E2、models.md OQ-2）~~ | 已决：见 decisions.md P-50（本文） | — |
| OQ-9 | ~~沙盒调用方按网络身份（源地址）映射到 job~~ | 已决：见 decisions.md P-49（`job_token`） | — |
| OQ-10 | ~~`models` 经 Unix socket（`models_uds`）还是 HTTP 调用 Egress~~ | 已决：见 decisions.md P-50（回环 HTTP + `svc_token`；`egress.sock` 只承载控制面） | — |
