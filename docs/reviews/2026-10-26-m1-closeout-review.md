# M1 阶段收尾评审（2026-10-26）

> 只读记录。评审对象：`main@a499318`（M1 引擎 #26–#57），以及本轮 PR #59、#60、#62、#63、#77 合并后的状态。
>
> **合并前提**：本记录与同批的 AGENTS.md 更新（#78）引用了 #59、#60、#62、#63、#77 的内容（engine.md §7.14、`docs/m1-acceptance.md`、NEXT-STEPS「后续规划」、F1–F3 的修正）。必须按 #59 → #60 → #63 → #62 → #77 → #78 的顺序合并；其中任何一个尚未合入时，下文写的"由 #xx 修正"只是计划中的处置，不代表 `main` 上已经修好。依据 AGENTS.md「里程碑收尾」的前两项：框架结构评审、代码评审。

## 1. 结论

- 框架方向正确：跨模块调用一律经 `core::ports`，引擎只依赖 `core` 与 `store`，替身只作 dev-dependency，契约链（文档 → core → xtask → schemas → 夹具）有 CI 守住漂移。
- 有三处确定缺陷，由 #77 修正（关联 #76；须先于本记录合入）。
- 有五处边界问题。它们会随 decide 的实现切片一起重写，现在单独修正属于返工，所以已挂到对应 issue（#69、#71、#73），并写进各自的完成标准。
- 契约层面的两处缺口已在本轮补齐：#48（`level_changed` 与 P-82 冲突，#59 / #60）和 #58（`decide` 类型只有名字，#63）。另有六处事件字段缺口（E1–E6）列在 engine.md §7.14.6，已开 issue #64–#68，E2 留到 M2。

## 2. 框架结构评审

| # | 发现 | 影响 | 处置 |
| --- | --- | --- | --- |
| A1 | **同一批事件有两份投影**：`gantry-store` 的快照与 `gantry-engine` 的 `Projection` 各自折叠 `delivery.*`。#48 就是两边派生规则不一致造成的：store 会把 effective 收紧到上限，引擎不会 | 两份投影会持续漂移；状态机前置条件看到的状态与引擎决策看到的状态可能不一致 | #69：折叠规则收敛到一处，并加一条两处结果相同的一致性测试 |
| A2 | **事务内读不到状态**：engine.md §7.1 要求在 `WriteTx` 里先 `tx.read()` 再写，但 `gantry_store::TxReader` 只能读设置 | 引擎只能"事务外读、事务内写"，存在竞态窗口；store 拦下并发写后，引擎返回存储错误而不是幂等结果 | #71：先给 store 补事务内读取接口，再实现执行体 |
| A3 | **控制面端口未对齐**：`Engine` 收 `Actor` 而非 `ActorCtx`（P-78），没有实现 `core::ports::DeliveryControl`；`advance_stage` 直接相信调用方传入的 `from` / `level_effective` | 等到 `api` / `conversation` 接入时，接口要再改一遍；公共 API 允许写出与当前状态不符的迁移（store 会拦，但错误类型不对） | #73：控制面实现 `DeliveryControl` 并收 `ActorCtx`；`advance_stage` 改为内部函数 |
| A4 | **投影重建的方式**：`load_projection` 每次调用都从 seq 0 扫全量日志，并按 `correlation_id` 过滤；`delivery_of_node` 同样全量扫描 | 复杂度随日志总量线性增长；其他模块写的事件不一定带 `correlation_id`，会漏折叠 | #69：按 engine.md §7.14.2 的路由表归属事件，扫描范围限制在本项目或本交付单元 |
| A5 | **agent 扩展点为零**：worker 规格禁用了仓库内的 `.claude/`、hooks、MCP、插件，这是正确的安全底线；但 `WorkerAdapter` 和 `JobSpec` 没有任何服务端可信的扩展通道 | 需求一变就没有地方落脚 | NEXT-STEPS「后续规划 · 阶段 6」：先调研前身项目 GraphAgent / SpecGit，再写 ADR，最后改契约；照搬 `injection` 的"先定义类型、M1 拒绝非空" |

依赖方向、端口归属、契约生成链没有发现问题。

## 3. 代码评审

| # | 发现 | 处置 |
| --- | --- | --- |
| F1 | `Engine::create_delivery` 遇到同一 `source_message_id` 时，store 已按 `dedup_key` 去重，但函数返回新传入的、从未创建过的交付 ID，违反 engine.md §6 | 由 #77 修正并加测试 |
| F2 | `budget::charge` 在超出上限时拒绝记账且不修改台账，而 Egress 计量的用量是已经发生的事实（§7.9）。现有测试把这个错误行为写成了预期 | 由 #77 修正：一律记账，记账后达到上限即返回耗尽；测试按规格改写 |
| F3 | `budget::low_limits()` 作为公共 API 导出硬编码限额，节点数为 8，与 `levels.low` 默认值 6 不一致 | 由 #77 修正：删除，改为测试夹具 |
| F4 | 桌面场景适配器发出的 `delivery.level_changed` data 为 `{delivery_id, level_cap}`，与契约不符 | #75（`fe-desktop`） |

## 4. 规格层面已修订的问题（供追溯）

engine.md §7.14 经过独立评审代理两轮评审和 Codex 一轮评审，修订了以下问题（详见 #63）：

- 收敛判据与在途副作用；
- 写权锁的取得与重建；
- 建图前置条件与人工重试（原 §7.4"下游 skipped、Workflow failed"与 core 状态机冲突）；
- T15、合并审批的上报时序、派发总闸、事件路由与游标低水位；
- 放弃后的收尾可以重新派生；
- 预算耗尽也覆盖"下一步只因预算不足而无法产出"；
- `RequiredSucceeded` 要等非必需节点也终结。
