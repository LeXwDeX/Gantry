# M0 契约冻结提案（草案）

> **状态：proposal（草案）；非权威（non-authoritative），未接线，不是已批准契约。**
>
> 本文件只供用户审核。它不改变 M0 / M1 的范围，不接入任何运行路径，不修改任何生产契约、schema、fixture、Rust、Mock 或前端，也不表示任何 schema 已经冻结。真正的契约变更必须另开独立契约 PR；实现必须在契约批准后另开实现 PR。
>
> 工作树：`backend-baseline`。日期：2026-10-26。
>
> 依据（按优先级）：[decisions.md](decisions.md) > [architecture.md](architecture.md) 的不变式 > [interfaces/](interfaces/) > [modules/](modules/)。本文与这些文档冲突时，以它们为准。
>
> 相关缺口记录：[m0-contract-gaps.md](m0-contract-gaps.md)、`fixtures/contracts/contract-gaps.json`、`fixtures/contracts/unverified.json`。

## 0. 一页结论

| 组 | 缺口 | 当前行为 | 本文产出 | 冻结前允许的变更 |
| --- | --- | --- | --- | --- |
| A | 14 个事件 `data`（`G-05`） | `ContractUndefined`；schema 为拒绝型；`store` 拒绝追加 | 14 行阻塞表、按族候选草形状、`NEEDS_DECISION` 清单 | 无 |
| B | `ConfigView.injection`（`G-03`） | 只接受空对象；非空在 serde、schema、`Debug` 均拒绝 | 既有安全复核约束、字段候选表、最小 JSON 草形状、边界负例 | 无 |
| C | drift 9 种 `kind`（`G-04`） | `DriftKind` 只有枚举；drift 与 `forge.drift_detected` schema 均拒绝一切 | 公共信封候选、9 行判别表、逐种 `NEEDS_DECISION` | 无 |
| 延期 | `ResultBundle` + 4 个纯示例/部署片段 | 继续登记为 `contract-undefined` 或 `example` | 单列说明，不计入生产协议阻塞 | 无 |

**当前安全默认仍是 fail-closed（失败闭合）：**

- 14 个未冻结事件继续由 `validate_event_data` 返回 `SchemaErrorCode::ContractUndefined`，`store` 继续拒绝追加。
- `ConfigView.injection` 非空继续拒绝；Rust 反序列化、序列化、schema 与 `Debug` 都不接受或泄漏非空内容。
- drift 未定义形状继续拒绝；`schemas/forge/drift.json` 与 `schemas/events/forge.drift_detected.json` 保持拒绝型。
- `OutboxEnqueueEnvelope`（G-06）已经存在，但**不是** `outbox.enqueued` 的 `data`，不能据此解冻该事件。
- `ResultBundle` 仍是 `[提议]`，不能当作已冻结的服务端清单。

### 0.1 术语

| 术语 | 含义 |
| --- | --- |
| **现有依据** | 权威契约或当前代码已经写出的字段、语义或拒绝行为。 |
| **候选方案** | 本文为方便审核给出的草形状，不是已批准字段。 |
| **`NEEDS_DECISION`** | 现有文档不足以安全推断，必须由用户或指定负责人决定；本文不选答案。 |
| **确认者** | 需要给出决定或会签的角色，不是个人。图例见 §0.2。 |
| **冻结** | 契约 PR 合并，文档成为权威契约。schema、夹具与 Rust 校验落地属于实现完成，不是“契约冻结”的同义词；只改文档不会让运行时接受新形状。 |

### 0.2 确认者图例

| 代号 | 角色 |
| --- | --- |
| `U` | 用户 / 产品负责人 |
| `C` | 契约维护者（`data-model` 与 `interfaces/`） |
| `G` | `guardian` 负责人 |
| `P` | `policy` 负责人 |
| `F` | `forge` 负责人 |
| `E` | `engine` 负责人 |
| `M` | `models` 负责人 |
| `X` | `egress` 负责人 |
| `W` | `worker` / `sandbox` 负责人 |
| `S` | `store` / schema 负责人 |
| `API` | `api` 负责人 |

本文中的 `core §x` 指 [modules/core.md](modules/core.md)；`guardian §x`、`policy §x`、`forge §x`、`engine §x`、`models §x` 分别指对应的模块规格；`§x` 均指该文档的章节。

## 1. 当前事实与不改变的内容

### 1.1 当前事实

| 项 | 事实 | 证据 |
| --- | --- | --- |
| 14 个事件 | `attempt.progress`、`guardian.triaged`、`guardian.heal_requested`、`guardian.escalated`、`approval.invalidated`、`approval.expired`、`approval.denied`、`outbox.enqueued`、`outbox.executing`、`outbox.cancelled`、`forge.synced`、`forge.drift_detected`、`budget.charged`、`policy.denied` | `m0-contract-gaps.md` §2；`crates/gantry-core/src/events.rs` 的 `contract_undefined_types()` |
| 拒绝行为 | 未定事件返回 `ContractUndefined`，并带 `schemas/events/<type>.json` 标识 | `crates/gantry-core/src/events.rs::SchemaError::undefined` |
| 拒绝型 schema | `schemas/events/<type>.json` 使用 `"not": {}` 与 `x-gantry-unresolved.rejects_all_data = true` | 例如 `schemas/events/attempt.progress.json` |
| `store` | `NewEvent::validate` 在追加前调用 `validate_event_data`；未定事件不能进入日志 | `crates/gantry-core/src/events.rs`；`crates/gantry-store` |
| `ConfigView.injection` | Rust 的 `empty_injection` 只接受空对象；schema 为 `maxProperties: 0`；`Debug` 输出 `<empty only; values redacted>` | `crates/gantry-core/src/egress.rs`；`schemas/egress/config-view.json` |
| drift | `core::ports::forge::DriftKind` 有 9 个变体，但没有 `expected` / `actual` 载荷；`schemas/forge/drift.json` 与 `schemas/events/forge.drift_detected.json` 都是拒绝型 | `crates/gantry-core/src/ports/forge.rs`；`schemas/forge/drift.json` |
| 入队信封 | `OutboxEnqueueEnvelope` 是 `forge-actions §2.1` 的入队信封，带 `idempotency_key`、`enqueued_by`；它不等于 `outbox.enqueued.data` | `crates/gantry-core/src/ports/forge.rs`；`schemas/forge/outbox-enqueue-envelope.json` |
| `ResultBundle` | 只有 `worker-protocol §5.2` 的 `[提议]`，`gantry-core` 没有对应生产类型 | `docs/interfaces/worker-protocol.md` §5.2；`unverified.json` |

### 1.2 本文件不改变的内容

- 不修改 `docs/interfaces/**`、`docs/decisions.md`、`docs/data-model.md`、`docs/modules/**`。
- 不修改 `schemas/**`、`fixtures/**`、`crates/**`、`tools/mock-api/**`、`apps/**`、`prototype/**`。
- 不增加事件类型、配置键、路由或 ID 前缀。
- 不把任何候选字段写成生产 schema。
- 不提交、不推送、不合并。

## 2. 第一组：14 个事件 `data`

### 2.1 阻塞总表

