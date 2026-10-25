# Gantry 架构设计

> 状态：设计阶段 v0.1（2026-10-24）。决策的确定程度见 [decisions.md](decisions.md)；本文中标注 **[提议]** 的内容尚未经用户确认。讨论过程见 [design.md](design.md)。

本文是所有模块共同遵守的"总图"：目标与边界、模块地图、信任边界与不变式、状态归属、核心流程、档位与模型分层。单个模块怎么做见 [modules/](modules/)，模块之间的 IO 契约见 [interfaces/](interfaces/)，数据结构见 [data-model.md](data-model.md)。

## 1. 目标与非目标

**目标**：一个基于 DAG 编排、与 GitHub 业务强关联、按阶段管理交付的 AI 开发平台。用户在对话里描述任务，Gantry 按档位把任务展开成一张由多个 agent 节点组成的图，在服务端沙盒里执行，以 GitHub 的 Issue / PR / Check / Review 作为交付证据，把工作推进到合并。

Gantry 中的"AI IDE"指**工程编排工作台**：主对话 + 交付关系地图 + 工作树 + 检视器，而不是代码编辑器。

**非目标**（第一版明确不做）：

| 非目标 | 说明 |
| --- | --- |
| 自研代码编辑器 | 人看代码用现有编辑器 `git fetch` agent 推送的分支 [提议] |
| 桌面端本地执行代码 | 执行全部在服务端沙盒 |
| 多租户 SaaS | 第一版单人自托管 [提议] |
| fork PR / 外部贡献者的不可信代码执行 | 第一版不接收，因此不需要 microVM [提议] |
| GitLab / GHES | 第一版只支持 github.com；Forge 层保留接口 |
| 手机端复杂功能 | 手机端只做"给 agent 发消息"的对话 |

## 2. 术语

