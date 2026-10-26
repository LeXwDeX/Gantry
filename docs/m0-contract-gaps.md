# M0 契约缺口与实现边界

本文记录 `core` 事件目录与 `store` 内存实现的 M0 交付状态。它不是新的契约来源：
冲突时仍以 `decisions.md` > `architecture.md` 不变式 > `interfaces/` > `modules/` 为准。

**结论：完整事件契约尚未冻结。** 78 个事件类型全部有名称、catalog 元数据与 schema 标识；
其中 64 个的 `data` 形状由权威契约或提供方规格明确写出，已类型化并通过校验；14 个
（下表）数据形状未定，`core::events::validate_event_data` 返回
`SchemaErrorCode::ContractUndefined` 并带 `schemas/events/<type>.json` 标识，
`store` 因此拒绝追加这些事件。不得把本文件当作"事件数据已经补全"的依据。

## 1. 事件目录

- 固定类型 55 个 + `forge.<kind>` 23 个 = 78 个；`EVENT_TYPES` 与
  `EventType::ALL` 逐项一致，测试见 `crates/gantry-core/tests/events.rs`。
- 正例夹具：`crates/gantry-core/fixtures/events/cases.json`（每个已定义类型一份）。
- 反例夹具：同文件的 `invalid[]`（未知字段、错误枚举、缺必填、SecretRef 带值）。

## 2. `ContractUndefined`：14 个未定 `data`（历史状态；2026-10-26 已全部关闭）

| 事件 | 已知来源 | 为什么不能类型化 | 处理 |
| --- | --- | --- | --- |
| `attempt.progress` | core §7.5："形状未定"；sandbox.md §11 只列"采样后的 stage / detail / 工具摘要 / 自报用量" | 没有字段名、类型与枚举 | 拒绝；schema `schemas/events/attempt.progress.json` |
| `guardian.triaged` | guardian.md §7.10：`{event_ref, class, action, delivery_id, rule, reason_code?, fingerprint?, also_matched[], config_rev, round_id?, inbox_id?, action_ids[]}`；`action` 是开放列表 | `event_ref` 有两种形状（`forge.*` 的 `{kind, dedup_key, head_sha}` 与轮结束时的 `attempt.finished` / 结算检查），后者未定义；`action` 无封闭词表 | 拒绝 |
| `guardian.heal_requested` | guardian.md §7.10：`{round_id, delivery_id, task, head_sha, problems[{fingerprint, kind}], batch_included[], request_digest, config_rev}` | `task` 引用 `HealTask` 但事件层未给序列化形状；`problems[].kind`、`batch_included[]` 元素、`request_digest` 类型未定 | 拒绝 |
| `guardian.escalated` | guardian.md §7.10：`{delivery_id, class, reason_code, inbox_id?, fingerprint?, round_id?}` | 字段名齐全，但 `reason_code` 词表由 InboxItem payload 分散定义；M0 未在事件层给出封闭 schema。**这是 14 个中唯一可安全补全的候选**，仍需契约 PR 确认 | 拒绝；记录为可补全候选 |
| `approval.invalidated` | policy.md §7.5："data 含新旧 head" | 新旧 head 的字段名未给出 | 拒绝 |
| `approval.expired` | data-model §5.2 只有事件名；policy.md 未给 `data` | 未定义 | 拒绝 |
| `approval.denied` | api.md §6.1 只规定 `deny` 请求体 `{reason?}`；policy.md 未给事件 `data` | 未定义 | 拒绝 |
| `outbox.enqueued` | core §7.5：`upsert_status_comment` 带 `revision`（P-61）；缺口是"完整动作（payload、依赖、优先级）" | 事件 `data` 只定义 `revision` 一项；其余字段未冻结。只校验 `revision` 会静默忽略 payload，等于假装成功。**入队信封类型（G-06）不是本事件的 `data`，不能据此解冻该事件** | 拒绝 |
| `outbox.executing` | core §7.5 未列；forge.md 只说 claim 时追加 | 未定义 | 拒绝 |
| `outbox.cancelled` | core §7.5 未列；forge.md 说 coalesce 时追加 | 未定义 | 拒绝 |
| `forge.synced` | core §7.5："data（快照差异）未定" | 未定义 | 拒绝 |
| `forge.drift_detected` | forge-actions §7.4 给 drift 对象示例（`kind`、`delivery_id`、`target`、`expected`、`actual`、`resolution`、`synthesized_events`、`detected_by`） | `expected` / `actual` 随 9 种 `drift.kind` 变化，只有 `head_diverged` 有示例；其余 8 种未定义 | 拒绝 |
| `budget.charged` | core §7.5：粒度（逐次调用 / 逐 Attempt）未定；engine.md §11 与 models.md 对 `source` 的写法不一致（`{kind,id}` vs `{kind:"direct_call",call_id}`） | 形状冲突 + 粒度未定 | 拒绝 |
| `policy.denied` | policy.md §7.3：`{action_id, kind, target, reason_code, config_rev, subject?}` | `target` 覆盖 20 个动作，`kind` 与 `target` 的结构未给出；沿用 `ApprovalTarget` 会抹平非审批动作 | 拒绝 |

