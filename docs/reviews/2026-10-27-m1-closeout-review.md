# M1.1–M1.3 收尾评审（2026-10-27）

> 只读记录。评审对象：`main@03f2d24`。方法是静态抽样：5 个并行只读审查切片，随后由主力回读关键源码核对。本文不声称逐行全审，也不是测试报告或真实环境验收报告；静态审查阶段没有运行测试、没有启动真实 Podman、没有执行真实 GitHub 集成或真实桌面集成。之后主力在该基线运行 `cargo check --workspace --all-targets` 并通过；其他合并门槛检查仍需复跑。

## 1. 结论

- **M1.1 的 issue 范围已关闭。** 18 个 issue 全部关闭；`decide`、执行体、运行时、控制面、崩溃点矩阵和失败路径均已合入。里程碑本身仍为 open，等 #291 收尾后关闭。
- **M1.2 的 issue 范围已关闭。** 16 个 issue 全部关闭；服务端模块和 SQLite store 都已有 crate 和测试；桌面外壳的完成状态见 M1.3。证据主要来自替身与模块测试，不等于真实 GitHub、模型、Podman 或密钥环境已联调。
- **M1.3 仍在进行。** #101、#102、#103、#104 已关闭，#289、#291 仍 open。
- **M1.4 仍开放。** #105 是 A 级跨泳道集成；#106 B 级本机 Ubuntu 虚拟机与 #107 C 级外部资源都需要先征求用户确认，C 级还需要真实资源。
- **生产装配未闭合。** SQLite 持久层已实现，但 `gantryd::assemble_real` 总是返回 `NotReady`（`crates/gantryd/src/assembly.rs:93-99`）；替身装配只在 `crates/gantryd/tests/support/`。#289 只补引擎侧的 settings 换算、默认预留策略和 `status()`，不等于完成真实装配；完整替换由 #105 负责。
- **验收宣布方式不变。** A + B 级全绿才记为「M1 本机验收通过」，C 级全绿才记为「M1 退出」（#87）。
- **已关闭 issue 不等于规格全部实现。** 本轮确认的问题已分别登记 #293–#310；#311–#315 仍是待复核疑点，不写成已证实缺陷。

里程碑计数于 2026-10-27 用 `gh` 只读核对：

| 里程碑 | open | closed | 说明 |
| --- | ---: | ---: | --- |
| M0.9 M0 遗留收口 | 2 | 7 | #86、#292 open |
| M1.1 决策器 | 0 | 18 | 等 #291 收尾 |
| M1.2 服务端泳道 | 0 | 16 | 等 #291 收尾 |
| M1.3 桌面端、组装与部署 | 2 | 4 | #289、#291 open |
| M1.4 M1 验收 | 3 | 0 | #105–#107 open |
| M2-prep Agent 扩展性预留 | 3 | 0 | #108–#110 open |

精确的 issue 范围：M1.1 为 `#58`、`#64–#75`、`#80`、`#125`、`#129`、`#133`、`#277`；M1.2 为 `#87–#100`、`#150`、`#220`。

## 2. 结构评审

### 2.1 包拓扑与依赖方向

- 根 workspace 有 20 个 package：19 个 `crates/*` 加 `apps/desktop/src-tauri/shell-core`（package `gantry-desktop-shell`）。
- `apps/desktop/src-tauri` 是独立的 Tauri 应用，因系统 WebView 依赖被根 workspace 排除；其可信逻辑在 `shell-core`，仍由根 workspace 覆盖。
- 模块 crate 均存在：`core`、`store`、`engine`、`api`、`forge`、`repo`、`sandbox`、`worker`、`egress`、`models`、`policy`、`notify`、`conversation`、`pki`、`secrets`、`deploy`、`gantryd`、`fakes`、`xtask`。
- `gantry-engine` 的普通依赖只有 `core` 与 `store`；`gantry-fakes` 只作为 dev-dependency。端口集中在 `core::ports`，替身留在 `gantry-fakes`。
- 上述是抽样核对的依赖方向，不是全部边界的验收结果。

### 2.2 质量、扩展性与稳固性

