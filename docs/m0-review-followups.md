# M0 复核修复清单

本文件记录实现期复核发现，不修改权威契约，也不代表验收已经通过。

## 首轮独立验证

- 格式与静态检查通过；Rust 测试首次 223 项通过，但带锁文件的复跑发现 `FakeGitHub` HTTP 测试间歇性 `ConnectionReset`。必须找出分帧或关闭连接的根因，不能用重试掩盖。
- Mock 测试 19 项通过，真实 HTTP 分页、事件回放、游标错误与响应头检查通过。
- schema 检查通过了生成物漂移与元校验，但仍有 45 个未验证文档块和 14 个拒绝型事件 schema。这不等于完整契约一致性验收通过。
- 已定义的 `OutboxAction.payload` 未接入强类型；`Proposal.params` schema 未约束与 `kind` 的对应关系；`ProposalsMode` 序列化与 Worker 契约不符。
- ~~替身与 `Store::WriteTx` 的事务互操作尚未实现，不能靠各替身自己的事务测试代替。~~
  已由第四轮实现并由 `crates/gantry-fakes/tests/tx_interop.rs` 覆盖；该条为历史记录。

上述实现缺陷正在修复，最终以修复后的验证和复审结果为准。

## 设置模块独立审查

审查结果：`BLOCKING`，两个 P1。该审查只读，未重跑测试。

1. `settings/rules.rs` 对 `sandbox.prewarm`、`sandbox.io` 使用不可比较规则，但两者不是仅 approver 可写的键。扩大数值后 `relaxes[]` 为空，错误地不要求签名。修复应覆盖全局数值放宽与收紧的差分、权限回归测试。
2. `settings/clamp.rs` 在对象规则中整体回退全局值。示例：全局 `nodes=10, rounds=4`，项目旧覆盖 `nodes=20, rounds=2`，预期生效值为 `nodes=10, rounds=2`，实际却是 `nodes=10, rounds=4`。修复应保留未冲突字段的收紧，并覆盖预算、沙盒限额、动作、路径规则和出网档案。

其他需要核对的边界：含 `null` 的项目输入不能使已收紧的全局值回退到更宽的内置值；`has_direction` 应识别受规则约束的子路径；有明确定义方向的敏感键也应准确记录放宽差分。

`sandbox.devcontainer.mode=off` 的安全语义尚不清晰。本轮不因名称猜测其方向，不把审查建议当成用户裁定。

## 待终审核对的假设

- `ConfigView.injection` 的控制面示例只有空对象，但 `config.md` §8.1 已定义设置侧的注入结构。控制面是否同形不能直接假定；应核对提供方模块的转换定义。[已处理：第三轮把 Rust 反序列化/序列化、schema 与 `Debug` 限定为只接受空对象；非空结构仍未定，不宣称同形。]
- OpenAPI 示例的形状验证与 Rust 类型往返验证是两种证据。统计必须分别列出完整验证、部分验证和跳过，不能把只校验 OpenAPI 的示例计作完整 Rust 契约验证。[已处理：manifest 与检查输出分为 `verified`（schema + Rust）、`schema-only`、`partial`、`skipped`。]

## 契约一致性修复（第三轮）

只改契约相关生产类型、schema、夹具与文档；未改 `docs/interfaces/`、`Cargo.toml`/`Cargo.lock`、Mock API 或并行节点的 store/fakes 源码。

- `Approval` / `approval.requested` 的 schema 按 `APPROVAL_SHAPES` 联动 `action`、`target.kind`、`subject_digest`；未定义 shape 的动作 fail-closed。xtask 对 Approval 调用 `validate()`。
- `ForgeEventEnvelope` 按 23 个 kind 绑定强类型 data；`origin=self` 要求 `caused_by_action` 非空。单 kind 事件 schema 从同一信封投影。
- `Card` schema 增加未知 kind 降级分支并排除已知 kind；OpenAPI `Card` 同步。
- `Id` / `Ulid` / `Subject` 与 OpenAPI ID pattern 首字符限制 `0-7`；`IdGen` 超过 48 位返回 `Overflow`。
- `settings.updated.value` 按 resource 绑定生产 `*Patch` schema；schema 与 Rust 拒绝 `SecretRef.value`（含 `null`）。负例改用真实 `value.providers`。
- `ConfigView.injection` 只支持空对象，`Debug` 不输出其中的值；非空形状仍待决。
- fixture 统计分列后为 36 + 14 + 20 + 6 = 76；负例 18 个，schema 与 Rust 分别执行。

