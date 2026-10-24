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
| 阶段 Stage | 交付单元的生命周期：规格 → 计划 → 实现 → 提出 → 评审 → 合并 → 关闭 |
| 图 Workflow | 一个阶段内展开的 DAG；节点是 agent 任务，边表示消费上游产物 |
| 角色 / 层级 / 执行位置 role / tier / exec | 节点的三个独立字段：做什么、用哪类模型、在哪里跑（沙盒 / 沙盒只读 / 直接调用），见 §9 |
| 节点 / 尝试 Node / Attempt | 节点是图上的一个任务；每次执行是一个尝试，尝试不可变 |
| Gate / 判定 | 负责判断的节点，输出 verdict：`ACCEPT` / `REVISE` / `REJECT` / `BLOCKED`；需求准入的 READY / NOT_READY 即 `ACCEPT` / `REVISE`，见 §8 |
| reasoner / arbiter | reasoner 预演执行路径；arbiter 对一组 review 结果做唯一裁决 |
| Gantry 审批 | 用户用设备私钥对"动作 + 目标 + 主体摘要（`subject_digest`）"签名，授权 Gantry 执行一个危险动作或一次敏感的内部操作；PR 类动作的主体摘要是 head SHA，补丁审批是补丁内容的 sha256 |
| GitHub 审批 | GitHub 上的 PR review approve。与 Gantry 审批是**两件事**，见 §6.4 |
| 镜像仓 Mirror | 每个项目在服务端的裸镜像仓，由 `forge` 拉取更新，由 `repo` 维护；沙盒工作区、工作树都从它准备 |
| 工作树 Worktree | 服务端上某分支的检出，归属一个交付单元 |
| 沙盒 Sandbox | 一次性的隔离执行环境，worker 在其中运行 |
| 三类模型 | 打杂（Utility）、大脑（Planner）、干活（Worker），见 §9 |
| 后台守护 Guardian | PR 打开后由事件驱动的审核与修复循环 |
| 上报 | 守护或引擎遇到需要人决定的事，写入收件箱并通知 |

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
  SBX -- "只经 Egress" --> EGR
  CORE -- "models 调用也经 Egress" --> EGR
  EGR -- "注入 key" --> LLM
  FORGE -- "App token" --> GH
  GH -- "Webhook（独立入口，HMAC）" --> FORGE