## 3. 已类型化但需记录的解释

以下事件的数据已由提供方或接口写出，但字段的**取值**或**子结构**仍有未定项；
实现按文档字面类型化，未扩展语义。

| 事件 | 已实现字段 | 仍开放的语义 |
| --- | --- | --- |
| `delivery.created` | `title`、`level_cap`、`level_effective`、`origin`、`source_message_id?`、`issues[]`、`budget{template,limits}`、`config_rev`（engine.md §11） | core §7.5 仍称"完整交付单元"是缺口；M0 采用提供方最小字段，不猜额外字段。`stage` 由投影按 data-model §4.1 置为 `spec` |
| `delivery.stage_changed` | `from`、`to`、`stage_status`（data-model §5.2 要求必带新状态）、`reason?`（engine.md §11） | core §7.5 的 `trigger` 只是建议，未纳入强制形状；`reason` 仍是自由字符串 |
| `delivery.level_changed` | `from`、`to`、`kind(cap/effective)`、`reason`（engine.md §11） | `reason` 的 `escalation:<rule_id>` 是字符串，未建封闭枚举 |
| `delivery.paused` / `resumed` | `by`、`status_before` / `status_after`（engine.md §11） | `by` 的结构未给，M0 用 `String` |
| `node.retry_requested` | `node_id`、`by`（engine.md §11） | `by` 同上 |
| `attempt.finished` | `status`、`result_ref?`、`failure_class?`、`failure_detail?`、`usage?`、`result_tree?`、`served_by?`（engine.md §11） | `served_by` 的形状只在 models.md 写作 `{provider, model, effort}`；M0 采用这三字段，`provider` / `model` 为 `String` |
| `patch.validated` | repo.md §11：`job_id`、`sha256`、`base_sha`、`result_tree`、`verdict`、`files[]`、`reasons[]`、`config_rev` | `subject` 取值（交付 / 尝试 / 工作树）未定；store.md OQ-11 也把 `patch.*` 排除在 `by_subject` 保证外 |
| `patch.rejected` | repo.md §11：`job_id`、`sha256`、`reason`、`detail`、`reasons[]?`、`config_rev?` | 同 `patch.validated` 的 `subject` 缺口 |
| `sandbox.created` / `destroyed` | sandbox.md §11 | `runtime` 为自由字符串 |
| `worktree.unlocked` | repo.md §11：`reason` 为 `released` / `expired` | core §7.5 曾把"到期是否发事件"列为未定；提供方 repo.md §7.5 已明确会发，采用提供方 |
| `message.posted` | 完整 `Message`（api.md §4.5） | 文本字段 `text` 在实体层是 `String`，不是 `UntrustedText`；这是实体契约现状，未在本节点改 |
| `inbox.raised` | 唯一的 `{item, supersedes?}`（notify.md §7.3）；schema 与 Rust 都拒绝扁平 `InboxItem` | 扁平写法曾出现在旧夹具与 core §7.5 的概括描述，但不是提供方的线上形状；M0 不为它保留双形状兼容 |
| `inbox.resolved` | `{inbox_id, resolution, option_id?, note?, message_id?, approval_id?, reason?}`（notify.md §7.3） | `resolution` 的 8 个值与 `chosen` 必带 `option_id` 已编码 |
| `approval.requested` | `action`、`target`、`subject_digest`、`expires_at`、`config_rev`、`requested_by`（policy.md §7.3） | `requested_by` 是 `Actor`（policy.md §6.1 明确）；`approval_id` 不在 data 中，由事件 subject 承载 |
| `approval.granted` | `approval_id`、`device_id`、`approval_pubkey_fpr`、`payload_sha256`（policy.md §7.5） | 无 |
| `approval.consumed` | `approval_id`、`consumed_by`、`subject_digest`（P-54） | `consumed_by` 为 `String`（`act_` / `att_` / `req:<request_id>`） |
| `outbox.succeeded` | `github_ref`（core §7.5；形状 forge-actions §4.2） | 无 |
| `outbox.failed` | `code`（forge-actions §2.6）、`message?` | 只在终态发（P-53）；未知 `code` 由 store 拒绝 |
| `budget.exhausted` | `{dimension, limit, used}`（core §7.5） | 无 |
| `device.paired` | pki.md §7.4.3 的 8 个字段 | `approval_pubkey` 为 base64url 字符串，未在此层解析曲线点 |
| `device.updated` | `device_id?`、`changed{name?,role?,cert_serial?,pubkey_fpr?}`、`via?`、`host_uid?`、`sudo_user?` | `changed` 的组合规则（至少一项）已校验；其余字段只在 pki.md 文字中出现 |
| `device.revoked` | `device_id`、`cert_serials[]`、`remaining_approvers`、`via?` | `remaining_approvers` 是 pki.md OQ-K5 的建议字段；实现保留为 `u64` |
| `forge.*` | 直接复用 `core::forge::ForgeEventEnvelope`（forge-actions §6.2–§6.3） | `subject` 取值未定（store.md OQ-11）；catalog 标 `Undefined`。信封里的 `kind` 必须与事件类型一致 |

