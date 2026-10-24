# 决策记录

> 每条决策有唯一编号和状态。**已确定** = 用户拍板；**提议** = 助手提出、用户未确认，可被推翻；**待决** = 需要用户决定。实现时以"已确定"为硬约束，"提议"为默认值。

## 已确定

| # | 决策 | 来源 |
| --- | --- | --- |
| D-01 | 项目名 Gantry；是 GraphAgent v1 与 SpecGit 的合并继任者 | 用户 |
| D-02 | 基于 DAG 编排、与 GitHub 业务强关联、按阶段管理交付 | 用户 |
| D-03 | 编排档位 None / Low / Medium / High / Max / Ultimate | 用户 |
| D-04 | PR 审核与修复作为后台任务，没有问题不打扰 | 用户 |
| D-05 | CS 架构：服务端 Ubuntu，桌面端 Windows / macOS | 用户 |
| D-06 | 首次连接双向配对，私有 CA 生成证书并部署，确保通信安全 | 用户 |
| D-07 | 需要沙盒机制 | 用户 |
| D-08 | 三类模型：打杂、大脑、干活 | 用户 |
| D-09 | 模型等配置放在设置页，不放在工作页面 | 用户 |
| D-10 | 主对话窗口是核心界面；顶部 tab 切换项目；左侧为项目信息（进行中 Issue / PR、工作树入口） | 用户 |
| D-11 | 界面工程化、现代、紧凑；未来手机版用 UIKit，且手机端只给 agent 发消息 | 用户 |
| D-12 | 对话不显示 agent 与用户头像；未来用气泡边框颜色区分 agent 种类 | 用户 |
| D-13 | 先落地设计文档，按模块拆分，使 agent 能凭模块规格独立开发 | 用户 |

## 提议（默认采用，可推翻）