| # | 事件 | 现有依据 | 缺失决策 | 候选方案（草） | 确认者 |
| --- | --- | --- | --- | --- | --- |
| 1 | `attempt.progress` | `data-model §5.2` 只用于展示；`worker-protocol §7.1` 定义 worker 进度事件，`§7.2` 说“转发给客户端的是采样节流后的 `attempt.progress` 事件”；`sandbox.md §11` 只列采样后的 stage / detail / 工具摘要 / 自报用量 | 转发哪些 `type`；是否由服务端再包一层；是否带 `attempt_id` / `received_at`；采样是否改变 schema；`hello` / `finish` 是否转发 | 候选 A：直接转发 worker 事件子集；候选 B：服务端包装 `{attempt_id, sampled_at, event}`。见 §2.2.1 | `W+E+C` |
| 2 | `guardian.triaged` | `guardian §7.10` 给出 `{event_ref, class, action, delivery_id, rule, reason_code?, fingerprint?, also_matched[], config_rev, round_id?, inbox_id?, action_ids[]}`；`§7.3` 给出处置词汇；`§7.6` 给出指纹 | `event_ref` 的两种/三种形状；`action` 是否封闭；轮结束 `round_closed` 的字段；`round_id` 类型；`class` 是否必填 | 判别联合：普通分流一支、`round_closed` 一支。见 §2.2.2 | `G+C` |
| 3 | `guardian.heal_requested` | `guardian §7.10` 给出字段列表；`§6` 给出 `HealTask` 与 `ProblemSpec` | `task` 的序列化；`problems[].kind` 词表；`batch_included[]` 元素；`request_digest` 覆盖什么、是什么类型 | JSON 骨架 + 4 个 `NEEDS_DECISION`。见 §2.2.2 | `G+E+C` |
| 4 | `guardian.escalated` | `guardian §7.10` 给出 `{delivery_id, class, reason_code, inbox_id?, fingerprint?, round_id?}`；`§7.9` 给出 `reason_code` 表；`m0-contract-gaps.md` 称它是 14 个中唯一可安全补全的候选 | `reason_code` 的封闭集；`suppressed_duplicate` 如何表达；`round_id` 类型；`Deduped` 时是否省略 `inbox_id` | 最小 JSON 候选。见 §2.2.2；仍不是冻结 | `G+U+C` |
| 5 | `approval.invalidated` | `policy §7.6` 第 5 步说 `data` 含新旧 head；`§7.7` 还列出 `pr_closed`、`pr_merged`、`delivery.abandoned`、head 变化等失效原因；`data-model §4.4` 定义审批状态机 | 失效原因枚举；非 head 主体（补丁摘要、内部动作）如何表达；新旧字段名；是否重复 `approval_id` | `reason` 判别联合；head 分支有候选，其他分支 `NEEDS_DECISION`。见 §2.2.3 | `P+F+E+C` |
| 6 | `approval.expired` | `data-model §5.2` 只有事件名；`policy §7.5` / `§7.7` 定义懒过期与清扫；审批实体已有 `expires_at`、`status` | `data` 是否为空；是否带 `previous_status`；是否带 `expired_at`；事件 `subject` 已是 `apr_` 时是否还要 `approval_id` | 最小候选 `{}`；可选 `previous_status` 待定。见 §2.2.3 | `P+C` |
| 7 | `approval.denied` | `api §6.1` 的 `deny` 请求体是 `{reason?}`；`policy §6.4` 的 `deny(id, device, reason)`；`§7.5` 定义 `pending → denied` | `reason` 是自由字符串还是 `UntrustedText`；是否带 `device_id` / `decided_at`；是否允许空对象 | 最小候选 `{reason?}`。见 §2.2.3 | `P+API+C` |
| 8 | `outbox.enqueued` | `m0-contract-gaps.md` 说只定义 `revision` 一项；`core §7.5` 的 `upsert_status_comment` 带 `revision`；`forge §7.8` 用 `revision` 保持幂等；`data-model` 的 `OutboxAction` 有完整动作字段 | 该事件是仅审计 `revision`，还是要能重建完整动作；是否所有动作都写；是否内嵌 payload、依赖、优先级、`enqueued_by`；G-06 信封不能替代 | 候选 A：`{revision}`（仅覆盖状态评论）；候选 B：完整 `OutboxAction` 快照。见 §2.2.4 | `F+G+E+S+C` |
| 9 | `outbox.executing` | `forge §7.8` 说 claim 时追加；`data-model §5.2` 只有事件名；`core §7.5` 未列 | 是否只写空对象；是否带 `attempts`、`lease_until`、`started_at`；`claim` 重放时如何区分 | 候选 `{}` 或 `{attempts, lease_until, started_at}`。见 §2.2.4 | `F+S+C` |
| 10 | `outbox.cancelled` | `forge §7.8` 的状态机列出 `cancel`、前置失败、`superseded`、执行前审批失败；`m0-contract-gaps.md` 说 coalesce 时追加；`OutboxAction` 没有 `reason` 字段 | 取消原因词表；是否带 `cancelled_by`；是否复用 `last_error.code`；是否允许空对象 | `{reason}` 候选枚举或 `{}`。见 §2.2.4 | `F+G+E+C` |
| 11 | `forge.synced` | `forge.md §7.5` 第 6 点给出提议形状 `{target, source, changes[{field, from, to}], synced_at}`；`guardian §4` 以该提议为输入；`data-model §5.2` 只有名称 | `target` 是否复用 ForgeEvent target；`changes[].from/to` 是任意 JSON 还是按 field 强类型；`source` 枚举；是否直接使用该形状 | 直接采用提议 JSON。见 §2.2.5 | `F+G+C` |
| 12 | `forge.drift_detected` | `forge-actions §7.4` 给出 drift 目录和 `head_diverged` 示例；`forge.md §7.7` 给出 drift 数据来源；`core::ports::forge::DriftKind` 有 9 个变体 | 9 种 `expected` / `actual`；公共信封；`target`；`resolution` / `detected_by` / `synthesized_events` 的封闭集 | 事件 `data` 直接使用 DriftRecord；见 §4 | `F+G+E+C` |
| 13 | `budget.charged` | P-74、`egress-control §4.3` / `§4.5`、`models §7.3`：`models` 按直接调用记账，以 `call_id` 去重、对账补记；`engine §7.9`：引擎不重复记直接调用，沙盒尝试另行记账；`engine §11` 给出字段骨架 | 沙盒尝试吊销时一次记账还是实时增量（`egress-control` OQ-4），增量与最终结算去重；source 的统一形状；dimension 与单位；归属边界；是否带 `config_rev` | `source` 判别联合 + 字段骨架；不能取消任一记账来源。见 §2.2.6 | `E+M+X+C+U` |
| 14 | `policy.denied` | `policy §7.3` 第 8 步给出 `{action_id, kind, target, reason_code, config_rev, subject?}`；`§6.1` 给出 `ActionRequest` / `ActionDecision`；`config §5.1` 有 20 个动作 id；`worker §9` 有 `policy.*` 子码 | `target` 覆盖 20 个动作的判别联合；`kind` 与 `action_id` 是否都保留；`subject` 类型；`reason_code` 封闭集；补丁拒绝与动作拒绝是否同一形状 | `target` 判别联合 + 字段骨架。见 §2.2.6 | `P+C+U` |

### 2.2 逐项候选与 `NEEDS_DECISION`

> 下面所有 JSON / JSONC 都是**候选草形状**。标 `jsonc` 的块含注释，注释只用于标记未定项，不是合法 JSON，不能直接作为 schema。

#### 2.2.1 `attempt.progress`

现有依据的冲突点：`worker-protocol §7.1` 定义的是 stdout JSONL 事件；`§7.2` 说客户端收到的是“采样节流后的 `attempt.progress` 事件”；`data-model §5.2` 只说“仅展示”。因此有两种合理解释，本文不选。

候选 A：直接转发 worker 事件子集。事件 `subject` 已是 `att_…`，所以 `data` 不必重复 `job_id` / `attempt_id`。

```jsonc
{
  "v": 1,
  "seq": 3,
  "type": "stage",              // NEEDS_DECISION: stage | tool | usage | heartbeat | warn 的封闭子集
  "stage": "edit",              // 仅 type = stage
  "detail": "reading src/api/items.rs"
}
```

候选 B：服务端包装，保留原始 worker 事件：

```jsonc
{
  "attempt_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "sampled_at": "2026-10-24T12:41:03.120Z",
  "event": {                    // NEEDS_DECISION: 原始事件字段
    "v": 1,
    "seq": 3,
    "type": "stage"
  }
}
```

`NEEDS_DECISION`：

- 选 A 还是 B；`hello` / `finish` 是否转发；`usage` 是累计值还是采样值。
- 是否保留 `seq`，采样后 `seq` 是否仍严格递增；服务端是否补 `received_at`。
- `detail` / `stage` 的长度上限；是否作为 `UntrustedText` 还是普通字符串。
- 采样策略是否属于契约（如果属于，schema 需要 `sampled` / `dropped` 字段；如果不属于，客户端不能依赖事件数）。

确认者：`W+E+C`。

#### 2.2.2 守护三事件

**`guardian.triaged`**：字段名来自 `guardian §7.10`，处置词汇来自 `§7.3`。候选是判别联合，不是一张扁平对象：

```jsonc
{
  "event_ref": {
    "source": "forge",          // NEEDS_DECISION: forge | attempt | settlement
    "kind": "check_suite_completed",
    "dedup_key": "...",
    "head_sha": "..."            // 仅 forge 的 PR/check 类分支；是否可选 NEEDS_DECISION
  },
  "class": "unknown",            // 取自 guardian §7.3 的 TriageClass
  "action": "escalate",          // NEEDS_DECISION: silent_fix | silent_reply | batch | defer | finalize | escalate | round_closed
  "delivery_id": "dlv_...",
  "rule": "K-10",
  "reason_code": "guardian.ci_low_confidence", // 可选
  "fingerprint": "sha256:...",   // 可选
  "also_matched": [],
  "config_rev": 18240,
  "round_id": "...",             // NEEDS_DECISION: ID 形状；rnd_ 不是现有前缀
  "inbox_id": "inb_...",         // 可选
  "action_ids": ["act_..."]
}
```

轮结束候选分支（`action = round_closed`）：

```jsonc
{
  "action": "round_closed",
  "outcome": "done",             // NEEDS_DECISION: 枚举
  "head_before": "f2c03d8...",
  "head_after": "1a1c743...",    // 可选
  "failing_before": ["sha256:..."],
  "failing_after": ["sha256:..."], // 可选
  "progress": "done",            // NEEDS_DECISION: improved | stalled | done
  "stalled_rounds": 0,
  "round_id": "..."
}
```

`NEEDS_DECISION`：