| 维度 | 抽样结论 | 处置 |
| --- | --- | --- |
| 质量 | 分层清楚，事件与状态机集中在 `core`/`store`；模块测试数量较多，错误类型和失败闭合有统一方向 | 继续按各 issue 的验收补测试；不把模块测试数量当成规格覆盖证明 |
| 扩展性 | 跨模块调用经 `core::ports`（P-51），写接口用 `ActorCtx`（P-78）；换实现只改装配是设计目标，并非已验证 | 真实生命周期与模型接线仍待 #105、#314；agent 扩展通道仍为零，归 #108–#110 |
| 稳固性 | 进程内与 SQLite 存储均有实现；引擎有启动恢复与崩溃点矩阵；但运行中失败回报、锁、Outbox 失败和部分恢复路径仍有缺口 | #293、#302、#309、#295、#304 跟踪；#296 是计数一致性冲突，不是“缺少 resolved 事件”违规 |
| 边界设计 | 沙盒、Forge、策略、桌面外壳的边界在代码上分开；审批保护、SSE 角色刷新、Forge 写串行等仍有已登记缺口 | 本轮只确认“端口归属抽样成立”，不能写成这些路径全部通过 |

### 2.3 装配与生命周期

- `gantryd::assemble_real` 是唯一的真实装配入口，目前固定返回 `NotReady`，不退回内存日志；这是失败闭合，不是可运行服务端。
- 替身装配在 `crates/gantryd/tests/support/`，注册 `fake.*` 命令组；进程级测试通过重执行测试二进制启动子进程，在其中调用替身装配和 `gantryd::serve`，覆盖 §12.2 的替身主路径；这不是生产 `gantryd` 二进制的运行证据。
- #289 只提供引擎侧前置，不负责把 SQLite、`api`、`pki`、`secrets`、真实 `policy`、真实 `forge` 等一起接入；完整替换与生命周期宿主归 #105，部分生命周期疑点归 #314。
- 桌面端有 `scenario`、`mock`、`Tauri` 三种适配器；`Tauri` 应用不在根 workspace，真实服务端联调尚未证明。`#103` 已关闭，但不能据此说真实 API 已联调。

### 2.4 契约与文档

- 契约链仍是：`docs/interfaces/` 与 `data-model.md` → `gantry-core` 类型/校验 → `cargo xtask schemas` → `schemas/` 与夹具。
- `AGENTS.md` 第 1、2 节已按本轮事实更新；`NEXT-STEPS.md` 已删除“M1.1 未开始”等过时表述。
- 其他入口仍有旧状态，例如 `docs/m1-acceptance.md` 顶部仍写“其他模块没有 crate”；由 #299 跟踪。本文不改动验收标准。

## 3. 代码评审：已确认并登记

以下问题均已开 issue；本轮不实施修复，也不声称已修复。行号是静态评审位置，后续重构可能移动，应以 issue 的验收为准。