| # | 决策 | 理由 |
| --- | --- | --- |
| P-01 | 不自研编辑器；"AI IDE"指编排工作台 | 编辑器是无底洞，现有编辑器足够 |
| P-02 | 编排状态以本地事件日志为准，交付事实以 GitHub 为准，对账器同步 | 见 architecture §5 I4 |
| P-03 | 沙盒内无 GitHub 网络、无长期凭据；GitHub 写操作只走 Forge Outbox | 防注入后的越权 |
| P-04 | 服务端 Rust，桌面端 Tauri 2 | 与 SpecGit 同栈，共用 TLS / 证书代码 |
| P-05 | 沙盒默认 rootless Podman + gVisor | 第一版不跑不可信 fork 代码，microVM 暂缓 |
| P-06 | 离线根 CA + 在线中间 CA | 降低在线 CA 私钥泄露的影响 |
| P-07 | Worker 第一实现为 Claude Code（headless），在沙盒内运行 | — |
| P-08 | 事件流用 SSE + 游标 | 简单、可穿透代理 |
| P-09 | 第一版单人自托管、只支持 github.com、不接收 fork PR | 收窄 MVP |
| P-10 | Low 档也走 PR，不直推默认分支 | 默认分支通常受保护 |
| P-11 | 改动 `.github/workflows/`、依赖清单、lockfile 一律需要 Gantry 审批。过渡：M1 的 `policy` v0 对这些改动一律拒绝并上报（fail-closed），M2 起改为"需要审批"（P-19） | 防注入借 CI 执行 |
| P-12 | 用户必须在 GitHub 上完成仓库要求的人工 review；Gantry 审批只授权 Gantry 自己的动作 | App 不能 approve 自己创建的 PR |
| P-13 | 内网部署的 webhook 入口用只转发 `/webhook` 的隧道，不用 smee.io | smee 仅供开发，且经第三方中继 |
| P-14 | License：不复用 GraphAgent v1 的 AGPL DAG 代码，重新实现，仓库保持 MIT | 与 D-01 中"独立重设计 runtime"一致 |
| P-15 | 沙盒与服务端之间只有文件（只读输入、可写输出目录）和 worker 进程的 stdout；没有任何网络端点或 Unix socket | 沙盒不可信，任何可回连的服务端端点都是攻击面（I1） |
| P-16 | 出网代理（Egress）两种模式：(a) 模型 API 用反向代理模式——调用方（沙盒内的 worker 与服务端的 `models` 模块）把模型 base URL 指向 Egress，Egress 注入 key 并按调用方计量 token（沙盒调用方以沙盒网络身份映射到 job，不信任沙盒自报）；(b) 包仓库用 HTTPS CONNECT 白名单，不注入凭据；私有包仓库走反向代理模式。模型 key 只存在 Egress；服务端 `models` 的调用也经 Egress | CONNECT 隧道既不能注入请求头也读不到用量；key 只在一处，计量只有一个事实源 |
| P-17 | 节点的角色（`role`）、模型层级（`tier`）、执行位置（`exec`）是三个字段。需要读仓库内容的角色经 Worker 协议在沙盒中运行，只处理结构化文本的角色由 `models` 直接调用；角色表见 architecture §9。推理深度随档位，由配置决定 | 执行位置由"是否要读仓库"决定，而不是由模型层级决定 |
| P-18 | 设备签名审批的最小实现提前到 M1（`sec` 与 `fe-desktop`）；M2 保留因新推送失效的完整交互、路径规则审批、审计日志、可配置策略。所有签名走同一原语：challenge → 规范化载荷 → 设备签名 → 服务端验签，动作为 `ForgeActionKind`（如 `merge_pr`）或内部动作（`settings_update`、`device_role_change`、`device_revoke`），见 api.md §6。设置写入：涉及凭据或放宽安全的改动需要审批设备签名，收紧类改动 operator 即可 | M1 退出标准要求用户审批 merge，M1 设置页要写模型凭据 |
| P-19 | M1 的 `policy` v0 对 `.github/workflows/`、依赖清单、lockfile 的改动一律拒绝并上报（fail-closed）；M2 起改为"需要审批"（P-11） | M1 没有路径规则审批的交互，宁可拒绝 |
| P-20 | 审批状态：`pending → granted → consumed \| invalidated \| expired`；`pending → denied \| invalidated \| expired`。审批绑定 `subject_digest`：PR 类动作是 head SHA，补丁审批（PR 创建前）是补丁内容的 sha256。`Approval.action` 用 `ForgeActionKind` 标识或内部动作标识 | 补丁审批发生在 PR 之前，没有 head 可绑 |
| P-21 | 事件目录补齐：`settings.updated`、`device.updated`、`inbox.dismissed`、`conversation.created`、`approval.expired`、`approval.consumed`、`outbox.executing`、`outbox.cancelled`、`forge.<kind>`（含 `forge.base_advanced`）、`attempt.progress`（采样，仅展示）、`patch.validated`、`patch.rejected`；全局事件的 `project_id` 为 `null` | 四份契约各自提出的缺口 |
| P-22 | data-model 补齐 Attempt / Sandbox / InboxItem / Project / PR 快照 / OutboxAction / BudgetLedger 的字段与枚举，`Level` 线上格式为 `"None"`…`"Ultimate"`，定义 `UntrustedText` / `Ref` / `Card` / `CheckSummary`；每个项目一个主对话；`default_level` 移到配置 `levels.default_level`（项目可覆盖）。用量以 Egress 计量为准；`forge_writes` 是累计数，按小时限速是配置项、由 Outbox 令牌桶执行，二者不混用 | 契约需要可生成 schema 的确定形状 |
| P-23 | 第一台配对设备为 approver（所有者）；之后配对的设备默认 operator；手机端按 api.md 的"客户端档案"只开放对话相关路由，默认不可审批。改角色、吊销需要 approver 签名 | 防会话劫持后自我提权 |
| P-24 | Gate 输出 `ACCEPT` / `REVISE` / `REJECT` / `BLOCKED`。准入 READY = `ACCEPT`，NOT_READY = `REVISE`（在对话中追问）。`REJECT` → 交付单元进入 `waiting_human`，上报 `decision_needed`（放弃或重新规划），记 `failure_class = verdict_fail`；`BLOCKED` → 上报 `escalation` 并附证据 | 统一判定语义，阶段机出边确定 |
| P-25 | 新增模块 `repo`（仓库与工作树，`exec` 泳道）：每个项目的服务端裸镜像仓（由 `forge` 拉取更新）、工作树、写权锁、从镜像准备沙盒工作区、在干净检出上应用补丁、调用 `policy` 校验、生成提交；推送仍经 `forge` Outbox | 补丁闸门与镜像仓原本无主 |
| P-26 | `forge` 订阅默认分支的 `push` 事件，产生 `forge.base_advanced`；引擎据此让 `merge` 阶段的交付单元重验 | GitHub 不会在 base 前移时给 PR 发事件 |
| P-27 | `delete_branch`、`publish_release`、`change_repo_settings` 第一版不实现，只在 forge-actions 中列为预留 | 第一版用不到，且需要高危权限 |
| P-28 | `policies.actions` 列出全部 `ForgeActionKind`（动作 id 与 `kind` 同名），出厂默认值覆盖全部动作；`unlisted_action = approve` 只是兜底 | 避免普通动作落入兜底而被迫审批 |
| P-29 | webhook secret 支持轮换：同时接受当前与上一个 secret | 轮换期间不丢投递 |
| P-30 | 已建立的事件流在设备吊销后断开：M1 用心跳内的吊销检查实现（上限 60 秒为建议值） | 吊销本身在 M1，不能等到 M3 才生效 |
| P-31 | 设置页分组：服务端（模型、档位与预算、GitHub App、沙盒与出网、危险动作策略、后台守护、通知）；项目（每个项目一页，只列可覆盖键）；本机（设备与证书、外观） | 全局、项目、本机三种存储位置分开呈现 |
| P-32 | 消息上下文显式指定：选中某项只驱动检视器；消息发往哪里由输入区的 `@` 决定，默认"全项目"，每次发送后复位；检视器提供"在对话中引用"，agent 气泡提供"回复" | 防止把消息静默发进错误的交付单元（交互评审 #2） |
| P-33 | 签名只在审批单中完成；其他入口只打开审批单。GitHub 侧必需的 review 与 checks 满足前禁用签名 | 签名前必须看清签的是什么（交互评审 #1、#3） |
| P-34 | 落实 D-12：agent 气泡 2px 边框为种类色——大脑紫 `#A371F7`、干活青 `#39C5CF`、打杂灰 `#8B949E`，气泡内有种类文字；档位徽标去色；"需要你"用独立的黄色药丸，不依赖 agent 颜色 | 原配色与状态色冲突，且颜色不能是唯一区分（交互评审 #20） |
| P-35 | 手机端可做文字答复、补规格、二选一；升档（增加花费）与签名只在桌面 | 手机只对话（D-11），但不应把可答复的事都推给桌面 |
| P-36 | 单个交付的升档、暂停、继续、重试、放弃属于"控制"，可在工作页完成；全局预算模板与上限只在设置页 | 区分控制与配置（I7） |
| P-37 | 工作页提供确定性控制：暂停 / 继续、重试失败节点、放弃交付（带确认），不依赖意图解析 | Max / Ultimate 跑偏时需要立即止损（交互评审 #7） |
| P-38 | 新增 `secrets` 模块（库）：服务端密钥加密存储、按使用方隔离读取、轮换、加密备份；`forge`、`pki`、`egress` 只能读各自的密钥 | 原"密钥模块"无主 |
| P-39 | 唯一 approver 设备丢失时，用服务端主机上的应急命令 `gantry device recover`（需主机 root，写审计事件）吊销设备或把一台设备提升为 approver | 否则吊销与改角色无人能签 |
| P-40 | 带选项的上报项与对话里的选择卡片是同一个 InboxItem，按钮统一调 `POST /v1/inbox/{id}/choose`；自由文本答复走发消息 | 选择结果需要确定的回传路径 |
| P-41 | 补丁中的二进制文件只有路径命中 `policies.patch.binary_allowlist` 且经审批才放行；符号链接一律拒绝 | 二进制与符号链接难以审查，后者还可越出工作区 |
| P-42 | `merge_pr`、强制推送、受保护路径改动不允许配置为 `auto`；其他动作可以 | 危险动作必须有人签名 |
| P-43 | 设置资源为 8 个：`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`、`notify`、`github` | 与设置页分组一一对应 |
| P-44 | token 用量按 input / output / cache_read / cache_write 分别记录；预算按加权 token 计，权重是配置项（默认按提供方价格比例，建议值） | 不同类别成本差异大 |
| P-45 | reasoner 不是判定节点：只产出预演报告，不输出 verdict | 判定只由 gate 与 arbiter 给出 |
| P-46 | 项目覆盖：安全策略、预算、守护阈值、沙盒限额只能收紧；模型选择可自由覆盖 | 防止项目级配置绕过全局安全基线 |