- `event_ref` 的判别标签与字段名；轮结束触发时的第二种形状（`attempt.finished` 或结算检查）如何表达。
- `action` 的封闭集；`class` 在 `round_closed` 分支是否必填。
- `defer` 的原因（`paused` / `lock_held` / `round_active` / `settling`）是否进入 `data`；如果进入，字段名与枚举。
- `also_matched[]` 的元素是 class 字符串还是对象。
- `round_id` 的类型；`outcome`、`progress` 的枚举；`failing_*` 元素是 fingerprint 还是对象。
- `noop` / `ignore` / `bookkeeping` 不写该事件，是否需要在文档中明确。

确认者：`G+C`。

**`guardian.heal_requested`**：只能给出骨架。`task` 在 `guardian §6` 是 Rust 枚举 `CiFailure | BotFinding | MechanicalConflict | NitBatch`，但事件层的序列化没有定义。

```jsonc
{
  "round_id": "...",              // NEEDS_DECISION: ID 形状
  "delivery_id": "dlv_...",
  "task": "ci_failure",           // NEEDS_DECISION: snake_case 还是别的编码
  "head_sha": "f2c03d8...",
  "problems": [
    {
      "fingerprint": "sha256:...",
      "kind": "..."                // NEEDS_DECISION: 词表
    }
  ],
  "batch_included": [],            // NEEDS_DECISION: fingerprint[] 还是 problem[]
  "request_digest": "sha256:...",  // NEEDS_DECISION: 覆盖范围与格式
  "config_rev": 18240
}
```

`NEEDS_DECISION`：

- `task` 的四个值是否公开为事件枚举；是否允许未来追加。
- `problems[].kind` 与 `ProblemSpec.kind` 的关系；`material_refs[]` 为什么不进事件。
- `batch_included[]` 的元素；是否允许为空。
- `request_digest` 的输入（是否含 `materials`、`config_rev`）与格式。

确认者：`G+E+C`。

**`guardian.escalated`**：`m0-contract-gaps.md` 把它标为 14 个中唯一可安全补全的候选。字段名与可选性直接来自 `guardian §7.10`。候选 JSON：

```json
{
  "delivery_id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD",
  "class": "unknown",
  "reason_code": "guardian.unknown_event",
  "inbox_id": "inb_01JBX2F5H7K9M1P3R5T7V9X0ZC",
  "fingerprint": "sha256:...",
  "round_id": "..."
}
```

上面的示例展示了所有候选字段；其中 `inbox_id`、`fingerprint`、`round_id` 在文档中标为可选，`round_id` 的 `"..."` 是占位符，`rnd_` 不是现有 ID 前缀。最小必填集仍需确认。

`NEEDS_DECISION`：

- `reason_code` 是否采用 `guardian §7.9` 的封闭表；`class` 是否采用 `TriageClass` 的封闭表。
- `suppressed_duplicate` 是独立布尔字段，还是用 `inbox_id` 缺失 + `reason_code` 表达；`§7.10` 说 `Deduped` 时也写，但字段列表没有该字段。
- `round_id` 的 ID 类型；`fingerprint` 是否固定为 `sha256:` + 64 位小写十六进制。
- `delivery_id` 是否允许为空（全局上报）；如果为空，事件 `subject` 用什么。

确认者：`G+U+C`。这是最接近可冻结的一项，但本文仍不把它写成最终 schema。

#### 2.2.3 审批三事件

**`approval.invalidated`**：`policy §7.6` 只明确“data 含新旧 head”。但 `§7.7` 还有 `pr_closed`、`pr_merged`、`delivery.abandoned` 等失效来源，补丁审批与内部动作的主体也不是 head。候选用 `reason` 判别联合：

```jsonc
{
  "reason": "head_changed",        // NEEDS_DECISION: 封闭集
  "previous_head_sha": "...",      // 仅 head_changed 分支
  "current_head_sha": "..."        // 仅 head_changed 分支
}
```

其他候选分支（字段均 `NEEDS_DECISION`）：

```jsonc
{ "reason": "pr_closed" }
{ "reason": "pr_merged" }
{ "reason": "delivery_abandoned" }
{ "reason": "subject_replaced", "previous_subject": "...", "current_subject": "..." }
```

`NEEDS_DECISION`：

- `reason` 的封闭集与线上字符串；是否允许未来追加。
- 非 head 主体（补丁 `sha256:…`、内部动作请求摘要）如何表达；是否统一用 `previous_subject` / `current_subject`。
- 是否重复 `approval_id`；事件 `subject` 已是 `apr_…`，候选不重复。
- `pr_closed` / `pr_merged` 是否需要 `repo` / `number`，还是由审批的 `target` 推出。

确认者：`P+F+E+C`。

**`approval.expired`**：最小候选是空对象，因为事件 `subject` 已经标识审批，实体已有 `expires_at`：

```json
{}
```

可选扩展（`NEEDS_DECISION`）：

```jsonc
{
  "previous_status": "pending",    // pending | granted
  "expired_at": "2026-10-25T08:00:01.120Z"
}
```

`NEEDS_DECISION`：

- 是否允许空对象；如果需要区分 `pending` 与 `granted` 的过期，必须增加 `previous_status`。
- `expired_at` 是否等于实体的 `expires_at`，还是实际清扫时间；如果两者可能不同，字段名要区分。
- 事件 `actor` 是 `system` 还是 `policy`；是否属于 `data`。

确认者：`P+C`。

**`approval.denied`**：`api §6.1` 的请求体是 `{reason?}`，但事件 `data` 未定义。最小候选：

```json
{
  "reason": "user declined"
}
```

`reason` 在 `api §6.1` 的请求体中是可选字段；上面的示例展示带值形态。最小必填集仍需确认。

`NEEDS_DECISION`：

- `reason` 是否允许省略；是否作为 `UntrustedText` 截断；最大长度。
- 是否带 `device_id`（谁拒绝）与 `decided_at`；事件信封的 `actor` 是否已经足够。
- 是否允许空对象 `{}`；如果允许，与 `api` 的 `{reason?}` 对齐。

确认者：`P+API+C`。

#### 2.2.4 Outbox 三事件

**`outbox.enqueued`**：这是最容易误判的一项。`core §7.5` 只写了 `upsert_status_comment` 的 `revision`；`m0-contract-gaps.md` 明确说“只校验 `revision` 会静默忽略 payload，等于假装成功”。因此有两个候选，不选：

候选 A（最小审计，不能重建完整动作）：

```jsonc
{
  "revision": 7                    // 仅 upsert_status_comment 有；其他动作没有等价字段
}
```

候选 B（完整动作快照，能重建）：

```jsonc
{
  "action": {
    "id": "act_...",
    "idempotency_key": "gk1_...",
    "project_id": "prj_...",
    "delivery_id": "dlv_...",
    "kind": "create_pr",
    "payload": { /* 按 kind 的强类型 payload */ },
    "requires_approval": null,
    "depends_on": [],
    "priority": "high",
    "not_before": null,
    "epoch": 0,
    "enqueued_by": { "module": "engine", "causation_id": "evt_..." }
  }
}
```

`NEEDS_DECISION`：

- 事件用途：只审计 `revision`，还是让消费者/恢复过程重建完整动作。
- 如果选 B，是否直接内嵌 `OutboxAction`，还是只放 `kind` + `payload` + `depends_on` + `priority` + `epoch` + `enqueued_by`；是否包一层 `action`。
- `revision` 只对 `upsert_status_comment` 有意义；其他动作是否也写该事件，还是只对状态评论写。
- **明确禁止**：用 `OutboxEnqueueEnvelope`（G-06）替代 `outbox.enqueued.data`。两者的字段与用途不同。

确认者：`F+G+E+S+C`。

**`outbox.executing`**：`forge §7.8` 只说 claim 时追加。最小候选 `{}`；可选候选：

```jsonc
{
  "attempts": 1,                   // 与 OutboxAction.attempts 对齐；是否进入事件 NEEDS_DECISION
  "lease_until": "2026-10-24T12:45:00.000Z",
  "started_at": "2026-10-24T12:43:00.000Z"
}
```

`NEEDS_DECISION`：

- 是否允许空对象；消费者是否需要 `attempts` / `lease_until`。
- `started_at` 是否等于事件 `ts`；如果等于，是否重复。
- claim 重放（租约过期）时是否再写一条；如再写，`data` 如何区分首次 claim 与重放。

确认者：`F+S+C`。

**`outbox.cancelled`**：`OutboxAction` 没有 `reason` 字段；`m0-contract-gaps.md` 说当前 `cancel` 只把原因写进 `last_error.code` 作占位。候选：

```jsonc
{
  "reason": "superseded"           // NEEDS_DECISION: superseded | dependency_failed | cancel_requested | approval_invalid
}
```

候选最小形态是 `{}`，但会丢失取消原因。`NEEDS_DECISION`：

- 取消原因是否进入事件；如果进入，词表由谁维护，是否与 `forge §7.8` 的状态机一一对应。
- 是否带 `cancelled_by`（`engine` / `guardian` / `system`）；是否带 `last_error.code`。
- `cancel`（用户/守护）与 `cancel_blocked`（依赖失败）是否同一 `reason` 空间。