## 4. 已修复的实体 / 协议缺陷

以下缺陷在本节点交付时存在，已在集成或本轮修复并重新生成 schema：

1. **`entities::InboxKind` 的 serde 名错误**：`rename_all = "lowercase"` 曾生成
   `approvalneeded` / `decisionneeded`；现改为 `snake_case`，与 api.md、config.md 和
   `fixtures/api/inbox.json` 一致。
2. **`forge::IssueCommentOn` 的 serde 名错误**：`pull_request` 曾被生成成
   `pullrequest`；现改为 `snake_case`。
3. **`entities::StageStatus` / `DeliveryOrigin` / `Exec` 的 serde 名错误**：曾生成
   `waitinghuman` / `githublabel` / `githubcommand` / `sandboxro`；现改为 `snake_case`，
   与 data-model 和 OpenAPI 一致。
4. **`ports/**` 编译错误**：已由端口节点修复；集成节点补充了 `validate_raise_kind`
   与 `Disposition` 的再导出。
5. **`OutboxAction.payload` 曾是 `serde_json::Value`**：现按 `kind` 严格解码为 14 个
   `ActionPayload` 变体（未知字段拒绝），并生成 `schemas/forge/outbox-action.json`。
   线上仍是 `kind` + 裸 payload 对象。
6. **`Proposal.params` 曾是 `Value`**：现按 `kind` 收窄为 `OpenPrParams` /
   `ReplyReviewThreadParams` / `ResolveReviewThreadParams` / `EscalateParams`；
   `schemas/worker/proposals.schema.json` 使用判别 `oneOf`。
7. **`ProposalsMode` 的线上形状错误**：现序列化为 `"forbidden"` /
   `{allowed_kinds,max}`，与 worker-protocol §4.2 一致。