验证（历史，已被第五轮取代）：`cargo test -p gantry-core --all-features --locked` 249 项通过；`cargo test -p gantry-xtask --all-targets --all-features --locked` 2 项通过；`cargo clippy -p gantry-core -p gantry-xtask --all-targets --all-features --locked -- -D warnings` 通过；`cargo --locked xtask schemas --check` 0 failure、0 warning。当时未运行 workspace 全量测试或跨平台 CI。

## 事务与模块快照修复（第四轮）

本轮修复 `crates/gantry-store/**`、`crates/gantry-fakes/**` 与 `docs/m0-implementation.md`、本文件；`Cargo.lock` 由集成侧更新，本轮用 `--locked` 验证。未改 `core`、`schemas`、`fixtures`、`docs/interfaces`、Mock API 或应用层。

- `ModuleSnapshot` 的 `as_any` / `clone_snapshot` 改为经底层 `dyn` 调用。修复前直接对 `Box<dyn ModuleSnapshot>` 调用会选中 blanket impl 并把包装类型当作 `T`，产生 `E0521` / `E0597`；现按具体类型动态分派，快照克隆为深拷贝，不会递归装箱或浅共享。
- 新增模块快照回归：同 key 同类型重复注册保留状态，异类型读写都拒绝；事务内修改提交前不可见、回滚无痕、直接写使旧事务提交冲突。最后一项同时验证同线程直接写不死锁。
- `FakeForge::enqueue_on` 与 `FakePolicy::consume_execution_on` 改为 async 并 await 内部 store 调用；修正 fakes 的熵常量、测试导入与死代码。普通 `push_branch` 不在审批形状表中，回归改为验证入口 fail-closed，且已消费的 force 审批不能授权普通 push。
- 已验证的事务/审批能力：全局 `revision` 使旧事务提交冲突且不覆盖直接写；跨 Store 的 `TxHandle` 被三个替身拒绝；Forge 入队写穿同一 Outbox；Policy 消费追加 `approval.consumed` 并更新状态；Outbox 终态守卫与 `claim` 依赖级联取消；审批消费先校验后看状态，head 变化不返回 `AlreadyConsumed`。

验证（历史快照）：`cargo fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` 通过；`cargo test --workspace --all-features --locked` 329 项通过（core 249、fakes 48、store 30、xtask 2）；`cargo --locked xtask schemas --check` 0 failure、0 warning（151 个生成 schema，36 + 14 + 20 + 6 = 76 个夹具块）；`npm test --prefix tools/mock-api` 26 项通过；`cargo check -p gantry-store --no-default-features --locked`、`--features testing --all-targets --locked` 与 `cargo check -p gantry-fakes --all-targets --locked` 均通过。该 329/26 结果已被第五轮 338/27 取代。

边界：未定事件数据形状（`core` §7.5，14 个）、`ConfigView.injection` 非空形状、forge drift 其余 8 种形状本轮不追加实现。只验证本机 macOS；Linux/Windows CI、真实 SQLite、真实 GitHub/Sandbox IO、前端接入与 M1 未验证。未提交、未推送、未改暂存区，不宣称终审完成。

## 设置模块修复（第二轮）

本轮只修 `core::settings` 的规则比较、逐键 clamp、`null` 回退与 `has_direction`，未改 `docs/interfaces/`、`schemas/` 或其他模块。