```

服务端第一版是**单个进程 + 外部沙盒运行时**：模块是同一个 Rust workspace 里的 crate，进程内调用；沙盒由 rootless Podman（+ gVisor）拉起，与服务端进程使用不同的系统用户。[提议]

## 4. 模块地图

每个模块有一份规格书 `docs/modules/<id>.md`，按 [模板](modules/_template.md) 写明职责、IO、依赖、状态、错误、验收。

| ID | 模块 | 职责（一句话） | 位置 | 依赖 |
| --- | --- | --- | --- | --- |
| `core` | 领域核心 | 领域类型、ID、状态机、事件定义；无 IO | 共享 crate | — |
| `store` | 事件存储 | 追加式事件日志、投影、游标、Outbox 表 | 服务端 | core |
| `engine` | 编排引擎 | 档位 → 图模板、调度、预算、阶段机推进 | 服务端 | core, store, models, sandbox, repo, policy |
| `conversation` | 会话服务 | 主对话、意图解析、把消息变成交付单元或查询 | 服务端 | core, store, engine, models |
| `guardian` | PR 后台守护 | 消费 PR 事件，分流自愈 / 上报 | 服务端 | engine, forge, policy |
| `forge` | GitHub 接入 | App 鉴权、API、Webhook 接收、对账、Outbox 执行、拉取更新镜像仓 | 服务端 | core, store, repo |
| `repo` | 仓库与工作树 | 每项目裸镜像仓、工作树、写权锁、从镜像准备沙盒工作区、在干净检出上应用补丁并调 `policy` 校验、生成提交（推送仍经 `forge` Outbox） | 服务端 | core, store, policy |
| `sandbox` | 沙盒管理 | 驱动、挂载 `repo` 准备的工作区、资源限制、产物提取 | 服务端 | core, repo |
| `egress` | 出网代理 | 模型 API 反向代理（注入 key、按调用方计量 token）、包仓库 CONNECT 白名单、私有包仓库反向代理、出网审计 | 服务端（独立进程） | — |
| `worker` | Worker 适配 | 在沙盒内运行 agent CLI，执行 Worker 协议 | 沙盒内 + 服务端 | sandbox |
| `models` | 模型路由 | 三类模型配置、直接调用（经 Egress）、计费记账 | 服务端 | core, store |
| `policy` | 策略与审计 | 危险动作判定、路径规则、审批验签、审计日志 | 服务端 | core, store |
| `notify` | 收件箱与通知 | 上报项、桌面/手机推送 | 服务端 | core, store |
| `api` | API 网关 | HTTP API、事件流、设备鉴权 | 服务端 | 全部服务端模块, pki |
| `pki` | 证书与配对 | 私有 CA、配对、签发、轮换、吊销；主机应急命令 `gantry device recover` | 服务端 | store, secrets |
| `secrets` | 密钥存储 | 服务端密钥的加密存储、按使用方隔离读取（`forge`：App 私钥与 webhook secret；`pki`：中间 CA 私钥；`egress`：模型 key 与私有包仓库凭据）、轮换、加密备份 | 服务端（库，各进程各自实例） | core |
| `desktop-shell` | 桌面外壳 | Tauri 进程：密钥存储、签名、mTLS 客户端、IPC 边界 | 桌面端 | api 契约 |
| `desktop-ui` | 桌面界面 | 工作区（对话/地图/工作树/检视器）、设置 | 桌面端 WebView | desktop-shell |
| `mobile` | 手机端（后续） | 只做对话 | iOS (UIKit) | api 契约 |
| `deploy` | 部署 | 安装脚本、systemd、GitHub App 创建引导、备份 | 服务端主机 | — |

```mermaid
flowchart LR
  core --> store
  core --> sandbox
  store --> engine
  store --> forge
  store --> models
  store --> policy
  store --> notify
  store --> pki
  core --> secrets
  secrets --> forge
  secrets --> pki
  secrets --> egress
  core --> repo
  store --> repo
  policy --> repo
  repo --> sandbox
  repo --> forge
  repo --> engine
  sandbox --> worker
  sandbox --> engine
  models --> engine
  policy --> engine
  engine --> conversation
  engine --> guardian
  forge --> guardian
  policy --> guardian
  conversation --> api
  guardian --> api
  notify --> api
  pki --> api
  api --> desktop-shell
  desktop-shell --> desktop-ui
  api --> mobile