8. **`delivery.stage_changed.stage_status` 曾是可选**：现为必填，满足 data-model §5.2。
9. **`inbox.raised` 曾是 `untagged` 双形状**：现只接受 `{item, supersedes?}`。
10. **`OutboxAction` 的 schema 未联动 `kind` 与 `payload`**：Rust 已按 `kind` 解码
    `ActionPayload`，但生成的 schema 只是 `kind` 枚举加 payload 的 `anyOf`，任意
    kind/payload 组合都能通过。现在顶层生成 14 个 `oneOf` 分支，每支要求 `kind`
    `const` 与该 kind 的 payload 子 schema 同时成立；顶层仍不设
    `additionalProperties: false`，保留实体输出对未知字段的兼容。
11. **`forge-actions §2.1` 的入队信封没有可序列化类型**：新增
    `core::ports::OutboxEnqueueEnvelope`，字段与原文一致（含 `idempotency_key`、
    `enqueued_by`），并生成 `schemas/forge/outbox-enqueue-envelope.json`。它不并入
    `OutboxAction`，也不改 `ActorCtx` / `EnqueueRequest` 的端口签名。
12. **`config.md §12` 的聚合示例没有完整文档类型**：不新增生产 wrapper；`xtask`
    按 8 个（全局）或 6 个（项目覆盖）resource 分别调用
    `settings::resource_schema`，再用对应 `*Patch` 类型反序列化。原始 JSON 不改写，
    逐资源验证，避免为凑一个“完整类型”强加原文没有的字段。
13. **`Approval` / `approval.requested` 的 schema 未联动动作形状**：生成器现在按
    `APPROVAL_SHAPES` 输出 8 个分支，同时约束 `action`、`target.kind` 与
    `subject_digest`；未定义 shape 的动作不出现在任何分支。xtask 的 Approval 检查
    调用生产 `validate()`。
14. **`ForgeEventEnvelope` 的 schema 未绑定 `kind` 与 `data`**：23 个 kind 各一支，
    每支引用该 kind 的强类型 data；`origin = self` 时 `caused_by_action` 必填且非
    null。单 kind 事件 schema 从同一信封投影。
15. **`Card` schema 拒绝未知 `kind`**：新增降级分支，并显式排除已知 kind；已知 kind
    的坏形状仍失败。OpenAPI `Card` 同步。
16. **ID schema 允许首字符 8 以上**：`Id`、`Ulid`、`Subject` 与 OpenAPI path/query
    的 ID pattern 统一为 `[0-7]` + 25 位 Crockford；`IdGen` 对超过 48 位的时钟值
    返回 `Overflow`。
17. **`settings.updated.value` 可携带 SecretRef.value**：schema 按 resource 绑定生产
    `*Patch` 形状，并把所有 `x-gantry-secret` 定义的 `value` 置为拒绝型；Rust 同规则
    拒绝字符串与 `null`。负例改用真实 `value.providers` 形状。
    schema 无法表达 `subject`、`project_id` 与 `value.resource` 的跨字段一致性；该
    一致性仍由 `NewEvent::validate` 在运行时检查。
18. **`ConfigView.injection` 接受任意值且 `Debug` 泄漏**：Rust 反序列化/序列化、
    schema 与 `Debug` 现在只允许空对象；非空注入形状仍未定义。
19. **fixture 统计把 `rust=none` 计作完整验证**：manifest 与检查输出分为
    `verified`（schema + Rust）、`schema-only`、`partial`、`skipped`。
20. **`settings.updated.value` 可用缺失绕过校验**：反序列化现在区分缺失与显式
    `null`；缺失一律拒绝。`global` 必须是资源文档，`null` 被拒绝；只有
    `project` 的 `DELETE` 允许 `value = null`。生成的 schema 由原来的 8 个资源分支
    改为 16 个 `scope × resource` 分支，并在顶层 `required` 中保留 `value`。
    `NewEvent::validate` / `Event::validate_data` 同时要求 `data.scope` 与
    `subject` / `project_id` 一致，不再能用缺失的 `project_id` 绕过。OpenAPI 的
    `SettingsUpdatedData` 也补了 `scope = global ⇒ value ≠ null` 条件，并由
    `xtask` 的 OpenAPI 检查覆盖。