- **P1-1 放宽方向**：`sandbox.prewarm`、`sandbox.io` 的数值上限改为递归数值比较；全局调大 `pool_size` 或任一 `io.*` 上限会记录 `relaxes[]` 并要求 approver 签名，调小不要求。`sandbox.runtime` 改为 `runsc → runc` 判定放宽。`egress.groups` / `egress.injection` 补上"新增分组 / 注入项或新增主机 = 放宽"。
- **P1-2 逐键 clamp**：`levels.*.budget`、`sandbox.limits`、`policies.actions`、`policies.paths`、`policies.escalation`、`policies.approval`、`policies.patch`、`guardian.triage`、`guardian.no_progress`、`egress.profiles` 不再整对象回退。示例：全局 `nodes=10, rounds=4`、项目 `nodes=20, rounds=2` 现在得到 `nodes=10, rounds=2`。路径规则的 `globs`、处置分别比较；`egress.profiles.<name>.groups` 在项目新增分组时整组回退到全局（与既有 clamp 语义一致），未新增分组时保留项目收窄。
- **`null` 回退**：`effective` 把存储文档中的 `null` 当作"未设置"再合并；项目 `null` 不能把已收紧的全局值带回内置默认，项目整体 `null` 等于不覆盖。存储层含 `null` 仍应由调用方按 V01 拒绝。
- **`has_direction`**：改为查最近的规则并排除 `Incomparable`，因此 `levels.high.budget.nodes`、`sandbox.prewarm.pool_size`、`sandbox.io.progress.max_lines`、`policies.paths.x.globs`、`egress.profiles.default.groups` 等子路径可被识别。
- **未翻转**：`sandbox.devcontainer.mode=off` 的安全含义仍待决，现有 `safe_subset → off = 放宽` 方向保持不变。`sandbox.prewarm.enabled` 的布尔方向、`egress.default_profile` 的跨档案比较、`sandbox.base_image` 的内容比较没有契约定义的机械方向，仍按 `Incomparable` / 仅 approver 处理。

验证：新增 `crates/gantry-core/tests/settings_regressions.rs`（16 项，先失败后通过）；`cargo test -p gantry-core` 通过；`cargo clippy -p gantry-core --all-targets -- -D warnings` 通过；`cargo check --workspace` 通过（`gantry-store` 有一条既有未使用函数警告，与本轮无关）。

## 终审 P1/P2 修复（第五轮，2026-10-25）

本轮只改 core、生成的 schema/catalog、xtask 检查、Mock API、回归测试与文档统计；未改
`docs/interfaces/`、`docs/decisions.md`、`docs/data-model.md`、`docs/reviews/`、apps、
prototype、store/fakes 源码或事务实现。

- `settings.updated.value` 现在必须存在。Rust 反序列化区分缺失与显式 `null`；global
  拒绝缺失与 `null`，project 只允许 `DELETE` 的 `null`。生成 schema 由 8 个资源分支
  改为 16 个 `scope × resource` 分支，顶层保留 `required: value`；`subject` /
  `project_id` / `scope` 的一致性由 `NewEvent::validate` 与 `Event::validate_data`
  同时检查；OpenAPI `SettingsUpdatedData` 同步补 global 非 `null` 条件。
- OpenAPI `Approval` 与 `ApprovalCreateRequest` 各加 8 个形状分支，联动
  `action` / `target.kind` / `subject_digest`；删除重复的 `merge_pr`；branch
  `expected_remote_oid` 改为可选且允许 `null`。settings target 要求 scope 与
  `project_id` 一致。`xtask` 新增形状集比较与合法/非法样例检查，`push_branch` 在
  OpenAPI 与 mock 中都 fail-closed。
- `ApprovalTarget::validate` 与 `approval.requested.validate` 共用 settings
  scope/project_id 规则；`KnownCard` 的已知分支补 `additionalProperties: false`，
  `Card` 反序列化同步拒绝已知 kind 的未知字段。
- Mock `handleCreateApproval` 现在执行与 core 一致的 allowlist / target shape 校验，
  非法形状返回 `422 validation_failed`，并新增 HTTP 回归。