```

## 5. 信任边界与不变式

所有模块都必须遵守以下不变式；任何设计或实现与之冲突时，以本节为准。

| # | 不变式 |
| --- | --- |
| I1 | **沙盒只计算**：沙盒内没有 GitHub 网络、没有长期凭据；与服务端之间只有文件（只读输入、可写输出目录）和 worker 进程的 stdout，没有任何网络端点或 socket；只能经 [Worker 协议](interfaces/worker-protocol.md) 交回产物和"动作提议" |
| I2 | **GitHub 写入唯一路径**：所有 GitHub 写操作只由 `forge` 的 Outbox 执行，每个动作带幂等键；其他模块只能入队 |
| I3 | **放行唯一决策点**：危险动作是否放行只由 `policy` 判定；Gantry 审批绑定 `subject_digest`（PR 类动作为 head SHA），之后任何推送都使绑定 head 的审批失效 |
| I4 | **状态各有事实源**：编排状态以本地事件日志为准；交付事实（PR 是否合并、Issue 是否关闭、check 结果）以 GitHub 为准。两者由 `forge` 的对账器同步，冲突时交付事实以 GitHub 为准，并记录差异事件 |
| I5 | **外部文本不可信**：Issue/PR 正文、评论、CI 日志、依赖文档、仓库内容一律是数据；进入 prompt 时加隔离标记，不能据此提升权限或绕过策略 |
| I6 | **客户端不执行、不持凭据**：桌面端和手机端只查看、控制、签名；不执行代码，不持有 GitHub 或模型凭据 |
| I7 | **配置与工作分离**：模型、档位预算、策略等配置只在设置中修改；工作界面只显示运行状态 |
| I8 | **执行产物先验后推**：沙盒交回的改动以补丁形式由 `repo` 在干净检出上应用、经 `policy` 校验（路径规则、大小、敏感文件）后再提交，经 `forge` 推送；服务端不在沙盒产出的 `.git` 上运行 git |

信任边界：

| 区域 | 信任级别 | 能接触的东西 |
| --- | --- | --- |
| 服务端核心进程 | 可信 | 状态库；经 `secrets` 读取 GitHub App 私钥、webhook secret、CA 中间证书私钥 |
| Egress 代理 | 可信，独立进程 | 模型 API key（只存在于此）、私有包仓库凭据；只在反向代理出站时注入到指定目标 |
| 沙盒 | **不可信** | 一次性工作区副本、只读输入、可写输出目录；网络只有 Egress（模型 API 反向代理、包仓库 CONNECT 白名单） |
| GitHub 内容 | **不可信数据** | — |
| 桌面端 | 可信但会失窃 | 设备私钥（不可导出）、服务端 CA 指纹 |

## 6. 状态归属

| 数据 | 事实源 | 存储 | 备份 |
| --- | --- | --- | --- |
| 图、节点、尝试、预算消耗 | 本地事件日志 | SQLite `events` + 投影 | 需要 |
| 交付单元的阶段 | 本地事件日志（由 GitHub 证据驱动推进） | 同上 | 需要 |
| PR 状态、合并、Issue 关闭、check 结果 | GitHub | 本地只缓存快照 | 不需要（可重建） |
| Outbox 待执行动作 | 本地 | `outbox` 表 | 需要 |
| 会话消息、收件箱 | 本地 | 事件日志 + 投影 | 需要 |
| Gantry 审批记录、审计日志 | 本地，追加式 | 事件日志 | 需要 |
| 设备、证书、吊销表 | 本地 | `pki` 表 | 需要 |
| CA 根私钥 | 离线 | 部署时导出到离线介质，服务端不常驻 | 离线保存 |
| CA 中间私钥、App 私钥、webhook secret | 本地密钥文件（加密） | `secrets/`，仅服务端用户可读 | 需要（加密备份） |
| 模型 key、私有包仓库凭据 | 本地密钥文件（加密） | Egress 的密钥存储，仅 Egress 进程用户可读；设置写入时 `api` 只转交、不留存 | 需要（加密备份） |
| 镜像仓 | GitHub | 每项目一个裸仓（`repo` 维护） | 不需要（可重建） |
| 工作树内容 | GitHub 分支 + 本地检出 | 文件系统 | 不需要（可重建） |
| 尝试结果（`result_ref`）、进度记录、被拒产物 | 本地 | 内容寻址结果库（CAS） | 需要（保留期待定） |

## 7. 核心流程

### 7.1 首次部署与配对

1. 在 Ubuntu 上运行安装脚本：创建系统用户、目录、systemd 服务；生成离线根 CA（导出备份后从服务端删除根私钥）和在线中间 CA；签发服务端证书。
2. 安装脚本输出 GitHub App 创建链接（App Manifest 流程），用户在 GitHub 上确认创建并安装到选定仓库；服务端保存 App 私钥与 webhook secret。
3. 配置 webhook 入口：公网主机直接暴露 `/webhook` 路径；内网主机用只转发 `/webhook` 的隧道（如 cloudflared），**不使用 smee.io**。
4. 服务端执行 `gantry pair`，显示二维码 / 配对串（地址 + CA 指纹 + 一次性 token）。
5. 桌面端扫码，本地生成密钥对，用 CA 指纹固定信任，经**单向 TLS 的配对端点**提交 CSR + token，拿到客户端证书；两端显示 6 位确认码，人工核对。第一台配对设备成为 approver（所有者），之后配对的设备默认 operator。
6. 之后桌面端所有请求走 mTLS。用户在设置页填入模型提供方凭据（凭据写入需要审批设备签名；只发送到服务端，由 Egress 保存）。

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
  E->>E: 规格准入（admission）→ ACCEPT
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
2. 会话服务用 `intent` 角色解析意图：查询类直接回答（涉及代码的问答走 `answer`）；任务类创建或定位交付单元。
3. 引擎按档位选图模板，进入"规格"阶段做准入（`admission`）；`REVISE`（NOT_READY）时在对话里追问。
4. 实现阶段：引擎为节点分配沙盒，worker 执行，交回补丁。
5. `repo` 在干净检出上应用补丁、经 `policy` 校验、生成提交；经 Outbox 推送分支、创建 PR（"提出"阶段）。
6. 评审阶段由档位决定图的形态（见 §8），PR 打开后后台守护接管 CI 与 review 事件。
7. 合并前经策略判定；需要时请求 Gantry 审批（设备签名），并遵守仓库的分支保护（见 §7.4）。默认分支前移（`forge.base_advanced`）时，处于 `merge` 阶段的交付单元回到评审重验。
8. GitHub 确认合并、关联 Issue 关闭后，交付单元进入"关闭"。

### 7.3 PR 后台守护

守护订阅交付单元 PR 的 webhook 事件，按 [modules/guardian.md](modules/guardian.md) 的分流规则处理：

- **静默处理**：本 PR 范围内的 CI 失败（非 workflow 文件、非依赖清单）、经验证不成立的非安全类 bot finding（回复理由）、普通 nit（攒到下次推送）。
- **上报**：security 类 finding、改动会触及 `.github/workflows/`、依赖清单或 lockfile、修复会扩大范围、同类问题重复或无进展、预算耗尽、需要任何审批、审批之后还需要推送。
- **兜底**：不在规则表里的事件一律上报。
- 守护忽略由 Gantry 自身动作产生的事件（按 Outbox 记录的动作 ID 识别），防止自触发循环。
- 唤醒以 webhook 为主；另有低频**对账巡检**补齐丢失或乱序的投递。

### 7.4 危险动作与两类审批

- **Gantry 审批**：授权 Gantry 以 App 身份执行一个危险动作：`merge_pr`、force push、推送到非 `gantry/` 前缀分支、改动 workflow / 依赖清单 / lockfile 的补丁（M1 一律拒绝并上报，M2 起需要审批）。删分支、发 release、改仓库设置第一版不实现（forge-actions 预留，策略默认 `deny`）。审批绑定 `(动作, 目标, subject_digest)`，由设备私钥签名，服务端验签后放行。
- 同一签名原语（challenge → 规范化载荷 → 设备签名 → 服务端验签，[api.md §6](interfaces/api.md)）也用于内部动作：涉及凭据或放宽安全的设置写入（`settings_update`）、改设备角色（`device_role_change`）、吊销设备（`device_revoke`）。
- **GitHub 审批**：仓库分支保护要求的 review。Gantry 用 App 创建的 PR，App 自己不能 approve；所以当仓库要求人工 review 时，**用户必须在 GitHub 上 approve**（第一版在控制台给出跳转链接）。
- merge 时两者都要满足：先满足 GitHub 规则（必需 review、check、merge queue），再凭 Gantry 审批执行 merge。
- 审批后若有新推送，审批失效，重新进入评审。守护在已审批的 PR 上**不得**静默推送。

## 8. 档位、阶段与图

档位是**上限**：引擎可在上限内按风险自动加码（例如改动触及 auth 路径时插入对抗 review 节点），不能超过上限；需要超过时上报，交付单元暂停在当前阶段等待用户。

| 档位 | 是否建交付单元 | 走的阶段 | 实现阶段图 | 评审阶段图 |
| --- | --- | --- | --- | --- |
| None | 否，纯对话 | — | — | — |
| Low | 是 | 规格(免准入) → 实现 → 提出 → 合并 → 关闭 | 单节点 `single` | 无 review 图，只等 CI |
| Medium | 是 | 全部 | plan → build → verify | 单 reviewer |
| High | 是 | 全部，含准入 | plan → build → verify | 多角色并行 review + 汇总节点 `summarize` |
| Max | 是 | 全部，含准入 | 同上 + reasoner 预演 | 对抗 review + arbiter 裁决，有限轮 |
| Ultimate | 是，按 Milestone 拆成多个 | 每个子交付单元按 Max | 多交付单元并行，带依赖 | 每个 PR 按 Max |

- Low 也走 PR，不直推默认分支（默认分支通常受保护）。
- 每个档位绑定一个**预算模板**（token、节点数、轮次、墙钟、并发、GitHub 写操作累计数；写操作的按小时限速另由配置与 Outbox 执行）。预算按档位上限记账；自动加码不追加预算。
- 降档不能撤销已经触发的闸门。
- 迭代是有界的图改写：`REVISE` 追加一轮修正 + 复审节点；超过轮次上限或判定"无进展"即上报。
- `REJECT`：交付单元进入 `waiting_human`，上报 `decision_needed`（放弃或重新规划）；`BLOCKED`：上报 `escalation` 并附证据。

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
| `plan` | 把规格拆成步骤 | planner | `sandbox_ro`（沙盒只读） |
| `build` | 实现 / 修复 | worker | `sandbox` |
| `verify` | 在候选 head 上跑检查并判定 | worker | `sandbox` |
| `single` | Low 档单节点：实现 + 自检 | worker | `sandbox` |
| `review:<视角>` | 某视角的评审 | worker | `sandbox_ro` |
| `reasoner` | 预演执行路径 | planner | `sandbox_ro` |
| `arbiter` | 对一组 review 做唯一裁决 | planner | `sandbox_ro` |
| `summarize` | High 档汇总 review | planner | `direct`（直接调用） |
| `admission` | 需求准入 | planner | `direct` |
| `heal` | 后台守护的自愈 | worker | `sandbox` |
| `answer` | None 档涉及代码的问答 | worker | `sandbox_ro` |
| `intent` | 意图解析 | planner | `direct` |
| `classify` | CI 日志分类、事件摘要、状态问答（及提交说明、通知文案等打杂文本） | utility | `direct` |

- 推理深度随档位，由配置决定（[config.md §3](interfaces/config.md)）。
- 所有模型调用都经 Egress 的反向代理：沙盒内的 agent CLI 与服务端 `models` 都把模型 base URL 指向 Egress，由 Egress 注入 key 并按调用方计量 token（沙盒调用方以沙盒网络身份映射到 job，不信任自报）。模型 key 只存在 Egress。
- 对话气泡的边框颜色标识发言的 agent 类别。

## 10. 技术选型 [提议]

| 部分 | 选型 |
| --- | --- |
| 服务端 | Rust：tokio、axum、rustls、rcgen、sqlx + SQLite |
| 事件流 | Server-Sent Events，带游标续传 |
| 沙盒 | rootless Podman + gVisor（runsc）；驱动接口可扩展到 Firecracker |
| Egress 代理 | 独立进程：模型 API 与私有包仓库走反向代理（注入凭据、计量）；公共包仓库走 HTTPS CONNECT 白名单（不注入凭据）；依赖指向 `github.com` 源默认拒绝（Q-08） |
| Worker | Claude Code（headless）为第一实现；Codex 等作为后续适配 |
| 桌面端 | Tauri 2 + TypeScript 前端 |
| 手机端 | iOS UIKit（后续） |
| 契约 | OpenAPI 3.1（客户端 API）、JSON Schema（事件、Worker 协议、配置） |

## 11. 扩展点

| 扩展点 | 接口 | 第一版实现 |
| --- | --- | --- |
| Forge | `Forge` trait（读、写动作、事件归一化） | GitHub.com |
| 沙盒驱动 | `SandboxDriver` trait | rootless Podman + gVisor |
| Worker | `WorkerAdapter` trait + Worker 协议 | Claude Code |
| 通知 | `Notifier` trait | 桌面端系统通知 |
| 模型提供方 | `ModelProvider` trait | Anthropic API |

## 12. 文档地图

| 文档 | 内容 |
| --- | --- |
| [decisions.md](decisions.md) | 决策记录：已确定 / 提议 / 待决 |
| [data-model.md](data-model.md) | 实体、状态机、事件 |
| [interfaces/](interfaces/) | 客户端 API、Worker 协议、Forge 动作、配置 schema |
| [modules/](modules/) | 每个模块的规格书 |
| [prototype.md](prototype.md) | 界面原型说明 |
| [milestones.md](milestones.md) | 可并行开发的里程碑 |
| [design.md](design.md) | 设计讨论记录（历史） |