21. **OpenAPI 的 `Approval` / `ApprovalCreateRequest` 没有联动形状**：两者现在各含
    8 个分支，约束 `action`、`target.kind` 与 `subject_digest`；`merge_pr` 的重复
    枚举项已删除。`ApprovalTarget.branch.expected_remote_oid` 与 Rust 的
    `Option` 对齐：不是 required，允许 `null` 或 40 位 hex。`ApprovalTarget.settings`
    在 schema 中要求 global 省略 / `null` `project_id`，project 必须带非空
    `project_id`；Rust `ApprovalTarget::validate` 与 `approval.requested` 共用同一规则。
    `xtask` 新增 OpenAPI 形状检查，覆盖合法 8 形状、`push_branch` fail-closed、
    kind 错配、无效 `expected_remote_oid` 与 settings scope/project_id 组合。
22. **Mock `createApproval` 接受任意 action/target**：`handleCreateApproval` 现在使用与
    `APPROVAL_SHAPES` 对齐的 allowlist，并校验 target kind、摘要格式与 settings
    scope/project_id；非法请求返回 `422 validation_failed`（RFC 9457 problem）。
    新增 HTTP 回归覆盖合法 settings 创建、`push_branch`、kind 错配与 project_id 错配。
23. **`KnownCard` 的已知分支允许未知字段**：生成的 `KnownCard` 分支现在与 OpenAPI
    一样设置 `additionalProperties: false`；`Card` 反序列化也对五个已知 kind 拒绝
    未知字段，未知 kind 仍保留降级分支。
24. **`ApprovalTarget.Settings` 的 scope/project_id 只有类型没有一致性约束**：运行时
    `validate()` 拒绝 global 带 `project_id` 与 project 缺 / `null` `project_id`；
    `Approval` 与 `approval.requested` 都调用它。schema 用 `if/then` 表达同一规则。

## 5. `store` M0 范围与缺口

已实现（`crates/gantry-store`，`testing` feature 的 `Store::open_in_memory`）：

- 单写者事务：`begin_write` / `append` / `append_with` / `commit` / `rollback`；
  M0 用 `Inner` 克隆做隔离，`commit` 原子替换。
- `dedup_key` 去重：命中返回 `Appended::Duplicate { existing_seq }`，无副作用。
- `seq` 从 1 连续；`ts` 按 `max(clock.now, last_ts)` 非递减；`log_epoch` 初始 1，
  `restore_epoch_bump` 模拟恢复加一。
- `settings.updated` 投影（`rev = seq`，`settings_current` / `settings_at_rev` /
  `settings_as_of` / `config_rev`）与 `secret_changes` 反查索引。
- `delivery.*` 最小投影（stage / stage_status / level_cap / level_effective），
  并校验 `from` 与当前投影一致。
- Outbox 表：`enqueue` 幂等键去重、`claim`（依赖 / `not_before` / 租约重放 /
  `exclude_deliveries`）、`record_effects`、`complete`、`fail`（Retry / Terminal）、
  `cancel`；`outbox.succeeded` / `outbox.failed` 与行状态同事务。
- `renew_lease`：仅 `executing` 可续租；`until` 不早于事务时间（实现允许等于）。
- `requeue`：仅 `executing` 置回 `queued`，写 `not_before` 并清空租约；
  `refund_attempt = true` 退回本次认领的 `attempts`（饱和减法），`false` 保持。
- `cancel_blocked`：按依赖链迭代到不动点，把依赖已 `failed` / `cancelled` 的
  `queued` 行置 `cancelled` 并返回本次取消的 ID；缺失依赖继续阻塞。与 `cancel`
  一样只改表，不追加未定义的 `outbox.cancelled` 事件。
