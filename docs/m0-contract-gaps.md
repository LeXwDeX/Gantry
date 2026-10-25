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

## 2. `ContractUndefined`：14 个未定 `data`

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

- 集成验收（2026-10-25）：`cargo test --workspace --all-features --locked`
  266 个测试通过，0 失败（core 215、fakes 30、store 20、xtask 1）；
  `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  通过；`cargo fmt --all -- --check` 通过。
- `cargo xtask schemas --check`：0 failure、0 warning；151 个生成 schema 无漂移。
  文档块统计：76 块 = 49 完整验证（schema + Rust）、20 部分验证（仅 schema；
  示例省略字段）、7 显式 skip。7 个 skip 均在 `fixtures/contracts/unverified.json`
  登记：3 个 `contract-undefined`（G-03、G-04、ResultBundle），4 个原文示意错误或
  非协议片段（G-13、G-15、github_ref 组合、App manifest）。实现漏项没有登记类别；
  新增未验证块会直接失败，不能用 warning 隐藏。
- `npm test --prefix tools/mock-api`：19 个测试通过。
- 真实 HTTP 回环回归：本次 workspace 运行中 `http_framing`（10 项）与
  `runtime_fakes`（11 项）通过；此前集成验收记录为各连续 50 次通过，包含 150ms
  延迟发送、分片 body、冲突/非法 Content-Length 和不完整 body 用例。
- 本节点未修改 `apps/**`、`prototype/**`、`docs/reviews/**`、`docs/interfaces/`、
  `decisions.md`、`data-model.md`、`Cargo.lock`。
- 完整事件契约仍未冻结；14 个事件的 `data` 形状见 §2，`ConfigView.injection` 与
  drift 其余 8 种形状见 G-03/G-04。config 聚合示例已逐资源验证（G-07 已关闭），
  但 `Notify` / `Github` 的覆盖策略仍由权威文档决定，M0 不新增生产类型。
