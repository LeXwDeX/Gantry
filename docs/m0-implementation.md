# M0 实施与验收边界

> **历史记录，不作为规格。** 本文是 M0 时期的实施与验收过程记录，保留作追溯。现行规格见 [architecture.md](architecture.md)、[decisions.md](decisions.md)、[interfaces/](interfaces/) 与 [modules/](modules/)，当前状态见 [AGENTS.md](../AGENTS.md) 与 [NEXT-STEPS.md](../NEXT-STEPS.md)；与现行文档冲突时以现行文档为准。

本次工作只交付后端开发基线、共享契约与测试替身，不修改 `apps/**`，不实现 M1 的真实业务，也不提交或推送 Git。

## 权威与未决项

遵循 `decisions.md` > 架构不变式 > `interfaces/` > `modules/` 的优先级。类型以 `data-model.md` 为准；客户端路由、请求和响应以 `interfaces/api.md` 为准；设置以 `interfaces/config.md` 为准。

- `core::ports` 包含 core 规格 §6.10 的 16 组端口。写操作传递 `ActorCtx`，不重新引入 `WriteCtx`、`ActorRef` 等别名。
- 文档已列出的提议默认值可用于 M0，但保留其提议性质。不得新增路由、事件类型、配置键或裁定 `decisions.md` 的待决事项。
- `core` §7.5 仍有事件数据形状缺口。未被权威契约或提供方明确规定的字段不得猜测。对未定事件保留事件名，校验明确拒绝未定数据，生成拒绝型 schema 并注明依据；不能据此声称完整事件契约已经冻结。
- `core` §13 的未定状态边只实现明确列出的边，其余拒绝。恢复暂停和 PR 重开需要显式上下文，不能通过纯状态值猜测原因。
- `egress-control.md` 与模块规格冲突时，控制面契约优先。Rust 名称可使用模块命名空间消除重名，但线上字段不变。
- M0 设置逻辑包含合并、收紧、放宽差分、权限要求与 V01–V03。V04–V22 的真实模块校验不在本次范围。

## 实施顺序

1. 并行交付 OpenAPI、夹具和 Mock API，以及 Rust 基础领域类型。
2. 完成协议、设置、事件和端口类型，交付类型生成的机器可读契约。
3. 完成全部指定替身、内存事件存储和示例测试。
4. 执行格式、静态检查、测试、schema 漂移检查和真实 HTTP 回放验证。
5. 独立终审和对抗性复核后，记录完成项、阻塞项与未验证项。

## 验收清单

- Rust workspace 可执行 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-features`。
- `core` 不含 IO、运行时依赖或不安全代码；ID、时间、严格 JSON、规范化、状态机有边界测试。
- `cargo xtask schemas --check` 检查生成物漂移；schema 通过元校验、夹具验证及 OpenAPI 路由和实体一致性检查。
- Mock 的路由与响应形状取自同一份 OpenAPI，不在实现中复制另一套路由表；事件流支持游标、过滤、心跳和纪元号。
- 每种 M0 替身至少一个示例测试；端口实现可由编译器检查。测试替身不伪装为真实业务实现。
- `Store::open_in_memory` 支持事件日志、事务、投影、Outbox 与游标的最小一致性行为。
- 前端接入说明给出启动命令、夹具标识、请求示例、事件续传和明确的模拟边界。

## 当前阻塞

完整事件契约冻结被 `core` §7.5 的未定数据形状阻塞。已尝试向用户确认处理方式，但自主目标执行期间交互提问不可用。继续实施不受影响部分，保留缺口，不把默认建议当作用户裁定。

## 集成验收结果（2026-10-25，历史；本轮已重跑为 345）

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`：通过。
- `cargo test --workspace --all-features --locked`：329 个测试通过，0 失败
  （core 249、fakes 48、store 30、xtask 2）。此数字为历史快照，当前结果见第五轮。
- `cargo xtask schemas --check`：0 failure、0 warning；151 个生成 schema 无漂移。
  76 个文档块 = 36 完整验证（schema + Rust）、14 schema-only（无 Rust 证据）、
  20 部分验证（仅 schema；示例省略字段）、6 显式 skip；skip 全部在
  `unverified.json` 登记。实现漏项没有 warning 类别，未登记的新缺漏直接失败。