确认者：`F+G+E+C`。

#### 2.2.5 Forge 两事件

**`forge.synced`**：`forge.md §7.5` 第 6 点已经给出提议形状，是本组最接近可冻结的候选：

```json
{
  "target": { "type": "pr", "number": 57, "node_id": "PR_kwDOabc123_example" },
  "source": "reconcile",
  "changes": [
    { "field": "mergeable_state", "from": "clean", "to": "dirty" }
  ],
  "synced_at": "2026-10-24T12:41:03.120Z"
}
```

`NEEDS_DECISION`：

- `target` 是否复用 `ForgeEventEnvelope.target` 的 `{type, number?, node_id?}`；是否支持 `branch` / `installation` / `repo`。
- `source` 的封闭集是否只有 `webhook` / `reconcile`；对账合成之外的来源（例如强制回读）是否也用它。
- `changes[].field` 是否限制为 `IssueRef` / `PullRequestRef` 的字段名；`from` / `to` 是否允许任意 JSON（例如 `checks` 对象）还是按 field 强类型；是否允许 `null`。
- `changes` 是否允许为空；是否每次快照变化都发一条。
- `delivery_id` 是否由事件信封/关联字段承载；项目级同步是否需要该事件。

确认者：`F+G+C`。

**`forge.drift_detected`**：候选为事件 `data` 直接使用 drift 记录；是否包一层仍待决，具体见 §4。`NEEDS_DECISION` 与 drift 组共享，不在此重复。

#### 2.2.6 预算与策略

**`budget.charged`**：`engine §11` 与 `models §7` 的字段表达待统一，但记账来源不是全局三选一。现有分工：

- **直接调用**：P-74、`egress-control §4.3` / `§4.5`、`models §7.3` 已要求 `models` 每次直接调用记账，以 `call_id` 去重，并在定期对账、令牌吊销时补记遗漏。`engine §7.9` 明确引擎不得重复记这部分用量。
- **沙盒尝试**：用量来自 Egress 的 `job_token` 计量，经 `sandbox` 交给 `engine`，是独立的记账路径，不能因保留直接调用记账而取消。`engine §7.9` 写终结时记账，但更高优先级的 `egress-control §4.2` 与 OQ-4 仍把“吊销时按 `FinalUsage` 一次记账”标为提议；它与“实时增量记账”的取舍仍为 `NEEDS_DECISION`。

候选用 `source` 判别联合；以下骨架不冻结沙盒记账时点：

```jsonc
{
  "dimension": "tokens_by_tier",   // NEEDS_DECISION: tokens_by_tier | nodes | rounds | wall_time | forge_writes
  "tier": "worker",                // 可选；仅 token 维度
  "delta": 32600,                  // 加权 token 或计数；单位随 dimension
  "used_after": 123456,
  "source": {
    "kind": "attempt",             // NEEDS_DECISION: attempt | direct_call | round | node | outbox_action
    "attempt_id": "att_..."
  },
  "delivery_id": "dlv_...",        // 或 conversation_id；是否互斥 NEEDS_DECISION
  "conversation_id": null,
  "config_rev": 18240              // 候选；用于审计权重快照
}
```

候选 `source` 分支：

```jsonc
{ "kind": "attempt", "attempt_id": "att_..." }
{ "kind": "direct_call", "call_id": "01JBX5D2F4H6K8M0P2R4T6V8XZ" }
```

`NEEDS_DECISION`：

- 仅沙盒尝试的记账时点：吊销时按 `FinalUsage` 一次记账，还是实时增量；如采用增量，如何与最终结算去重，避免重复或漏记。不得取消直接调用或沙盒尝试中的任一来源。
- `source` 的统一形状：`engine` 的 `{kind, id}` 与 `models` 的 `{kind:"direct_call", call_id}` 冲突；是否统一为带具体 ID 字段的判别联合。
- `dimension` 的枚举与单位；`delta` / `used_after` 是加权 token 还是原始 token/计数。
- `delivery_id` / `conversation_id` 的统一归属规则；`models` OQ-3 暂用无项目全局调用两者都空，该无归属边界仍待决。
- 是否带 `config_rev`；如果带，是权重快照还是事件写入时的配置版本。

确认者：`E+M+X+C+U`。需明确的是沙盒记账时点、结算去重与统一字段，不是重新选择是否保留直接调用记账。

**`policy.denied`**：`policy §7.3` 第 8 步给出字段名，但没有给出 `target` 的结构。候选：

```jsonc
{
  "action_id": "push_branch",       // 20 个 PolicyActionId 之一
  "kind": "push_branch",            // NEEDS_DECISION: ForgeActionKind 还是 PolicyActionId，是否两者都保留
  "target": {
    "kind": "branch",               // NEEDS_DECISION: 20 个动作的判别联合
    "repo": "acme/shop-api",
    "branch": "gantry/42-fix-pagination"
  },
  "reason_code": "policy.path_outside_allow", // NEEDS_DECISION: 封闭集
  "config_rev": 18240,
  "subject": "sha256:..."           // 可选；NEEDS_DECISION: 类型与语义
}
```

候选 `target` 的判别思路（不是最终字段）：

```jsonc
{ "kind": "pr", "repo": "...", "number": 57 }
{ "kind": "branch", "repo": "...", "branch": "..." }
{ "kind": "review_thread", "repo": "...", "thread_node_id": "..." }
{ "kind": "patch", "repo": "...", "attempt_id": "att_...", "paths": ["..."], "rules": ["..."] }
{ "kind": "issue", "repo": "...", "number": 42 }
{ "kind": "comment", "repo": "...", "number": 42, "comment_id": 123 }
{ "kind": "label", "repo": "...", "number": 42, "labels": ["..."] }
{ "kind": "run", "repo": "...", "run_id": 123 }
{ "kind": "other", "summary": "..." }
```

`NEEDS_DECISION`：

- `target` 是否必须覆盖 `config §5.1` 的全部 20 个动作（14 个 `ForgeActionKind` + 3 个条件伪动作 + 3 个预留动作）；未列出的动作是否 fail-closed（不写事件或写 `other`）。
- 内部动作（`settings_update`、`device_role_change`、`device_revoke`）不属于这 20 个 `PolicyActionId`。如果 `policy.denied` 也要覆盖内部动作，需要另立决策；本文不把 `settings` / `device` 放进候选目标。
- 补丁策略拒绝（`evaluate_patch` 的 `deny`）的 `action_id` 归属未定：是记 `push_branch`，还是记 `patch_apply_protected`，还是允许 `action_id` 为空。`patch.rejected` 仍只用于机械拒绝，不用于策略拒绝（P-69）。
- `kind` 与 `action_id` 是否都保留；两者在伪动作（`force_push`、`push_outside_prefix`、`resolve_review_thread_human`）与补丁拒绝下如何对应。
- `subject` 是 `subject_digest`、补丁摘要、请求摘要还是实体 ID；是否允许省略。
- `reason_code` 是否采用 `worker §9` 的 `policy.*` 封闭集；补丁路径/规则 id 是否放进 `target` 还是额外的 `details`。
- 补丁策略拒绝与动作策略拒绝是否同一形状；机械拒绝仍只发 `patch.rejected`，不发 `policy.denied`（P-69）。

确认者：`P+C+U`。

### 2.3 通用候选约束

以下只是候选评审原则，不是已冻结规则：

- 已定义形状的 `data` 使用 `additionalProperties: false`；未知字段拒绝。
- 来自 GitHub、沙盒或设备的文本使用 `UntrustedText` 或明确的截断字符串；事件 `data` 不出现 secret 值。
- 事件 `subject` 已承载的实体 ID 不重复；需要跨实体关联时再增加字段。
- 判定类事件带 `config_rev`；纯生命周期事件不带。
- 跨字段一致性（例如 `delivery_id` / `conversation_id` 互斥）在 Rust 校验；schema 能用 `if/then` 表达的也表达。
- 优先复用现有实体和枚举；不新增 ID 前缀，除非现有类型无法表达。

## 3. 第二组：`ConfigView.injection`

### 3.1 阻塞表

| 项 | 现有依据 | 缺失决策 | 候选方案（草） | 确认者 |
| --- | --- | --- | --- | --- |
| `ConfigView.injection` 非空形状 | `egress-control §4.6` 示例只给出 `"injection": {}`，但已要求 Egress 复核 V10/V11/V13；`§6` 已规定复核失败响应；`config §8.1` 定义 settings 侧字段，`§2.4` 定义 `SecretRef`；当前 Rust `empty_injection` 与 schema `maxProperties: 0` | ConfigView 是否复用 settings 的注入文档；`credential` 是 `{secret_ref}` 还是字符串；是否带 `present`；模型 provider 的自动派生项是否出现；非空是否只到 M2 | 复用 `egress.injection` 的字段，`credential.secret_ref` 只放引用；不改变既有复核约束。见 §3.2 | `X+C+U` |