## 待决

| # | 问题 | 推荐 | 影响 |
| --- | --- | --- | --- |
| Q-01 | 目标用户：个人还是团队 | 先个人 | 权限模型、是否需要"用户"实体 |
| Q-02 | 服务端主机在公网还是内网 | 两者都支持，文档以内网 + 隧道为默认 | webhook 入口 |
| Q-03 | Worker 调用模型用 API key 还是订阅登录 | API key（无人值守更稳定，便于计费） | 部署、成本 |
| Q-04 | 任务入口是否支持 GitHub 原生（Issue label、`/gantry` 评论命令） | M2 以后支持 | 规格阶段交互 |
| Q-05 | `graph-agent` 仓库与 Gantry 的关系 | 待用户说明 | 代码复用 |
| Q-06 | SpecGit 是并入还是作为独立 CLI 被调用 | 并入其交付语义，不调用其 CLI | forge 模块 |
| Q-07 | 上报的离线通知渠道（桌面端关闭时） | 手机端推送；之前用邮件或 GitHub 通知兜底 | notify 模块 |
| Q-08 | 依赖指向 `github.com` 源（git 依赖）时如何供给：沙盒内无 GitHub 网络（I1），第一版默认拒绝 | 服务端代理缓存（由服务端用只读令牌拉取，沙盒经 Egress 读缓存） | egress、repo、sandbox |