- `npm test --prefix tools/mock-api`：26 个测试通过。
- `http_framing` 与 `runtime_fakes`：此前集成记录为各连续 50 次通过；本次 workspace 运行各 10/11 项通过。含 150ms 延迟发送、分片 body、非法/冲突 Content-Length 与不完整 body。

集成阶段修复了 `InboxKind`、`StageStatus`、`DeliveryOrigin`、`Exec`、`IssueCommentOn` 的线上名称，补齐 `ports` 的 `validate_raise_kind` 与 `Disposition` 再导出，修复 `gantry-fakes` 的借用、`Clone`、`Ord` 和 clippy 问题，并让 OpenAPI 的 `Approval.action`、`BudgetLedger.config_rev`、`delivery.stage_changed` 与 core 对齐。夹具映射改为独立的 `workflows-list`、`node`、`device-renew` 和 `settings-status`，避免一个夹具被不同响应形状复用。

本轮继续修复了以下实现缺陷：

- `OutboxAction.payload` 由 `serde_json::Value` 改为按 `kind` 严格解码的 `ActionPayload`，并生成 `schemas/forge/outbox-action.json`；store 入队与所有调用方改用强类型。
- `OutboxAction` 的 schema 从“`kind` 枚举 + payload `anyOf`”改为 14 个顶层 `oneOf` 分支；每支绑定 `kind` 常量与对应 payload 子 schema，kind/payload 错配由 schema 拒绝。顶层仍容忍未知字段，保持实体输出兼容。
- 新增 `core::ports::OutboxEnqueueEnvelope` 与 `schemas/forge/outbox-enqueue-envelope.json`，准确验证 forge-actions §2.1 的入队信封（含 `enqueued_by`）；`OutboxAction` 仍不含该字段，`ActorCtx` / `EnqueueRequest` 签名未变。
- `config.md §12` 的全局与项目聚合文档按 8/6 个 resource 逐资源验证：`xtask` 调用生产 `resource_schema`，再用对应 `*Patch` 类型反序列化；不生成新 wrapper，也不为凑完整类型强加字段。catalog 示例仍标 `partial`。`*Patch` 刻意不实现 `Serialize`（凭据写入类型不应回写 secret），因此 G-07 的 Rust 证据只覆盖反序列化；schema 由同一批生产类型派生。
- `Proposal.params` 按 `kind` 收窄为 4 个强类型变体；`ProposalsMode` 序列化为 worker-protocol §4.2 的 `"forbidden"` / `{allowed_kinds,max}`，移除 G-01/G-02/G-14 的 skip。
- `delivery.stage_changed.stage_status` 改为必填，依据 data-model §5.2；OpenAPI、store 投影、事件夹具和负例同步。
- `inbox.raised` 按提供方 notify.md §7.3 统一为唯一 `{item, supersedes?}` 形状；删除扁平 `InboxItem` 的 `untagged` 兼容，同步事件夹具、OpenAPI、schema 与负例，不新增协议。
- 两个 HTTP 回环替身的监听连接先解除 `O_NONBLOCK` 再读写；抽出共享 HTTP 请求读取器；重复/非法 Content-Length、非法 Transfer-Encoding、缺 method/path、body 未读满一律返回 400；真实 IO 错误仍向上返回。
- store 拒绝重复 `delivery.created`，比对 `settings.updated.base_rev` 并在失败时回滚，补充项目级 settings 读取；新增对应测试。
- 未验证夹具改为显式 `unverified.json`：只允许 `example` 与 `contract-undefined`；没有 `implementation-gap` 类别，实现漏项必须修复。统计按完整验证 / 部分验证 / skip 分开报告。