**现有约束，不是待决项**：`egress-control §4.6` 已要求 Egress 对视图做 V10/V11/V13 防御性复核（规则见 `config §11`）。复核失败返回 `422 config_invalid`，附 `errors[]`，其中 `pointer` 相对视图；保留旧视图，`health = degraded`。`egress-control §6` 再次规定 `config_invalid`，不能与其他字段校验的 `validation_failed` 二选一。这些规则不需要用户重新批准；待定的是非空视图的字段映射与产品范围，不是是否复核。

### 3.2 候选草形状

候选（字段名来自 `config §8.1`，但 ConfigView 的确切形状仍需确认）：

```json
{
  "injection": {
    "npm_private": {
      "hosts": ["npm.corp.example"],
      "kind": "bearer",
      "credential": { "secret_ref": "registry.npm.corp" }
    }
  }
}
```

字段候选：

| 字段 | 候选 | 依据 | 状态 |
| --- | --- | --- | --- |
| `injection` | 以 id 为键的对象；空对象合法 | `egress-control §4.6`；`config §8.1` | 候选 |
| `hosts` | `string[]`；必须被某个分组覆盖；不得含 GitHub 域名族、IP 字面量、裸 `*` | `config §8.1`、V10、V13 | 候选 |
| `kind` | `bearer` 或 `header` | `config §8.1` | 候选 |
| `header` | `kind = header` 时必填；`kind = bearer` 时禁止；不得是逐跳/框架头 | `config §8.1`、V13 | 候选 |
| `credential` | `{ "secret_ref": "..." }`；不含 `value` | `config §2.4`、`egress-control §4.6` | 候选；是否用字符串 `secret_ref` 待定 |
| `present` | 是否出现在 ConfigView | `config §2.4` 的 GET 响应有 `present`；`egress-control §4.6` 说视图只含引用 | `NEEDS_DECISION` |
| 模型 provider | 不重复出现在 `injection`；由 `models.providers` 自动派生 | `config §8.1` | 候选；需确认 ConfigView 是否也排除 |

`NEEDS_DECISION`：

- ConfigView 的 `injection` 是“settings 文档的镜像”，还是“Egress 解析后的路由视图”。前者用 `credential: {secret_ref}`，后者可能用 `secret_ref` 字符串或内部 `injection_id`。
- `credential` 是否复用 `SecretRef` 对象；是否包含 `present`；如果包含，是否属于“只含引用”的例外。
- `kind = bearer` 时是否允许 `header` 字段存在但为 `null`；`kind = header` 时 `header` 是否必填。
- 非空 `injection` 是否只从 M2 开始接受；M1 是否继续只允许 `{}`。
- 是否需要在 `ConfigView` 顶层增加 `injection_rev` 或类似字段；当前没有依据。

确认者：`X+C+U`。用户确认字段映射与启用阶段；“凭据只发往列出的主机”及上述复核失败行为是已有安全约束。本轮未接线，控制面行为须由后续另获授权的实现验证，不能用 serde/schema 校验冒充 HTTP 服务验证。

## 4. 第三组：drift 9 种 `kind`

### 4.1 现有依据与公共候选

`forge-actions §7.4` 给出 drift 目录、`head_diverged` 示例和同时合成的事件；`forge.md §7.7` 给出每种 drift 的比较来源；`crates/gantry-core/src/ports/forge.rs::DriftKind` 有 9 个变体；`schemas/forge/drift.json` 与 `schemas/events/forge.drift_detected.json` 当前拒绝一切。

公共候选信封（字段名来自 `forge-actions §7.4` 的 `head_diverged` 示例）：

```jsonc
{
  "kind": "head_diverged",          // 9 选 1；封闭枚举
  "delivery_id": "dlv_...",         // NEEDS_DECISION: 全局 drift 是否允许 null
  "target": { "type": "pr", "number": 57 }, // NEEDS_DECISION: 是否复用 ForgeEvent target，是否支持 comment/repo
  "expected": { /* 按 kind 的判别分支 */ },  // NEEDS_DECISION: 每种 kind 的形状
  "actual": { /* 按 kind 的判别分支 */ },    // NEEDS_DECISION: 每种 kind 的形状
  "resolution": "github_wins",      // NEEDS_DECISION: 封闭集
  "synthesized_events": ["pr_synchronized"], // NEEDS_DECISION: 封闭集与是否允许空
  "detected_by": "patrol"           // NEEDS_DECISION: patrol | startup_sweep | recovery | on_demand | webhook
}
```

### 4.2 九种 `kind` 的候选判别

| `kind` | 文档给出的判定与合成事件 | `expected` 候选 | `actual` 候选 | 仍需决定 |
| --- | --- | --- | --- | --- |
| `pr_state_diverged` | 本地期望 PR open，GitHub 已 closed / merged，且没有对应事件；合成 `pr_closed` / `pr_merged` | `{ "state": "open" }` | `{ "state": "closed" }` 或 `{ "state": "merged" }` | 是否带 `merged` 布尔、`closed_at`、`merge_commit_sha`；`target` 是否只允许 PR |
| `head_diverged` | GitHub head ≠ Outbox 最近一次推送，且无 self 归因；合成 `pr_synchronized(origin=external)` | `{ "head_sha": "...", "basis_action": "act_..." }`（来自示例） | `{ "head_sha": "...", "pushed_by": { "login": "alice", "github_id": 1234567, "type": "user" } }`（来自示例） | 是否保留示例字段名；`pushed_by` 是否允许 `null`；是否带 `previous_head_sha` |
| `effect_missing` | Outbox 记为成功的对象不存在（评论被删、分支被删、PR 被关）；合成事件“视情况” | 候选 `{ "effect": "status_comment", "action_id": "act_...", "render_digest": "..." }` | 候选 `{ "exists": false }` | 目标可能是评论、分支或 PR；`target` 是否需要 `comment` 类型；`effect` 词表；合成事件为空是否允许 |
| `orphan_effect` | GitHub 上有 bot 作者 + Gantry 标记的对象，Outbox 无对应行（`forge-actions §4.4` 规则 4） | 候选 `{ "outbox": "none" }` 或 `null` | 候选 `{ "object": "comment", "github_id": 123, "marker": "gantry:v1 ..." }` | 对象类型词表；是否带 `marker`；`target` 是否能指向评论；`expected` 是否允许 `null` |
| `issue_state_diverged` | 关联 Issue 被关闭 / 重开 / 删除 / 转移，本地未知；合成 `issue_closed` / `issue_reopened` | `{ "state": "open" }` | `{ "state": "closed", "state_reason": "not_planned" }` | `state_reason` 是否必填；删除/转移如何表达；`target` 是否只允许 Issue |
| `review_state_diverged` | 评审结论与本地不一致（含被撤销）；合成 `review_*` | `{ "review_state": "approved" }` | `{ "review_state": "changes_requested" }` | 是否带 `review_id` / `reviewer`；`review_state` 的三个值是否封闭；`target` 是 PR 还是 review |
| `merge_state_diverged` | 本地认为在 merge queue 中，GitHub 已移出；合成 `pr_dequeued` | `{ "in_merge_queue": true }` | `{ "in_merge_queue": false }` | 是否带 `reason`；`reason` 词表；是否带 `queue_entry_id` |
| `default_branch_changed` | 默认分支改名 / 变更；合成 `repo_changed` | `{ "default_branch": "main" }` | `{ "default_branch": "master" }` | 是否带 `renamed_from`；`target` 是 repo 还是 branch；是否同时写 `project.updated` |
| `installation_unreachable` | 无法换令牌或仓库不可访问；合成 `installation_changed` | 候选 `{ "installation_id": 123, "reachable": true }` | 候选 `{ "reachable": false, "code": "..." }` | 错误码词表；`target` 是 installation；是否带 `project_id`；`delivery_id` 是否允许 null |

`head_diverged` 的完整示例（直接来自 `forge-actions §7.4`，可作为候选，不建议改字段名）：

```json
{
  "kind": "head_diverged",
  "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
  "target": { "type": "pr", "number": 57 },
  "expected": {
    "head_sha": "f2c03d84a858156b0121130fd1ec2f391aa90865",
    "basis_action": "act_01JBA1H5T7XA3P9C2E6J8M1QWD"
  },
  "actual": {
    "head_sha": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
    "pushed_by": { "login": "alice", "github_id": 1234567, "type": "user" }
  },
  "resolution": "github_wins",
  "synthesized_events": ["pr_synchronized"],
  "detected_by": "patrol"
}
```

### 4.3 公共 `NEEDS_DECISION`

- `forge.drift_detected.data` 是直接等于 drift 记录，还是包一层 `{ "drift": {...} }`；`schemas/forge/drift.json` 与事件 schema 是否共享同一个 `$defs`。
- `target` 是否复用 `ForgeEventEnvelope.target`。现有 target 只有 `pr` / `issue` / `branch` / `installation`；`effect_missing` / `orphan_effect` 可能需要 `comment` 或 `repo`。
- `delivery_id` 是否必填；`installation_unreachable`、`default_branch_changed` 这类可能没有交付单元。
- `resolution` 的封闭集；示例只有 `github_wins`，其他值（如 `reported` / `manual`）未定义。
- `detected_by` 的封闭集；示例只有 `patrol`，但 `forge.md §7.7` 还有启动全量、投递恢复、强制回读。
- `synthesized_events` 是否允许空；元素是事件类型字符串还是 `forge.<kind>` 字符串；是否必须与 `forge-actions §7.4` 的“同时合成的事件”列一致。
- 每种 `expected` / `actual` 是否 `additionalProperties: false`；是否允许 `null`。
- `drift.kind` 是否严格 9 个值；未来新增是否只追加。