| 术语 | 含义 |
| --- | --- |
| 档位 Level | None / Low / Medium / High / Max / Ultimate，用户为任务设定的**编排上限** |
| 交付单元 Delivery | 一次"从规格到合并"的工作：绑定 1..n 个 Issue、0..1 个 PR、0..1 个工作树。阶段机作用于交付单元 |
| 阶段 Stage | 交付单元的生命周期：规格 → 计划 → 实现 → 提出 → 评审 → 合并 → 关闭；任何非终态阶段都可进入"放弃"（`abandoned`）。阶段内的状态（`stage_status`）见 [data-model §4.1](data-model.md#41-交付单元阶段) |
| 图 Workflow | 一个阶段内展开的 DAG；节点是 agent 任务，边表示消费上游产物。None 档读代码的问答用挂在会话上的临时图（无交付单元，P-58） |
| 角色 / 层级 / 执行位置 role / tier / exec | 节点的三个独立字段：做什么、用哪类模型、在哪里跑（沙盒 / 沙盒只读 / 直接调用），见 §9 |
| 节点 / 尝试 Node / Attempt | 节点是图上的一个任务；每次执行是一个尝试，尝试不可变 |
| Gate / 判定 | 负责判断的节点，输出 verdict：`ACCEPT` / `REVISE` / `REJECT` / `BLOCKED`；需求准入的 READY / NOT_READY 即 `ACCEPT` / `REVISE`，见 §8 |
| reasoner / arbiter | reasoner 预演执行路径；arbiter 对一组 review 结果做唯一裁决 |
| Gantry 审批 | 用户用设备上的**审批签名密钥**（与 mTLS 客户端密钥分开，每次使用需用户在场，P-47）对"动作 + 目标 + 主体摘要（`subject_digest`）"签名，授权 Gantry 执行一个危险动作或一次敏感的内部操作；PR 类动作的主体摘要是 head SHA，补丁审批（`patch_apply_protected`）是补丁内容的 sha256；各动作的形状见 [data-model](data-model.md#审批动作的-target-与-subject_digest) |
| GitHub 审批 | GitHub 上的 PR review approve。与 Gantry 审批是**两件事**，见 §6.4 |
| 镜像仓 Mirror | 每个项目在服务端的裸镜像仓，由 `forge` 拉取更新，由 `repo` 维护；沙盒工作区、工作树都从它准备 |
| 工作树 Worktree | 服务端上某分支的检出，归属一个交付单元 |
| 沙盒 Sandbox | 一次性的隔离执行环境，worker 在其中运行 |
| 能力令牌 job_token / svc_token | Egress 用来识别调用方并计量的令牌：`job_token` 每个尝试一枚，放在沙盒内模型 base URL 的路径里与包仓库 `CONNECT` 的 `Proxy-Authorization` 里，只对该尝试有效（P-49、P-72）；`svc_token` 每个服务端进程一枚，供 `models` 经回环反向代理调用模型，每次调用以 `X-Gantry-Ledger` 头声明计入哪个台账（P-50、P-74）。见 [interfaces/egress-control.md](interfaces/egress-control.md) |
| 三类模型 | 打杂（Utility）、大脑（Planner）、干活（Worker），见 §9 |
| 后台守护 Guardian | PR 打开后由事件驱动的审核与修复循环 |
| 上报 | 守护或引擎遇到需要人决定的事，写入收件箱（InboxItem）并通知 |
| 端口 Port | 跨模块调用的 trait，统一定义在 `core::ports`（P-51），替身实现同一 trait；写操作的调用方上下文统一为 `core::ports::ActorCtx`（P-78） |

## 3. 部署拓扑

```mermaid
flowchart TB
  subgraph Clients["客户端"]
    D["桌面端 Win/macOS<br/>Tauri：查看 · 控制 · 签名"]
    M["手机端（后续）<br/>只做对话"]
  end
  subgraph Server["服务端 Ubuntu"]
    API["API 网关 + PKI<br/>mTLS · 事件流"]
    CORE["编排核心<br/>会话 · 引擎 · 守护 · 策略"]
    FORGE["Forge<br/>GitHub App · Webhook · Outbox"]
    SBXM["沙盒管理 + 仓库<br/>镜像仓 · 补丁校验"]
    EGR["Egress 代理（独立进程）<br/>模型 API 反向代理 · 包仓库 CONNECT"]
    STORE[("事件日志 + 投影<br/>SQLite · 结果库 CAS")]
  end
  SBX["沙盒 × N<br/>worker 在此运行"]
  GH["GitHub"]
  LLM["模型提供方"]
  D -- "mTLS" --> API
  M -- "mTLS" --> API
  API --- CORE
  CORE --- STORE
  CORE --- FORGE
  CORE --- SBXM
  SBXM -- "只读输入 / 输出目录 / stdout" --> SBX
  SBX -- "只经 Egress（job_token）" --> EGR
  CORE -- "控制面 egress.sock" --> EGR
  CORE -- "models 调用：回环 HTTP + svc_token" --> EGR
  EGR -- "注入 key" --> LLM
  FORGE -- "App token" --> GH
  GH -- "Webhook（反向代理 / 隧道 → 回环入口，HMAC）" --> FORGE
```

服务端第一版是**单个进程 + 外部沙盒运行时 + 独立的 Egress 进程**：模块是同一个 Rust workspace 里的 crate，进程内调用；沙盒由 rootless Podman（+ gVisor）拉起，与服务端进程使用不同的系统用户。[提议] `gantryd` 与 Egress 之间只有主机本地 Unix socket `/run/gantry/egress.sock`（控制面）与回环 HTTP（`models` 的模型调用），契约见 [interfaces/egress-control.md](interfaces/egress-control.md)（P-50）。

## 4. 模块地图

每个模块有一份规格书 `docs/modules/<id>.md`，按 [模板](modules/_template.md) 写明职责、IO、依赖、状态、错误、验收。跨模块调用的端口 trait（`ForgeReader`、`OutboxPort`、`SandboxService`、`ModelProvider`、`RepoService`、`PolicyPort`、`InboxPort`、`EgressControl`，以及跨泳道的服务接口 `DeliveryControl`、`ConversationService`、`WorktreeLocks`、`PrRefresher`、`HealPort`、`ClassifyPort`、`WorkerAdapter`、`SecretSource` / `SecretSink`）统一定义在 `core::ports`，M0 交付（P-51，清单见 [core §6.10](modules/core.md#610-跨模块端口coreportsp-51)）；下表"依赖"列指依赖对方端口的实现，编译期只依赖 `core`。

| ID | 模块 | 职责（一句话） | 位置 | 依赖 |
| --- | --- | --- | --- | --- |
| `core` | 领域核心 | 领域类型、ID、状态机、事件定义、跨模块端口 trait（`core::ports`）；无 IO | 共享 crate | — |
| `store` | 事件存储 | 追加式事件日志、投影、游标、Outbox 表 | 服务端 | core |
| `engine` | 编排引擎 | 档位 → 图模板、调度、预算、阶段机推进；经 `ForgeReader` 按需读取 Issue / PR 正文与评论（不可信文本，不进快照） | 服务端 | core, store, models, sandbox, repo, policy, forge, notify, worker（`WorkerAdapter`） |
| `conversation` | 会话服务 | 主对话、意图解析、把消息变成交付单元或查询；把引擎与守护事件渲染为事件行 | 服务端 | core, store, engine, models, notify |
| `guardian` | PR 后台守护 | 消费 PR 事件，按决策表分流自愈 / 上报 / 无需动作 | 服务端 | core, store, engine, forge, policy, notify |
| `forge` | GitHub 接入 | App 鉴权、API、Webhook 接收、对账、Outbox 执行（执行时经 `policy` 验证并消费审批）、拉取更新镜像仓 | 服务端 | core, store, repo, policy, notify, secrets |
| `repo` | 仓库与工作树 | 每项目裸镜像仓、工作树、写权锁、从镜像准备沙盒工作区、在干净检出上应用补丁并产出 `PatchFacts`（机械检查）交 `policy` 判定、生成提交（提交身份取 `github.commit_identity`；推送仍经 `forge` Outbox）（P-69） | 服务端 | core, store, policy |
| `sandbox` | 沙盒管理 | 对引擎提供 `SandboxService`（提交 JobSpec、等待结果、取消），对下经 `SandboxDriver` 操作容器（P-52）；挂载 `repo` 准备的工作区、资源限制、产物提取；经 Egress 控制面签发 / 吊销 `job_token` | 服务端 | core, repo, egress（控制面） |
| `egress` | 出网代理 | 模型 API 反向代理（注入 key、按 `job_token` / `svc_token` 计量 token）、包仓库 CONNECT 白名单、私有包仓库反向代理、出网审计；控制面契约见 [egress-control.md](interfaces/egress-control.md) | 服务端（独立进程） | secrets |
| `worker` | Worker 适配 | 在沙盒内运行 agent CLI，执行 Worker 协议 | 沙盒内 + 服务端 | sandbox |
| `models` | 模型路由 | 三类模型配置、直接调用（经 Egress 回环反向代理，带 `svc_token`）、计费记账 | 服务端 | core, store, egress（控制面） |
| `policy` | 策略与审计 | 危险动作判定、路径规则、审批验签、审计日志 | 服务端 | core, store |
| `notify` | 收件箱与通知 | 上报项（去重、按项目计数）、桌面 / 手机推送 | 服务端 | core, store, models（可选，通知文案）, secrets（M4 APNs） |
| `api` | API 网关 | HTTP API、事件流、设备鉴权 | 服务端 | 全部服务端模块, pki |
| `pki` | 证书与配对 | 私有 CA、配对、签发、轮换、吊销；主机应急命令 `gantry device recover` | 服务端 | store, secrets |
| `secrets` | 密钥存储 | 服务端密钥的加密存储、按使用方隔离读取（`forge`：App 私钥与 webhook secret；`pki`：中间 CA 私钥；`egress`：模型 key 与私有包仓库凭据；`notify`：M4 APNs 密钥）、轮换、加密备份 | 服务端（库，各进程各自实例） | core |
| `desktop-shell` | 桌面外壳 | Tauri 进程：两把密钥（mTLS 客户端密钥、审批签名密钥，P-47）、签名、mTLS 客户端与证书续期、IPC 边界 | 桌面端 | api 契约 |
| `desktop-ui` | 桌面界面 | 工作区（对话/地图/工作树/检视器）、设置 | 桌面端 WebView | desktop-shell |
| `mobile` | 手机端（后续） | 只做对话 | iOS (UIKit) | api 契约 |
| `deploy` | 部署 | 安装脚本、systemd、GitHub App 创建引导、备份 | 服务端主机 | — |

```mermaid
flowchart LR
  core --> store
  core --> sandbox
  core --> guardian
  store --> engine
  store --> forge
  store --> models
  store --> policy
  store --> notify
  store --> pki
  store --> guardian
  core --> secrets
  secrets --> forge
  secrets --> pki
  secrets --> egress
  secrets --> notify
  core --> repo
  store --> repo
  policy --> repo
  policy --> forge
  notify --> forge
  repo --> sandbox
  repo --> forge
  repo --> engine
  egress -. 控制面 .-> sandbox
  egress -. 控制面 .-> models
  sandbox --> worker
  sandbox --> engine
  worker -. WorkerAdapter .-> engine
  models --> engine
  models -. 可选 .-> notify
  policy --> engine
  forge --> engine
  notify --> engine
  notify --> conversation
  engine --> conversation
  engine --> guardian
  forge --> guardian
  policy --> guardian
  notify --> guardian
  conversation --> api
  guardian --> api
  notify --> api
  pki --> api
  api --> desktop-shell
  desktop-shell --> desktop-ui
  api --> mobile
```

箭头由被依赖方指向依赖方；虚线是跨进程调用（Egress 控制面，P-50）或可选依赖。依赖关系按 P-59 修正。

## 5. 信任边界与不变式

所有模块都必须遵守以下不变式；任何设计或实现与之冲突时，以本节为准。

| # | 不变式 |
| --- | --- |
| I1 | **沙盒只计算**：沙盒内没有 GitHub 网络、没有长期凭据（只有本尝试的 `job_token`，它只能让 Egress 给本尝试记账，P-49）；与服务端之间只有文件（只读输入、可写输出目录）和 worker 进程的 stdout，没有任何网络端点或 socket；只能经 [Worker 协议](interfaces/worker-protocol.md) 交回产物和"动作提议" |
| I2 | **GitHub 写入唯一路径**：所有 GitHub 写操作只由 `forge` 的 Outbox 执行，每个动作带幂等键；其他模块只能入队 |
| I3 | **放行唯一决策点**：危险动作是否放行只由 `policy` 判定（入队方在入队时调用，执行方在执行时验证审批，P-62）；Gantry 审批绑定 `subject_digest`（PR 类动作为 head SHA），之后任何推送都使绑定 head 的审批失效 |
| I4 | **状态各有事实源**：编排状态以本地事件日志为准；交付事实（PR 是否合并、Issue 是否关闭、check 结果）以 GitHub 为准。两者由 `forge` 的对账器同步，冲突时交付事实以 GitHub 为准，并记录差异事件 |
| I5 | **外部文本不可信**：Issue/PR 正文、评论、CI 日志、依赖文档、仓库内容一律是数据；进入 prompt 时加隔离标记，不能据此提升权限或绕过策略 |
| I6 | **客户端不执行、不持凭据**：桌面端和手机端只查看、控制、签名；不执行代码，不持有 GitHub 或模型凭据 |
| I7 | **配置与工作分离**：模型、档位预算、策略等配置只在设置中修改；工作界面只显示运行状态 |
| I8 | **执行产物先验后推**：沙盒交回的改动以补丁形式由 `repo` 在干净检出上应用并做机械检查（产出 `PatchFacts`），经 `policy` 规则判定（路径规则、大小、敏感文件）后再由 `repo` 提交，经 `forge` 推送（P-69）；服务端不在沙盒产出的 `.git` 上运行 git |

信任边界：

| 区域 | 信任级别 | 能接触的东西 |
| --- | --- | --- |
| 服务端核心进程 | 可信 | 状态库；经 `secrets` 读取 GitHub App 私钥、webhook secret、CA 中间证书私钥 |
| Egress 代理 | 可信，独立进程 | 模型 API key（只存在于此）、私有包仓库凭据；只在反向代理出站时注入到指定目标；`job_token` / `svc_token` 的登记表（只存哈希）。控制面只对主机本地的 `gantryd` 进程用户开放 |
| 沙盒 | **不可信** | 一次性工作区副本、只读输入、可写输出目录、本尝试的 `job_token`；网络只有 Egress 的沙盒侧监听器（模型 API 反向代理、包仓库 CONNECT 白名单，均凭 `job_token`，P-72），到不了控制面与回环监听器 |
| GitHub 内容 | **不可信数据** | — |
| 桌面端 | 可信但会失窃 | mTLS 客户端密钥与审批签名密钥（均不可导出；后者每次使用需用户在场，P-47）、离线根 CA 指纹（P-64） |

## 6. 状态归属

| 数据 | 事实源 | 存储 | 备份 |
| --- | --- | --- | --- |
| 图、节点、尝试、预算消耗 | 本地事件日志 | SQLite `events` + 投影 | 需要 |
| 交付单元的阶段 | 本地事件日志（由 GitHub 证据驱动推进） | 同上 | 需要 |
| PR 状态、合并、Issue 关闭、check 结果 | GitHub | 本地只缓存快照 | 不需要（可重建；计数器除外，见下） |
| Outbox 待执行动作 | 本地 | `outbox` 表 | 需要 |
| 会话消息、收件箱 | 本地 | 事件日志 + 投影 | 需要 |
| Gantry 审批记录、审计日志 | 本地，追加式 | 事件日志 | 需要 |
| 设备（含 `approval_pubkey`）、证书、吊销表 | 本地 | `pki` 表 | 需要 |
| 吊销日志 `revocations.journal` | 本地，只追加，**在事件日志之外** | `pki` 目录下的独立文件；每次写入在吊销提交前 `fsync` 并同步复制到备份目标，事务回滚不删除条目 | 不随备份回滚：从备份恢复时重放时间戳晚于备份时刻的条目（P-67、P-80） |
| `log_epoch` | 本地 | 事件日志元数据；每次从备份恢复加一 | 恢复流程维护（P-67） |
| PR 的 `head_epoch`、状态清单评论的 `revision` 计数 | 本地事件日志 | `forge.pr_synchronized` 等事件 | 需要（随日志；不得从可重建的快照推出，P-61） |
| CA 根私钥 | 离线 | 部署时导出到离线介质，服务端不常驻 | 离线保存 |
| CA 中间私钥、App 私钥、webhook secret | 本地密钥文件（加密） | `secrets/`，仅服务端用户可读 | 需要（加密备份） |
| 模型 key、私有包仓库凭据 | 本地密钥文件（加密） | Egress 的密钥存储，仅 Egress 进程用户可读；设置写入时 `api` 经 Egress 控制面只写转交、不留存（P-50） | 需要（加密备份） |
| `job_token` / `svc_token` | Egress | Egress 只存哈希；令牌原文不进事件日志 | 不需要（`job_token` 尝试结束即作废；`svc_token` 随服务端进程重启轮换，P-74） |
| 镜像仓 | GitHub | 每项目一个裸仓（`repo` 维护） | 不需要（可重建） |
| 工作树内容 | GitHub 分支 + 本地检出 | 文件系统 | 不需要（可重建） |
| 尝试结果（`result_ref`）、进度记录、被拒产物 | 本地 | 内容寻址结果库（CAS） | 需要（保留期待定） |

## 7. 核心流程

### 7.1 首次部署与配对

1. 在 Ubuntu 上运行安装脚本：创建系统用户、目录、systemd 服务；生成离线根 CA（导出备份后从服务端删除根私钥）和在线中间 CA；签发服务端证书。
2. 配置 webhook 入口（P-65）：`forge` 的 webhook 监听只在回环地址上以明文 HTTP 提供；对公网暴露一律经终止 TLS 的反向代理或隧道（公网证书，如 cloudflared）。隧道只转发 `/webhook` 与 App 创建期间的 `/setup/github/callback`，**不使用 smee.io**。
3. 运行 `gantry github setup`：生成一次性的 App Manifest 提交页，用户在 GitHub 上确认创建并安装到选定仓库；GitHub 经同一入口的 `/setup/github/callback` 回传 `code`，服务端换取并保存 App 私钥、webhook secret 与提交身份（`github.commit_identity`）（细节需核实，见 [modules/deploy.md](modules/deploy.md)）。
4. 安装引导期（第一台设备配对之前，P-66）：主机命令（需 root）可写入 `github.*`、`sandbox.base_image` 等"仅 approver"配置，并写审计事件；`sandbox.base_image` 默认是随版本发布、固定摘要的官方 worker 镜像。第一台设备配对之后，这类写入改为需要审批签名。
5. 服务端执行 `gantry pair`，显示二维码 / 配对串（地址 + **离线根 CA 指纹** + 一次性 token，P-64）。
6. 桌面端扫码，本地生成**两把**密钥（P-47；手机档案可省略审批签名密钥，且永远不能成为 approver，P-76）：mTLS 客户端密钥与审批签名密钥（Secure Enclave / TPM，不可导出，使用需用户在场）；经**单向 TLS 的配对端点**提交 CSR + `approval_pubkey` + token，拿到证书链 `[叶子, 中间 CA, 根]`，校验根与固定的指纹一致；两端显示 6 位确认码，人工核对。第一台配对设备成为 approver（所有者），之后配对的设备默认 operator。
7. 之后桌面端所有请求走 mTLS；证书剩余 1/3 有效期时经 `POST /v1/devices/self/renew` 自动续期（建议值）。用户在设置页填入模型提供方凭据（凭据写入需要审批签名；只发送到服务端，经 Egress 控制面转交 Egress 保存）。

细节见 [modules/pki.md](modules/pki.md) 与 [modules/deploy.md](modules/deploy.md)。

### 7.2 从对话到合并

```mermaid
sequenceDiagram
  participant U as 用户（桌面端）
  participant C as 会话服务
  participant E as 引擎
  participant S as 沙盒 + worker
  participant P as 策略
  participant F as Forge
  participant G as GitHub
  U->>C: 消息（档位上限 High，@#5）
  C->>E: 创建 / 定位交付单元
  E->>E: 规格：评估 level_effective；High 起准入（admission）→ ACCEPT
  E->>S: 计划阶段（plan，沙盒只读）
  E->>S: 节点任务（Worker 协议 JobSpec）
  S-->>E: 结果（补丁 + 报告 + 动作提议）
  E->>P: 校验补丁（repo 在干净检出上应用）与动作
  P-->>E: 放行 / 需审批 / 拒绝
  E->>F: 入队：推送分支、创建 PR
  F->>G: 执行（幂等键）
  G-->>F: Webhook：check / review
  F-->>E: 对账后的交付事实
  E->>U: 阶段推进事件；需要人时写收件箱
```

1. 用户在主对话里发消息，可带档位上限和 `@` 引用（Issue / PR / 工作树）。
2. 会话服务用 `intent` 角色解析意图：查询类直接回答（涉及代码的问答走 `answer`，挂在会话上的临时图，记会话台账，P-58）；任务类创建或定位交付单元。
3. 引擎按 `level_cap` 固定预算模板，在"规格"阶段按风险评估给出 `level_effective` 初值（≤ 上限，P-57）。High 及以上做准入（`admission`），`REVISE`（NOT_READY）时在对话里追问；Medium、Low 免准入（P-55）。
4. 计划阶段（Low 跳过）：`plan` 节点产出计划（Max 起另有 `reasoner` 预演）。实现阶段：引擎为节点分配沙盒，worker 执行 `build → verify`（Low 为 `single`），交回补丁。
5. `repo` 在干净检出上应用补丁并产出 `PatchFacts`，`policy` 判定，`repo` 生成提交（P-69）；经 Outbox 推送分支、创建 PR（"提出"阶段）。
6. 评审阶段由档位决定图的形态（见 §8），PR 打开后后台守护接管 CI 与 review 事件。
7. 合并前经策略判定；需要时请求 Gantry 审批（设备签名），并遵守仓库的分支保护（见 §7.4）。默认分支前移（`forge.base_advanced`）时，处于 `merge` 阶段的交付单元回到评审重验。
8. GitHub 确认合并、关联 Issue 关闭后，交付单元进入"关闭"。

### 7.3 PR 后台守护

守护订阅交付单元 PR 的 webhook 事件，按 [modules/guardian.md](modules/guardian.md) §7.3 的**有序决策表**处理（P-60）。该决策表是分流规则的**唯一来源**，本节与 [config.md §6](interfaces/config.md#6-guardian) 只是摘要；冲突时以决策表为准。

- **静默处理**（可配置类，默认值见 config §6.1）：本 PR 范围内的 CI 失败（非 workflow 文件、非依赖清单）静默修复；经验证属实的非安全类 bot finding 静默修复；经验证不成立的非安全类 bot finding 回复理由（`reply_review_thread`，M2）；普通 nit 攒到下次推送。
- **无需动作**：决策表里显式列出的 `noop` / `ignore` / `bookkeeping` 行（如 CI 通过、GitHub approve、自身事件回流、陈旧事件），只记账、不打扰（D-04）。
- **上报**（锁定类，不可配置）：security 类 finding、改动会触及受保护路径（`.github/workflows/`、依赖清单、lockfile 等）、修复会扩大范围、同类问题重复或无进展、预算耗尽、需要任何审批、审批之后还需要推送、人类评审意见、出队等。
- **兜底**：决策表之外的事件一律上报。
- 守护忽略由 Gantry 自身动作产生的事件（按 Outbox 记录的动作 ID 识别），防止自触发循环。
- 唤醒以 webhook 为主；另有低频**对账巡检**补齐丢失或乱序的投递。
- 每个分流处置写审计事件 `guardian.triaged`（`event_ref`、`class`、`action`）；请求自愈写 `guardian.heal_requested`，上报写 `guardian.escalated`；会话服务把它们渲染为对话里的事件行。
- 守护的静默自愈**不改变交付阶段**（`heal` 节点挂在 `review` 阶段）；只有需要改代码且超出自愈范围时，引擎才走 `review → implement`。

### 7.4 危险动作与两类审批

- **Gantry 审批**：授权 Gantry 以 App 身份执行一个危险动作：`merge_pr`、force push（`force_push`）、推送到非 `gantry/` 前缀分支（`push_outside_prefix`）、解决含人类评论的 review 线程（`resolve_review_thread_human`）、改动依赖清单 / lockfile 的补丁（`patch_apply_protected`；M1 一律拒绝并上报，M2 起需要审批）。删分支、发 release、改仓库设置第一版不实现（forge-actions 预留，策略默认 `deny`）。审批绑定 `(动作, 目标, subject_digest)`，由设备的审批签名密钥签名（P-47），服务端用登记的 `approval_pubkey` 验签后放行。动作标识与形状见 [data-model](data-model.md#审批动作的-target-与-subject_digest)（P-54）；**审批在执行开始前消费**，执行终态失败不退回。
- **Gantry 不修改 `.github/workflows/`**（P-48）：App 不申请 `workflows` 权限；补丁触及 workflow 文件时 `policy` 一律拒绝（`policy.path_protected:workflows`），上报 `decision_needed`（`payload.kind = manual_action`）并附补丁，由用户手动应用。没有任何审批能放行它。
- 同一签名原语（challenge → 规范化载荷 → 审批签名密钥签名 → 服务端验签，[api.md §6](interfaces/api.md)）也用于内部动作：涉及凭据或放宽安全的设置写入（`settings_update`）、改设备角色（`device_role_change`）、吊销设备（`device_revoke`）。安装引导期（第一台设备配对前）的主机命令写入是唯一例外（P-66）。
- **GitHub 审批**：仓库分支保护要求的 review。Gantry 用 App 创建的 PR，App 自己不能 approve；所以当仓库要求人工 review 时，**用户必须在 GitHub 上 approve**（第一版在控制台给出跳转链接）。
- merge 时两者都要满足：先满足 GitHub 规则（必需 review、check、merge queue），再凭 Gantry 审批执行 merge。
- 审批后若有新推送，审批失效，重新进入评审。守护在已审批的 PR 上**不得**静默推送。

### 7.5 多工作树协作（目标设计，提案）

D-14 与 D-15 记录了用户目标：项目主对话统筹多个工作树并行；同项目 agent 可只读观察、通信和跨树文件阅读。具体机制仍是待契约批准的提案（P-83–P-86，Q-13 未确认），不是当前实现默认，也不改变 I1–I8。

- **独立任务并行**：每个任务保留独立 Delivery、Worktree、Workflow、Attempt、预算和范围；主对话统筹不是共享执行上下文。现行单消息单 Delivery、`source_message_id` 幂等、多 `refs` `Ambiguous` 不变。
- **服务端中介**：观察、消息和跨树读取都经 `engine` 编排、`repo` 快照、`sandbox` 包装、`store` 保存状态；用户显式授权是上限，范围包括协作 delivery、可观察字段、文件路径及快照；服务端计划只能在此上限内分配已授权目标、排序和收窄，不能自行纳入整个项目或新增路径，路径存在也不等于授权。身份只由运行上下文派生。中介与不跨树写是从沙盒只计算与工作树隔离推导的方案约束，不是 I1 / I5 / I8 的字面规定，也不是 D-15 另行拍板的机制。
- **只读边界**：同树仍单写者（租约 + fence），`sandbox_ro` 读固定副本；跨树只读用户授权的已发布快照，内部结果 CAS 读取也受同一授权检查，无跨树写。消息在目标原节点、原 `graph_rev` 的下一次正常调度中绑定，是保守提案，不是命令或已批准结论；当前 Attempt 正常结束不等于目标 node / workflow / delivery 终结，失败或取消不转投后续 Attempt。
- **组合验证**：独立组合验证绑定固定候选 / 基线 SHA、契约版本和结果树摘要；分别通过不等于组合通过，组合通过也不是审批。`policy`、Forge Outbox 与 GitHub 规则保留。
- **档位不变**：基础协作不以 Ultimate 为前提；Ultimate 仍只用于大目标自动拆分和依赖统筹，None 仍纯问答、不创建交付。

契约批准前不新增路由、字段、事件或端口。本轮 UI / shell 不新增路由或 IPC，不代表未来可依赖既有路由实现跨树功能；G1 契约批准后另行定义。详见 [collaboration.md](collaboration.md#多工作树协作目标设计)；门槛见 [milestones.md](milestones.md#协作扩展提议契约批准后)。

## 8. 档位、阶段与图

档位是**上限**：引擎在规格阶段按风险评估给出 `level_effective` 初值（≤ `level_cap`），之后可在上限内按风险自动加码（例如改动触及 auth 路径时插入对抗 review 节点），只升不自动降，不能超过上限；需要超过时上报（`escalation`），交付单元停在当前阶段等待用户（P-57）。

| 档位 | 是否建交付单元 | 走的阶段 | 计划阶段图 | 实现阶段图 | 评审阶段图 |
| --- | --- | --- | --- | --- | --- |
| None | 否，纯对话；读代码的问答建挂在会话上的临时图（`answer`），记会话台账（P-58） | — | — | — | — |
| Low | 是 | 规格（免准入）→ 实现 → 提出 → 评审（只等 CI）→ 合并 → 关闭；**无计划阶段** | — | 单节点 `single` | 无 review 图，只等 CI |
| Medium | 是 | 规格（免准入）→ 计划 → 实现 → 提出 → 评审 → 合并 → 关闭 | `plan` | `build → verify` | 单 reviewer |
| High | 是 | 全部，规格阶段含准入（`admission`） | `plan` | `build → verify` | 多角色并行 review + 汇总节点 `summarize` |
| Max | 是 | 全部，含准入 | `plan` + `reasoner` 预演 | `build → verify` | 对抗 review + arbiter 裁决，有限轮 |
| Ultimate | 是，按 Milestone 拆成多个 | 每个子交付单元按 Max | 每个子交付单元按 Max | 多交付单元并行，带依赖 | 每个 PR 按 Max |

- 准入只在 High 及以上（P-55）：Medium 为 `spec → plan`，Low 为 `spec → implement`，都免准入。
- Low 也走 PR，不直推默认分支（默认分支通常受保护）；Low 的阶段列表包含 `review`，但该阶段没有评审图，只等 CI。
- 每个档位绑定一个**预算模板**（加权 token、节点数、轮次、墙钟、并发、GitHub 写操作累计数；写操作的按小时限速另由配置与 Outbox 执行）。预算模板在交付单元**创建时按 `level_cap` 固定**（P-57）；自动加码与之后调整上限都不改变已固定的预算。token 按 `levels.token_weights` 加权计入（P-44、P-70）。
- 已经触发的闸门永不撤销（降低上限也不撤销）。
- 迭代是有界的图改写：`REVISE` 追加一轮修正 + 复审节点；超过轮次上限或判定"无进展"即上报。
- `REJECT`：`stage_status = waiting_human`，上报 `decision_needed`（放弃或重新规划）；选"重新规划"回到 `plan` 阶段（P-55）。`BLOCKED`：上报 `escalation` 并附证据。
- 控制（P-36、P-37、P-56）：单个交付单元可暂停 / 继续（`stage_status = paused`）、重试失败节点、放弃，均有直达路由（[api.md §4.4](interfaces/api.md#44-deliveries)），不依赖意图解析。

## 9. 三类模型

模型在**设置页**配置（服务端全局，项目可覆盖），工作界面不出现模型配置。

| 类别 | 用途 | 特点 |
| --- | --- | --- |
| 打杂 Utility | 事件摘要、CI 日志分类、状态问答、提交说明、状态清单与通知文案 | 量大，便宜，低推理 |
| 大脑 Planner | 意图解析、规划拆图、需求准入、reasoner 预演、arbiter 裁决、High 档汇总 | 量少，要准，高推理 |
| 干活 Worker | 在沙盒内实现、修复、担任各 review 视角 | 主力产量，推理深度随档位 |

节点的角色（`role`）、模型层级（`tier`）、执行位置（`exec`）是三个独立字段。需要读仓库内容的角色经 [Worker 协议](interfaces/worker-protocol.md) 在沙盒中运行；只处理结构化文本的角色由 `models` 直接调用（无工具、无代码执行）。

| `role` | 用途 | `tier` | `exec` |
| --- | --- | --- | --- |
| `plan` | 把规格拆成步骤（计划阶段） | planner | `sandbox_ro`（沙盒只读） |
| `build` | 实现 / 修复 | worker | `sandbox` |
| `verify` | 在候选 head 上跑检查并判定 | worker | `sandbox` |
| `single` | Low 档单节点：实现 + 自检 | worker | `sandbox` |
| `review:<视角>` | 某视角的评审 | worker | `sandbox_ro` |
| `reasoner` | 预演执行路径（计划阶段，Max 起；不是判定节点，P-45） | planner | `sandbox_ro` |
| `arbiter` | 对一组 review 做唯一裁决 | planner | `sandbox_ro` |
| `summarize` | High 档汇总 review | planner | `direct`（直接调用） |
| `admission` | 需求准入（High 及以上） | planner | `direct` |
| `heal` | 后台守护的自愈 | worker | `sandbox` |
| `answer` | None 档涉及代码的问答（挂在会话上的临时图） | worker | `sandbox_ro` |
| `intent` | 意图解析 | planner | `direct` |
| `classify` | CI 日志分类、事件摘要、状态问答（及提交说明、通知文案等打杂文本） | utility | `direct` |

- 推理深度随档位，由配置决定（[config.md §3](interfaces/config.md)）。
- 所有模型调用都经 Egress 的反向代理，由 Egress 注入 key 并计量 token，不信任任何自报：沙盒内的 agent CLI 把模型 base URL 指向 `http://<egress>/j/<job_token>/<provider>`，`job_token` 每个尝试一枚、只对该尝试有效（"只能从沙盒网段使用"的强制方式需核实，能力令牌本身是安全底线，P-49、P-73）；服务端 `models` 经 Egress 的回环 HTTP 反向代理调用，带进程级 `svc_token`，每次调用以 `X-Gantry-Ledger` 声明计入交付台账或会话台账（P-50、P-74）。两种令牌同一计量口径，契约见 [interfaces/egress-control.md](interfaces/egress-control.md)。模型 key 只存在 Egress。
- 对话气泡的边框颜色标识发言的 agent 类别。

## 10. 技术选型 [提议]

| 部分 | 选型 |
| --- | --- |
| 服务端 | Rust：tokio、axum、rustls、rcgen、sqlx + SQLite |
| 事件流 | Server-Sent Events，带游标续传 |
| 沙盒 | rootless Podman + gVisor（runsc）；驱动接口可扩展到 Firecracker |
| Egress 代理 | 独立进程：模型 API 与私有包仓库走反向代理（注入凭据、按 `job_token` / `svc_token` 计量）；公共包仓库走 HTTPS CONNECT 白名单（不注入凭据）；依赖指向 `github.com` 源默认拒绝（Q-08）；`gantryd` 经 `/run/gantry/egress.sock` 调用控制面 |
| Worker | Claude Code（headless，适配器标识 `claude-code`）为第一实现；Codex 等作为后续适配 |
| 桌面端 | Tauri 2 + TypeScript 前端 |
| 手机端 | iOS UIKit（后续） |
| 契约 | OpenAPI 3.1（客户端 API）、JSON Schema（事件、Worker 协议、配置） |

## 11. 扩展点

跨模块端口都定义在 `core::ports`（P-51）；下表是其中可替换实现的扩展点。

| 扩展点 | 接口 | 第一版实现 |
| --- | --- | --- |
| Forge | `ForgeReader`（读）、`OutboxPort`（写动作入队）+ 事件归一化 | GitHub.com |
| 沙盒 | `SandboxService`（引擎面：提交 JobSpec、等待结果、取消）与 `SandboxDriver`（容器面），P-52 | rootless Podman + gVisor |
| Worker | `WorkerAdapter` trait（`core::ports`）+ Worker 协议 | Claude Code |
| 通知 | `InboxPort`（上报）+ `Notifier` trait（推送） | 桌面端系统通知（客户端收到 `severity ≥ warn` 的 `inbox.raised` 时弹出，P-63） |
| 模型提供方 | `ModelProvider` trait | Anthropic API |

## 12. 文档地图

| 文档 | 内容 |
| --- | --- |
| [decisions.md](decisions.md) | 决策记录：已确定 / 提议 / 待决 |
| [data-model.md](data-model.md) | 实体、状态机、事件 |
| [interfaces/](interfaces/) | 客户端 API、Worker 协议、Forge 动作、配置 schema、Egress 控制面 |
| [modules/](modules/) | 每个模块的规格书 |
| [prototype.md](prototype.md) | 界面原型说明 |
| [collaboration.md](collaboration.md) | 多工作树协作的目标设计（待契约批准的提案） |
| [milestones.md](milestones.md) | 可并行开发的里程碑 |
| [design.md](design.md) | 设计讨论记录（历史） |