仍未完成（不声称完整 M0 冻结）：未定事件数据形状（`core` §7.5，14 个，含 `outbox.enqueued` 的 `data`）、`ConfigView.injection` 的非空形状、forge drift 的其余 8 种形状，以及两处原文示意错误（egress 25 位 request_id、Message 示例 id）。入队信封（G-06）与 config 聚合示例（G-07）已按真实字段/资源验证，但 `outbox.enqueued` 事件 `data` 仍未冻结。OpenAPI 与 core 实体的差异已逐项判定（#83）：`xtask` 规范化纯表示差异后逐关键字比对，剩余差异要么命中通用视图规则（API 封闭对象；API `required` 多出的只能是 core 可空字段），要么登记在 `crates/gantry-xtask/src/viewdiff.rs` 的白名单并写明理由，否则判为漂移直接失败；另有回归测试把 core 实例序列化后直接用 OpenAPI 组件校验。

## 契约一致性修复（第三轮，2026-10-25）

本轮在既有 numeric / settings / mock 修复之上，只修已确定契约与 Rust / schema 的不一致；不新增协议、不实现 M1，未回退前序修复。

- `Approval` 与 `approval.requested` 的 JSON Schema 由 `APPROVAL_SHAPES` 生成：每个已定义动作一支，同时约束 `action`、`target.kind` 与 `subject_digest` 格式；未定义 shape 的动作不在任何分支中出现。`xtask` 的 `entities::Approval` 检查在 round-trip 前调用 `validate()`。
- `ForgeEventEnvelope` 的 schema 为 23 个 `kind` 分支，每支绑定该 kind 的强类型 `data`；`origin = self` 时 `caused_by_action` 必填且非 null。单 kind 事件 schema 从同一信封投影，不手写字段。Rust 检查同样调用 `validate()`。
- `Card` schema 增加未知 `kind` 降级分支；该分支显式排除五个已知 kind，已知 kind 的坏形状不会落入降级分支。OpenAPI `Card` 同步该分支。
- `Id`、`Ulid`、`Subject` 与 OpenAPI 的 path/query ID pattern 首字符限制为 `0-7`，其余 25 位保持 Crockford 字符集；`IdGen` 对超过 48 位的时间戳返回 `Overflow`，不再静默截断。
- `settings.updated` 的 `value` 按 `resource` 逐个绑定生产 `*Patch` schema；所有带 `x-gantry-secret` 的 SecretRef 定义把 `value` 置为拒绝型 schema，`null` 也拒绝。负例改用真实 `value.providers` 形状；旧 `value.models` 包装仍被 Rust 拒绝，但不再是“错误位置被放过”的示例。
- `ConfigView.injection` 只支持空对象：反序列化、序列化、schema 与 `Debug` 都按此处理；非空形状仍以 egress-control §4.6 未定义为由保持待决。
- fixture 统计分列：36 个 schema + Rust 完整验证、14 个 schema-only（`rust=none`，不再计作 Rust 证据）、20 个部分验证、6 个显式 skip；共 76 块。`egress-control` §4.6 的 `injection: {}` 示例已从 skip 转为完整验证。`manifest.json` 的 `summary` 同步为 `verified` / `schema-only` / `partial` / `skipped`。

本轮验证（历史，已被第五轮取代）：`cargo test -p gantry-core --all-features --locked` 249 项通过；`cargo test -p gantry-xtask --all-targets --all-features --locked` 2 项通过；`cargo clippy -p gantry-core -p gantry-xtask --all-targets --all-features --locked -- -D warnings` 通过；`cargo fmt` 对本轮改动文件通过（并行节点的 `gantry-fakes` 文件有未格式化差异，未触碰）；`cargo --locked xtask schemas --check` 0 failure、0 warning，151 个生成 schema 无漂移，18 个负例的 schema 与 Rust 检查均按声明运行。未运行 workspace 全量测试、Linux/Windows CI 或真实 HTTP 回放；这些不计为本轮通过证据。

## 事务与模块快照集成修复（第四轮，2026-10-25）

本轮集中修复 store/fakes 的事务汇合与编译问题。没有改 `core`、`docs/interfaces/`、`docs/decisions.md`、`docs/data-model.md`、schema 或应用层；`Cargo.lock` 由集成侧更新，本轮用 `--locked` 验证。

### Store 的模块快照