确认者：`F+G+E+C`。

## 5. 继续延期：`ResultBundle` 与四个纯示例/部署片段

这些项目**不计入**本文三组生产协议阻塞，也不应被用来解冻任何事件或 schema。它们继续留在 `fixtures/contracts/unverified.json` 的现有分类中。

| 项 | 依据 | 当前处理 | 为什么不计入阻塞 | 何时再议 |
| --- | --- | --- | --- | --- |
| `ResultBundle` | `worker-protocol §5.2`，标 `[提议]`；清单由服务端生成，`Attempt.result_ref` 指向其哈希 | 继续 `contract-undefined`；不新增类型 | 它是服务端内部结果库清单，不是 worker 产出，也不是 M0 当前跨模块契约；M0 只要求 `Attempt.result_ref` 为内容寻址引用 | M1 需要结果读取/保留期时，单独提案 |
| `G-13` egress stage 示例 | `egress-control §4.4` 的暂存示例 `request_id` 为 25 位，`gantry-core` 的 Ulid 要求 26 位 | 继续 `example`；不作为实现通过证据 | 原文示意错误，不是新的协议形状 | 文档清理 PR 修正示例 |
| `G-15` Message 示例 | `api §4.5` 的 `Message.id` 不是合法 26 位 ID | 继续 `example`；不作为实现通过证据 | 原文示意错误，不是新的协议形状 | 文档清理 PR 修正示例 |
| `github_ref` + `expected_effects` 组合 | `forge-actions §4.2` 把两者放在一个示例里展示 | 继续 `example`；不新增组合类型 | 不是单一生产类型；实际字段分属 `OutboxAction` 的 `expected_effects` 与 `github_ref` | 需要时补 `OutboxAction` 的字段级示例 |
| App Manifest 片段 | `forge-actions §9.6` 供 `deploy` 使用 | 继续 `example`；不纳入 Gantry 协议 schema | 是 GitHub App 的部署输入，不是 Gantry 的线上协议类型 | 部署文档单独维护 |

补充：

- 这五项不进入本文 §2 / §3 / §4 的冻结范围；不要因为“文档里有示例”就生成生产 schema。
- `ResultBundle` 与 `forge-actions §7.4` 的 drift 示例不同：drift 是生产事件 `forge.drift_detected` 的 `data` 候选，属于第三组；`ResultBundle` 不是。
- 四个 `example` 不需要负例；`ResultBundle` 继续由 `unverified.json` 的 `contract-undefined` 记录，而不是新增负例。

## 6. 建议的契约 PR 与实现写集

### 6.1 独立契约 PR（只改文档）

建议拆成 3 个契约 PR。每个 PR 只做一件可独立验证的事；不要与实现、schema 生成或 Mock 修改混在一起。下表是未来写集建议，不授权本轮创建补充文档。

**仅改文档也必须保留提取基线**：`crates/gantry-xtask/src/fixtures.rs::DOCUMENTS` 固定提取 `docs/interfaces/` 下的 `api.md`、`config.md`、`egress-control.md`、`forge-actions.md`、`worker-protocol.md`。开围栏行号决定夹具 `id` 与文件名；`manifest` 还记录 `source`、`line`、上文最近的 `#` 标题（`context`）、原始正文的 SHA-256 与字节数。`main.rs::check_drift` 逐字节比较夹具与 `manifest`，并拒绝多余夹具。因此，即使 JSON 值没变，只改这些文档的行号或标题也可能失败，不能要求夹具不动却忽略漂移。

优先采用**独立补充文档 + 原文末尾引用**：原五份受提取文档只允许在文件末尾追加没有 `json` 围栏的章节与引用；原有字节、围栏正文、开围栏行号、上文标题 `context` 全部不变，也不新增 `json` 围栏。其他 `data-model` / 模块文档只同步说明和引用，不另定义字段。补充文档尚不在现有五文档提取范围，须直接检查其内容，不能声称旧夹具已覆盖它们。

| PR | 目标 | 写集（仅文档） | 明确不写 |
| --- | --- | --- | --- |
| `PR-C1` 事件 data | 冻结除 drift 外 13 个事件的 `data` 与适用负例；维护 14 事件目录 | 拟新增 `docs/interfaces/event-data.md`，承载 §2 中除 `forge.drift_detected` 外的事件细节；`docs/data-model.md` §5.2 与 `docs/modules/guardian.md` §7.10、`docs/modules/policy.md` §7.3 / §7.6 / §7.7、`docs/modules/forge.md` §7.5 / §7.8、`docs/modules/engine.md` §11、`docs/modules/models.md` §7 只同步说明和引用；若涉及 `docs/interfaces/worker-protocol.md` §7.1 / §7.2 的转发语义，仅在该接口文档末尾追加引用 | `schemas/**`、`fixtures/**`、`crates/**`、`tools/mock-api/**`、`apps/**` |
| `PR-C2` `ConfigView.injection` | 冻结非空注入视图的字段映射，沿用 §3.1 既有复核约束 | 拟新增 `docs/interfaces/config-view-injection.md`，补全 `egress-control.md` §4.6 的注入视图细节；仅在 `docs/interfaces/egress-control.md`、`docs/interfaces/config.md` 末尾追加适用范围与引用（对应 config §8.1 / §8.3 / §11，不改 settings 语义） | 同上 |
| `PR-C3` drift | 冻结 9 种 drift 的公共信封、`expected` / `actual` 判别与 `forge.drift_detected.data` | 拟新增 `docs/interfaces/forge-drift.md`，承载 `forge-actions.md` §7.4 的 drift 形状与事件 data；仅在 `docs/interfaces/forge-actions.md` 末尾追加适用范围与引用；`docs/modules/forge.md` §7.7、`docs/data-model.md` §5.2 只同步说明和引用 | 同上 |

补充：

- 14 个事件中的 `forge.drift_detected` 由 `PR-C3` 冻结；`PR-C1` 只保留它在事件目录中的引用，避免两个 PR 重复定义同一个 `data`。
- 每份补充及原文末尾引用须列明适用章节、字段与替代范围。补充获批后，范围内原有未定项或候选形状由该补充替代；范围内旧 JSON 示例在实现阶段机械同步前只作历史候选，不是另一套契约。范围外规则及更高优先级的决策、不变式不变。原五份文档需要更新的 JSON 示例留到实现阶段按已批准字段同步，不在那时重新决定语义。
- 如果用户要求一次审核，也可以合并为 1 个契约 PR，但建议保留 3 个独立提交/章节，避免事件、注入和 drift 的决策互相掩盖。
- `docs/decisions.md` 只在用户要求把确认结果登记为“已确定”时修改；不要把本文的候选自动写成硬决策。
- 契约 PR 的验收不生成 schema；直接检查补充文档的 JSON、链接、格式及字段对照，另以现有 `schemas --check` 证明旧基线未漂移，不能把它当作新字段验证。既有检查不删、不跳过，门禁仍为 0 failure、0 warning，运行时仍 fail-closed。
- 若上述排版不能保持旧提取基线，停止并标 `NEEDS_DECISION`，另提最小写集方案；本文不授权扩大到夹具、manifest、注解或实现文件。

### 6.2 后续实现写集（契约批准且另获实施授权后）

