# 统一开发基线后续工作

本文件是当前迭代的入口：现状、待拍板事项和后续规划。项目总体情况与目录入口见 [AGENTS.md](AGENTS.md)，运行与检查入口见[根目录说明](README.md#本地接入)和[前端说明](apps/desktop/README.md)。

## 现状（2026-10-29）

代码基线 `main@37c61ef`（#362 的 PR #366 已合入）。M1.3 里程碑已关闭，收尾评审见 [M1.3 收尾评审](docs/reviews/2026-10-29-m13-closeout-review.md)。

- **M0 已合入。** 领域核心、端口、内存存储、测试替身、内存模拟接口和浏览器模拟场景前端都在 `main` 上；M0 契约文本已于 2026-10-27 由用户统一批准。
- **M1.1、M1.2、M1.3 的 issue 与里程碑均已关闭。** M1.1 18/18、M1.2 16/16、M1.3 6/6。#289 于 2026-10-29 依用户确认关闭：PR #323 提供了 `from_settings`、`EqualShareReservation` 与 `status()`；真实消费路径归 #105，自动热加载归 #318，在途预留归 #317。已关闭 issue 不等于每条规格边界都已满足，评审发现见 [M1.1–M1.3 评审](docs/reviews/2026-10-27-m1-closeout-review.md) 与 M1.3 收尾评审。
- **服务端各模块已有 crate 与测试。** 证据主要来自替身、模块测试和 #105 已合入的本机联测切片（SQLite 跨进程重启、真实策略审批、Forge 执行前审批消费、真实 Git 仓库与持久存储、仓库/沙盒与存储 CAS、执行器补丁策略链）；#319 的内置 schema 注册表与 #362 的任务身份已合入。真实 GitHub、模型、Podman、密钥环境尚未联合验证。
- **生产装配**：`assemble_real` 仍总是返回 `NotReady`（`crates/gantryd/src/assembly.rs:93-99`），生产二进制没有替身回退。替身装配在 `crates/gantryd/tests/support/`，仍用手写引擎配置；#323 的三个入口在 engine 之外尚无调用方，控制通道 `status` 未用 `status()`，生产命令组为空。切入点已写入 [#105 评论](https://github.com/LeXwDeX/gantry/issues/105#issuecomment-6487468346)。
- **桌面端**：命令白名单、CSP、编译期折叠 scenario/mock、仅 mock 可联网等边界已静态核对；Tauri 适配器没有自动化测试（#370），真实服务端联调尚未证明。
- **工作区**：20 个 package，即 19 个 `crates/*` 加 `apps/desktop/src-tauri/shell-core`（package `gantry-desktop-shell`）。Tauri 应用本身在 `apps/desktop/src-tauri`，因系统 WebView 依赖不在根 workspace。
- **验收口径**：A + B 级全绿才记为「M1 本机验收通过」，C 级全绿才记为「M1 退出」（#87）。当前三级证据都未全部取得。
- **已知外部阻塞**：GitHub Actions 因账户计费问题未启动（#86）；没有真实的 GitHub App、模型密钥、Podman 目标主机和代码签名。

**本机检查**：各 PR 描述记录了合入前的全工作区本机门槛；M1.3 收尾评审 PR 的描述记录了在合入后的结果上复跑的全部门槛（fmt、clippy、test、MSRV 1.85 两条 check、`xtask schemas --check`、mock-api、desktop lint/typecheck/test/build）。本机门槛不能替代真实环境证据。

**未实现或未验证**：
- 真实装配（#105）、真实 GitHub App、模型、Podman、目标主机与代码签名；桌面端真实 API 联调。
- #362 任务身份的真实 Podman 与生产装配路径。
- store 的剩余 §12 验收条目（#267）和 api 的读路由 / 共用用例（#280）。
- 评审确认的问题 #293–#310（文档项 #299）、#317–#322，以及 M1.3 收尾评审新登记的 #367–#371；逐项见 issue 与评审记录。
- #311–#315 是尚待复核的疑点，不代表已证实缺陷。
- 远端 CI 诊断项 #346、#347、#352。

**M0 遗留**：#292 已由 PR #324 修复并关闭；仍开放 #86（恢复远端 CI 并在 `main` 上补一次全绿）。

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
| [M0.9 M0 遗留收口](https://github.com/LeXwDeX/gantry/milestone/2) | M0 遗留 | #82–#86、#122、#135、#179、#292 |
| [M1.1 决策器：契约补缺与 decide 实现](https://github.com/LeXwDeX/gantry/milestone/1) | 阶段 1、2 | #58、#64–#75、#80、#125、#129、#133、#277 |
| [M1.2 服务端泳道（替身对接）](https://github.com/LeXwDeX/gantry/milestone/3) | 阶段 3 | #87–#100、#150、#220 |
| [M1.3 桌面端、组装与部署](https://github.com/LeXwDeX/gantry/milestone/4) | 阶段 3 | #101–#104、#289、#291 |
| [M1.4 M1 验收（B / C 级）](https://github.com/LeXwDeX/gantry/milestone/5) | 阶段 4、5 | #105–#107 |
| [M2-prep Agent 扩展性预留](https://github.com/LeXwDeX/gantry/milestone/6) | 阶段 6 | #108–#110 |

M0.9 只剩 #86；#82–#85、#122、#135、#179、#292 已关闭。M1.1（18/18）、M1.2（16/16）、M1.3（6/6）的 issue 与里程碑均已关闭。

### 当前优先级（2026-10-29 起）

1. **#105**：A 级跨泳道集成与真实装配，当前主线。第一步建议让替身装配改走 `from_settings` 与 `EqualShareReservation`，使进程级 §12.2 用例覆盖真实换算，再逐个替换替身；接线时一并核对 #317–#322，`settings.bootstrap_set` 的参数契约先按 #371 单独提契约 PR。
2. **#280、#267**：可与 #105 并行。
3. **M1.3 收尾评审的独立小项**：#367（deploy doctor 衔接）、#369（shell-core token 清零）、#370（前端适配器对账测试）无前置，可穿插；#368 需先改规格。
4. **#106 → #107**：B 级本机 Ubuntu 虚拟机、C 级外部资源。两项都要先征求用户确认，C 级还需要真实资源。
5. **M2-prep**：#108 → #109 → #110 按顺序推进；可与 M1 并行，不阻塞 M1.4。

### 本轮新发现

M1.3 收尾评审（2026-10-29）新登记：#367 deploy doctor 未检查区域与安装脚本退出码衔接、`install-state.json` 不可读时失败闭合、`--page` 文件权限；#368 控制通道客户端对端校验（需先改规格）；#369 shell-core 配对 token 清零与审批/地址绑定澄清；#370 前端 Tauri 适配器与路由表对账测试；#371 `settings.bootstrap_set` 参数契约与替身白名单。另在 #105 写入接线切入点，在 #315 补充已授权审批恢复路径的待复核点。

上一轮（2026-10-27）确认的问题仍按 #293–#310 跟踪，#311–#315 仍是待复核疑点。

### 阶段 0：合并上一轮 PR（已完成）

#59、#60、#63、#62 已于 2026-10-26 按序合入，#77、#78、#79 随后合入；#48、#61 已关闭。#58 已在 `decide` 实现合入后关闭。

### 阶段 1–2：M1.1 决策器（已收尾）

M1.1 的 18 个 issue（#58、#64–#75、#80、#125、#129、#133、#277）已全部关闭，不再是待开工列表。契约缺口 E1、E3–E6 已由 #64–#68 合入；`decide` 的六个切片（#69–#74）和桌面场景修正（#75）已合入。E2（`node.finished.fingerprints[]`）仍按规格留到 M2，不属于本里程碑。

已关闭 issue 不表示每条规格边界都没有缺陷；本轮评审确认的运行时缺口由 #293、#302、#306、#309、#310 跟踪，#313 是待复核疑点，不能把 M1.1 的关闭状态当成 engine.md §12.2 全部通过。

### 阶段 3：M1.2、M1.3 已关闭（真实装配未完成）

服务端泳道见 [M1.2](https://github.com/LeXwDeX/gantry/milestone/3)（#87–#100、#150、#220，16 个 issue 与里程碑均已关闭），组装、桌面与部署见 [M1.3](https://github.com/LeXwDeX/gantry/milestone/4)（#101–#104、#289、#291 均已关闭，里程碑已关闭）。

这些泳道已有 crate、模块测试和替身对接。**测试替身通过不等于真实模块已在生产装配中协同运行**：`gantryd::assemble_real` 仍返回 `NotReady`，SQLite 存储尚未接入生产装配；真实 GitHub、Podman、模型、密钥环境和桌面真实 API 联调都未验证。#289 引擎前置已合入但尚无调用方，真实装配由 #105 推进，不能把已关闭的泳道 issue 当成端到端完成。

#104 已关闭，但实机两次幂等执行、`systemd-analyze verify`、`nft -c` 及依赖 root、systemd 或沙盒的 doctor 检查延期到 #106；见 [#104 评论](https://github.com/LeXwDeX/gantry/issues/104#issuecomment-6433339355)和 [PR #207](https://github.com/LeXwDeX/gantry/pull/207)。#106 需先征求用户确认本机虚拟机环境。

### 阶段 4：A 级集成与 B 级环境（本机）

见 [M1.4](https://github.com/LeXwDeX/gantry/milestone/5)：#105（A 级跨泳道集成）、#106（B 级本机 Ubuntu 虚拟机）。两项通过后才算「M1 本机验收通过」。

A 级先按 [M1 验收证据分级](docs/m1-acceptance.md) §3 的 A 列，用真实模块替换替身并通过 engine.md §12.2；B 级再把每条退出标准做成可重复脚本。B 级不需要任何第三方账号，但 #106 需要用户先确认本机虚拟机与 Podman 环境。

### 阶段 5：C 级外部资源

见 [M1.4](https://github.com/LeXwDeX/gantry/milestone/5)：#107（需拍板）。

B 级全绿后，按 [§4 清单](docs/m1-acceptance.md#4-外部资源最小清单c-级)申请资源，只复跑 B 级已通过的场景。动作、GitHub App、模型密钥、目标主机和签名证书都需用户确认后才使用。

> [M1 验收证据分级](docs/m1-acceptance.md) 顶部的项目现状仍停在 2026-10-26；验收标准和分级口径没有变化，过时表述由 #299 跟踪。

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
