# 后端修复启动文件

## 当前状态（2026-10-25，第八轮）

B1–B9 原始缺陷已关闭，最新独立审计为 **PASS，限定于 B1–B9 原始修复和当前 M0 替身基线**。
下方旧启动提示词与缺陷清单保留为历史材料，不再是待修清单，不应据此重做 B1–B9 或启动 M1。
最新审计标识为 `gantry-dual-audit-20261025T205733/backend/final-review.md`，其 364 passed、
2 ignored / 模拟接口 29 passed 是本轮小修之前的实测值。

本轮仅处理该审计的 R1/P3：可疑动态键名进入校验错误路径及消息。已在报告构造处复用
既有凭据式字符串检测，定位到最近安全父路径；结构校验继续执行，正常 JSON Pointer
转义和合法 SecretRef 不变。4 个新增回归先失败后通过，`validate` / `validate_and_parse`
均覆盖。只用假值，没有证明真实密钥存储泄露或生产日志泄密。

本轮八项命令全部通过：Rust **368 passed、2 ignored、0 failed**（core 264、fakes 65、store 37、
xtask 2），模拟接口 **29 passed**，schema **0 failure、0 warning**。两个 ignored 为父测试
执行的看门狗入口；具体命令、缓存使用、修复证据与未验证平台见
[第八轮实施记录](docs/m0-implementation.md#错误路径回显加固与验收第八轮2026-10-25)，独立终审见
[第八轮跟进记录](docs/m0-review-followups.md#第八轮-r1p3-诊断回显收尾2026-10-25)。
实现内容标识（排除本轮三份记录）为
`09ce1f7bb157763e3052edf7fc176b9f13ec233bff20abfa471ceaf0aba4614c`。

本轮独立只读终审为 **PASS，限定于 R1/P3 诊断回显修复及验收记录**，阻塞项 0。
未重做 B1–B9，未启动 M1；R1 已完成，不再作为下一轮待修任务。

本轮只允许 core 的相关实现/测试及上述两份文档和本文件；起点已有修改保留，生产契约
与开始时摘要一致。R2 人为缺失 delivery 引用只跟踪，不加兼容逻辑。其余契约歧义不自行
裁定。完整 M0 冻结仍受 14 个未定事件 data、`ConfigView.injection` 非空形状及 8 种 drift
形状阻塞；Rust 1.85、Linux/Windows、真实外部系统及前后端联调未验证。不得从限定 PASS
外推生产可用，不提交、不推送、不合并、不发布。

## 历史启动材料

以下保留当时的提示词、范围和数字，不代表当前未修状态。

本文件供下一轮实施直接使用。本轮只交付提示词，没有修复实现，没有提交或推送。

## 启动提示词

```text
请在 backend-baseline 工作树中读取 AGENTS.md 和根目录 NEXT-STEPS.md，修复本文件 B1-B9 九项已确认问题，并完成回归验证和独立终审。这是实施任务，不要只返回计划。

先核对实际代码、HEAD 和全部未提交差异，固定本轮起点；保留已有工作，不重置、不回滚。按当前源码复现，先补失败回归，再做最小修正。已修复的条目用新证据确认，不重复改。

只在本文件授权的后端目录内工作，不改 apps/**、原型或前端工作树，不修改生产契约。优先修复 core 的 B3/B4 和 store 的 B5，再完成 fakes 的 B1/B2/B6、store 的 B7 及 Mock 的 B8/B9。同一文件只能有一个写入者，依赖步骤顺序汇合。

不要处理被驳回或仅待证的候选，不因测试替身而豁免已定义契约，也不要把已披露 M0 边界扩成生产实现。禁止删除有效断言、伪造事件序号、禁用并发、放宽安全校验或变更契约来换取通过。

运行本文件全部八项检查和针对性回归，再进行独立只读 review。遇到待决规格或必须修改契约，暂停该依赖项并说明阻塞，继续其他独立修复。最终按 B1-B9 报告状态、修改文件、修复前后证据、命令结果和 PASS/BLOCKING。不提交、不推送、不合并、不发布。
```

## 版本与先读材料

- 工作树：`/Users/suntao/orca/workspaces/gantry/backend-baseline`。不要在默认主仓库、前端工作树或审核副本实施。
- 上轮审核 HEAD：`dbf3bc546c9084f37556e26486d5199df1858a74`；merge-base：`19d85a5388d90ccfd1c5373253699599804f54b8`。
- 上轮 142 个变更文件指纹：`407ce949066a77111aa96b6981272c08c8eeba3bab5898db1345c51f3189c398`；全量 439 文件指纹：`bd9d8e2b229688df0b153f74b1f86d58bf4b7e99b2451c83abd43d6b5b0f20c6`。口径不同，不是漂移。本文件不在两份清单内。
- 上轮实现裁决：**BLOCKING，5 项 P1、4 项 P2，无 P0**。独立终审对报告给出 PASS，不代表后端实现通过。
- 执行 `git status --short`、`git rev-parse HEAD`，阅读现有差异。行号仅用于定位上轮版本，不能覆盖新修改或将历史证据当成本轮验证。
- 先读 `AGENTS.md`、`docs/milestones.md`、`docs/decisions.md` 已确定项、`docs/architecture.md` §5，以及 `docs/m0-implementation.md`、`docs/m0-contract-gaps.md`、`docs/m0-review-followups.md`。领取模块后按仓库规定阅读相关模块全文与其引用契约。
- 本轮主要涉及 `docs/modules/core.md`、`store.md`、`notify.md`、`policy.md`、`forge.md`，以及 `docs/interfaces/config.md`、`api.md`、`forge-actions.md`、`docs/data-model.md` 的相关章节。
- 若仍存在，审核证据位于 `/var/folders/jm/w5g1j5td4vl8164rqwl96fn80000gn/T/opencode/gantry-backend-audit-20261025/`。以其中 `final-review.md` 为最终清单，配合 `rust-repro.md`、`rust-extra-repro.md`、`mock-repro.md`、`verify/SUMMARY.md`。不要照搬已被纠正的静态候选表。
- 本文件已包含实施与验收所需信息，不依赖临时目录继续存在。外部探针断言的是错误现象，必须改为契约预期并纳入仓库回归，不能沿用其“通过”证明修复成功。

## 授权范围与硬边界

- 默认写入范围：`crates/gantry-core/**`、`crates/gantry-store/**`、`crates/gantry-fakes/**`、`tools/mock-api/**` 的相关实现、测试及模块内说明。每个工作包只写分配给它的目录或文件。
- 允许按真实结果更新 `docs/m0-implementation.md`、`docs/m0-review-followups.md` 和本文件的任务状态，不改写历史评审记录或删除真实限制。
- 仅确有必要时调整上述 crate 清单以启用已存在依赖的最小特性；不升级依赖、不新增依赖、不修改根 workspace 配置或锁文件来绕过问题。若现有依赖无法完成正确修复，先提出原因和最小额外授权。
- `core` 保持无 IO、无运行时依赖、无 unsafe；不得为修 B5 把异步运行时引入 core。
- 不修改 `apps/**`、`prototype/**`、`deploy/**`、其他工作树、`docs/reviews/**`。
- `docs/interfaces/**`、`docs/data-model.md`、`schemas/**`、`docs/decisions.md`、架构和模块规格保持相对本轮开始时字节一致。端口签名、线上字段和事件形状保持既有契约。确需改变时单独报告，不在本轮顺手修改或创建 PR。
- 后端起点本来已有未提交 schema 等变更，不能要求相对 HEAD 的 diff 为空，更不能回退它们。应记录起点内容摘要，证明本轮没有新增契约变化。
- 保留 `ActorCtx`、跨 Store 拒绝、revision 冲突保护、同一 Store 的唯一 Outbox 和现有幂等语义。按既有机制修根因，不造第二套状态、事件总线或并行锁权威。
- 不实现真实 SQLite、GitHub、Sandbox、PKI、完整引擎或未定事件；不把测试替身包装成生产安全保证。规格冲突按已确定决策 > 架构不变式 > 接口 > 模块 > 其他处理，待决项不自行拍板。

## 修复清单

路径均相对本工作树。P1 表示本基线必须优先修复的契约或保护失效，不意味着已证明生产漏洞。

### B1 / P1：收件箱事件与返回序号脱节

- 定位：`crates/gantry-fakes/src/inbox.rs:70-73,236-242,310-322,369-374`。
- 复现：共享空 Store 上 `raise(None, ...)` 后条目可读，但 `last_seq=1 head=0 events=0`。
- 最小修正：在同一 WriteTx 更新状态并追加已定义 `inbox.raised`，返回真实事件序号，重复请求沿用原序号。核对共用写路径及已定义收件箱事件，不能只修测试用的一条分支；不猜未定事件形状。
- 验收：首次一条事件，`last_seq == event.seq <= head`；预先存在其他事件仍成立；重复不增长；外部事务提交前不可见，提交后同时可见，回滚同时撤销；跨 Store 仍拒绝。
- 依据：`docs/modules/notify.md:124,382-383`。HEAD 旧有缺口，本轮共享 Store 改造未补齐。

### B2 / P1：审批申请缺少申请事件

- 定位：`crates/gantry-fakes/src/policy.rs:607-649`。
- 复现：合法 `PatchApplyProtected` 申请为 Pending，但 `head=0 events=0`。
- 最小修正：同事务创建审批并追加已定义 `approval.requested`，保留 `ActorCtx` 和真实配置修订来源。同主体命中 Pending 不重复追加。
- 验收：创建一条、重复零条；subject、requested_by、config_rev 等遵循契约；事件追加失败不留下审批；现有消费事件及事务回归仍正确。不能将旧“总共两条事件”硬编码保留为错误期望，也不能删除事务断言。
- 依据：`docs/modules/policy.md:102-104,390,421`。HEAD 旧有。测试辅助 grant 不要求补完整签名流程。

### B3 / P1：预算权重溢出归零

- 定位：`crates/gantry-core/src/entities.rs:2361-2369,2652-2653`；`crates/gantry-core/src/settings/validate.rs:368-377,725-736`。
- 复现：通过配置校验的有限正权重 `1e308`，1 个输入 token 得 `u64::MAX`，2 个得 0。
- 最小修正：合法正输入产生的溢出不能清零，优先沿用现有返回类型的饱和行为；不要修改公共接口或只在上游禁止极大值来掩盖计算方法缺陷。
- 验收：乘法及求和溢出不为零；合法权重下用量增加结果单调不减；默认权重、零用量、有限上限边界不退化；预算台账实际记账覆盖该值。
- 依据：`docs/interfaces/config.md:343`。HEAD 旧有。没有无权限用户修改权重或真实计费链路的攻击证据。

### B4 / P1：数组字符串绕过秘密值检测

- 定位：`crates/gantry-core/src/settings/validate.rs:193-228,833-835`。
- 复现：`{"allow_globs":["sk-audit-placeholder-not-a-real-credential"]}` 通过校验及解析，相同假值放合法标量字段则被拒绝。
- 最小修正：递归到字符串叶子时统一检查，对象键名检查和合法 SecretRef 例外保留。核对调用者，不复制第二套检测规则。
- 验收：`/allow_globs/0` 返回 `inline_secret_detected`，嵌套数组及其他已知前缀拒绝；普通 globs、合法 SecretRef.value 通过；解析入口也拒绝；错误和日志不得回显秘密原值。测试只用假密钥。
- 依据：`docs/interfaces/config.md:156,645` 的 V02。HEAD 旧有；不将本轮扩成全部事件数据泄漏审计。

### B5 / P1：异步写事务阻塞执行线程

- 定位：`crates/gantry-store/src/lib.rs:99-110,431-437,450-454`。
- 复现：单线程 Tokio 中两个独立任务竞争 WriteTx；持有者本可 yield 后释放，等待者却同步卡在 `recv()`，持有者再也得不到调度。
- 最小修正：异步竞争使用可返回 Pending 的等待机制，既有同步和异步入口共享同一单写者权威。检查同步辅助入口在异步调用链中的使用，不能新增隐蔽阻塞或两把互不关联的锁。
- 验收：两个独立任务正常完成；commit、rollback、Drop 均释放并唤醒；取消等待不丢令牌、不破坏排队和互斥；保留适用特性组合。使用外部进程看门狗使回归有界，只清理自己启动的进程。
- 禁止：同任务持锁再取锁作为唯一反例；仅靠运行时内 timeout；增加线程、禁用并发、移除测试或直接放宽互斥来掩盖死锁。
- 依据：`docs/modules/store.md:118,342`。HEAD 旧有；不要求 M1 的 SQLite、跨进程锁或大规模压力实现。

### B6 / P2：等价标签操作生成不同幂等键

- 定位：`crates/gantry-fakes/src/forge.rs:376-390,440`。
- 复现：同项目、交付、目标、epoch 下 `[a,b]` 与 `[b,a,a]` 创建两行；完全相同请求可以去重。
- 最小修正：AddLabels 使用 `labels:{n}` 和排序去重后的 labels intent，不用完整 payload，不修改通用幂等键算法。
- 验收：顺序及重复变化保持同键同行，真实标签集合或 epoch 改变才生成新键；增加 §3.2 固定向量，保持现有事务、依赖和完全重放测试。
- 依据：`docs/interfaces/forge-actions.md:269-285`。HEAD 旧有。其他动作只保留静态线索，发现具体违约另报证据，不自行扩大成本轮全量重写。

### B7 / P2：终态预写污染成功窗口

- 定位：`crates/gantry-store/src/lib.rs:1564-1578,1637-1652`。
- 复现：成功后推进时钟，调用 `record_effects(id, [])` 仍成功，updated_at 改变，窗口成功数由 0 变 1。
- 最小修正：仅 Executing 允许预写，Queued 和终态返回 PreconditionFailed。当前没有 succeeded_at 字段，不为修复先新增公开字段。
- 验收：Succeeded、Failed、Cancelled、Queued 调用均不改变行及统计；正常 Executing 仍成功；窗口边界和事务回滚保持正确。
- 依据：`docs/modules/store.md:494,497,506,647`。无守卫旧有，本轮新增窗口统计使组合影响可观察。

### B8 / P2：模拟恢复把忽略当成已处理

- 定位：`tools/mock-api/lib/app.mjs:595-601,619-625,839-842`。
- 复现：waiting_human → pause → dismiss escalation → resume，错误变成 running；不 dismiss 的对照仍 waiting_human。
- 最小修正：根据相关阻塞原因真正解除来恢复，不把任意收件箱项不再 open 当作完成；dismissed 或缺失不能解除等待。复用已有脚本状态，不复制完整引擎。
- 验收：无操作、dismiss 后均仍等待；多个阻塞仅处理一个仍等待；全部有效解除后运行；原 running 的暂停恢复正常。操作经真实本机 HTTP；如注入内存前置状态须明确披露。
- 依据：`docs/interfaces/api.md:476`、`docs/modules/notify.md:229`、`docs/modules/engine.md:619`。本轮修复不完整，HEAD 原来无条件 running，不称同输入新增回归。

### B9 / P2：模拟 retry 缺少状态守卫

- 定位：`tools/mock-api/lib/app.mjs:794-805`。
- 复现：failed 节点所属交付 paused、abandoned、closed，或 superseded_by 非空，仍返回 202。
- 最小修正：返回前核对已替换状态和所属交付暂停/终态，保留无交付的合法临时 workflow。无需实现实际调度。
- 验收：拒绝态均 409 且节点、尝试集合不变；正常 failed 仍 202 且保持当前状态；非 failed 仍拒绝；无交付合法对照、不同幂等键及同 key 重放分开验证。
- 依据：`docs/interfaces/api.md:694`。HEAD 旧有，返回体保持 failed 本身正确，不得回退成 queued。

## 工作包与协作

1. 核心包：B3/B4，负责 core 相关源码和测试。优先修安全及预算根因。
2. 存储包：B5/B7，统一负责 store；不要拆成两个并发写 `lib.rs` 的 agent。
3. 替身包：B1/B2/B6，统一负责 fakes。对 store 的依赖以其既有接口为准；涉及存储包改动时先确认接口，等汇合后跑事务集成。
4. 模拟接口包：B8/B9，统一负责 `tools/mock-api`；不依赖前端修复完成。
5. 主对话负责跨包验收与结果文档，正式编码优先交 coder。只有写集不重叠且依赖清楚时才并行；遵循当前仓库的代理与工作流规则，不为凑数量拆分状态机。
6. 完成必要本机检查后，独立 review 只读终审。审核者不改代码；有阻塞返回原写入者修正，再验证、复审。

前后端可并行，交集契约只读。前端只改自己的场景适配器，不替后端修 Mock HTTP；后端不借修改前端来适配错误返回。

## 验证与交付

在本工作树根目录执行，先检查已有依赖，不无故重装或升级：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo --locked xtask schemas --check
npm test --prefix tools/mock-api
cargo check -p gantry-store --no-default-features --locked
cargo check -p gantry-store --features testing --all-targets --locked
cargo check -p gantry-fakes --all-targets --locked
```

- 历史结果为 345 Rust 测试、27 Mock 测试、151 schema、21 负例以及文档块 36/14/20/6；是对照，不是本轮实测，也不是新增测试数量上限。
- 八项检查之外，要有 B1-B9 针对性回归。允许把同根因场景组织在少量测试中，不为了数量复制重复断言。
- schema 只运行 `--check`；发生漂移先调查原因，不自动重生成并写回契约，不放宽 schema 或删负例。
- 记录实际 rustc、cargo、node 版本和运行平台。若本机已有 Rust 1.85 可补最低版本检查；没有则标未验证，不安装工具链或把较新 stable 的通过冒充 1.85 通过。
- 核对本轮新增变更只落授权写集，契约与起点摘要一致，保留所有他人已有修改。启动的 Mock 服务、看门狗和子进程在验证后清理。
- 更新实施证据，区分修复前失败与修复后通过，不能用“探针成功观察 bug”作为产品通过证据。
- 最终逐项列出 B1-B9 的已修复/已先行修复/未修复/阻塞、文件位置、回归证据；附八项命令结果、实际测试数、剩余边界和独立终审 PASS/BLOCKING。没有全部关闭就不要宣称整体验收通过。
- 完成不自动授权提交、推送、合并、发布或创建 Issue/PR。

## 本轮不修与禁止误修

- 重放后 `requeue(true)` 退款：现有反例没有证明错误扣除历史失败，已驳回“只要 replay 就不能退款”。不得据旧静态报告禁止退款。
- 惰性过期引发 revision 冲突：读入口实际写入 Expired，旧事务拒绝覆盖是正确保护；未过期读不冲突，重开事务可成功。不得删冲突检查或恢复 Pending。
- Mock 设置八资源联合校验、无真实 policy/完整 TTL 语义是已披露边界，不是 B1-B9 修复范围；GantryEvent 四个录制分支也不扩成全部事件。
- Mock 无 in-progress 登记是待证线索，480 次并发请求未复现重复副作用；不为假想窗口重写架构。
- RepoSlug 点段、Nonce 长度、其他事件信封/字段校验、文本截断预算、core 续租可达性等仍属后续静态线索或契约解释问题，本轮不按确认缺陷自动修复。
- 14 个未定事件 data、ConfigView.injection 非空形状、8 个 drift 形状、完整 M0 未冻结仍保留；不猜字段、不把未实现 M1 算成本轮失败。
- Linux/Windows CI、Rust 1.85 实测、真实 SQLite/GitHub/Sandbox/PKI、生产授权和真实秘密值泄漏，没运行就明确写未验证。

本文件是后端独立启动入口。前端任务以其工作树 `apps/desktop/NEXT-STEPS.md` 的 F1-F9 为准，双方不修改对方实现。