| 区域 | 预计文件 | 工作 |
| --- | --- | --- |
| `core` 事件 | `crates/gantry-core/src/events.rs`、`crates/gantry-core/tests/events.rs`、`crates/gantry-core/tests/schema_regressions.rs` | 增加事件 data 结构、`validate_event_data` 分支、跨字段一致性、正负例 |
| `core` egress | `crates/gantry-core/src/egress.rs`、`crates/gantry-core/tests/*` | 用非空注入类型替换 `empty_injection`；按批准字段实现语义校验，沿用 V10/V11/V13；保留 `Debug` 脱敏；serde/schema 结果不代替控制面 HTTP 行为验证 |
| `core` forge/drift | `crates/gantry-core/src/forge.rs`、`crates/gantry-core/src/ports/forge.rs` | 给 `DriftKind` 增加载荷类型；实现 9 种 `expected` / `actual` 判别 |
| `core` 设置 | `crates/gantry-core/src/settings/resource.rs` | 如果 ConfigView 复用 settings 的 `EgressInjection`，对齐字段；不改变 settings 线上形状 |
| 接口示例与提取覆盖 | 原五份接口文档、§6.1 的三份补充；`crates/gantry-xtask/src/fixtures.rs::DOCUMENTS`（如需） | 按批准字段机械同步原接口 JSON 示例，或将补充加入 `DOCUMENTS`；逐项确认补充定义都有提取与验证覆盖，不得遗漏。只对齐文档，不改已批准语义 |
| `xtask` | `crates/gantry-xtask/src/catalog.rs`、`schema.rs`、`fixtures.rs`、`main.rs` | 按已批准契约更新 schema/catalog 与校验映射；只有已实现并验证的项才移除未解析标记 |
| 生成 schema | `schemas/events/*.json`、`schemas/egress/config-view.json`、`schemas/forge/drift.json`、`schemas/forge/forge.event.v1.json`（如需） | 只由 `CARGO_NET_OFFLINE=true cargo --locked xtask schemas` 生成；不手改 |
| 事件夹具 | `crates/gantry-core/fixtures/events/cases.json`、`fixtures/contracts/negative/cases.json` | 每个新形状一个正例；负例按 §6.4 覆盖所有适用规则与判别分支 |
| 提取夹具与注解 | `fixtures/contracts/blocks/**`、`fixtures/contracts/manifest.json`、`fixtures/contracts/annotations.json` | 更新原始正文哈希及 schema/Rust 映射；由生成命令重建 blocks 与 manifest，包含行号变化与旧夹具清理 |
| 契约登记 | `fixtures/contracts/contract-gaps.json`、`fixtures/contracts/unverified.json` | 按真实实现与验证结果关闭对应 gap、移除 undefined 登记、转为验证；保留五个延期项；实现漏项不得登记为 `example` / `contract-undefined` 或用 warning 隐藏 |
| 替身 | `crates/gantry-fakes/src/forge.rs`、`crates/gantry-fakes/src/egress.rs`、相关测试 | 只在替身实际发出/读取新字段时改；`FakeForge` 的 drift 列表、`EgressStub` 的非空注入测试 |
| `store` | `crates/gantry-store/src/lib.rs`、测试 | 只有在新事件需要投影时改；M0 的追加校验会自动接受已定义形状，不需要“为了让事件通过”而改 store |
| 文档证据 | `docs/m0-contract-gaps.md`、`docs/m0-implementation.md` | 实现后更新缺口状态与验证证据；契约 PR 阶段不改 |
| Mock | `tools/mock-api/**` | 只有 Mock 实际返回 `ConfigView` 或这些事件时才改；不要为未接线的契约新增模拟分支 |

实现顺序建议（已批准字段如需改语义，先回契约 PR，不在实现 PR 里决定）：

1. `core` 类型与校验（A / B / C 可分别提交，但 `forge.drift_detected` 要等 C）。
2. 按批准字段同步接口 JSON 示例或扩展 `DOCUMENTS`，确认补充覆盖；更新 schema/catalog、`annotations.json` 哈希及 schema/Rust 映射、正负例和真实登记状态。
3. `CARGO_NET_OFFLINE=true cargo --locked xtask schemas` 生成 schema、blocks 与 manifest，清理旧行号夹具；再运行 `CARGO_NET_OFFLINE=true cargo --locked xtask schemas --check`，门禁仍为 0 failure、0 warning。
4. 替身/store 按实际使用点接入；不要批量接线。控制面服务实现另获授权后，验证复核失败的 HTTP 响应与旧视图/健康状态；类型校验不能替代这项证据。
5. 文档证据更新；未经实际验证不关闭缺口。

### 6.3 验收命令

**本提案（docs-only）**：

```sh
git status --short --untracked-files=all
git diff --check
rg -n '[ \t]+$' docs/m0-contract-proposal.md
CARGO_NET_OFFLINE=true cargo --locked xtask schemas --check
```

期望：

- 本轮只修改 `docs/m0-contract-proposal.md`；不写其他工作树文件，其他已有修改保持原样。未跟踪文件也须与修订前副本作全文差异检查，不能只依赖 `git diff`。
- `git diff --check` 无输出；`rg` 无输出；全文围栏、JSON 与链接检查按下述共同要求执行。
- `CARGO_NET_OFFLINE=true cargo --locked xtask schemas --check` 为 0 failure、0 warning；本文件不改变生成 schema。本轮不运行生成命令，也不把未运行的检查记为通过。

**契约 PR（只改文档）**：

```sh
git diff --name-only
git status --short --untracked-files=all
git diff --check
CARGO_NET_OFFLINE=true cargo --locked xtask schemas --check
```

期望：

- 变更只出现 §6.1 声明的文档；新建、未跟踪的补充文档也须全文检查，不能只看 `git diff --name-only`。
- 不出现 `schemas/**`、`fixtures/**`、`crates/**`、`tools/mock-api/**`、`apps/**`。
- 原五份受提取文档只在末尾追加无 `json` 围栏的范围说明和引用，原字节及所有提取元数据不变；schema 检查仍为 0 failure、0 warning，生成文件与提取夹具不漂移。
- 直接检查新补充的 JSON、链接、格式、字段与已确认答案的对照，以及替代范围是否清楚。新补充尚不在 `DOCUMENTS` 中，现有 `schemas --check` 只证明旧基线未漂移，不验证新字段；不删、不跳过既有检查。
- 契约获批只使补充成为对应范围的权威文档，运行时继续按 §7 拒绝；未获批字段仍标 `NEEDS_DECISION`。

**实现 PR（契约批准且另获实施授权后）**：

```sh
CARGO_NET_OFFLINE=true cargo fmt --all -- --check
CARGO_NET_OFFLINE=true cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
CARGO_NET_OFFLINE=true cargo test --workspace --all-features --locked
CARGO_NET_OFFLINE=true cargo --locked xtask schemas --check
npm test --prefix tools/mock-api
```

聚焦检查：

```sh
CARGO_NET_OFFLINE=true cargo test -p gantry-core --all-features --locked events
CARGO_NET_OFFLINE=true cargo test -p gantry-core --all-features --locked schema_regressions
CARGO_NET_OFFLINE=true cargo test -p gantry-xtask --all-targets --all-features --locked
CARGO_NET_OFFLINE=true cargo test -p gantry-store --all-features --locked
```

说明：

- 先运行 `CARGO_NET_OFFLINE=true cargo --locked xtask schemas` 生成 schema、blocks 与 manifest（含行号变更和旧夹具清理），再运行 `--check`；不得手改生成物。补充文档的已批准字段必须由同步后的示例或扩展的 `DOCUMENTS` 覆盖，注解哈希及 schema/Rust 映射、正负例、登记状态一并核对，门禁仍为 0 failure、0 warning。
- 不引用历史测试数量作为本轮通过证据；以命令的实际输出为准。
- 如果 schema 检查出现漂移，先调查原因，不放宽 schema、不删适用负例、不把实现漏项登记成 `example` / `contract-undefined`。离线依赖不足应如实报告阻塞，不自动改锁文件或联网。
- 控制面行为只在另获授权的服务实现中验证：非法视图返回 `422 config_invalid` 与 `errors[]`（`pointer` 相对视图），保留旧视图且 `health = degraded`；serde/schema 通过不是 HTTP 服务验证通过。

**全文检查共同要求**：检查所有围栏配对与语言标记；`json` 块逐个严格解析，`jsonc` 块单独识别为带注释的候选，不作为合法 JSON 或已通过 schema 的证据。检查 Markdown 链接及章节引用能定位到目标；尚未创建的拟议路径只用代码标识展示，不写成失效链接。文档格式检查须覆盖全文（含未跟踪文件），不能只检查差异行。

### 6.4 负例矩阵

