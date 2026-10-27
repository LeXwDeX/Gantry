# 统一开发基线后续工作

本文件是当前迭代的入口：现状、待拍板事项和后续规划。项目总体情况与目录入口见 [AGENTS.md](AGENTS.md)，运行与检查入口见[根目录说明](README.md#本地接入)和[前端说明](apps/desktop/README.md)。

## 现状（2026-10-27）

基线 `main@b90437b`。

- **M0 已合入。** 领域核心、端口、内存存储、测试替身、内存模拟接口和浏览器模拟场景前端都在 `main` 上；前后端尚未接线，真实联调未验证。契约文本已于 2026-10-27 由用户统一批准。
- **M1 只做了引擎的局部。** `crates/gantry-engine` 约 1100 行，已实现部分见下表；`decide`、执行体、调度和终结管线都还没有。M1 端到端路径上另外 13 个服务端 / 桌面 crate（`forge`、`repo`、`sandbox`、`worker`、`egress`、`models`、`policy`、`notify`、`conversation`、`api`、`pki`、`secrets`、`desktop-shell`）都还没有。

| 已实现（`crates/gantry-engine`） | PR |
| --- | --- |
| `low.single` 模板的 `low.implement` 单节点图 | #26 |
| 事件投影：delivery / workflow / node / attempt / inbox | #33 |
| 预算台账构造、检查、记账、余额 | #35 |
| 阶段转移表驱动的 `advance` | #37 |
| `budget_from_level` 与 `delivery.created` | #39 |
| Outbox 意图构造与幂等键派生 | #41 |
| `create_delivery` 单事务写入 | #43 |
| `advance_stage` 写入 `delivery.stage_changed` | #45 |
| 暂停 / 继续 / 放弃 | #47 |
| 调整 `level_cap`；压低上限派生 `level_effective`（契约 #59、实现 #60） | #49 |
| 失败节点重试 | #51 |
| 模板目录、崩溃恢复重放 | #53 |
| 上报项写入 inbox | #55 |
| 收尾评审修正：创建去重与预算记账 | #77 |

以上 PR 均已合入；`decide` 的类型契约（engine.md §7.14）由 #63 合入，M1 验收证据分级（`docs/m1-acceptance.md`，提议）由 #62 合入。

**本机检查（2026-10-27 实测）**：`cargo fmt`、`cargo clippy` 通过；工作区测试 463 passed / 0 failed / 2 ignored（两个 ignored 是由父测试启动的看门狗子进程入口）；MSRV `cargo +1.85 check` 通过；`cargo --locked xtask schemas --check` 0 failure、0 warning，保留 3 个显式未验证块，`schemas/`、`fixtures/` 无漂移；模拟接口测试 29 passed。前端在当前 `main` 上重跑（含 vitest 4.1.12 等依赖升级之后）：`npm ci`、边界检查、类型检查通过；单元测试 24 个文件、287 passed；正式与场景资源构建通过，仍有超过 500 kB 的构建块警告；浏览器端到端测试 66 passed。环境为 macOS arm64、Rust 1.97.0 / 1.85.1、Node 26.12.0、npm 11.20.0；未验证 Node 22、Linux / Windows。结果只覆盖内存存储、测试替身与浏览器模拟场景。

**远端 CI**：GitHub Actions 因账户计费问题全部在数秒内失败，#77、#78、#79 是在没有 CI 的情况下按本机检查合入的。最近一次可追溯的全绿远端结果是 M0 时期 PR #17 的 run 40334369717。

**未实现或未验证**：Outbox 执行与审批消费（`PolicyPort` 尚未接入引擎）；终结管线与 `waiting_human` / `blocked` 自动切换；`attempt.*`、`budget.charged` 的事件写入；真实 GitHub App、模型、Podman、目标主机与代码签名。M1 端到端退出标准全部未验证，取证分级见 [M1 验收证据分级](docs/m1-acceptance.md)。

**M0 遗留**（里程碑 [M0.9 M0 遗留收口](https://github.com/LeXwDeX/gantry/milestone/2)）：

- #82：B-01 Egress 调用级终结：`core::ports::egress::EgressControl` 只有 `call_usage` 等用量查询和整令牌吊销，还没有调用级关闭屏障；候选方案见 [调用级终结](docs/interfaces/egress-call-terminal.md)。在它批准并实现之前，直接调用的最终结算、释放和零判定都没有可信证据。
- #83：`xtask schemas --check` 报告的 OpenAPI / core 人工核对差异尚未逐项判定是允许的视图差异还是漂移。
- #84：3 个显式未验证文档块（`github_ref` 组合、App Manifest、`ResultBundle`）仍按 `example` / `contract-undefined` 登记。

## 待拍板

以下事项需要用户决定，不自行拍板。2026-10-27 用户授权协调会话代为拍板，已决的事项保留在表中，拍板内容见各 issue。

| 事项 | 阻塞什么 | 状态 |
| --- | --- | --- |
| #66 E3：`Deny { report: None }` 的上报码 | #72（decide 切片 4，终结管线） | 已决：方案 A，见 [#66](https://github.com/LeXwDeX/gantry/issues/66) |
| #67 E6：Outbox 终态失败后的重做入口 | #73（decide 切片 5，控制面与异常路径） | 已决：方案 A，见 [#67](https://github.com/LeXwDeX/gantry/issues/67) |
| #125 `create_pr` 的 `ActionKey` 与 CAS 提议原文如何衔接（#68 遗留） | #69、#70 | 已决：方案 1，见 [#125](https://github.com/LeXwDeX/gantry/issues/125) |
| #87 M1 宣布方式：单步还是两步（[m1-acceptance.md §5](docs/m1-acceptance.md#5-推进方式与宣布方式已确定)） | M1 对外的中间状态怎么说；不阻塞开发 | 已决：两步，见 [#87](https://github.com/LeXwDeX/gantry/issues/87) |
| #87 `decisions.md` 待决的 Q-10（首台配对为手机档案）、Q-11（`raise_cap` 追加预算） | 分别阻塞 `pki` 的安装引导期、`notify` 的选项与引擎预算追加 | 已决：见 [#87](https://github.com/LeXwDeX/gantry/issues/87)，写入 decisions.md D-18、D-19；`budget.limit_raised` 的契约见 #129 |
| GitHub 账户计费（#86，由用户处理） | 远端 CI；恢复前所有 PR 只能靠本机检查合入 | 待用户处理 |
| Q-12（结果对象读取路由） | M2 的补丁签名审批；不在 M1 范围 | 待决 |

M0 契约文本已由用户在 2026-10-27 统一批准，状态标注已在 #85 更新。

## 后续规划（2026-10-26 起，按顺序；每步单独授权）

原则不变：契约先于实现、一个 PR 一件事、待决项不自行拍板、替身通过不等于真实环境通过。

**协作方式**：Claude 负责指挥与评审，开发按 GitHub 里程碑与 issue 推进。每完成一个里程碑回来做一次**里程碑评审**：对照本节与 issue 的完成标准检查已合入代码，纠偏、查错，必要时调整框架结构；评审结论与下一轮的里程碑和 issue 再写回本文件。2026-10-27 起全部任务已建成 issue，按里程碑推进：

| 里程碑 | 对应阶段 | issue |
| --- | --- | --- |
| [M0.9 M0 遗留收口](https://github.com/LeXwDeX/gantry/milestone/2) | M0 遗留 | #82–#86 |
| [M1.1 决策器：契约补缺与 decide 实现](https://github.com/LeXwDeX/gantry/milestone/1) | 阶段 1、2 | #64–#75 |
| [M1.2 服务端泳道（替身对接）](https://github.com/LeXwDeX/gantry/milestone/3) | 阶段 3 | #88–#100；待拍板汇总 #87 |
| [M1.3 桌面端、组装与部署](https://github.com/LeXwDeX/gantry/milestone/4) | 阶段 3 | #101–#104 |
| [M1.4 M1 验收（B / C 级）](https://github.com/LeXwDeX/gantry/milestone/5) | 阶段 4、5 | #105–#107 |
| [M2-prep Agent 扩展性预留](https://github.com/LeXwDeX/gantry/milestone/6) | 阶段 6 | #108–#110 |

### 阶段 0：合并上一轮 PR（已完成）

#59、#60、#63、#62 已于 2026-10-26 按序合入，#77、#78、#79 随后合入；#48、#61 已关闭。#58 留到 `decide` 实现合入后关闭。

### M1.1 关键路径（阶段 1、2）

M1.1 的 issue（#64–#75）都还没有动工。

- **可以立即开工**：#64（E1）、#65（E4）只依赖已合入的 #63，可与 #69 并行；#68（E5）不阻塞，随时可做。
- **串行主线**：#69 → #70 → #71 → #72 → #73 → #74。#72 等 #66 拍板，#73 等 #67 拍板，#74 另依赖 E1（#64）、E4（#65）。
- **可并行**：#75（桌面场景适配器 `level_changed` data 与契约不符，归 `fe-desktop` 泳道）与引擎主线无关。

### 阶段 1：契约补缺（engine.md §7.14.6 的 E1–E6，各自独立契约 PR）

| 序 | 缺口 | 改动面 | 完成标准 | 阻塞 |
| --- | --- | --- | --- | --- |
| 1 | E1 `attempt.finished.result_commit`（#64） | `core/events.rs`、`schemas/events/attempt.finished.json`、夹具正反例、engine.md §11、repo.md OQ-R2 | `status = succeeded` 且有补丁时必填，与 `result_tree` 同现；反例：有 tree 无 commit 被拒 | **M1 端到端、重启一致** |
| 2 | E4 `SandboxService::recover` 区分"未受理 / 受理后丢失"（#65） | `core::ports::sandbox`、`FakeSandboxService`、sandbox.md §6.1 | 替身一致性测试覆盖两种结果 | M1 重启一致 |
| 3 | E3 `Deny { report: None }` 的上报码（#66，需拍板） | policy.md §6（推荐：对 `engine` 发起的动作 `Deny` 必带 `ReportHint`）或 data-model 词表 | 二选一由用户定；`FakePolicy` 同步 | M1 失败路径 |
| 4 | E6 Outbox 终态失败后的重做入口（#67，需拍板） | data-model 上报词表（推荐给 `outbox_failed:*` 加 `retry` / `abandon` 选项）、notify、api | 选 `retry` → 新 `epoch` 的动作与新审批 | M1 失败路径 |
| 5 | E5 `attempt.finished.accepted_open_pr`（#68） | 同 E1 | 执行器据 `result_ref` 从 CAS 取提议原文填入 `create_pr` | 不阻塞（先用模板） |
| 6 | E2 `node.finished.fingerprints[]`（M2，暂不开 issue） | 同 E1 | ≤ 64 条，仅 `REVISE` / `REJECT` | M2 |

### 阶段 2：`be-engine` 实现 `decide`（契约批准后，按切片各一 PR）

1. **类型与状态折叠**（#69）：§7.14 的类型落到 `crates/gantry-engine`；投影补 `outbox`、`approvals`、`locks`、`open_inbox`、`stage_entered`、`abandon_cause`、`worktree`、`Evidence`。验收：从夹具事件序列重建的 `DeliveryState` 与逐字段期望一致。
2. **Low 正常路径的纯决策**（#70）：T2 → 建 `low.implement` → 派发 → `Finalized(Committed)` → T6 → `expected_actions` 出 `push_branch` / `create_pr` → T8 → T10（只等 CI）→ 合并审批两段式 → T12。验收：§12.2"端到端（替身）""合并需要审批"在纯函数层面通过。
3. **执行体与路由**（#71）：`DeliveryActor`、在途集合、收敛循环、`EventRouter` 路由表与低水位游标。验收：§12.2"决策纯度""两段式判定""关闭客户端任务继续"。
4. **`AttemptRunner` 与终结管线**（#72）：接 `FakeSandboxService`、`FakeRepo`、`FakePolicy`；V1–V6 与失败分类。验收：§12.2"失败分类优先级""重试策略""自报不驱动状态""JobSpec 隔离""材料来源""预算与计量""受保护路径"。
5. **控制面与异常路径**（#73）：暂停 / 继续 / 重试 / 放弃、T11 / T14 / T15、`base_advanced` 对账。验收：§12.2 对应条目。
6. **崩溃点矩阵**（#74）：§12.2"重启一致""不重复创建 PR""release"。依赖 E1、E4 已落地。

每片都附上 §7.14.5 前置条件表中对应行的正例与反例。

### 阶段 3：M1 其他泳道（按对引擎端到端的依赖排序）

服务端泳道见 [M1.2](https://github.com/LeXwDeX/gantry/milestone/3)（#88–#100），组装、桌面与部署见 [M1.3](https://github.com/LeXwDeX/gantry/milestone/4)（#101–#104）。

`policy` v0 与 `notify`（端口与替身已有，体量小）→ `repo`（镜像、工作树、锁、补丁）→ `forge`（Outbox 执行、webhook、对账，先对 `FakeGitHub`）→ `models` + `egress`（回环反向代理、计量）→ `sandbox` + `worker`（rootless Podman、Claude Code 适配）→ `api` + `pki` + `secrets`（mTLS、配对、签名审批）→ `desktop-shell`（密钥、事件流、签名）。每个泳道按其模块规格"测试与验收"一节逐条完成；规格不足时停下开 Issue。

**并行推进（2026-10-27）**：已按泳道开 issue，与阶段 2 并行推进。各泳道只依赖 `core::ports` 和现有替身，不依赖 `decide`；上面的顺序是对引擎端到端的依赖顺序，不是开工顺序。

### 阶段 4：B 级环境（本机虚拟机）

见 [M1.4](https://github.com/LeXwDeX/gantry/milestone/5)：#105（A 级跨泳道集成）、#106（B 级环境）。

按 [M1 验收证据分级](docs/m1-acceptance.md) §3 的 B 列，把每条退出标准做成可重复脚本；不需要任何第三方账号。

### 阶段 5：C 级外部资源

见 [M1.4](https://github.com/LeXwDeX/gantry/milestone/5)：#107（需拍板）。

B 级全绿后，按 [§4 清单](docs/m1-acceptance.md#4-外部资源最小清单c-级)申请资源，只复跑 B 级已通过的场景。

### 阶段 6：Agent 可扩展性预留（调研 → 设计 → 契约，不早于阶段 2 完成）

见 [M2-prep Agent 扩展性预留](https://github.com/LeXwDeX/gantry/milestone/6)：#108（调研）、#109（ADR）、#110（契约）。

**现状**：worker 规格把仓库内的 `.claude/`、hooks、`.mcp.json`、插件、项目级技能一律禁用（worker.md K2、§6.4；worker-protocol §10"适配层：Claude Code 与预留"；I5），`WorkerAdapter`（`core::ports::worker`）只有镜像、命令、环境、能力表与 `validate_agent_options`，没有任何扩展点。这是正确的安全底线，但未来要换需求时没有落脚处。

**目标**：给 skill、MCP、hooks、上下文管理各留一条**只来自服务端可信配置**的扩展通道，需求变更时只改配置与适配层，不改引擎与契约骨架。

**设计约束（先写进 ADR，再动契约）**：

1. **来源只有服务端**：扩展定义存放于服务端设置（I7，放宽类改动需 approver 签名，P-54），按项目覆盖、只能收紧；仓库内容永远不是扩展来源（I5）。
2. **照搬 `injection` 的落地节奏**：先在契约里定义类型，M1 拒绝非空，后续里程碑逐类启用（与 `ConfigView.injection` 一致）。
3. **分层归属**：
   - 运行在沙盒内的扩展（技能文件、hooks、本地 MCP 服务）由 `worker` 适配层写入 `HOME`，计入沙盒资源限制；
   - 需要出网的 MCP 走 Egress 白名单与计量（P-49、P-50），不获得新凭据；
   - 服务端侧的生命周期钩子（节点开始 / 结束、阶段迁移）只能观察事件或产出建议，不得绕过 `policy` / Outbox（I2、I3）。
4. **能力协商**：`AdapterCaps` 声明支持哪些扩展种类与版本；`JobSpec` 增加 `extensions`（引用 + 摘要，不内联可执行文本）；不支持即拒绝，失败闭合（W8）。
5. **上下文管理是策略端口**：材料截断、上游产物摘要、多轮压缩、记忆，统一收敛为可替换的策略（`sandbox.stage` 与 `engine` 的 `ReservationPolicy` 同形），可单测、可按角色配置，不写死在适配层。
6. **可审计**：每次尝试记录实际生效的扩展清单与摘要（进入 `attempt.started` 或 JobSpec 摘要），便于事后核对"这次 agent 带了什么"。

**参考实现调研**（重点看边界划分与功能取舍，记录前身项目踩过的坑，避免在 Gantry 重犯；不照搬代码）：

| 方向 | 候选参考 | 要回答的问题 |
| --- | --- | --- |
| Agent 扩展模型 | Claude Code 自身的技能、MCP、hooks、插件、子代理机制（worker 第一实现即它） | 哪些扩展在 headless 模式下可控、可关、可审计；配置作用域如何分层 |
| DAG 编排（**首要**） | 前身项目 [GraphAgent](https://github.com/LeXwDeX/OpenCode-GraphAgent)（AGPL：只借鉴设计与教训，不复用代码，见 README）；对照参考 Argo Workflows、Temporal、LangGraph | GraphAgent 在图定义与执行状态的分离、重试与人工介入、图修订方面的边界哪里划错了、Gantry 现行设计（`graph_rev`、`decide` 纯决策、§7.14）是否已经避开 |
| 规格驱动交付（**首要**） | 前身项目 [SpecGit](https://github.com/LeXwDeX/SpecGit)（"Issue → PR → 验证完成"交付语义的来源）；对照参考 GitHub spec-kit | SpecGit 的规格 / 计划 / 任务与 GitHub 证据之间的边界、哪些功能取舍后来证明是错的；能否映射到 `spec` / `plan` 阶段、`admission` / `plan` 角色与 Outbox |
| 沙盒化编码 agent | OpenHands、SWE-agent 等 | 沙盒与宿主的接口面、工具调用的信任边界、上下文压缩策略 |

**产出**：一份调研记录（docs/reviews/，只读）；一条 ADR（扩展通道的边界与分层）；随后才是契约 PR（`JobSpec.extensions`、`AdapterCaps`、设置资源、上下文策略端口），M1 只拒绝非空。

## 历史

M0 时期的检查记录、送审与收口记录、后端修复启动材料等已从本文件删去，不另存档。需要时查 [docs/reviews/](docs/reviews/) 下的评审记录和本文件的 git 历史。