| 编号 | Issue | 静态证据 | 处置 |
| --- | --- | --- | --- |
| C1 | #293 engine 失败回报与锁 | `crates/gantry-engine/src/decider.rs:824-830` 忽略 `EffectFailed`/`LockBusy`；`effects.rs:144-206` 会产生回报；`runtime.rs:244-275` 只在启动时恢复 | 已登记实现缺口；按 issue 补故障注入与可控时钟测试 |
| C2 | #302 Outbox 终态失败 | `crates/gantry-engine/src/control.rs:361-388` 未处理 `OutboxFailed`；`decider.rs:738-759` 跳过旧动作；`decider.rs:55-56,868` 固定初始 `epoch` | 已登记；修复后补失败、重试、重启测试 |
| C3 | #309 预算记账与耗尽 | `crates/gantry-engine/src/state.rs:499-518` 折叠包括 `forge_writes` 在内的预算维度，但未产生 Forge 写入的记账事件；`decider.rs:14-15,1587-1590` 在预留为零时直接不产出 | 已登记；补 Forge 写入记账与耗尽上报 |
| C4 | #306 T12 证据 | `crates/gantry-engine/src/decider.rs:304-322` 的 `merged` 只检查 PR 状态和审批；`tests/decide_low.rs:666-670` 直接断言进入 closed | 已登记；补 Issue 关闭与合并证据的负向测试 |
| C5 | #310 引用取材 | `crates/gantry-engine/src/decider.rs:1515-1546` 固定填写空 `refs`；`attempt.rs:696-751` 按 `refs` 拉材料 | 已登记；补真实派发路径测试 |
| C6 | #300 store 状态机 | `crates/gantry-store/src/projection.rs:110-186` 的交付投影只比较部分旧值，未调用完整状态机；`lib.rs:921-968` 依赖该折叠 | 已登记；补非法阶段/档位/暂停/放弃迁移拒绝测试，与 #267 协调 |
| C7 | #303 Forge `head_epoch` | `crates/gantry-forge/src/normalize/snapshot.rs:7-8` 明确暂用快照；`normalize/mod.rs:988-1025` 无前一快照时从 0 开始 | 已登记；补删除/重建快照后的序号测试 |
| C8 | #308 未定义审批形状 | `crates/gantry-forge/src/outbox/mod.rs:95-114` 只限制 merge 与 push 的缺失审批；其他动作携带审批仍可入队 | 已登记；执行前按契约失败闭合 |
| C9 | #305 `ForgeReader` 未接通 | `crates/gantry-forge/src/reader.rs:95-112,244-250` 返回 unavailable；`outbox/executor.rs:218` 已有镜像拉取实现 | 已登记；通过端口接快照与 `sync_mirror`，与 #105 协调 |
| C10 | #298 Forge 写串行与预算排除 | `crates/gantry-forge/src/outbox/executor.rs:274-305` 传入空排除集合；安装级写闸和间隔需并发验证 | 已登记；补并发、间隔、重启测试 |
| C11 | #307 SSE 角色与吊销 | `crates/gantry-api/src/sse.rs:315-385` 只在心跳刷新角色；目录读取错误时沿用旧角色 | 已登记；补降级、吊销、目录不可用测试 |
| C12 | #295 sandbox 受理恢复 | `crates/gantry-sandbox/src/service/mod.rs:584-586` 的 `Meta::new` 没有 `resume`；`job.rs:179-192` 只在内存填入；`service/mod.rs:334-361` 无 `resume` 时按未知作业处理 | 已登记；补崩溃点测试 |
| C13 | #304 上游错误摘要 | `crates/gantry-sandbox/src/service/job.rs:987-1007,1088-1093` 吊销令牌后丢弃上游错误并固定 `upstream_error_summary=None`；`engine/attempt.rs:805` 依赖该字段 | 已登记；先按 #304 推荐传递计数，持续错误阈值仍待 OQ-E5 决定 |
| C14 | #296 notify supersedes 计数 | `engine.md:641` 明确规定被 `supersedes` 的旧项不会另发 `inbox.resolved`，所以“未发 resolved”本身不是违规。静态证据是 `crates/gantry-notify/src/inbox.rs:86-131` 只写 `inbox.raised`，而 `notify.md:250` 的客户端计数规则只列 `raised` / `resolved` / `dismissed`，没有说明 `supersedes`，形成文档与客户端计数的冲突；`crates/gantry-store/src/lib.rs:1233-1287` 在投影中直接关闭旧项 | 已登记；按 #296 评论保持单条 `raised`，由客户端按已知被取代的 open 项扣减，缺少本地旧项时重取 summary；需要新增事件前先做契约 PR |
| C15 | #297 conversation 引用 | `crates/gantry-conversation/src/service.rs:697-720` 对 PR 无条件返回 false，Issue 只在已有交付内搜索 | 已登记；按 store 快照补解析，与 #105 协调 |
| C16 | #301 Forge HTTPS | `crates/gantry-forge/src/github/http.rs:1-4,51-55` 只支持明文 HTTP；遇到 `https://api.github.com` 失败闭合 | 已登记；真实 GitHub 验证归 #107 |
| C17 | #294 models 令牌与对账 | `crates/gantry-models/src/router.rs:614-642` 缓存命中不检查到期时间；`gantryd/src/lib.rs:93-97` 目前只运行引擎与控制通道 | 已登记；按 #294 补轮换、对账和生命周期 |
| C18 | #299 其他入口文档 | `docs/m1-acceptance.md` 顶部仍称服务端/桌面 crate 不存在；README、桌面说明等仍需同步 | 已登记文档项；本任务只改 AGENTS、NEXT-STEPS 和本记录 |

另有既有追踪项：

- **#267**：store §12 剩余验收条目，不是本轮新发现，也不因 #300 而关闭。
- **#280**：api 读路由、`PageQuery` 方向、SSE 与 store 的衔接，以及共用行为用例扩展。
- **#292**：只记录 MSRV 1.85 下 `--all-targets` 的已知失败；普通 `cargo +1.85 check --locked --workspace --all-features` 通过。不能把两者混为同一结果。

## 4. 待复核项

以下 issue 的正文明确写“尚未复现”或“待核实”，因此只登记为疑点，不写成已证实缺陷。

