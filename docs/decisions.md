# 决策记录

> 每条决策有唯一编号和状态。**已确定** = 用户拍板；**提议** = 助手提出、用户未确认，可被推翻；**待决** = 需要用户决定。实现时以"已确定"为硬约束，"提议"为默认值。
>
> 例外：`## 待契约批准提案（非默认）` 中的 P-83–P-86 尚未获契约批准，不是当前实现默认，也不改变 M0 / M1 的契约、端口、替身或退出标准；接线前须单独批准契约 PR。

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
| D-14 | 项目主对话统筹多个工作树并行 | 用户 |
| D-15 | 同项目 agent 间可只读观察、通信和跨树文件阅读 | 用户 |
| D-16 | GraphAgent（`graph-agent` 仓库）是 Gantry 的前身项目，Gantry 是其继任者；GraphAgent 是 AGPL 许可，只借鉴设计与教训，不复用代码 | 用户（[AGENTS.md §1](../AGENTS.md#1-项目当前情况)）；原 Q-05 |
| D-17 | SpecGit 是 Gantry 的前身项目：并入其交付语义（Issue → PR → 验证完成），不作为独立 CLI 调用 | 用户（[AGENTS.md §1](../AGENTS.md#1-项目当前情况)）；原 Q-06 |
| D-18 | **第一台配对设备是手机档案时**：安装引导期（P-66）一直持续到第一台**桌面**设备配对；第一台桌面设备成为 approver（P-23）；在此之前配对的手机都是 `operator`（P-76） | 用户 2026-10-27 授权协调会话代为拍板（[#87](https://github.com/LeXwDeX/gantry/issues/87)）；原 Q-10 |
| D-19 | **`budget_exhausted` 的 `raise_cap`**（P-79）：每选一次，给耗尽的那个维度追加一份该档模板的原值（如 `rounds` 3 → 6）；新增事件 `budget.limit_raised`（`dimension`、`from`、`to`、`by`），其契约另开 issue 跟踪；operator 即可操作，不需要签名，只在桌面档案出现 | 用户 2026-10-27 授权协调会话代为拍板（[#87](https://github.com/LeXwDeX/gantry/issues/87)）；原 Q-11 |

D-15 行只记录用户目标。服务端中介与不跨树写是从沙盒只计算与工作树隔离推导出的方案约束，不是 I1 / I5 / I8 的字面规定，也不是 D-15 行新增的用户机制决定；机制见待契约批准提案节的 P-83–P-86。

## 提议（默认采用，可推翻）

P-47–P-70 来自[第二轮裁决](reviews/2026-10-24-rulings-round2.md)（产品负责人代理，C1–C24）；同一轮的 C25–C29 是契约小修，直接落在相应文档里，不另立编号。被修订的旧条目保留原文，并在条目末尾注明"已被 P-xx 修订"。P-71–P-82 是最后一批裁决（2026-10-24，产品负责人代理），落实情况见[终审记录](reviews/2026-10-24-final-review.md)。

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
| P-11 | 改动 `.github/workflows/`、依赖清单、lockfile 一律需要 Gantry 审批。过渡：M1 的 `policy` v0 对这些改动一律拒绝并上报（fail-closed），M2 起改为"需要审批"（P-19）（workflow 部分已被 P-48 修订：workflow 改动一律拒绝，不再"需要审批"） | 防注入借 CI 执行 |
| P-12 | 用户必须在 GitHub 上完成仓库要求的人工 review；Gantry 审批只授权 Gantry 自己的动作 | App 不能 approve 自己创建的 PR |
| P-13 | 内网部署的 webhook 入口用只转发 `/webhook` 的隧道，不用 smee.io（已被 P-65 修订：webhook 只在回环地址明文监听，对公网经终止 TLS 的反向代理或隧道；隧道还要转发 `/setup/github/callback`） | smee 仅供开发，且经第三方中继 |
| P-14 | License：不复用 GraphAgent v1 的 AGPL DAG 代码，重新实现，仓库保持 MIT | 与 D-01 中"独立重设计 runtime"一致 |
| P-15 | 沙盒与服务端之间只有文件（只读输入、可写输出目录）和 worker 进程的 stdout；没有任何网络端点或 Unix socket | 沙盒不可信，任何可回连的服务端端点都是攻击面（I1） |
| P-16 | 出网代理（Egress）两种模式：(a) 模型 API 用反向代理模式——调用方（沙盒内的 worker 与服务端的 `models` 模块）把模型 base URL 指向 Egress，Egress 注入 key 并按调用方计量 token（沙盒调用方以沙盒网络身份映射到 job，不信任沙盒自报）；(b) 包仓库用 HTTPS CONNECT 白名单，不注入凭据；私有包仓库走反向代理模式。模型 key 只存在 Egress；服务端 `models` 的调用也经 Egress（沙盒身份部分已被 P-49 修订：改用每个尝试一枚的 `job_token`；`models` 的调用方式已被 P-50 修订：经回环 HTTP 反向代理并使用 `svc_token`） | CONNECT 隧道既不能注入请求头也读不到用量；key 只在一处，计量只有一个事实源 |
| P-17 | 节点的角色（`role`）、模型层级（`tier`）、执行位置（`exec`）是三个字段。需要读仓库内容的角色经 Worker 协议在沙盒中运行，只处理结构化文本的角色由 `models` 直接调用；角色表见 architecture §9。推理深度随档位，由配置决定 | 执行位置由"是否要读仓库"决定，而不是由模型层级决定 |
| P-18 | 设备签名审批的最小实现提前到 M1（`sec` 与 `fe-desktop`）；M2 保留因新推送失效的完整交互、路径规则审批、审计日志、可配置策略。所有签名走同一原语：challenge → 规范化载荷 → 设备签名 → 服务端验签，动作为 `ForgeActionKind`（如 `merge_pr`）或内部动作（`settings_update`、`device_role_change`、`device_revoke`），见 api.md §6。设置写入：涉及凭据或放宽安全的改动需要审批设备签名，收紧类改动 operator 即可（动作集合已被 P-54 修订；签名所用密钥已被 P-47 修订：审批签名密钥独立于 mTLS 密钥） | M1 退出标准要求用户审批 merge，M1 设置页要写模型凭据 |
| P-19 | M1 的 `policy` v0 对 `.github/workflows/`、依赖清单、lockfile 的改动一律拒绝并上报（fail-closed）；M2 起改为"需要审批"（P-11）（workflow 部分已被 P-48 修订：M2 起仍拒绝，只有依赖清单与 lockfile 改为需要审批） | M1 没有路径规则审批的交互，宁可拒绝 |
| P-20 | 审批状态：`pending → granted → consumed \| invalidated \| expired`；`pending → denied \| invalidated \| expired`。审批绑定 `subject_digest`：PR 类动作是 head SHA，补丁审批（PR 创建前）是补丁内容的 sha256。`Approval.action` 用 `ForgeActionKind` 标识或内部动作标识（`Approval.action` 的取值与消费时点已被 P-54 修订；补丁审批的动作标识改为 `patch_apply_protected`） | 补丁审批发生在 PR 之前，没有 head 可绑 |
| P-21 | 事件目录补齐：`settings.updated`、`device.updated`、`inbox.dismissed`、`conversation.created`、`approval.expired`、`approval.consumed`、`outbox.executing`、`outbox.cancelled`、`forge.<kind>`（含 `forge.base_advanced`）、`attempt.progress`（采样，仅展示）、`patch.validated`、`patch.rejected`；全局事件的 `project_id` 为 `null` | 四份契约各自提出的缺口 |
| P-22 | data-model 补齐 Attempt / Sandbox / InboxItem / Project / PR 快照 / OutboxAction / BudgetLedger 的字段与枚举，`Level` 线上格式为 `"None"`…`"Ultimate"`，定义 `UntrustedText` / `Ref` / `Card` / `CheckSummary`；每个项目一个主对话；`default_level` 移到配置 `levels.default_level`（项目可覆盖）。用量以 Egress 计量为准；`forge_writes` 是累计数，按小时限速是配置项、由 Outbox 令牌桶执行，二者不混用 | 契约需要可生成 schema 的确定形状 |
| P-23 | 第一台配对设备为 approver（所有者）；之后配对的设备默认 operator；手机端按 api.md 的"客户端档案"只开放对话相关路由，默认不可审批。改角色、吊销需要 approver 签名（手机档案设备已被 P-76 修订：永远不能成为 approver；第一台配对设备是手机档案时已被 D-18 修订：第一台**桌面**设备成为 approver） | 防会话劫持后自我提权 |
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
| P-35 | 手机端可做文字答复、补规格、二选一；升档（增加花费）与签名只在桌面（消息带 `level_cap` 且 `@` 已有交付时的语义见 P-82） | 手机只对话（D-11），但不应把可答复的事都推给桌面 |
| P-36 | 单个交付的升档、暂停、继续、重试、放弃属于"控制"，可在工作页完成；全局预算模板与上限只在设置页 | 区分控制与配置（I7） |
| P-37 | 工作页提供确定性控制：暂停 / 继续、重试失败节点、放弃交付（带确认），不依赖意图解析 | Max / Ultimate 跑偏时需要立即止损（交互评审 #7） |
| P-38 | 新增 `secrets` 模块（库）：服务端密钥加密存储、按使用方隔离读取、轮换、加密备份；`forge`、`pki`、`egress` 只能读各自的密钥（读取方已被 P-59 修订：`notify` 也读取自己的密钥，M4 APNs） | 原"密钥模块"无主 |
| P-39 | 唯一 approver 设备丢失时，用服务端主机上的应急命令 `gantry device recover`（需主机 root，写审计事件）吊销设备或把一台设备提升为 approver | 否则吊销与改角色无人能签 |
| P-40 | 带选项的上报项与对话里的选择卡片是同一个 InboxItem，按钮统一调 `POST /v1/inbox/{id}/choose`；自由文本答复走发消息 | 选择结果需要确定的回传路径 |
| P-41 | 补丁中的二进制文件只有路径命中 `policies.patch.binary_allowlist` 且经审批才放行；符号链接一律拒绝（已被 P-68 修订：撤销 `binary_allowlist`，二进制一律拒绝） | 二进制与符号链接难以审查，后者还可越出工作区 |
| P-42 | `merge_pr`、强制推送、受保护路径改动不允许配置为 `auto`；其他动作可以 | 危险动作必须有人签名 |
| P-43 | 设置资源为 8 个：`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`、`notify`、`github` | 与设置页分组一一对应 |
| P-44 | token 用量按 input / output / cache_read / cache_write 分别记录；预算按加权 token 计，权重是配置项（默认按提供方价格比例，建议值） | 不同类别成本差异大 |
| P-45 | reasoner 不是判定节点：只产出预演报告，不输出 verdict | 判定只由 gate 与 arbiter 给出 |
| P-46 | 项目覆盖：安全策略、预算、守护阈值、沙盒限额只能收紧；模型选择可自由覆盖 | 防止项目级配置绕过全局安全基线 |
| P-47 | **双密钥**：桌面端有两把不可导出的密钥。mTLS 客户端密钥用于无提示的后台连接；**审批签名密钥**单独生成，存于 Secure Enclave / TPM，每次使用必须用户在场（Touch ID / Windows Hello）。配对请求 `POST /v1/pair` 同时提交 CSR 与 `approval_pubkey`，服务端登记在 Device 上；所有审批签名都用审批签名密钥，服务端用登记的 `approval_pubkey` 验签（第二轮裁决 C1）（手机档案已被 P-76 修订：配对时 `approval_pubkey` 可省略） | 后台连接不能每次弹生物识别；若签名也用 mTLS 密钥，被植入的进程可以无提示地签审批 |
| P-48 | **Gantry 不修改 `.github/workflows/`**：App 不申请 `workflows` 权限；补丁触及 workflow 文件时 `policy` 一律拒绝（`failure_detail = policy.path_protected:workflows`），上报 `decision_needed`（`payload.kind = manual_action`），附补丁供用户手动应用。依赖清单与 lockfile 仍按 P-11 / P-19：M1 拒绝，M2 起需要审批（C2） | 没有 `workflows` 权限 GitHub 本身也会拒绝推送；改 CI 等于借 CI 的 secrets 执行代码，宁可让人手动做 |
| P-49 | **沙盒身份用每个尝试一枚的能力令牌 `job_token`**：服务端为每个 Attempt 签发，放在模型 API base URL 路径中（`http://<egress>/j/<job_token>/<provider>/…`），只对该尝试有效、尝试结束即作废、只能从沙盒网段使用；Egress 校验令牌并按尝试计量。沙盒即使读到令牌也只能给自己的尝试记账（C3）（"只能从沙盒网段使用"的强制方式已被 P-73 修订：标"需核实"，能力令牌本身是安全底线；包仓库 `CONNECT` 的携带方式见 P-72） | rootless Podman 下按源地址识别沙盒不可靠；能力令牌不依赖网络拓扑，泄露的后果被限定在该尝试自身 |
| P-50 | **Egress 控制面契约**：新增 [interfaces/egress-control.md](interfaces/egress-control.md)。`gantryd` 经主机本地 Unix socket `/run/gantry/egress.sock` 调用控制面：签发 / 吊销 `job_token`、写入 secret（只写不读）、查询 secret 是否存在（`present`）、拉取用量、签发服务令牌 `svc_token`。服务端 `models` 的模型调用走 Egress 的**回环 HTTP 反向代理**，带 `svc_token`（与 `job_token` 同一计量口径，计入会话台账或交付台账）。控制面统一称 `egress.sock`，不再使用 `models_uds`（C4）（`svc_token` 的粒度与归属已被 P-74 修订：每个服务端进程一枚，每次调用带 `X-Gantry-Ledger`） | 几份模块规格各自给 Egress 接口留了缺口；计量只有一个事实源、一套口径 |
| P-51 | **共享端口与替身**：所有跨模块端口 trait 定义在 `core::ports`（M0 交付）：`ForgeReader`、`OutboxPort`、`SandboxService`、`ModelProvider`、`RepoService`、`PolicyPort`、`InboxPort`、`EgressControl`。M0 替身补齐：`Store::open_in_memory`、`FakePolicy`、`FakeInbox`、HTTP 级 `FakeGitHub`（供 `forge` 测试）、`EgressStub`（C5）（替身清单已被 P-81 修订：加入 `MemorySecrets`；终审按本条原则把跨泳道的服务接口也收进 `core::ports`，见 [终审记录](reviews/2026-10-24-final-review.md)） | 端口放在被依赖方的 crate 里会让替身与调用方互相等待；放进 `core` 才能真正并行 |
| P-52 | **沙盒端口分两层**：`SandboxService`（引擎面：提交 JobSpec、等待结果、取消）与 `SandboxDriver`（容器面：rootless Podman + gVisor 的具体操作）。`engine` 只依赖 `SandboxService`（C6） | 引擎不该知道容器细节；驱动可替换（Firecracker）而不动引擎 |
| P-53 | **Outbox 失败语义**：临时错误留在 `queued`，以 `not_before` 退避重试，不进入 `failed`；重试用尽或不可重试错误才进入终态 `failed`，只在终态发 `outbox.failed`；人工重试创建新动作（新 `epoch`）。删除 `failed → queued` 边（C7） | `failed` 既表示"会重试"又表示"已放弃"时，消费者无法据事件决定是否上报 |
| P-54 | **审批动作标识与形状**：`Approval.action` ∈ `ForgeActionKind` ∪ {`settings_update`、`device_role_change`、`device_revoke`、`force_push`、`push_outside_prefix`、`resolve_review_thread_human`、`patch_apply_protected`}。每个动作的 `target` 与 `subject_digest` 形状在 [data-model](data-model.md#审批动作的-target-与-subject_digest) 中列表定义；未定义形状的动作一律 fail-closed（按拒绝处理并上报）。**审批在执行开始前消费**（`consumed`，记录消费方的 Outbox 动作 ID 或尝试 ID）；执行终态失败不退回，需要新审批（C8） | 形状不确定就无法规范化签名；"成功后消费"会让失败重试反复使用同一授权 |
| P-55 | **阶段机补全**：任何非终态阶段 → `abandoned`（用户放弃、PR 关闭未合并、Issue `not_planned`）；`REJECT` 后选择"重新规划" → `plan`；`merge` 阶段 PR 被关闭 → `abandoned`，PR 重新打开则回到 `review`。`stage_status` 为 `running` / `waiting_human` / `blocked` / `paused` / `done`，转移规则见 data-model §4.1。**准入只在 High 及以上**：Medium 为 `spec → plan`（免准入），Low 为 `spec → implement`（免准入、无计划阶段）。**计划是独立阶段**：计划阶段图 = `plan`（Max 起加 `reasoner`），实现阶段图 = `build → verify`（Low 为 `single`）。architecture §8 按"计划阶段 / 实现阶段 / 评审阶段"三列重写；Low 的阶段列表包含 `review`（只等 CI）（C9） | 原阶段机缺放弃、重新规划与暂停的出边；计划节点放在实现阶段图里与阶段名不符 |
| P-56 | **控制路由与事件**：新增 `POST /v1/deliveries/{delivery_id}/pause`、`POST /v1/deliveries/{delivery_id}/resume`、`POST /v1/nodes/{node_id}/retry`（operator，`Idempotency-Key` 必带）；事件 `delivery.paused`、`delivery.resumed`、`node.retry_requested`（C10） | P-37 的确定性控制此前没有路由与事件 |
| P-57 | **档位实际值**：`level_effective` 初值由引擎在规格阶段按风险评估给出（≤ `level_cap`），之后只能在上限内升，不自动降；已触发的闸门永不撤销。预算模板在交付单元创建时按 `level_cap` 固定（C11） | 自动降档会撤销已承诺的审视；预算随档位漂移会让台账失去可比性 |
| P-58 | **None 档记账**：None 档问答不建交付单元。需要读代码时创建挂在会话上的临时图：`Workflow.delivery_id` 可空，此时必须有 `conversation_id`；用量记入每个项目的**会话台账**（`BudgetLedger.subject = conversation`），每日上限是配置项 `levels.None.daily_tokens`（C12）（配置键大小写已被 P-71 修订：`levels.none.daily_tokens`） | None 档的 `answer` 节点也花 token，不能不记账 |
| P-59 | **模块依赖修正**：`engine` 依赖 `forge`（只用 `ForgeReader` 按需读取 Issue / PR 正文与评论，作为不可信文本，不进快照）与 `notify`；`conversation` 依赖 `notify`；`forge` 依赖 `policy`（执行时验审批）与 `notify`；`guardian` 依赖 `core`、`store`、`notify`；`notify` 依赖 `models`（可选，打杂模型生成通知文案）与 `secrets`（M4 APNs 密钥）；`forge`、`egress` 依赖 `secrets`（C13） | 模块规格写作时发现的实际依赖与 architecture §4 不符 |
| P-60 | **守护事件与范围**：新增事件 `guardian.triaged`（`event_ref`、`class`、`action`）、`guardian.heal_requested`、`guardian.escalated`，会话服务把它们渲染为事件行。分流规则以 [modules/guardian.md](modules/guardian.md) 的决策表为唯一来源（含"无需动作"的显式行），architecture §7.3 与 config `guardian` 小节与之对齐；规则表外的事件一律上报。`reply_review_thread` 提前到 M2；M2 的 `be-engine` 泳道交付 `request_heal` 与 `classify` 端口。守护静默自愈不改变交付阶段；需要改代码且超出自愈范围时才走 `review → implement`（C14） | 静默处理必须可审计；M2 的"回复理由"依赖 `reply_review_thread` |
| P-61 | **状态清单评论幂等**：`upsert_status_comment` 的幂等键含单调递增的 `revision`；`revision` 与 `head_epoch` 计数保存在本地事件日志中（随日志备份），不依赖可重建的 PR 快照（C15） | 快照重建若从 0 计数，会与历史幂等键 / 去重键相撞而被静默吞掉 |
| P-62 | **forge 不读策略**：是否需要审批由入队方在入队时调用 `policy` 决定，结果写入 `OutboxAction.requires_approval`；`forge` 只在执行时调用 `policy` 验证并消费审批（C16） | 判定只有一个入口（I3），执行方不做第二次判定 |
| P-63 | **上报项**：`InboxItem` 增加 `project_id` 与 `dedup_key`；`escalation`、`spec_needed`、`decision_needed` 的 `payload.options[]` 为 `{id, label, primary?}`；升档一律用 `escalation`（手机档案不可 `choose`，P-35）。新增 `GET /v1/inbox/summary`（按项目的 open 计数，手机可用）。桌面系统通知由 `inbox.raised`（`severity ≥ warn`）触发，不新增事件。`notify` 归 `be-engine` 泳道（M1 上报与计数），APNs 归 M4 `ops`（C17）（`reason_code` 与选项 id 词表已由 P-79 固定；手机推送令牌路由见 P-77） | 项目 tab 的徽标需要计数；重复上报需要去重键 |
| P-64 | **证书链与续期**：配对时固定的是**离线根 CA** 指纹；服务端下发链 `[叶子, 中间 CA, 根]`，客户端校验根指纹与固定值一致；中间 CA 轮换不影响已配对设备。新增 `POST /v1/devices/self/renew`（mTLS，提交新 CSR），M1 交付；客户端在证书剩余 1/3 有效期时自动续期（建议值）（C18） | 固定中间 CA 会让中间 CA 轮换（M3）迫使所有设备重新配对 |
| P-65 | **webhook 入口与 App 创建**：webhook 监听只在回环地址上以明文 HTTP 提供；对公网暴露一律经终止 TLS 的反向代理或隧道（公网证书）。首次部署顺序：先配置 webhook 入口，再用 App Manifest 创建 App；`gantry github setup` 生成一次性的 Manifest 提交页，并经同一入口的 `/setup/github/callback` 接收 `code`（细节需核实）（C19） | Manifest 创建时就要填 webhook URL；TLS 由成熟的反向代理 / 隧道终止，`gantryd` 不管理公网证书 |
| P-66 | **安装引导期**：第一台设备配对之前，主机命令（需 root）可以写入 `github.*` 与 `sandbox.base_image` 等"仅 approver"配置，并写审计事件；第一台设备配对之后改为需要审批签名。`sandbox.base_image` 默认使用随版本发布、固定摘要的官方 worker 镜像（C20）（引导期结束时机已被 D-18 修订：持续到第一台**桌面**设备配对） | 配对前没有 approver 能签名，而 App 凭据与镜像又是跑通前提 |
| P-67 | **备份恢复**：事件日志有 `log_epoch`，每次从备份恢复加一；事件流与快照响应携带 `log_epoch`，客户端发现变化就全量重同步。设备吊销同时追加写入日志之外的 `revocations.journal`（只追加），恢复时重放晚于备份的吊销（C21）（`revocations.journal` 的写入、复制与重放规则已由 P-80 细化） | 恢复后 `seq` 可能回退并被复用，客户端游标会错位；恢复不能让已吊销的设备复活 |
| P-68 | **二进制文件**：第一版补丁中的二进制文件一律拒绝（与 worker-protocol 不生成 `--binary` 一致），撤销 P-41 中的 `binary_allowlist`；符号链接一律拒绝（C22） | 补丁格式本身就不携带二进制内容，白名单无从生效 |
| P-69 | **补丁校验分工**：`repo` 做机械检查并产出 `PatchFacts`（能否干净应用、文件列表、大小、二进制、符号链接、子模块、权限位）；`policy` 基于 `PatchFacts` 和配置做规则判定（路径规则、大小上限）。应用失败等机械拒绝由 `repo` 发 `patch.rejected`（`reason`）；`policy.denied` 只用于策略判定。提交由 `repo` 创建；提交身份来自配置 `github.commit_identity`（App 创建时写入）（C23） | 事实与判定分开：`policy` 不解析补丁、不碰文件系统，`repo` 不做可配置的判断 |
| P-70 | **token 加权**：保留 P-44；配置增加 `levels.token_weights`（`input`、`output`、`cache_read`、`cache_write`，默认值为建议值，标"待按价格调优"）；config 原 OQ-7 关闭（C24） | P-44 已定加权，但没有配置键可落 |
| P-71 | **配置里的档位键一律小写**：`levels.none`、`levels.low`、`levels.medium`、`levels.high`、`levels.max`、`levels.ultimate`（例如 `levels.none.daily_tokens`、`levels.high.budget.nodes`）；`effort_by_level` 的键同样小写。线上 `Level` 枚举值仍为 `"None"`…`"Ultimate"`（`Message.level_cap`、`Delivery.level_cap`、`levels.default_level`、`policies.escalation.*.raise_to` 等取值处）。由 `Level` 值找配置条目时一律转小写；`levels.none` 只允许 `daily_tokens` 一个键（V07） | 对象键与枚举值是两套词汇；统一小写避免"一个大写例外"的校验分支 |
| P-72 | **包仓库 `CONNECT` 用 `Proxy-Authorization` 携带 `job_token`**：沙盒内 `HTTPS_PROXY=http://job:<job_token>@<egress>:3128`，Egress 从 `Proxy-Authorization`（Basic，用户名固定 `job`）取令牌，按尝试放行白名单、计量（字节与连接数）与审计；令牌校验规则与模型 API 路径中的 `job_token` 相同（[egress-control.md §5](interfaces/egress-control.md#5-数据面上的令牌约定)） | 没有令牌就无法按尝试的出网档案放行，也无法把出网审计归到尝试 |
| P-73 | **"令牌只能从沙盒网段使用"的强制方式标"需核实"**：rootless Podman（slirp4netns / pasta）下 Egress 能否看到沙盒的真实源地址未经核实；能强制时按引导配置 `egress.sandbox_cidrs` 拒绝网段外使用。即使不能强制，安全底线仍是能力令牌本身：每个尝试一枚、短期（吊销或 `expires_at` 即失效）、只能给本尝试记账、不授予任何控制面权限 | 不把未核实的网络性质写成保证；令牌的安全性不依赖网络拓扑 |
| P-74 | **`svc_token` 每个服务端进程一枚**：`gantryd` 启动时经控制面签发（同时吊销此前进程留下的 `svc_token`），进程内所有 `models` 调用共用。每次调用带请求头 `X-Gantry-Ledger: <台账 ID>`（交付台账为 `dlv_…`，会话台账为 `cnv_…:YYYY-MM-DD`，即 [data-model](data-model.md#device--budgetledger) 的台账 ID）以及 `x-gantry-tier`、`x-gantry-call-id`；这些头来自服务端，Egress 视为可信（`svc_token` 只在回环监听器上有效，沙盒不可达）。Egress 按请求实时计量并记录归属；`models` 按调用记账，令牌吊销时（进程重启或轮换）`gantryd` 按该令牌的全部用量记录对账、补记遗漏 | 按（台账，tier）签发令牌要频繁签发与复用管理；归属放在可信请求头里更简单，计量事实源仍只有 Egress |
| P-75 | **接受以下解释**：① `abandoned` 只有在 PR 被关闭后重新打开时可恢复（回到 `review`）；用户放弃、Issue `not_planned` 的 `abandoned` 不可恢复。② 手机档案可调用证书续期 `POST /v1/devices/self/renew`。③ 内部动作（`settings_update`、`device_role_change`、`device_revoke`）的审批有效期与 Forge 动作统一用 `policies.approval.ttl_s`（默认 24 小时，建议值） | 第二轮裁决后各文档的写法，确认为准 |
| P-76 | **手机档案与审批**：手机档案（`platform = ios`）配对时 `approval_pubkey` 可省略（省略则 Device 不登记审批签名公钥）；手机档案设备**永远不能**成为 approver：配对时角色至多 `operator`（即使是第一台），`PUT /v1/devices/{id}/role` 与 `gantry device recover --promote` 把它设为 approver 一律拒绝（`409 conflict`，`reason: mobile_cannot_approve`） | 手机端不签名（P-35），没有理由登记审批密钥；角色与档案不应出现"approver 却不能审批"的组合 |
| P-77 | **新增路由 `PUT /v1/devices/self/push-token`**（M4，operator 起，手机档案可用）：注册或更新本设备的 APNs 推送令牌，体 `{"token": "…", "environment": "production" \| "sandbox"}`；令牌存于 `notify` 的私有表，不进事件日志、不出现在任何读响应里；设备吊销时清除。`notify` 的 APNs 渠道与 `mobile` 的推送注册都引用它 | M4 退出标准"手机端能收到上报通知"需要令牌注册入口 |
| P-78 | **统一写操作上下文 `core::ports::ActorCtx`**：`{actor: Actor, device: Option<DeviceId>, role: Option<DeviceRole>, client_profile: Option<ClientProfile>, request_id, idempotency_key: Option<String>, dedup_key: Option<String>, causation_id: Option<EventId>}`。由 `api` 在鉴权后构造（设备请求）或由服务端模块自行构造（`actor.kind ∈ engine / guardian / system`，设备字段为空）；所有跨模块写接口一律用它，替换 `api` 模块的 `WriteCtx`、`engine` 的 `ActorRef`、`notify` 的 `DeviceActor`、`policy` 的 `RequestCtx` 等别名 | 同一概念四个名字会让各泳道的签名对不上 |
| P-79 | **上报项 `reason_code` 与选项 id 词表固定在 [data-model](data-model.md#上报项的-reason_code-与选项词表)**：`escalation`（升档，`level_cap_exceeded`）→ `upgrade` / `narrow` / `abandon`；`decision_needed`（`gate_reject`，REJECT 后）→ `replan` / `abandon`；`decision_needed`（`manual_action`）→ `done` / `abandon`；`decision_needed`（`review_dispute`，评审争议）→ `fix` / `keep`；`spec_needed` 是自由文本答复（走发消息），无选项；`budget_exhausted` → `raise_cap` / `abandon`（`raise_cap` 只在桌面）。其余 `reason_code` 一律无选项（经控制路由、发消息或 `dismiss` 处理） | 选项 id 是 `engine` / `guardian` / `notify` / 客户端之间的契约，不能各写一套 |
| P-80 | **`revocations.journal`**：只追加；每次写入在吊销事务提交前 `fsync`，并**同步复制到备份目标**（复制失败则吊销失败）；从备份恢复时重放时间戳晚于备份时刻的条目；事务回滚**不删除**已写的条目（重放时照样吊销，偏向安全） | 吊销日志必须比事件日志的备份更新，否则恢复会让被吊销设备复活 |
| P-81 | **M0 替身清单加入 `MemorySecrets`**：`secrets` 的读写 trait 与内存实现在 M0 冻结与交付，供 `forge`、`pki`、`egress`、`api` 与 `secrets` 并行开发 | `secrets` 与它的使用方在 M1 同期开发 |
| P-82 | **桌面端消息带 `level_cap` 且 `@` 的是已有交付**：视为对该交付调整上限，等同 `PATCH /v1/deliveries/{id}`（产生 `delivery.level_changed`，预算模板不变，P-57）；界面在发送前提示改动影响（"上限 X → Y；已触发的闸门不撤销"）。值与该交付当前上限相同则不变。手机档案忽略 `level_cap` | 原型已这样交互，此前只有 `conversation` 的临时处理 |

## 待契约批准提案（非默认）

以下条目尚未获契约批准，不是当前实现默认，也不改变 M0 / M1 的契约、端口、替身或退出标准；接线前须单独批准契约 PR。每项只留摘要，完整边界与负例见 [collaboration.md](collaboration.md)。

| # | 摘要 | 详见 |
| --- | --- | --- |
| P-83 | **基础并行与独立上下文**：独立并行是现行基线；新增协作待契约批准；同一交付的状态推进由单 actor 串行，图内就绪节点按 DAG、锁与并发 / 预算闸并行；主对话统筹不是共享执行上下文；基础协作不依赖 Ultimate。 | [§1 结论与边界](collaboration.md#1-结论与边界)、[§3 目标模型与授权](collaboration.md#3-目标模型与授权) |
| P-84 | **服务端中介与下一尝试投递**：engine / repo / sandbox / store 中介；用户显式授权是上限，计划只可分配、排序和收窄（含 delivery、node、可观察字段、路径 / 快照），delivery 授权不自动覆盖其全部 node；消息仅作原节点原 `graph_rev` 下一次正常调度 Attempt 的不可信材料，不注入运行中；已投递以 worker 实际启动且消息在该 Attempt 固定输入内为准，不是 `attempt.started` 事件或 `submit` 受理；目标 node / workflow / delivery 终结、节点替换或图版本变化时不转投，无下一次正常调度时有限等待后过期，不自建 Attempt；当前 Attempt 正常结束不等于目标终结，失败或取消不转投后续 Attempt；接收、组装输入、实际启动前检查；撤权覆盖已受理未启动 job，启动许可与撤权按有序决策处理。 | [§3 目标模型与授权](collaboration.md#3-目标模型与授权)、[§4 只读观察](collaboration.md#4-只读观察)、[§5 消息：下一尝试投递](collaboration.md#5-消息下一尝试投递) |
| P-85 | **固定快照只读**：仅读用户授权、同项目、已发布且服务端验证产生的快照，按 commit + 相对路径 + digest 固定；路径存在不等于授权，SHA / CAS 摘要不是权限；不读活跃工作树或 `/gantry/work/`；不跟随 symlink / submodule，不执行读到的文本；CAS 是内部无鉴权字节存储，调用方 `engine` / `api` 在 `open` / `stat` 前校验同项目、用户授权与 snapshot 映射，不给沙盒直接入口。 | [§6 跨树文件只读](collaboration.md#6-跨树文件只读) |
| P-86 | **独立组合验证**：engine 编排、repo 固定源、sandbox 独立临时副本、store 持久化证据；绑定候选 / 基线 SHA、契约版本、组合顺序与结果树摘要；分别通过不等于组合通过；变化即过期；对明确参加组合交付的目标，缺失 / 失败 / 过期证据阻止推进或写入合并动作，用户审批不能覆盖组合失败；组合通过不是审批，保留 `policy`、Forge Outbox 与 GitHub 规则，默认不自动合并；与 policy / Forge 的接线待 G1。 | [§7 独立组合验证](collaboration.md#7-独立组合验证) |

P-83–P-86 只记录目标设计提案，均待契约批准，不作为当前实现默认，也不改变 M0 / M1 的契约、端口、替身或退出标准；详见 [collaboration.md](collaboration.md)。

## 待决

| # | 问题 | 推荐 | 影响 |
| --- | --- | --- | --- |
| Q-01 | 目标用户：个人还是团队 | 先个人 | 权限模型、是否需要"用户"实体 |
| Q-02 | 服务端主机在公网还是内网 | 两者都支持，文档以内网 + 隧道为默认 | webhook 入口 |
| Q-03 | Worker 调用模型用 API key 还是订阅登录 | API key（无人值守更稳定，便于计费） | 部署、成本 |
| Q-04 | 任务入口是否支持 GitHub 原生（Issue label、`/gantry` 评论命令） | M2 以后支持 | 规格阶段交互 |
| Q-07 | 上报的离线通知渠道（桌面端关闭时） | 手机端推送；之前用邮件或 GitHub 通知兜底 | notify 模块 |
| Q-08 | 依赖指向 `github.com` 源（git 依赖）时如何供给：沙盒内无 GitHub 网络（I1），第一版默认拒绝 | 服务端代理缓存（由服务端用只读令牌拉取，沙盒经 Egress 读缓存） | egress、repo、sandbox |
| Q-09 | 仓库中的二进制资源（图片、字体、数据文件等）如何供给：补丁格式不携带二进制（P-68 一律拒绝），需要改二进制文件的任务目前无法完成 | 第一版一律拒绝并上报；M3 起"经审批放行"：worker 在 `out/` 另交二进制清单与内容（受大小上限约束），`repo` 校验后进入补丁审批 `patch_apply_protected` | worker-protocol、`repo`、`policy`、`worker` |
| Q-12 | 结果对象（补丁、`result_ref`）的读取路由：`patch_apply_protected` 审批要签补丁摘要，但客户端拿不到补丁内容（补丁尚未提交，GitHub 上也看不到），只能"盲签"；`manual_action` 的 workflow 补丁也没有下载入口（api.md OQ-11、desktop-shell OQ-S9） | 新增 `GET /v1/results/{sha256}`（viewer，仅桌面档案，M2；按内容寻址返回原始字节，大小上限同 `policies.patch.max_bytes`，客户端校验 `sha256` 后按不可信文本渲染）；补丁审批单在取不到内容时不允许签名。无论路由形态，`engine` / `api` 读取内部 CAS 前须校验同项目、用户授权与已发布快照映射，未授权对外不确认对象存在，不给沙盒直接入口，不以 digest 当权限 | `api`、`desktop-shell`、`desktop-ui`、`engine`（CAS 保留期）、M2 退出标准"签名批准后才提交推送"；该权限前置待 G1 冻结，Q-12 仍未决 |
| Q-13 | 同项目 agent 之间是否需要在运行中即时通信（而不是只在下一次尝试投递） | 推荐只在下一次尝试投递（P-84）：不注入运行中 Attempt，未答复前不算已确认 | 协作契约、`core::ports`、worker 协议、desktop 呈现；不解决 Q-01–Q-12 |

Q-05（`graph-agent` 仓库与 Gantry 的关系）、Q-06（SpecGit 的定位）已于 2026-10-27 移至已确定，见 D-16、D-17；Q-10（首台配对为手机档案）、Q-11（`raise_cap` 追加预算）同日移至已确定，见 D-18、D-19。编号不复用。