| 矩阵 | 冻结前（当前） | 契约 PR 后、实现前 | 实现后 | 断言 |
| --- | --- | --- | --- | --- |
| A1：14 个事件空对象 | 全部 `ContractUndefined`；`store` 拒绝 | 仍全部拒绝；不得因为文档改了就放行 | 每个已冻结事件用最小正例通过；未冻结事件仍拒绝 | `schemas/events/<type>.json` 与 `event:<type>` 同步；store 追加行为一致 |
| A2：14 个事件坏形状 | 大多数没有逐事件负例；至少 `attempt.progress` 有拒绝用例 | 保持现状；契约 PR 只定义适用负例，不改夹具 | 每个事件必测非对象类型与未知字段；最终契约实际存在的必填、枚举、判别分支、一致性规则全部覆盖 | 无相应规则时明确记“不适用”及原因；不得新增字段凑数，也不得删除适用断言或只测空对象拒绝 |
| B1：`ConfigView.injection` 非空 | 拒绝；现有负例 `config-view-non-empty-injection` | 仍拒绝 | 合法非空通过；非法非空按 B2 拒绝 | serde、schema、`Debug` 三者一致 |
| B2：`ConfigView.injection` 坏形状 | 无细分 | 仍拒绝 | 覆盖对象类型、未知字段和既有 V10/V11/V13；按最终字段映射覆盖缺 `hosts` / `credential`、未知 `kind`、`header` 与 `kind` 不匹配、`credential.value` 等适用负例 | 不泄漏 secret 值；`Debug` 不输出注入内容；后续控制面实现另验 `422 config_invalid`、`errors[]` 相对视图 pointer、保留旧视图与 `health = degraded` |
| C1：drift 9 种 | 全部拒绝；`drift.json` 与事件 schema 拒绝一切 | 仍拒绝 | 9 种各有正例；按最终包装契约验证 `forge.drift_detected.data` 与 drift 记录的对应关系：直接使用时校验整个 `data`，包装时校验对应的记录字段 | 9 个 `kind` 都能被识别；未知 `kind` 拒绝；不预先要求事件与记录的顶层同构 |
| C2：drift 坏形状 | 无细分 | 仍拒绝 | 必测非对象与未知字段；按最终契约覆盖未知/缺 `kind`、缺 `expected` / `actual`、分支错配、错误 `target`、非法 `resolution` / `detected_by` / `synthesized_events` 等全部适用规则 | 公共信封与每种判别分支都覆盖；无相应规则须说明不适用原因 |
| D：跨契约混淆 | `outbox.enqueued` 仍拒绝；drift 仍拒绝 | 仍拒绝 | 按最终形状验证 G-06 信封不能替代 `outbox.enqueued.data`（完整动作快照即使复用字段也不等于该信封）；drift 拒绝裸 `kind` 字符串；若 `policy.denied` 冻结动作 target 判别，则覆盖误用 `ApprovalTarget` 的负例 | 防止 G-06、ApprovalTarget、ResultBundle 误用；按最终契约判定，不把候选当硬规则 |
| E：延期项 | 5 项继续 skip/example | 不变 | 不变；不为它们生成生产 schema | `unverified.json` 仍保留 5 项；没有新增生产类型 |

负例细节建议（各族附加例子均按最终契约条件适用，不预先冻结候选）：

- **事件 A2**：对象类型和 `unknown_field` 必测；逐事件列出 `missing`、`enum`、判别分支、`consistency` 的适用规则并全部覆盖。没有相应规则就说明“不适用”，不为满足固定数量增加字段。例如最终选 `approval.expired = {}` 时无缺必填、枚举或一致性负例；`approval.denied = {reason?}` 没有必填字段，若也没有枚举/一致性要求，则这三项不适用，但仍须验证 `reason` 出现时的类型等实际约束。判别联合若有分支必填字段，覆盖“判别值正确但分支字段缺失”；若判别值封闭，覆盖未知值。
- **各事件族的附加例子**：仅在最终契约规定相应规则时，覆盖 `attempt.progress` 的非法 `type`、`seq` 非递增、超长 `detail`；`guardian.*` 的未知 `class` / `action` / `reason_code`；`approval.invalidated` 的 `head_changed` 缺新旧 head、非 head 分支错配；`outbox.*` 的 `revision` 用于非状态评论动作、未定义 `cause`；`forge.synced` 的未知 `field` / `source`；`policy.denied` 的 `target.kind` 与 `action_id` 不匹配。若 outbox 最终选 `{}`，不虚构必填或枚举。`forge.drift_detected` 与 C2 共用。`budget.charged` 按最终归属规则测试两种归属同时出现/缺失（无归属边界未决，不能预断两者都空非法），按最终 source 形状覆盖 ID 错配；直接调用的 `call_id` 重放/补记不得重复扣账，沙盒按批准时点验证结算幂等，若采用增量则验证与最终结算去重。
- **injection B2**：既有 V10/V11/V13 的安全规则不得删减；按最终视图映射构造 host 不在分组、GitHub 域名/IP 字面量等禁止主机、provider 主机覆盖缺失、重复 `(host, header)`、逐跳/框架 header 等负例。所有错误都不能回显凭据值；`credential.value` 即使为 `null` 也须拒绝（若最终改用字符串引用，则作为错误字段/形状测试，不为凑路径新增 `credential` 对象）。
- **drift C2**：最终采用 `kind` 分支时，`expected` / `actual` 按该分支校验，不能只校验“是对象”；是否允许 `null` 也按最终契约。只有最终要求 `synthesized_events` 与 `forge-actions §7.4` 的合成事件列一致时才作此断言；未采用的候选枚举或一致性规则明确记不适用，不删其他适用断言。
- **延期 E**：不要为 `ResultBundle`、G-13、G-15、`github_ref` 组合、App Manifest 增加负例；它们不是生产 schema 的输入。

## 7. 当前安全默认：fail-closed

在用户确认并完成契约 PR + 实现 PR 之前，以下行为**不得改变**：

1. **14 个事件继续拒绝**：`validate_event_data` 返回 `ContractUndefined`；`schemas/events/<type>.json` 保持拒绝型；`store` 继续拒绝追加。
2. **`ConfigView.injection` 非空继续拒绝**：Rust 反序列化、序列化、schema 与 `Debug` 都只接受空对象；不输出非空内容。
3. **drift 未定义形状继续拒绝**：`schemas/forge/drift.json` 与 `schemas/events/forge.drift_detected.json` 保持拒绝型；不生成 `expected` / `actual` 猜测字段。
4. **不部分解冻**：不得只校验 `outbox.enqueued.revision` 而忽略 payload；不得用 G-06 入队信封替代事件 `data`；不得用 `ResultBundle` 的 `[提议]` 形状解冻 `attempt.finished.result_ref`。
5. **契约 PR 不等于实现**：只改文档不会让 schema 接受新形状；实现 PR 必须显式生成 schema、增加正负例并跑通检查。
6. **未知字段/未知枚举继续拒绝**：新形状落地后也应保持 `additionalProperties: false` 与封闭枚举；新增值只追加，不静默接受。

## 8. 需要用户一次确认的最小问题

请用户按编号回答；未确认项保持 `NEEDS_DECISION`。模块负责人只提供影响评估，不替用户做产品/安全决定。

1. **总范围**：是否确认本文件只是 proposal；在三组全部冻结前，维持 §7 的 fail-closed，不做生产 schema、Rust、Mock 接线？
2. **事件族**：是否接受按事件族冻结 14 个 `data`，并优先把 `guardian.escalated` 作为唯一可先补全的候选？`suppressed_duplicate` 用独立字段还是用缺省表达？
3. **守护事件**：`guardian.triaged` 是否把 `round_closed` 作为同一事件的判别分支？`event_ref` 是否用 `{source, ...}` 判别联合？`action` / `class` / `reason_code` 是否都采用封闭集？
4. **治疗请求**：`guardian.heal_requested` 是否等 `engine` 的 `HealTask` / `ProblemSpec` 契约先定，再冻结 `task`、`problems[].kind`、`batch_included[]`、`request_digest`？
5. **审批事件**：`approval.invalidated` 是否用 `reason` 判别联合并带新旧主体？`approval.expired` / `approval.denied` 是否允许最小 `{}` / `{reason?}`，还是必须带 `previous_status` / `device_id` / `decided_at`？
6. **Outbox 事件**：`outbox.enqueued` 选“仅 `revision` 审计”还是“完整动作快照”？如果选完整快照，是否内嵌 `OutboxAction`？`outbox.executing` / `outbox.cancelled` 是否允许最小 `{}`，还是必须带 `attempts` / `lease_until` / `cause`？
7. **同步与 drift**：`forge.synced` 是否直接采用 `forge.md §7.5` 的提议形状？`changes[].from/to` 是否允许任意 JSON？drift 是否采用“公共信封 + 9 个 `expected` / `actual` 判别”，并扩展 `target` 以支持评论/仓库？
8. **预算**：直接调用由 `models` 按调用记账、`call_id` 去重与对账补记、引擎不重复记，是既有分工；沙盒尝试也必须独立记账。仅沙盒时点仍 `NEEDS_DECISION`（OQ-4）：吊销时按 `FinalUsage` 一次，还是实时增量？若增量，如何与最终结算去重？`source` 是否统一为带 `attempt_id` / `call_id` 的判别联合？两种归属与无归属调用的边界如何定义？
9. **策略拒绝**：`policy.denied.target` 是否为 `config §5.1` 的 20 个动作定义判别联合？`kind` 与 `action_id` 是否都保留？`subject` 是什么？
10. **注入视图**：`ConfigView.injection` 是否复用 settings 的 `egress.injection` 字段？`credential` 用 `{secret_ref}` 还是字符串 `secret_ref`？是否包含 `present`？非空是否只从 M2 开始？V10/V11/V13 复核及失败时 `422 config_invalid`、视图相对 pointer、保留旧视图与 `health = degraded` 已有规定，不再请求批准。
11. **延期项**：是否确认 `ResultBundle`、G-13、G-15、`github_ref` 组合、App Manifest 继续延期，不计入本次生产协议阻塞？
12. **PR 拆分**：是否接受 §6.1 的 3 个独立补充文档与原提取文档仅末尾追加引用（`PR-C1` 事件、`PR-C2` 注入、`PR-C3` drift），还是合并为 1 个契约 PR？实现是否必须等三个契约 PR 全部批准后才开始？任何实施仍须另获授权，不因契约批准自动开始。

确认后，契约维护者再把答案回写到对应文档；本文不自动改变任何生产契约或 schema。