| Issue | 待核实内容 | 当前处置 |
| --- | --- | --- |
| #311 | sandbox/worker/egress 的令牌脱敏、活动作业恢复、collect 重试窗口与清理 | 已开 review issue；#311 评论已记录规格冲突的推荐解释，仍待隔离替身或故障注入复核 |
| #312 | Forge 出站归因、PR 关闭归因、405/422 重试、`policy.denied` 与 `patch.rejected` 因果、镜像仓恢复 | 已开 review issue；待契约比对和故障注入 |
| #313 | engine 无补丁成功上报、`BaseConflict` 重试 base、预算耗尽 `used`、多项目快照隔离、全量折叠规模 | 已开 review issue；待可重复测试或源码证据 |
| #314 | `gantryd` 生命周期宿主、conversation 角色/错误、models 重载、notify dismiss 映射 | 已开 review issue；与 #105 协调，不把待实现装配写成已运行 |
| #315 | 审批已消费后 head 变化、pki 配对重启恢复/吊销日志权限、secrets 目录隔离、桌面敏感 IPC 与签名取消竞态 | 已开 review issue；待复现，不在本 issue 改变审批或密钥契约 |

#304 的补充边界：源码事实是上游错误数据被丢弃；持续错误阈值仍由 `docs/modules/egress.md` OQ-E5 待决。本文不把“仅存在 summary”当成已批准阈值，也不自行确定阈值。

## 5. I1–I8 的有界证据与已知缺口

下表只记录本轮能核对到的局部证据和已知反例。它不是不变式验收表，不能据此说 I1–I8 已全部满足。

| 不变量 | 有界证据 | 已知缺口 | 处置 |
| --- | --- | --- | --- |
| I1 沙盒只计算 | `crates/gantry-sandbox/src/invariants.rs` 校验容器名、标签、环境、挂载、资源与网络，只允许一个具名 sandbox 网络；`crates/gantry-worker/src/prompt.rs:42-64` 用 nonce 标记不可信材料；`sandbox`/`worker` 的 crate 依赖中没有直接 HTTP 客户端 | 真实 Podman 网络隔离未运行；#311 令牌/恢复疑点待复核；#295 受理恢复；#304 错误摘要。真实网段限制归 #106 | #295、#304 修复；#311 复核；#106 B 级 |
| I2 GitHub 写入只经 Outbox | GitHub 交付写动作（`push_branch`、`create_pr`、`merge_pr`）在 `crates/gantry-forge/src/outbox/executor.rs` 执行；`gantry-repo` 只做本地提交；静态搜索未发现 repo 层直接 `git push` | #298 写串行与预算排除、#303 `head_epoch`、#305 `ForgeReader`、#308 审批闭合、#301 HTTPS 均未完成；真实 GitHub 未验证 | 按 issue 修复；#105 集成；#107 C 级 |
| I3 放行只由 policy 决定 | `gantry-engine/src/effects.rs` 调用 `PolicyPort` 判定动作/审批；`attempt.rs` 调用策略做补丁提案判定；`gantry-forge/src/outbox/executor.rs` 执行前调用 `consume_for_execution`；`gantry-policy/tests/acceptance_m1.rs` 有签名审批用例 | #308 未定义审批形状；#315 审批消费后的 head 变化与重放待复核；#293 失败回报；真实签名设备未验证 | #308 修复；#315 复核；#107 C 级 |
| I4 状态事实源与对账 | `gantry-store` 单写者事件日志、SQLite 持久层、投影和 CAS 已实现；`gantry-forge/src/normalize` 有删除投递后对账补发、重复 webhook 去重和快照测试 | #300 状态机检查缺失；#303 `head_epoch`；#305 快照读取；#267 剩余验收。真实 GitHub 语义未验证 | #300、#303、#305 修复；#267 继续；#107 C 级 |
| I5 外部文本不可信 | `gantry-core/src/text.rs` 提供 `UntrustedText` 和规范化；`worker/src/prompt.rs` 注入 nonce 隔离标记；`worker/tests/prompt.rs`、`runner.rs` 覆盖注入尝试和输出解析 | #310 实际派发时 `refs` 为空，材料拉取路径未被真实覆盖；#311 令牌写入进度/报告疑点；#304 上游摘要。真实模型行为未验证 | #310 修复；#311 复核；#304 修复 |
| I6 客户端不执行、不持凭据 | Tauri 应用只注册白名单 IPC；可信逻辑在 workspace 成员 `shell-core`；前端只有 `src/ipc/mock/` 可 `fetch`，scenario/mock 按构建 mode 折叠；`shell-core` 有硬件/软件密钥与审批签名测试 | 真实 Tauri、CSP、系统钥匙串、真实 API 均未验证；#315 敏感 IPC、重试、签名取消竞态待复核；#307 SSE 吊销 | #315 复核；#307 修复；#105 真实集成 |
| I7 配置与工作分离 | `gantryd/src/config.rs` 使用 `deny_unknown_fields`，并有“settings 键不能出现在引导配置”的测试；设置写入经 `api/settings.rs` 与 policy 审批路径 | 真实 settings 热加载与生产装配未验证；#299 其他入口文档过期；不能把 `injection` 的非空拒绝写成已全部验收 | 保留测试；#299 文档；真实装配归 #105 |
| I8 产物先验后推 | `gantry-repo/src/gate.rs` 做补丁校验、机械事实与策略判定，`commit.rs` 只在干净检出上提交；`gantry-forge` 负责后续推送；`repo/tests/patch_gate.rs`、`commit.rs` 覆盖路径、二进制、符号链接、base 冲突和幂等 | #300 状态机；#309 预算；#306 T12；#310 材料；#312 归因与仓库恢复待复核；#313 运行时疑点 | 按 issue 修复/复核；#105 集成 |