验证（第五轮历史快照）：`cargo test --workspace --all-features --locked` 345 项通过
（core 254、fakes 55、store 34、xtask 2）；`cargo clippy --workspace --all-targets
--all-features --locked -- -D warnings` 通过；`cargo fmt --all -- --check` 通过；
`cargo xtask schemas --check` 0 failure、0 warning（151 个生成 schema、76 个文档块、
21 个负例）；`npm test --prefix tools/mock-api` 27 项通过。未验证 Linux/Windows CI、
真实 SQLite / GitHub / Sandbox IO 与 M1 能力。

## B1–B9 修复与复审（第六轮，2026-10-25）

本轮先按 `NEXT-STEPS.md` 补失败回归，再完成最小修复。修复范围限定为 core、store、
fakes、Mock API、回归测试和结果文档；未修改生产契约、schema 或 apps。

- B1/B2：收件箱和审批申请事件与状态使用同一 WriteTx；收件箱返回真实事件序号，
  审批事件保留 ActorCtx/config_rev，重复 Pending 不追加。
- B3/B4：预算加权正溢出饱和不归零；秘密值检测递归到数组字符串叶子。
- B5：Store 与 fakes 的异步写路径不再同步阻塞；FIFO waker/Condvar 共享门闩，
  commit、rollback、Drop 和取消等待均有竞争回归及外部进程看门狗。
- B6/B7：AddLabels 幂等意图规范化；record_effects 只允许 Executing，终态不刷新
  `updated_at` 或成功窗口。
- B8/B9：Mock resume 严格要求全部相关 blocker resolved；retry 检查 superseded、
  paused 和交付终态。

### 第六轮独立复审

初次只读复审发现一个 P1：fakes 的 async 端口仍调用 `begin_write_sync`，可能在
单线程 Tokio 竞争时死锁。已将 FakeForge、FakeInbox、FakePolicy 的 async 入口改为
直接 await `Store::begin_write`，并增加 `tx_interop` 子进程看门狗回归；修复后复审
未发现新的 B1–B9 阻塞项。

第六轮验证：`cargo fmt --all -- --check`、workspace clippy、workspace test、
schema 漂移检查、三个 Rust check 命令和 Mock test 均通过。Rust workspace 363 项
通过、2 项由父进程驱动的看门狗子测试 ignored；Mock 29 项通过。工作树为 macOS
Darwin arm64，rustc/cargo 1.97.0、Node 26.12.0、npm 11.20.0。

复审边界：未验证 Rust 1.85、Linux/Windows CI、真实 SQLite/GitHub/Sandbox/PKI、生产
授权和 M1；B8 在暂停期间新增收件箱 blocker 的完整引擎调度不属于当前 Mock 能力，仍需
真实 engine 语义冻结后补测。14 个未定事件 data、`ConfigView.injection` 非空形状和
drift 其余 8 种形状仍保持显式缺口。

## 第七轮独立复审与 B8 补修（2026-10-25）

独立只读复审对 B1–B9 逐项检查后，没有确认新的 P0/P1。复审发现一个可复现的 P2：
Mock 在部分 blocker 解除后第一次 `resume` 会删除 `pausedStages`，之后即使剩余 blocker
已解决，第二次 `resume` 也不会恢复运行。已在 `tools/mock-api/lib/app.mjs` 保留未完成的
暂停记录，并允许 `waiting_human + pausedStages` 继续检查；重复暂停也保留原快照，空快照
恢复后清理记录；回归放在
`tools/mock-api/test/regressions.test.mjs`，真实 HTTP 测试已通过。

复审提出的其他候选不在本轮扩大范围：

- `supersedes` 的关闭关系已有 `inbox.raised.data.supersedes` 定义，B1 启动文件明确要求
  该最小形状；是否另发每个旧项的 `inbox.resolved` 需要单独契约决策，本轮不猜字段。
- supersede 旧项的 `last_seq` 语义与 `supersedes` 事件承载方式需要契约责任方确认；不在
  未冻结的接口上改写行为。
- `dismiss` 对已 resolved 项的错误变体与 notify 文档存在既有不一致，但当前端口没有
  `InboxError::NotOpen`，修改它会改变生产契约，不属于 B1–B9 实现修复。