- `count_succeeded_since`：按 `delivery_id` 过滤，统计 `updated_at >= since` 的
  `succeeded` 行；`since` 为闭区间，其他状态不计入。
- `fail(Retry)` 边界：同样要求 `executing`，置回 `queued`、写 `not_before` 与
  `last_error`，不改变 `attempts`，也不追加 `outbox.failed`；退还尝试次数只能用
  `requeue(refund_attempt = true)`。`fail(Terminal)` 才进入 `failed` 并写恰好一条
  `outbox.failed`；任何路径都不存在 `failed → queued`（P-53）。
- `EventLog`：`head` / `check_cursor` / `scan`（`scanned_to` 语义、include/exclude、
  project 过滤、全局事件始终命中）/ `get` / `get_by_seq` / `by_dedup_key` / `by_subject`。
- `Cursors`：`load` / `advance`（只增）。
- 项目级 settings 读取：`settings_current_for` / `settings_at_rev_for` /
  `settings_as_of_for`（保留不带项目的旧方法）。
- `WriteTx` 实现 `core::ports::TxHandle`；`as_any_mut` 供提供方下转型。
- 未知 / 未定义事件在 `append` 前被 `validate_event_data` 拒绝；已定义但无投影器的
  事件写入 `unprojected`（含 `seq`、类型、原因），可查询，不静默吞掉。

未实现（显式记录，不伪装）：

| 缺口 | 原因 / 归属 |
| --- | --- |
| SQLite、`open_temp`、备份、恢复、迁移、CAS | store.md M1；M0 只交付内存替身 |
| `ModuleSpec` 的迁移 / `Projector` / `ModuleConn` | store.md §7.10 是 **[提议]**；`open_in_memory` 收到非空模块返回 `Unsupported` |
| 完整 `ProjectionRead`（项目、消息、交付列表、审批、设备等） | M1；M0 只投影 settings / delivery / outbox |
| `tail` / `watch_head` / `EventStream` | 需要运行时与广播；M0 只给 `scan` |
| `by_secret_change` 的公开查询 | 索引已维护，查询方法留到 M1 |
| `outbox.enqueued` / `executing` / `cancelled` 事件 | `data` 未定义（见 §2），M0 只改表不写事件；投影可重建性因此不完整 |
| `outbox.cancelled` 的 reason | `OutboxAction` 无 reason 字段；`cancel` 只把原因写进 `last_error.code` 作占位 |
| `delivery.created` 其他状态机约束 | 同 ID 重复 `created` 已拒绝；`settings.updated.base_rev` 已比对并在失败时回滚。M0 仍只校验 `delivery.stage_changed.from` 与当前投影，完整状态机校验留到 M1 |
| 同一事务内多行的 Outbox 排序 | `enqueued_seq` 已有单调计数，但 M0 未测试多行 `(priority, enqueued_seq)` 的完整顺序；M1 补 |
| `assert` 级崩溃注入、并发 16 写者、50k 事件性能 | M1 的 store 验收；M0 只覆盖最小一致性 |

## 6. 验证边界

- 历史记录：集成验收曾记录 workspace 329 项（core 249、fakes 48、store 30、
  xtask 2）；core 曾单独跑到 252 项。这些数字是旧快照，**已被本轮全量重跑取代**，
  不得再作为当前结果引用。