- `ModuleSnapshot` 的方法经底层 `dyn ModuleSnapshot` 调用（`as_ref()` + 限定调用），不再让方法解析把 `Box<dyn ..>` 或引用当成泛型 `T`。这消除了 `E0521` / `E0597` 借用逃逸，也避免 `Box` 包装再装箱导致的递归或浅共享。克隆仍调用具体类型的 `Clone`，产生独立值。
- `module_state`、`try_update_module_state`、`register_module_state` 与 `Inner::clone` 使用同一路径。类型不匹配与缺失注册都返回 `PreconditionFailed`，不会把字节解释成另一类型。
- 同 key 同类型的重复注册是幂等操作，保留已有状态与 `revision`；这保证 `FakeForge::new_in` / `FakePolicy::new_in` / `FakeInbox::new_in` 重复创建替身时不重置已写入的脚本状态。
- 新增两项回归：重复注册与异类型拒绝；事务内模块状态在提交前不可见、回滚无痕、直接写使旧事务提交冲突。后者同时证明同线程持有事务时直接写不会死锁。

### 事务与审批语义（已验证）

- 每次直接写（游标、`log_epoch`、模块状态）在同一把 `Inner` 写锁下改值并递增全局 `revision`；`WriteTx::commit` 比对 `base_revision`，旧事务提交冲突而不覆盖直接写。
- `WriteTx` 的模块快照是深拷贝；提交以整份快照替换 `Inner`，回滚丢弃。跨 Store 的 `TxHandle` 由 `FakeForge` / `FakePolicy` / `FakeInbox` 拒绝。
- `FakeForge` 的入队写穿同一个 `Store` 的 Outbox，`FakePolicy` 的消费追加已定义的 `approval.consumed` 并更新审批状态；同一事务内提交/回滚一致。
- Outbox 终态守卫：只有 `Executing` 可成功/失败，终态不可回退；`claim` 对依赖失败做级联取消；执行的租约过期才可重领。
- `Policy` 的审批消费先校验 action/target/subject/远端前置，再判断状态；已消费的重放在 head 变化时仍报 `HeadChanged`，不返回 `AlreadyConsumed`。`Forge(PushBranch)` 等形状表未定义的 action 不能申请审批（fail-closed）。

### 本轮修复

- store：`ModuleSnapshot` 动态分派、`WriteGate::new` 的 testing 条件编译、模块快照回归 2 项。
- fakes：`enqueue_on` 与 `consume_execution_on` 改为 async 并 await 内部 store 调用；修正熵常量与测试导入；移除未用的 `ForgeState.counter` / `next_id`；`push_branch` 回归改为验证未定义形状的普通 push 在入口被拒绝，以及已消费的 force 审批不能授权普通 push；新增重复 `new_in` 保留脚本状态的回归。
- 文档统计纠正为实测值（历史快照）：329 项测试、36 + 14 + 20 + 6 夹具块、26 项 mock 测试；
  当前结果见第五轮。

### 边界

- 未定事件数据形状仍为 14 个（`core` §7.5），`ConfigView.injection` 非空形状与 forge drift 其余 8 种形状仍未定；本轮不追加猜测实现。
- 只验证本机 macOS 的 workspace 测试、schema 检查和 mock 测试；Linux/Windows CI、真实 SQLite、真实 GitHub/Sandbox IO、前端接入与 M1 能力未验证。
- 未提交、未推送、未改暂存区；不宣称独立终审完成。

## 终审 P1/P2 修复（第五轮，2026-10-25）

本轮按当前源码复核结果修复 P1/P2，不信任旧报告；未改 `docs/interfaces/`、
`docs/decisions.md`、`docs/data-model.md`、`docs/reviews/`、apps、prototype、
store/fakes 源码或事务实现。

- `settings.updated.value` 必填：Rust 反序列化区分缺失与显式 `null`，global 拒绝
  缺失与 `null`，project 只允许 `DELETE` 的 `null`。生成的 schema 改为 16 个
  `scope × resource` 分支，顶层 `required` 含 `value`；`data.scope` 与信封
  `subject` / `project_id` 的一致性由 `NewEvent::validate` 与
  `Event::validate_data` 检查；OpenAPI `SettingsUpdatedData` 同步补 global 非 `null`
  条件，并由 `xtask` 的 OpenAPI 检查覆盖。