- B9 的“省略 `superseded_by`”候选不符合 retryNode 的 OpenAPI 响应要求；当前有效响应
  始终包含该字段，未增加未经授权的兼容分支。

第七轮后的验证保持：Mock 29 项通过；Rust workspace 363 项通过、2 项父进程看门狗
子测试 ignored；clippy、fmt、schema 检查和三个 Rust check 命令均通过。完整 M0 仍受
14 个未定事件 data、`ConfigView.injection` 非空形状及 drift 其余 8 种形状阻塞。

## 第八轮 R1/P3 诊断回显收尾（2026-10-25）

最新审计已给出限定 PASS：B1–B9 原始反例关闭，当前 M0 替身基线通过；没有确认新的
P0/P1/P2。该审计实测 364 项 Rust 测试通过、2 项 ignored，模拟接口 29 项通过。上文
345/363 等数字是历史快照，本段及 [第八轮实施记录](m0-implementation.md#错误路径回显加固与验收第八轮2026-10-25)
记录此后的 R1 小修，不重启旧修复或 M1。

R1 的错误回显在报告构造处统一修复，不改变校验规则：可疑动态键被截断出 JSON Pointer，
消息中的该键原值及转义形式被替换；安全路径和字段名保留。新增 4 个回归，先观察到
3 失败/4 通过，修复后 7 项定向测试全通过；结构错误未停止生成。原数组 `/allow_globs/0`
和合法 SecretRef 回归继续通过。只证明这个报告边界已加固，不外推真实日志或密钥存储泄露。

八项检查本轮全部实跑通过，命令及日志逐项见实施记录。Rust 368 passed、2 ignored、0 failed
（core 264、fakes 65、store 37、xtask 2）；模拟接口 29 passed；schema 检查 0 failure、0 warning。
使用离线依赖和既有增量缓存，非清洁构建；Rust 1.85 和其他平台未运行。

本轮实现内容标识为 `09ce1f7bb157763e3052edf7fc176b9f13ec233bff20abfa471ceaf0aba4614c`，
起点全量摘要、排除验收记录的计算口径及 190 份保护文件的字节一致性证据见实施记录。
独立只读终审已核对本轮增量、最终源码、先失败后通过日志与八项检查，不用相对 HEAD 的
历史契约差异判定本轮越权。

继续保留的边界：R2 模拟 retry 的人为缺失 delivery 引用仅跟踪，没有公开可达证据；
supersede 旧项 `last_seq`、额外 `inbox.resolved`、暂停期间新 blocker 和其他 forge 动作格式
仍需契约确认，不自行裁定。14 个未定事件 data、`ConfigView.injection` 非空形状和 8 种
drift 形状仍阻塞完整 M0 冻结。真实 SQLite/GitHub/Sandbox/PKI、生产授权、前后端联调和 M1
未验证。本轮不提交、不推送、不合并、不发布。

### 独立终审裁决

**PASS，限定于本轮 R1/P3 校验错误路径回显修复及验收记录；阻塞项 0。**
审核标识：`ses_fe5e8ca66bb0svY2TLDxzWcNc9`。送审全量内容标识为
`acc48e4bfadde4de85827b37bff1c91ce6c35fbbd95597bb8cff84e8dccb6751`，实现标识与上文一致。
裁决后仅追加本节等结果文字，最终含记录的全量摘要保存在证据目录的 `after.json`，
不在记录自身嵌入递归摘要。

终审已核对实际增量、原反例及结构错误的完整报告、转义、正常数组与 SecretRef、错误/警告
共用诊断边界和日志计数。未发现违反本轮验收的缺陷。重复父路径诊断的展示、解析器原生
错误消息的后续防护和解码辅助函数去重仅作为建议，未形成当前反例，不扩大本轮实现。
审核没有自行运行测试；命令退出码与摘要由主会话实跑记录。当前会话未暴露代码图工具，
源码证据使用读取与文本搜索，不声称索引代次或图覆盖。此 PASS 不替代完整 M0 冻结。