- 终审 P1/P2 修复（第五轮历史快照）：
  `cargo test --workspace --all-features --locked`
  **345 项通过，0 失败**（core 254、fakes 55、store 34、xtask 2）；
  `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  通过；`cargo fmt --all -- --check` 通过。
- `cargo xtask schemas --check`：0 failure、0 warning；151 个生成 schema 无漂移。
  文档块统计：76 块 = 36 完整验证（schema + Rust）、14 schema-only（`rust=none`，
  不提供 Rust 证据）、20 部分验证（仅 schema；示例省略字段）、6 显式 skip。
  6 个 skip 均在 `fixtures/contracts/unverified.json`
  登记：2 个 `contract-undefined`（G-04、ResultBundle），4 个原文示意错误或
  非协议片段（G-13、G-15、github_ref 组合、App manifest）。实现漏项没有登记类别；
  新增未验证块会直接失败，不能用 warning 隐藏。
- 负例夹具：21 个；每个都分别运行声明的 schema 与 Rust 校验，未只注册不执行。
- `npm test --prefix tools/mock-api`：27 个测试通过（第五轮历史快照）。
- 真实 HTTP 回环回归：本次 workspace 运行中 `http_framing`（10 项）与
  `runtime_fakes`（11 项）通过；此前集成验收记录为各连续 50 次通过，包含 150ms
  延迟发送、分片 body、冲突/非法 Content-Length 和不完整 body 用例。
- 第五轮（终审 P1/P2）未修改 `apps/**`、`prototype/**`、`docs/reviews/**`、
  `docs/interfaces/`、`decisions.md`、`data-model.md`、store/fakes 源码与
  `Cargo.lock`；该段为历史记录。本轮 B1–B9 的 store/fakes 改动见下方第 7 节。
- 完整事件契约仍未冻结；14 个事件的 `data` 形状见 §2，`ConfigView.injection` 的
  非空形状与 drift 其余 8 种形状见 G-03/G-04。config 聚合示例已逐资源验证（G-07
  已关闭），但 `Notify` / `Github` 的覆盖策略仍由权威文档决定，M0 不新增生产类型。
  跨替身的事务 / 审批 / Outbox 集成已由 `crates/gantry-fakes/tests/tx_interop.rs`
  覆盖（提交/回滚、同事务去重、跨 Store 拒绝、Outbox 终态与审批消费）；SQLite
  持久化等 M1 能力仍不在本轮范围。

## 7. B1–B9 修复后的验证边界（第六轮，2026-10-25）

- Rust workspace：`cargo test --workspace --all-features --locked` 通过 363 项、失败 0、
  ignored 2（core 260、fakes 64、store 37、xtask 2；ignored 是父测试驱动的两类看门狗子进程）。
  `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` 与
  `cargo fmt --all -- --check` 通过。
- schema：`cargo --locked xtask schemas --check` 为 0 failure、0 warning，151 个生成
  文件无漂移；21 个负例与 76 个文档块仍按 36/14/20/6 分类。
- Mock：`npm test --prefix tools/mock-api` 29 项通过。B8 覆盖无操作、dismissed、缺失、
  多 blocker 部分解除和全部解除；B9 覆盖正常 failed、非 failed、superseded、暂停/终态
  交付和无 delivery workflow。
- Store/fakes：B5 的 Store 与 fakes async contention 都使用外部子进程看门狗；B1/B2/B6
  的跨模块事务测试覆盖提交、回滚、真实序号、事件信封、Pending 去重和规范化幂等键；
  B7 覆盖四种拒绝状态、Executing 预写和成功窗口不变。
- 工作树本轮没有修改 `docs/interfaces/**`、`docs/decisions.md`、`docs/data-model.md`、
  `schemas/**`、`apps/**`、`prototype/**` 或 `docs/reviews/**`。起点已有的其他未提交修改
  保留，不能用相对 HEAD 的整体 diff 判断本轮契约变化。
- 环境：macOS Darwin arm64，rustc/cargo 1.97.0、Node 26.12.0、npm 11.20.0。Rust 1.85、
  Linux/Windows CI、真实 SQLite/GitHub/Sandbox/PKI、生产授权和 M1 未验证。
- 独立复审先发现 fakes async 入口使用阻塞 `begin_write_sync` 的 P1，已修复并增加看门狗
  回归；修复后未发现 B1–B9 新阻塞项。暂停期间新增 blocker 的完整 engine 调度语义、
  14 个未定事件 data、`ConfigView.injection` 非空形状和 drift 其余 8 种形状仍是已披露
范围外缺口。

## 8. 第七轮复审补修边界（2026-10-25）

- Mock B8 已补齐连续操作：部分 blocker 解除后第一次 resume 仍保持
  `waiting_human`，保留暂停记录；重复暂停也不丢失 dismissed blocker；剩余 blocker 解除后
  再次 resume 才恢复 `running`。
  `npm test --prefix tools/mock-api` 实测 29 项通过。
- B1 的 supersede 事件仍按既有契约由 `inbox.raised.data.supersedes` 承载；是否同时为
  每个旧项追加 `inbox.resolved`，以及旧项 `last_seq` 的客户端语义，属于待冻结的契约
  解释，不在本轮猜测或修改。
- B9 的可选字段候选与 retryNode 的 OpenAPI 响应要求不一致，未增加额外兼容路径。
- 第七轮未修改 `docs/interfaces/**`、`schemas/**` 或端口签名；完整 M0 的既有契约缺口
  不变。

## 9. 契约实现后状态（2026-10-26）

契约文档经 PR #8 / #6 / #9 合入 `main` 后，按 `docs/interfaces/event-data.md`、`config-view-injection.md`、`egress-call-terminal.md`、`forge-drift.md` 完成 M0 类型与校验实现：

- 14 个事件的 `data` 全部类型化；`contract_undefined_types()` 现在为空。`schemas/events/<type>.json` 不再生成拒绝型。
- `ConfigView.injection` 改为 `InjectionView` / `InjectionCredentialView`；合法非空通过类型校验，非法非空拒绝。M1 控制面仍不启用非空注入。
- `DriftPayload` 实现 9 种 `kind` 的强类型 `expected` / `actual`；`schemas/forge/drift.json` 与 `forge.drift_detected` 事件 schema 由真实类型生成。`PullRequestReadback` / `IssueReadback` 已定义，端口方法接入仍留给 M1。
- `contract-gaps.json` 的 G-03 / G-04 / G-05 / G-23 已移入 `fixed`；`gaps` 为空。
- `forge-actions §7.4` 的 drift 示例仍缺契约必填的 `target.repo`，保留为 `example` skip；这不表示 drift 类型未实现。
- 目标平台证据仍不完整：macOS / Ubuntu / 契约与 Mock 作业通过，Windows 与 Rust 1.85 未验证。

## 10. OpenAPI 与 core 的视图差异（2026-10-26）

`schemas/openapi.yaml` 是 `api` 泳道手写契约，`xtask` 只读；M0 的一致性以 `docs/interfaces/api.md` 为权威，core schema 描述内部类型。两者不完全相同不等于 openapi 与 api.md 漂移。

本轮修复两处真实不一致：

- `BudgetLedger.limits`：OpenAPI 原为 `oneOf(DeliveryBudgetLimits, ConversationBudgetLimits)`，改为单一 `DeliveryBudgetLimits`。依据是 api.md §4.4 的交付台账示例与“会话台账不经本路由暴露”。
- `inbox-item.payload` 的 escalation 分支：`evidence` 按 data-model 的 `evidence[]` 改为必填。

其余 `xtask schemas --check` 的 info 属于已解释的视图差异，不清零、不隐藏：

- API 响应总是完整对象，core 内部实体允许可选字段，因此 `required` 集合不同。
- OpenAPI 默认允许未知响应字段，core 实体 `deny_unknown_fields`，因此 `additionalProperties` 不同；客户端应容忍新增字段。
- `Device.approval_pubkey` 由 core 持有、不进 API；`Workflow.nodes` 是 API 聚合字段；`event-envelope.type` 在 OpenAPI 更宽、由具体事件分支约束。这些已由检查器注明。
- `node.role` 的 pattern 与 enum、`inbox-item.payload` 的枚举展开是表示差异，语义一致。

核对证据：48 个操作与 api.md §4.1 逐条相等；53 个响应夹具通过；`SettingsUpdatedData` 的 16 个 scope×resource 分支与 core 一致；Approval 覆盖 8 个形状。上述结论不等于所有实体字段已逐项人工签字；若后续发现真实漂移，按独立契约 PR 修正。