- OpenAPI `Approval` 与 `ApprovalCreateRequest` 按 `APPROVAL_SHAPES` 绑定 8 个形状；
  删除重复 `merge_pr`；branch `expected_remote_oid` 与 Rust `Option` 对齐；settings
  target 的 `scope` / `project_id` 条件在 schema 与 Rust 运行时一致。`xtask` 新增
  OpenAPI 形状检查与负向样例。
- Mock `handleCreateApproval` 使用同一 allowlist / target shape 校验，非法形状返回
  `422 validation_failed`；新增 HTTP 回归。
- `KnownCard` 已知分支补 `additionalProperties: false`，`Card` 反序列化同步拒绝
  已知 kind 的未知字段。

验证（第五轮历史快照）：`cargo test --workspace --all-features --locked` 345 项通过
（core 254、fakes 55、store 34、xtask 2）；`cargo clippy --workspace --all-targets
--all-features --locked -- -D warnings` 通过；`cargo fmt --all -- --check` 通过；
`cargo xtask schemas --check` 0 failure、0 warning（151 个生成 schema、76 个文档块、
21 个负例）；`npm test --prefix tools/mock-api` 27 项通过。未验证 Linux/Windows CI、
真实 SQLite / GitHub / Sandbox IO 与 M1 能力。

## B1–B9 缺陷修复（第六轮，2026-10-25）

本轮按 `NEXT-STEPS.md` 修复 9 项确认缺陷。未修改 `docs/interfaces/**`、
`docs/decisions.md`、`docs/data-model.md`、`schemas/**`、apps、prototype 或 M1
真实实现；保留本工作树起点已有的未提交修改。

- **B1**：`FakeInbox::raise` 在同一 `WriteTx` 中追加已定义的 `inbox.raised`，状态、
  supersede 关系和事件一起提交；返回真实全局事件序号，重复 open 项沿用原序号。
- **B2**：`FakePolicy::request_approval` 在同一事务中追加 `approval.requested`，
  事件带 `requested_by`、`config_rev`、真实 subject 和 ActorCtx causation；Pending
  重复申请不追加事件，追加失败不提交审批。
- **B3**：预算加权的乘法与求和溢出改为饱和到 `u64::MAX`，不再把合法正输入归零；
  回归覆盖极大权重、单调性、四舍五入和台账记账。
- **B4**：秘密值检测递归到数组字符串叶子，保留键名检查与合法 SecretRef 子树例外；
  `/allow_globs/0`、嵌套数组和已知前缀均拒绝且不回显原值。
- **B5**：Store 异步写入口使用可返回 `Pending` 的 FIFO waker 队列；同步入口仍用
  Condvar，但与异步入口共享同一门闩。Store 与 fakes 的异步入口均不再调用阻塞同步
  写入口；外部进程看门狗覆盖两类竞争。
- **B6**：`AddLabels` 使用 `labels:{n}` target 和排序去重的 labels intent，不修改
  通用幂等键算法。
- **B7**：`record_effects` 只允许 `Executing`，Queued 与全部终态返回
  `PreconditionFailed`，不得刷新成功窗口。
- **B8**：Mock resume 只有在暂停时记录的全部相关收件箱项仍存在且均为 `resolved` 时
  才恢复 running；dismissed、open、缺失或只处理一部分都继续等待。
- **B9**：Mock retry 增加 superseded 节点、暂停交付和 closed/abandoned 交付的状态守卫；
  无 delivery 的临时 workflow 仍允许重试，202 响应保持节点为 failed。