## 6. 覆盖限制与已排除

### 6.1 覆盖限制

- 本文是 `main@03f2d24` 上的静态抽样，不是逐行全审，也不是验收报告。
- 静态审查阶段没有运行测试、没有启动真实 Podman、没有执行真实 GitHub 集成或真实桌面集成；不能把静态证据写成运行结果。之后主力在 `main@03f2d24` 运行 `cargo check --workspace --all-targets` 并通过，该检查不是测试或真实环境验收。
- 代码图谱：project `Users-suntao-Documents-code_resource-gantry`，generation `2026-10-27T06:33:21Z`。范围级没有记录缺口，但“无记录缺口”不保证覆盖完整；`decider`、`store`、`gantryd`、`forge`、`policy` 等关键记录为 `metadata_changed`，结论以源码读取为准。
- 子代理无法完成逐文件图谱 coverage；本记录不宣称逐文件覆盖，也不把图谱结果当成唯一证据。
- 远端 Actions 仍因计费问题不可用；`main` 的 m0 run 40492372086 失败且各 job 没有执行任何 step，依据 #86 和 handoff 记录，不把它当作代码失败。

### 6.2 已排除

- 一个关于 `crates/gantry-egress/src/model_proxy.rs` 的 E0382 静态怀疑已排除：静态审查后，主力在 `main@03f2d24` 运行 `cargo check --workspace --all-targets` 并通过。
- 没有把 #311–#315 写成已证实缺陷；它们仍是待复核疑点。
- 没有把 M1.1/M1.2 的“issue 全关闭”写成“所有规格已验证”；已确认缺口按 #293–#310 跟踪。
- 没有把 `assemble_real` 的 `NotReady` 写成可运行生产装配；SQLite 已实现与生产接线是两件事。

## 7. 处置汇总

| 处置 | 项目 |
| --- | --- |
| 本轮文档更新 | `AGENTS.md` 第 1、2 节；`NEXT-STEPS.md`；本记录 |
| 已确认、已开 issue、未处理（含文档项） | #293、#294、#295、#296、#297、#298、#299、#300、#301、#302、#303、#304、#305、#306、#307、#308、#309、#310 |
| 既有追踪，未在本轮关闭 | #267、#280 |
| 待复核，未证实 | #311、#312、#313、#314、#315 |
| 外部阻塞 | #86 Actions 计费；#107 真实资源 |
| 已知 MSRV 项 | #292；普通 MSRV `check` 通过，`--all-targets` 失败 |
| 里程碑状态 | M1.1 18/18、M1.2 16/16 issue 已关闭，里程碑等 #291；M1.3 剩 #289/#291；M1.4 #105–#107 开放 |

本轮评审不执行 GitHub 写操作，不关闭 issue 或里程碑，不提交代码，也不修改验收标准。下一步按 [NEXT-STEPS.md](../../NEXT-STEPS.md) 的优先级执行：先完成 #291 收尾，再处理 #289、#105；#280/#267 可并行，#292 可插入；#106/#107 先征求用户确认。