### 第六轮验证

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`：通过。
- `cargo test --workspace --all-features --locked`：363 项通过，0 失败，2 项 ignored
  （core 260、fakes 64、store 37、xtask 2；ignored 为由父测试驱动的两类看门狗子进程）。
- `cargo --locked xtask schemas --check`：0 failure、0 warning；151 个生成 schema 无漂移，
  21 个负例，76 个文档块按既有 36/14/20/6 分类。
- `npm test --prefix tools/mock-api`：29 项通过；其中 B8/B9 回归通过。
- `cargo check -p gantry-store --no-default-features --locked`、
  `cargo check -p gantry-store --features testing --all-targets --locked`、
  `cargo check -p gantry-fakes --all-targets --locked`：均通过。

验证环境为 macOS Darwin arm64；rustc/cargo 1.97.0、Node 26.12.0、npm 11.20.0。
Rust 1.85、Linux/Windows CI、真实 SQLite/GitHub/Sandbox/PKI、生产授权与 M1 能力仍未验证。
完整事件契约仍受 14 个未定 data 形状、`ConfigView.injection` 非空形状和 drift 其余 8
种形状阻塞；本轮不把这些已披露边界算作 B1–B9 缺陷。

## B8 补修（第七轮，2026-10-25）

独立只读复审发现 Mock 在“部分 blocker 解除后先 resume、再解除剩余 blocker、再次
resume”时删除了暂停记录，导致第二次 resume 无法重新评估。`handleResumeDelivery` 现在
在未全部解除时保留 `pausedStages`，并允许后续对仍处于 `waiting_human` 的交付再次检查；
暂停前需要人工等待的交付只有在全部记录项仍存在且均为 `resolved` 时才恢复 `running`。
暂停前已经 `running` 的交付仍按 API 约定恢复为 `running`。

新增真实 HTTP 回归覆盖该连续操作、重复暂停和空快照场景；空快照恢复后会清理暂停记录。
B9 的 `superseded_by` 缺失字段候选未纳入修复：
retryNode 的 OpenAPI 响应要求该字段，当前有效线路不会产生该形状；不为未授权契约变化
增加兼容分支。

### 第七轮验证

- `npm test --prefix tools/mock-api`：29 项通过；新增连续 resume 回归通过。
- Rust workspace、clippy、fmt、schema 漂移检查和三个 Rust check 命令在第六轮已通过；
  B8 补修不触及 Rust 源码。

## 错误路径回显加固与验收（第八轮，2026-10-25）

本轮只修复 R1/P3：凭据式动态键名进入设置校验报告的 `pointer` / `message`。
最新审计 `gantry-dual-audit-20261025T205733/backend/final-review.md` 已对 B1–B9 原始
缺陷修复及当前 M0 替身基线给出限定 PASS，实测 364 项 Rust 测试通过、2 项 ignored，
模拟接口 29 项通过。本轮不重做 B1–B9，不启动 M1；第五至第七轮的 345/363 等数字
保留为对应版本的历史结果，不作为当前测试数。

### 修改与定向证据

- 只修改 `crates/gantry-core/src/settings/validate.rs` 和 `tests/settings.rs` 的相关实现与
  测试；另更新本文、`m0-review-followups.md` 和根 `NEXT-STEPS.md`。
- `ValidationReport::error` / `warning` 共用诊断脱敏逻辑：按 JSON Pointer 解码路径段，
  复用既有 `looks_like_secret_value`，遇到可疑段时定位到最近安全父路径，并替换消息中
  该可疑段的原始及转义形式。正常路径、字段名、错误码、严重性和校验流程不变。
- 不修改 `child`、`secret_slots` 或真实秘密值寻址；不修改接受规则、公共类型、端口、
  线上字段、schema、依赖或锁文件。未扩展 SecretRef 例外或凭据检测规则。
- 先补 4 个回归测试。原实现定向运行结果为 3 失败、4 通过，退出码 101；原反例的路径
  包含 `sk-audit-placeholder-not-a-real-credential`，actions 的枚举消息也回显该假值。
  修复后同组 7 项全通过；设置测试共 34 项通过。
- 原反例在 `validate` 和 `validate_and_parse` 中仍报 `inline_secret_detected`，路径改为
  `/paths`，完整报告不含可疑键原值。结构错误仍保留 `type`、`enum`、`unknown_field`、
  `required`；额外断言保证安全的 `extra` / `disposition` 等消息内容不被误删。
- 回归覆盖可疑键与安全父路径的 `~` / `/` 转义、高熵假键、普通动态规则解析、原数组
  `/allow_globs/0` 定位、警告报告及合法 `SecretRef.value`。全部只用假值；未证明真实
  密钥存储泄露、生产日志泄密或跨主体泄露。

### 内容标识与写集

- HEAD 保持 `dbf3bc546c9084f37556e26486d5199df1858a74`。
- 起点全量 440 文件内容标识：`ea83ae25da55a48d28e56e9eef7eefd29bbea3f8814aa3480e411b1c4d2f1fc6`，
  与最新审计快照一致。
- 本轮实现内容标识：`09ce1f7bb157763e3052edf7fc176b9f13ec233bff20abfa471ceaf0aba4614c`。
  口径为全部受 Git 管理及未忽略的未跟踪文件，排除本轮三份验收记录，避免记录自身摘要。
  对按路径排序的 `路径 + NUL + 文件 SHA-256 + 换行` 求 SHA-256。
- 190 份受保护文件的起止摘要相同：
  `ee85fcfc407c815e1352ffe4217727c465706ae84ba4566bd07aa35866864e40`。
  范围含现存 `schemas/**`、接口/模块/架构/决策/数据模型/评审文档及应用、原型、部署文件。
  本轮增量仅上述 5 个文件；起点已有未提交契约修改完整保留，不以相对 HEAD 无差异作证明。
- 临时证据目录：
  `/var/folders/jm/w5g1j5td4vl8164rqwl96fn80000gn/T/opencode/gantry-dual-audit-20261025T205733/backend/pointer-hardening/`。
  `before.json` / `after.json` 记录文件摘要，`change.patch` 是相对本轮起点的真实增量；
  `red-v02.log`、`red-v02-suffix.log` 和 `green-v02-suffix.log` 保存失败、消息保留检查及修复证据。

### 八项实测

所有命令在本工作树执行，退出码均为 0。Cargo 使用 `CARGO_NET_OFFLINE=true` 和既有
依赖/增量缓存，没有清理构建目录，也没有安装工具链或依赖。日志仍显示受影响 crate
重新执行编译或检查，测试二进制均实际运行，不将缓存命中当作未运行。

| 命令 | 实测结果 | 证据目录内日志 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | PASS | `01-fmt.log` |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | PASS | `02-clippy.log` |
| `cargo test --workspace --all-features --locked` | PASS，368 passed、0 failed、2 ignored | `03-workspace-test.log` |
| `cargo --locked xtask schemas --check` | PASS，0 failure、0 warning | `04-schemas.log` |
| `npm test --prefix tools/mock-api` | PASS，29 passed、0 failed | `05-mock.log` |
| `cargo check -p gantry-store --no-default-features --locked` | PASS | `06-store-no-default.log` |
| `cargo check -p gantry-store --features testing --all-targets --locked` | PASS | `07-store-testing.log` |
| `cargo check -p gantry-fakes --all-targets --locked` | PASS | `08-fakes.log` |

Rust 通过数：core 264、fakes 65、store 37、xtask 2。两项 ignored 仍是父测试实际执行的
看门狗子进程入口。schema 只检查未重生成：151 个生成文件、78 个事件 schema、21 个
负例、48 个 OpenAPI 操作、53 个响应夹具；文档块仍为 36 完整、14 仅 schema、20 部分、
6 显式未验证，不表述为全部 Rust 类型验证通过。

环境：Darwin 27.2.0 arm64；rustc/cargo 1.97.0、Node 26.12.0、npm 11.20.0。
本机未安装 Rust 1.85。独立终审结果见 [本轮跟进记录](m0-review-followups.md#第八轮-r1p3-诊断回显收尾2026-10-25)。
验收范围仅本轮诊断加固和 B1–B9 原始缺陷的现有回归，不代表完整 M0 冻结或生产后端完成。
14 个未定事件 data、`ConfigView.injection` 非空形状、8 种 drift 形状仍保留；Rust 1.85、
Linux/Windows、真实 SQLite/GitHub/Sandbox/PKI、生产授权、前后端联调及 M1 均未验证。

独立只读终审：**PASS，限定于本轮 R1/P3 诊断回显修复及验收记录**。审核绑定送审全量
内容标识 `acc48e4bfadde4de85827b37bff1c91ce6c35fbbd95597bb8cff84e8dccb6751` 及上述实现标识，
无阻塞项；此后只追加裁决记录，实现未变。审核读取了本轮真实增量、最终源码及验证日志，
没有自行执行测试或声称代码图覆盖。未提交、未推送、未合并、未发布。
