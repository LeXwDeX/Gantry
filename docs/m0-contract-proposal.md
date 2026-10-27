# M0 契约冻结提案（草案）

> **历史记录，不作为规格。** 本文是 M0 时期的契约冻结草案，已被 [interfaces/](interfaces/) 下的正式契约取代，保留作取舍与来源追溯。现行规格见 [architecture.md](architecture.md)、[decisions.md](decisions.md)、[interfaces/](interfaces/) 与 [modules/](modules/)，当前状态见 [AGENTS.md](../AGENTS.md) 与 [NEXT-STEPS.md](../NEXT-STEPS.md)；与现行文档冲突时以现行文档为准。

> **状态：proposal（草案）；非权威（non-authoritative），未接线，不是已批准契约。**
>
> 本文件只供用户审核。它不改变 M0 / M1 的范围，不接入任何运行路径，不修改任何生产契约、schema、fixture、Rust、Mock 或前端，也不表示任何 schema 已经冻结。真正的契约变更必须另开独立契约 PR；实现必须在契约批准后另开实现 PR。
>
> 原编写工作树：`backend-baseline`（历史来源；当前前后端代码已合入 `main`，形成统一开发基线）。当前入口：[根目录 NEXT-STEPS.md](../NEXT-STEPS.md)。日期：2026-10-26。
>
> 2026-10-26 用户答复“全部按照推荐方案”，确认 §9 表格前三行的三个方向（Outbox 完整入队意图、沙盒终结结算、`injection` 引用视图）。同日另答复“go”，确认 §3.2 的四项注入视图选择；这些确认不批准完整字段表、契约 PR 或实施。
>
> 同日用户要求持续按架构设计和推荐方案完善产品，草案据此补齐字段来源、恢复责任和验收条件。随后用户答复“接受”，确认 §9.1 的六项推荐方向；仅更新确认状态，不新增取舍。其余字段与建议（含进度投影、drift 包装）仍待审，完整契约仍未批准，生产契约与实现不改。
>
> 同日用户进一步确认 B-01 的调用级关闭、只读 `terminal` 证明与长期最小关闭标记方向，并授权创建关联 Issue、隔离分支与三份独立待审契约 PR，以及修正两处错误示例。该授权不批准契约、不授权实现、不合并或发布。
>
> 依据（按优先级）：[decisions.md](decisions.md) > [architecture.md](architecture.md) 的不变式 > [interfaces/](interfaces/) > [modules/](modules/)。本文与这些文档冲突时，以它们为准。
>
> 相关缺口记录：[m0-contract-gaps.md](m0-contract-gaps.md)、`fixtures/contracts/contract-gaps.json`、`fixtures/contracts/unverified.json`。

## 0. 一页结论

| 组 | 缺口 | 当前行为 | 本文产出 | 冻结前允许的变更 |
| --- | --- | --- | --- | --- |
| A | 14 个事件 `data`（`G-05`） | `ContractUndefined`；schema 为拒绝型；`store` 拒绝追加 | 14 行阻塞表、按族推荐字段与规则、正负例 | 无 |
| B | `ConfigView.injection`（`G-03`） | 只接受空对象；非空在 serde、schema、`Debug` 均拒绝 | 既有安全复核约束、字段与校验映射、边界负例 | 无 |
| C | drift 9 种 `kind`（`G-04`） | `DriftKind` 只有枚举；drift 与 `forge.drift_detected` schema 均拒绝一切 | 推荐公共信封、9 行判别表、合成边界与正负例 | 无 |
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
| **`NEEDS_DECISION`** | 现有文档不足以安全推断，必须由用户或指定负责人决定；未确认答案不由本文自行选择。 |
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

| # | 事件 | 现有依据 | 缺失决策 | 候选/确认状态（草） | 确认者 |
| --- | --- | --- | --- | --- | --- |
| 1 | `attempt.progress` | `data-model §5.2` 只用于展示；`worker-protocol §7.1` 定义 worker 进度事件，`§7.2` 说“转发给客户端的是采样节流后的 `attempt.progress` 事件”；`sandbox.md §11` 只列采样后的 stage / detail / 工具摘要 / 自报用量 | 最终白名单、`warn` 范围、时间与文本字段的边界 | 推荐 A：服务端白名单投影并净化；B 的包装仅作对照。见 §2.2.1 | `W+E+C` |
| 2 | `guardian.triaged` | `guardian §7.10` 给出 `{event_ref, class, action, delivery_id, rule, reason_code?, fingerprint?, also_matched[], config_rev, round_id?, inbox_id?, action_ids[]}`；`§7.3` 给出处置词汇；`§7.6` 给出指纹 | 审定 §2.2.2 推荐的触发引用判别、封闭 action、`round_closed` 字段、轮 ID 与分支必填规则 | 判别联合：普通分流一支、`round_closed` 一支。见 §2.2.2 | `G+C` |
| 3 | `guardian.heal_requested` | `guardian §7.10` 给出字段列表；`§6` 给出 `HealTask` 与 `ProblemSpec` | `task` 的序列化；`problems[].kind` 词表；`batch_included[]` 元素；`request_digest` 覆盖什么、是什么类型 | 推荐字段表、`request_digest` 投影与分支负例。见 §2.2.2 | `G+E+C` |
| 4 | `guardian.escalated` | `guardian §7.10` 给出 `{delivery_id, class, reason_code, inbox_id?, fingerprint?, round_id?}`；`§7.9` 给出 `reason_code` 表；`m0-contract-gaps.md` 称它是 14 个中唯一可安全补全的候选 | `reason_code` 的封闭集；`suppressed_duplicate` 如何表达；`round_id` 类型；`Deduped` 时是否省略 `inbox_id` | 最小 JSON 候选。见 §2.2.2；仍不是冻结 | `G+U+C` |
| 5 | `approval.invalidated` | `policy §7.6` 第 5 步说 `data` 含新旧 head；`§7.7` 还列出 `pr_closed`、`pr_merged`、`delivery.abandoned`、head 变化等失效原因；`data-model §4.4` 定义审批状态机 | 6 个事件 reason 与 `InvalidateReason` 的端口映射；`pr_closed` / `pr_merged` 缺端口变体；`SubjectReplaced` / `Other` 待审 | 推荐 `reason` 判别联合（6 值，含 `owner_cancelled`）与映射；见 §2.2.3 | `P+F+E+C` |
| 6 | `approval.expired` | `data-model §5.2` 只有事件名；`policy §7.5` / `§7.7` 定义懒过期与清扫；审批实体已有 `expires_at`、`status` | 确认已推荐的 `data={}` 是否冻结；不新增 `previous_status` / `expired_at`；不重复 `approval_id` | 推荐 `data={}`；见 §2.2.3 | `P+C` |
| 7 | `approval.denied` | `api §6.1` 的 `deny` 请求体是 `{reason?}`；`policy §6.4` 的 `deny(id, device, reason)`；`§7.5` 定义 `pending → denied` | 确认已推荐的 `{reason?: string}` 与空对象是否冻结；纯文本处理与体积门禁 | 推荐 `{reason?: string}`，受 64 KiB 请求体门禁；见 §2.2.3 | `P+API+C` |
| 8 | `outbox.enqueued` | `m0-contract-gaps.md` 说只定义 `revision` 一项；`core §7.5` 的 `upsert_status_comment` 带 `revision`；`forge §7.8` 用 `revision` 保持幂等；`data-model` 的 `OutboxAction` 有完整动作字段 | 字段来源与恢复责任；哪些动作写及如何取值；是否内嵌 payload/依赖/优先级/`enqueued_by` 及包装；不能单靠入队事件恢复后续租约、重试或预写效果，G-06 信封不能替代 | 已确认方向：记录完整入队意图，而非仅 `revision` 审计；保留状态评论 `revision`。候选 A/B 仅作字段对照；B 的完整快照不是已批准包装。见 §2.2.4 | `F+G+E+S+C` |
| 9 | `outbox.executing` | `forge §7.8` 说 claim 时追加；`data-model §5.2` 只有事件名；`core §7.5` 未列 | 审计字段及其与 claim 结果的一致性 | 推荐 `{attempts, replay}`；空对象与租约快照仅作对照。见 §2.2.4 | `F+S+C` |
| 10 | `outbox.cancelled` | `forge §7.8` 的状态机列出 `cancel`、前置失败、`superseded`、执行前审批失败；`m0-contract-gaps.md` 说 coalesce 时追加；`OutboxAction` 没有 `reason` 字段 | 4 值取消原因；不新增 `cancelled_by`；不塞入 `last_error` 对象；不允许空对象 | 推荐 `{reason}`，4 值复用 `store::CancelReason`；见 §2.2.4 | `F+G+E+C` |
| 11 | `forge.synced` | `forge.md §7.5` 第 6 点给出提议形状 `{target, source, changes[{field, from, to}], synced_at}`；`guardian §4` 以该提议为输入；`data-model §5.2` 只有名称 | target 范围、字段白名单、可空性及 source 枚举 | 推荐 `target` 仅 PR/Issue、`changes` 按 field 强类型、`subject=prj_…`；字段与正负例见 §2.2.5.1 | `F+G+C` |
| 12 | `forge.drift_detected` | `forge-actions §7.4` 给出 drift 目录和 `head_diverged` 示例；`forge.md §7.7` 给出 drift 数据来源；`core::ports::forge::DriftKind` 有 9 个变体 | 9 种 `expected` / `actual`；公共信封；`target`；`resolution` / `detected_by` / `synthesized_events` 的封闭集 | 推荐 `data` 直接使用 drift 记录、`DriftTarget` 判别、9 种合成边界；见 §4 | `F+G+E+C` |
| 13 | `budget.charged` | P-74、`egress-control §4.3` / `§4.5`、`models §7.3`：`models` 按直接调用记账，以 `call_id` 去重、对账补记；`engine §7.9`：引擎不重复记直接调用，沙盒尝试另行记账；`engine §11` 给出字段骨架 | 收尾时序、并发预留、取消/崩溃与晚到补记、跨日细则、`source` 统一形状、dimension 与单位、归属边界及是否带 `config_rev` | 已确认方向：沙盒尝试在吊销令牌并完成在途请求收尾后按稳定 `FinalUsage` 一次结算，实时仅展示/预警；直接调用仍由 `models` 按 `call_id` 记账/对账，引擎不重复扣账。候选 `source` 判别联合 + 字段骨架；不能取消任一记账来源。见 §2.2.6 | `E+M+X+C+U` |
| 14 | `policy.denied` | `policy §7.3` 第 8 步给出 `{action_id, kind, target, reason_code, config_rev, subject?}`；`§6.1` 给出 `ActionRequest` / `ActionDecision`；`config §5.1` 有 20 个动作 id；`worker §9` 有 `policy.*` 子码 | `target` 覆盖 20 个动作的判别联合；`kind` 与 `action_id` 是否都保留；`subject` 类型；`reason_code` 封闭集；补丁拒绝与动作拒绝是否同一形状 | `target` 判别联合 + 字段骨架。见 §2.2.6 | `P+C+U` |

### 2.2 逐项推荐与待决标记

> 下面所有 JSON / JSONC 都是**候选草形状**。标 `jsonc` 的块含注释，注释只用于标记未定项，不是合法 JSON，不能直接作为 schema。

#### 2.2.1 `attempt.progress`

**推荐规格，待契约审查。** 服务端按白名单投影 worker §7.1 事件，不原样转发。`data` 只允许 `stage` / `tool` / `usage`；`hello` / `heartbeat` / `warn` / `finish` 不进入事件。字段名复用 `core::worker` 的 `StageEvent` / `ToolEvent` / `UsageEvent`。

```json
{"v":1,"seq":3,"type":"stage","stage":"edit","detail":"reading src/api/items.rs"}
```

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `v` | 是 | 整数 | 常量 `1`。 |
| `seq` | 是 | 整数 | `≥1`；采集层校验严格递增；采样可跳号；不是信封 `seq`。 |
| `type` | 是 | 枚举 | `stage` / `tool` / `usage`。 |

**`stage` 分支**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `stage` | 是 | `ProgressStage`（`core::worker`） | `init` / `explore` / `plan` / `edit` / `test` / `review` / `package` / `finalize`。 |
| `detail` | 是 | 字符串 | `≤200` Unicode 码点；去 ANSI/控制符/bidi；纯文本展示。 |

**`tool` 分支**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `phase` | 是 | `ToolPhase` | `start` / `end`。 |
| `call_id` | 是 | 字符串 | 非空；原始行 ≤8 KiB；净化后纯文本；不新增字段长度。 |
| `tool` | 是 | 字符串 | 非空；原始行 ≤8 KiB；净化后纯文本；不新增字段长度。 |
| `summary` | 仅 `phase=start` | 字符串 | `≤200` 码点；去控制符；不得含文件内容/命令输出；`phase=end` 禁止出现。 |
| `ok` | 仅 `phase=end` | 布尔 | `phase=start` 禁止出现。 |
| `duration_ms` | 仅 `phase=end` | 整数 | `≥0`；`phase=start` 禁止出现。 |

**`usage` 分支**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `input_tokens` / `output_tokens` / `cache_read_tokens` / `cache_write_tokens` / `turns` | 是 | 整数（`UsageEvent`） | 各自 `≥0`；累计值，非增量。 |

`usage` 是 worker 自报，仅展示；不进入预算台账，不产生 `budget.charged`。`attempt.progress.data` 不重复 `attempt_id` / `job_id`；信封 `subject=att_…` 已表达。`received_at` 留在采集记录，不进入 `data`；信封 `ts` 只是事件追加时间，不等同 `received_at`。`hello` / `heartbeat` / `warn` / `finish` 只留在 `progress.jsonl` 诊断。

**信封**：`subject=att_…`；`actor={kind:"engine",id:"engine"}`（`ActorKind` 无 `sandbox`）；`project_id` 非空；`correlation_id` 为 `delivery_id` 或 `conversation_id`；`causation_id=null`。可选字段一律省略，不接受 `null`；未知字段拒绝。

**分支验收**

| 分支 | 正例 | 负例 |
| --- | --- | --- |
| `stage` | `{"v":1,"seq":2,"type":"stage","stage":"edit","detail":"reading src/api/items.rs"}` | `seq:0`；`v:2`；缺 `detail`；`detail:null`；`detail` 超 200；`type:"warn"` 被采集层过滤，不产生事件 |
| `tool` start | `{"v":1,"seq":3,"type":"tool","phase":"start","call_id":"c1","tool":"Bash","summary":"cargo test -p api"}` | 缺 `summary`；带 `ok` / `duration_ms`；`summary:null`；`call_id` 空 |
| `tool` end | `{"v":1,"seq":4,"type":"tool","phase":"end","call_id":"c1","tool":"Bash","ok":true,"duration_ms":4210}` | 缺 `ok` / `duration_ms`；带 `summary`；`ok:null`；`duration_ms:null` |
| `usage` | `{"v":1,"seq":5,"type":"usage","input_tokens":18200,"output_tokens":2400,"cache_read_tokens":9000,"cache_write_tokens":1200,"turns":3}` | 缺任一 token 字段；负数；`null`；多出 `detail` / `attempt_id` |

#### 2.2.2 守护三事件

**推荐规格，待契约审查。** 三事件共用信封：`subject=dlv_…`，`data.delivery_id` 必须等于 subject；`actor={kind:"guardian",id:"guardian"}`；`project_id` 非空；`correlation_id=delivery_id`；`causation_id` 为直接触发事件 ID，settlement/定时器为 `null`。可选字段一律省略，不用 `null`。

**`round_id`**：裸 ULID（26 位 Crockford Base32，与 `core::ports::models::CallId` 同等表示；端口 `RoundId` 在构造时包裹，当前是不透明 newtype，事件层需新增裸 ULID 序列化契约），不是 `evt_…`，不新增前缀。首次开轮前生成；同一事务写开轮 `guardian.triaged` 与 `guardian.heal_requested`，两者同值；后续沿用；`request_heal` 以该值幂等。开轮 `silent_fix` / `silent_reply` 与 H-02/H-03/H-09 的 `round_closed` 使用同一值。`batch` 不独立开 nit 轮；nits 以 `problems[].kind=nit` 随当前轮提交。

**`event_ref`** 四源；普通 triage 只用 `forge` / `event`，`attempt` / `settlement` 只用于 `round_closed`：

| source | 必填 | 可选 | 约束 |
| --- | --- | --- | --- |
| `forge` | `kind`（`ForgeEventKind` 23 值）、`dedup_key` | `head_sha`（`GitOid`） | `head_sha` 仅在触发数据确有 head 时出现；`causation_id` 指向该 `forge.<kind>` 事件。 |
| `event` | `event_id`（`evt_…`） | — | 仅限 guardian 实际订阅的固定事件：`forge.synced`、`forge.drift_detected`、`approval.granted` / `invalidated` / `expired` / `denied` / `consumed`、`delivery.stage_changed` / `abandoned`、`budget.exhausted`、`worktree.unlocked`、`settings.updated`、`outbox.failed`。不开放全部 78 事件；`causation_id == event_id`。 |
| `attempt` | `event_id`（`evt_…`）、`attempt_id`（`att_…`） | — | 仅 `attempt.finished`；`causation_id == event_id`。 |
| `settlement` | `head_sha`（`GitOid`） | — | 结算/定时器触发；`causation_id=null`。 |

`event` 分支不再冗余固定 `EventType.kind`；事件类型由 `event_id` 在日志中解析，契约校验只接受上表闭集。

**`guardian.triaged` 字段**

| 字段 | 必填性 | 类型/来源 | 一致性 |
| --- | --- | --- | --- |
| `event_ref` | 是 | 上表四源 | 普通 triage 只用 `forge` / `event`。 |
| `action` | 是 | 枚举 | `silent_fix` / `silent_reply` / `batch` / `defer` / `finalize` / `escalate` / `round_closed`。 |
| `class` | 条件必填 | `TriageClass` 14 值 | `finalize` 省略；`round_closed` 保留开轮 class；其余必填。 |
| `delivery_id` | 是 | `dlv_…` | 等于 subject。 |
| `rule` | 是 | 非空字符串 | 规则/决策标识，不封闭。 |
| `also_matched` | 是 | `TriageClass[]` | 可为空；`finalize` / `round_closed` 必须 `[]`。 |
| `config_rev` | 是 | 整数 `≥0` | 当前生效配置版本。 |
| `reason_code` | 条件必填 | §7.9 现有 27 值封闭表 | 仅 `escalate` 与 `round_closed` 的上报终态使用；其余禁止。 |
| `fingerprint` | 可选 | `sha256:` + 64 位小写十六进制 | `finalize` / `round_closed` 禁止。 |
| `round_id` | 条件必填 | 裸 ULID | 开轮 `silent_fix` / `silent_reply` 必填；`round_closed` 必填；`defer(round_active)` 必填；`batch` 禁止；其他按是否有活跃轮可选。 |
| `inbox_id` | 可选 | `inb_…` | 仅 `escalate` / `round_closed` 上报终态允许；其余禁止；与 `guardian.escalated.inbox_id` 相等。 |
| `defer_reason` | 仅 `defer` 必填 | `paused` / `lock_held` / `round_active` / `settling` | 其他 action 禁止。 |
| `action_ids` | 是 | `act_…[]` | 排序去重，可为空；同一事务已入队动作。 |

**普通 action 矩阵**

| action | `class` | `reason_code` | `defer_reason` | `round_id` | `inbox_id` |
| --- | --- | --- | --- | --- | --- |
| `silent_fix` | `ci_failure_in_scope` / `bot_finding_valid` / `mechanical_conflict` | 禁止 | 禁止 | 必填（开轮） | 禁止 |
| `silent_reply` | `bot_finding_invalid` | 禁止 | 禁止 | 必填（开轮） | 禁止 |
| `batch` | `nit` | 禁止 | 禁止 | 禁止 | 禁止 |
| `defer` | `ci_failure_in_scope` / `bot_finding_valid` / `bot_finding_invalid` / `mechanical_conflict` | 禁止 | 必填 | `round_active` 必填；`settling` 可选；其余禁止 | 禁止 |
| `finalize` | 省略 | 禁止 | 禁止 | 有活跃轮可选 | 禁止 |
| `escalate` | 任意 14 值 | 必填 | 禁止 | 有活跃轮可选 | 可选；与 escalated 相等 |

开轮 `guardian.triaged` 示例（`round_id` 与随后 `heal_requested` 同值）：

```json
{
  "event_ref": {"source":"forge","kind":"check_suite_completed","dedup_key":"check_suite:acme/web:1122334455:2026-10-24T12:44:10Z","head_sha":"f2c03d84a858156b0121130fd1ec2f391aa90865"},
  "action": "silent_fix",
  "class": "ci_failure_in_scope",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "rule": "K-13",
  "also_matched": [],
  "config_rev": 18240,
  "round_id": "01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "action_ids": []
}
```

**`round_closed` 分支**

`action="round_closed"`，其余字段：

| 字段 | 必填性 | 类型/来源 | 一致性 |
| --- | --- | --- | --- |
| `outcome` | 是 | 枚举 | `pushed` / `replied` / `escalated` / `no_change` / `failed` / `denied` / `superseded` / `cancelled`。 |
| `class` | 是 | `TriageClass` | 始终保留开轮 class；不改成上报 class。 |
| `head_before` | 是 | `GitOid` | 轮基线 head。 |
| `head_after` | 可选 | `GitOid` | 观察到与 `head_before` 不同的 head 时出现；不用 `null`。 |
| `failing_before` | 是 | `fingerprint[]` | 可为空；保持采集顺序。 |
| `failing_after` | 条件 | `fingerprint[]` | 仅已评估时出现；`progress=done` 时为 `[]`；未评估时省略。 |
| `progress` | 是 | 枚举或 `null` | `done` / `improved` / `stalled` / `null`。 |
| `stalled_rounds` | 是 | 整数 `≥0` | 按 §7.6 算法；H-14 额外 +1；不发明新策略。 |
| `round_id` | 是 | 裸 ULID | 开轮值。 |
| `reason_code` | 条件 | §7.9 现有 27 值 | `escalated` / `no_change` / `failed` / `denied` 必填；其余禁止。 |
| `inbox_id` | 可选 | `inb_…` | 仅上报终态；与配对 `guardian.escalated.inbox_id` 相等。 |
| `action_ids` | 是 | `act_…[]` | 可为空。 |
| `fingerprint` | 禁止 | — | 用 `failing_before` / `failing_after`。 |
| `also_matched` | 是 | `[]` | 固定空数组。 |

**`round_closed` 结果矩阵**

| `outcome` | `progress` | `failing_after` | `reason_code` | `inbox_id` |
| --- | --- | --- | --- | --- |
| `pushed` | `done` / `improved` / `stalled` | 必填；`done` 为 `[]` | 禁止 | 禁止 |
| `replied` | `null` | 省略 | 禁止 | 禁止 |
| `escalated` | `stalled` / `null` | `progress != null` 时必填；`null` 时省略 | 必填 | 与 escalated 相等；审计省略时同省 |
| `no_change` / `failed` / `denied` | `null` | 省略 | 必填 | 与 escalated 相等；审计省略时同省 |
| `cancelled` / `superseded` | `null` | 省略 | 禁止 | 禁止 |

`round_closed` 示例（成功推送后评估为 `done`）：

```json
{
  "event_ref": {"source":"settlement","head_sha":"1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b"},
  "action": "round_closed",
  "outcome": "pushed",
  "class": "ci_failure_in_scope",
  "head_before": "f2c03d84a858156b0121130fd1ec2f391aa90865",
  "head_after": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
  "failing_before": ["sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a"],
  "failing_after": [],
  "progress": "done",
  "stalled_rounds": 0,
  "round_id": "01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "rule": "H-02",
  "also_matched": [],
  "config_rev": 18240,
  "action_ids": ["act_01JBA1H5T7XA3P9C2E6J8M1QWD"]
}
```

**`guardian.heal_requested`**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `round_id` | 是 | 裸 ULID | 与开轮 triaged 同值。 |
| `delivery_id` | 是 | `dlv_…` | 等于 subject。 |
| `task` | 是 | `HealTask` 4 值 | `ci_failure` / `bot_finding` / `mechanical_conflict` / `nit_batch`；当前不产生独立 nit 轮，nits 以 `problems[].kind=nit` 随当前任务提交。 |
| `head_sha` | 是 | `GitOid` | 40 位小写十六进制。 |
| `problems` | 是 | 数组 | 非空；每项仅 `{fingerprint, kind}`；`fingerprint` 为 `sha256:` + 64 位小写；`kind` 为 `ci_failure_in_scope` / `bot_finding_valid` / `bot_finding_invalid` / `mechanical_conflict` / `nit`；保持提交顺序，fingerprint 唯一。 |
| `batch_included` | 是 | `fingerprint[]` | 可为空；必须是 `problems` 中 `kind=nit` 的子集；保持提交顺序；唯一。 |
| `request_digest` | 是 | `sha256:` + 64 位小写 | 见下。 |
| `config_rev` | 是 | 整数 `≥0` | 请求时配置版本。 |

**`request_digest`**：`"sha256:" + lowercase_hex(SHA-256(JCS(["gantry/heal-request/v1", P])))`，`JCS` 为 RFC 8785。`P` 是完整 `HealRequest` 的规范投影：`round_id`、`delivery_id`、`pr`、`head_sha`、`task`、`problems`（含 `material_refs`）、`scope_allow`、`targets`、`materials`、`config_rev`。数组保持已提交请求顺序，不额外排序，以免把不同请求当成等价；唯一集合约束不变。`materials` 用 `{id, kind, source, text_sha256}`，`text_sha256` 是材料正文 UTF-8 字节的 `sha256:` 摘要；`truncated` 等元数据仅在实际材料对象携带时纳入，不补造缺失字段。**不包含**材料正文、事件 `id` / `seq` / `ts`、`batch_included`、`event_ref`、`causation_id`。示例：

```json
{
  "round_id": "01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "task": "ci_failure",
  "head_sha": "f2c03d84a858156b0121130fd1ec2f391aa90865",
  "problems": [{"fingerprint":"sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a","kind":"ci_failure_in_scope"}],
  "batch_included": [],
  "request_digest": "sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a",
  "config_rev": 18240
}
```

示例中的 `request_digest` 与 `fingerprint` 相同只是占位，不是计算值；实际值按上方 JCS 规则生成。

**`guardian.escalated`**

| 字段 | 必填 | 类型/来源 | 一致性 |
| --- | --- | --- | --- |
| `delivery_id` | 是 | `dlv_…` | 等于 subject。 |
| `class` | 是 | `TriageClass` 14 值 | 与 §7.9 的 reason 映射一致；不要求等于 `round_closed.class`。 |
| `reason_code` | 是 | §7.9 现有 27 值 | H-11 非安全 proposal 的未知分类用 `guardian.unknown_event`，保留 `causation_id` 指向触发事件；不新增 reason。 |
| `inbox_id` | 条件 | `inb_…` | 见下表。 |
| `suppressed_duplicate` | 是 | 布尔 | 无默认值，不接受 `null`。 |
| `fingerprint` | 可选 | `sha256:` + 64 位小写 | 与 triaged 配对时若都有则相等。 |
| `round_id` | 可选 | 裸 ULID | 配对 `round_closed` 时必填且相等。 |

| `suppressed_duplicate` | `inbox_id` | 允许条件 |
| --- | --- | --- |
| `false` | 必填 | 本次创建 InboxItem。 |
| `true` | 必填 | `InboxPort::raise` 返回 `Deduped`，返回已有 `inb_…`。 |
| `true` | 省略 | 仅当其他模块已写同一去重键的 open InboxItem，且 `causation_id` 指向的 `inbox.raised` / 配对 triaged 能证明该事实；否则禁止。 |

**`guardian.escalated` 示例**

```json
{
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "class": "unknown",
  "reason_code": "guardian.unknown_event",
  "inbox_id": "inb_01JBX2F5H7K9M1P3R5T7V9X0ZC",
  "suppressed_duplicate": false,
  "fingerprint": "sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a",
  "round_id": "01JBX4Q9ZK3M8T5V2N7RHC6WYD"
}
```

**分支验收**

| 事件/分支 | 正例 | 负例 |
| --- | --- | --- |
| `triaged` 开轮 | `silent_fix` + `class=ci_failure_in_scope` + `round_id` | 开轮无 `round_id`；`batch` 带 `round_id`；`finalize` 带 `class`；`defer` 无 `defer_reason`；`escalate` 无 `reason_code` |
| `triaged` `round_closed` | `outcome=pushed` + `progress=done` + `failing_after=[]` + `class=开轮 class` | `pushed` + `progress=null`；`done` + 非空 `failing_after`；`escalated` 无 `reason_code`；`cancelled` 带 `reason_code`；`round_closed` 带 `fingerprint` |
| `heal_requested` | `task=ci_failure` + 一个 `ci_failure_in_scope` + 合法 digest | 空 `problems`；`batch_included` 含非 nit；`round_id` 为 `evt_…`；digest 非 `sha256:` |
| `escalated` | `suppressed_duplicate=false` + `inbox_id` | `false` 无 `inbox_id`；`Deduped` 无 id；`true` 无 id 且无其他模块写入证据；`class/reason` 不匹配 |

#### 2.2.3 审批三事件

**推荐规格，待契约审查。** `approval.*` 的 `subject=apr_…`，`data` 不重复 `approval_id`。`actor` 取实际执行写入的 P-78 `ActorCtx.actor`：API 拒绝为设备，engine/后台任务为 engine/system；不新增 `policy` actor 枚举。`project_id` 按审批所属项目；内部动作为 `null`。`correlation_id` 为 `delivery_id`（内部动作为 `null`）。

**`approval.invalidated`**：`reason` 判别联合，6 值：

| `reason` | 必填字段 | 约束 |
| --- | --- | --- |
| `head_changed` | `previous_head_sha`、`current_head_sha` | 两个完整 `GitOid`（40 位小写十六进制），必须不相等。 |
| `pr_closed` | — | `{}` 除 `reason`。 |
| `pr_merged` | — | `{}` 除 `reason`。 |
| `delivery_abandoned` | — | `{}` 除 `reason`。 |
| `attempt_superseded` | `previous_attempt_id`、`current_attempt_id` | 两个 `att_…`，必须不同。 |
| `owner_cancelled` | — | 推荐新增分支；`{}` 除 `reason`；审批发起方主动取消。 |

- `head_changed` 覆盖 `forge.pr_synchronized` / `forge.synced` 的 head 变化；`pr_closed` / `pr_merged` 分别由 `forge.pr_closed` / `forge.pr_merged` 触发；`pr_merged` 只使 `pending` 失效，`granted` 由消费路径处理；`pr_closed` / `delivery_abandoned` / `owner_cancelled` 覆盖对应目标的活跃审批；`attempt_superseded` 由 engine 在补丁被新 Attempt 取代时触发。这 6 个分支之外不发 `approval.invalidated`。
- 与现有 `core::ports::policy::InvalidateReason` 的关系：`HeadMoved` → `head_changed`，`AttemptSuperseded` → `attempt_superseded`，`DeliveryAbandoned` → `delivery_abandoned`，`OwnerCancelled` → `owner_cancelled`。`SubjectReplaced` 与 `Other(String)` 在 M0 不产生本事件：前者由 policy 记录内部失效并待契约审查是否新增分支，后者不把任意字符串写入封闭 `reason`；`pr_closed` / `pr_merged` 若当前端口没有对应变体，契约 PR 需补齐端口枚举或映射，本轮不改代码。`docs/modules/policy.md §7.7` 尚未列 `owner_cancelled`，`PR-C1` 需同步；当前不是已生效路径。
- 内部动作主体不匹配仍按 `policy §7.6` 拒绝消费，不新增内部失效原因；`base_advanced` 仍按 OQ-P9 不使审批失效。
- 不重复 `approval_id`，不要求 `repo` / `number`（由审批 `target` 推出）。示例：

```json
{"reason":"head_changed","previous_head_sha":"f2c03d84a858156b0121130fd1ec2f391aa90865","current_head_sha":"1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b"}
```

**`approval.expired`**：`data={}`。`subject=apr_…` 已标识审批，实体 `expires_at` 与事件 `ts` 足够；不新增 `previous_status` / `expired_at`。懒过期由触发读写的实际 `ActorCtx` 写入；后台清扫使用现有 `system` actor，不新增 `policy` 枚举。

**`approval.denied`**：`data={reason?: string}`。`reason` 可省略；出现时必须是字符串，不接受 `null`；不新增字段级上限，受 `api.md` 现有 64 KiB 请求体门禁（settings 256 KiB 不适用于本路由）。用户备注按纯文本展示，禁止凭据；不新增 `device_id` / `decided_at`，由信封 `actor` / `ts` 表达。示例：

```json
{"reason":"先别合"}
```

`policy §11` 要求事件不含未截断的外部文本。本规格推荐将用户备注解释为“非 GitHub/沙盒正文入口”，沿用 64 KiB 体积门禁；替代范围是契约审查时另定截断上限并只存截断后文本，不改变字段形状。

**分支验收**

| 事件/分支 | 正例 | 负例 |
| --- | --- | --- |
| `invalidated` `head_changed` | 两个不同完整 `GitOid` | 两个 SHA 相等；缺一个；短 SHA |
| `invalidated` 其他 | `{"reason":"pr_closed"}`、`{"reason":"owner_cancelled"}` 等 | 未知 `reason`；`pr_closed` 带 head 字段；`attempt_superseded` 两个 AttemptId 相同；`owner_cancelled` 带额外字段 |
| `expired` | `{}` | 非空 `data`；`previous_status`；`expired_at` |
| `denied` | `{}` 或 `{"reason":"先别合"}` | `reason:null`；`reason` 非字符串；带 `device_id` / `decided_at` |

#### 2.2.4 Outbox 三事件

**推荐规格，待契约审查。** 用户已确认记录完整入队意图，而非仅 `revision` 审计；以下字段与事务规则是据此收敛的推荐，不是已批准契约。三事件仍为 `ContractUndefined`。依据：`forge-actions §2–§3`、`store §6.3 / §7.6`、`forge §7.8`，以及 `core::forge::ActionPayload`、`store::OutboxInsert / Claimed / CancelReason`。

**共用信封**：`subject=act_…`；`project_id` 为动作所属项目，非空；`correlation_id=delivery_id`。入队使用入队方 P-78 `ActorCtx.actor`；认领使用 `{kind:"forge",id:"forge"}`；取消使用实际取消方的 `ActorCtx.actor`（自动合并、依赖检查与执行前复核为 forge）。`causation_id` 为直接触发事件或 `null`，入队时须与 `action.enqueued_by.causation_id` 相等。不新增 actor 类别。

**`outbox.enqueued`：`data={action}`，只保存不可变入队意图。** 不内嵌完整可变 `OutboxAction`，也不直接复用 G-06 `OutboxEnqueueEnvelope`。动作 ID 只在信封 `subject`，项目只在信封 `project_id`；不再在 `action` 中重复。

| `action` 字段 | 必填 | 类型与来源 | 约束 |
| --- | --- | --- | --- |
| `delivery_id` | 是 | `DeliveryId`，入队请求 | 等于信封 `correlation_id`；必须属于信封项目。 |
| `kind` | 是 | `ForgeActionKind` | 下表 14 值，决定 `payload` 的唯一分支；条件伪动作不是 Outbox kind。 |
| `payload` | 是 | `ActionPayload`，入队方提供、forge 校验 | 保存完整已校验 payload，不能只保存幂等 intent；未知字段、缺少必需字段或分支不符均拒绝。 |
| `target` | 是 | 字符串，forge 推导 | 按 `forge-actions §3.2` 的稳定目标格式，须与 payload 一致。 |
| `intent` | 是 | 对象，forge 推导 | 严格按 `§3.2` 的本 kind 字段集合；空 intent 为 `{}`，不是 `null`。保存当时的实际值，不用后来的渲染规则重算历史。 |
| `idempotency_key` | 是 | `IdempotencyKey`，store 派生 | 按 `forge-actions §2.2`，用信封项目、交付、kind、target、intent、epoch 派生；须完全相等，沿用 `gk1_`。 |
| `requires_approval` | 是 | `ApprovalId` 或 `null`，入队方调用 policy 的结果 | 无引用必须显式 `null`。引用不是消费凭证；P-54 / P-62 的执行前复核与消费不变。 |
| `depends_on` | 是 | `ActionId[]`，入队请求 | 无依赖为 `[]`；不得重复或包含自身；保留已校验顺序。缺失的前置不能当成功。 |
| `priority` | 是 | `high` / `normal` / `low`，入队请求 | 记录实际值，不以读取时默认值补齐。 |
| `not_before` | 是 | `Timestamp` 或 `null`，入队请求 | 只记录初始延后时间；以后重试/退避不得改历史事件。 |
| `epoch` | 是 | `u32`，入队请求 | `≥0`；有意重做已终态动作创建新行并加 1，不复活旧行（P-53）。 |
| `coalesce_key` | 是 | 非空字符串或 `null`，forge 推导 | 仅状态评论按同一交付/目标/slot 合并时有值；其他动作为 `null`。不提前启用 M2 行为。 |
| `enqueued_by` | 是 | `{module, causation_id}`，入队请求 | `module` 为实际模块的非空字符串；`causation_id` 为 `EventId` 或显式 `null`。不从信封 actor 猜模块，也不将模块替换成设备身份。 |

**14 个 payload 分支**：复用现有 `core::forge` 同名类型和 `forge-actions §3.3`，不另创宽松对象。表中 `?` 表示既有可省略/可为 `null` 的字段；其余必须出现且不可为 `null`。`items[].detail` 沿用可省略字符串；数组可以为空与否仍按动作契约的业务校验，不用事件层放宽。

| `kind` | payload 字段（全部含 `repo: RepoSlug`） | 特别约束 |
| --- | --- | --- |
| `push_branch` | `branch, base_ref, worktree_id, local_head_oid, expected_tree_oid, expected_remote_oid?, validation_ref, transport, force` | OID 为完整 `GitOid`；`validation_ref` 为 `Sha256Digest`；`transport=git\|api`；保留 I8 校验引用。 |
| `create_pr` | `head, base, title, body, draft, closes, expected_head_oid, maintainer_can_modify` | `closes` 为 Issue 编号数组；不丢弃不参与键计算的并发前置字段。 |
| `update_pr` | `pull_number, title?, body?, base?, state?` | `state=open\|closed`；`null` 表示不修改该字段，不表示清空正文。 |
| `upsert_status_comment` | `target, slot, revision, head_sha, title, items` | `target={type:pr\|issue,number}`；`revision:u32`；item 为 `{id,state,label,detail?}`，state 沿用既有六值。 |
| `create_comment` | `target, client_ref, body` | `target={type:pr\|issue,number}`；普通评论不是状态评论。 |
| `reply_review_thread` | `pull_number, thread_node_id, in_reply_to_comment_id, body` | 不丢失回复锚点。 |
| `resolve_review_thread` | `pull_number, thread_node_id, expected_comment_count, reason, fixed_in_oid` | `reason` 留在 payload 作审计，不进入 intent。 |
| `create_issue` | `client_ref, title, body, labels, assignees, milestone?` | 保留完整创建意图。 |
| `update_issue` | `number, spec_block_markdown, expected_block_hash?, title?, labels?` | `expected_block_hash=null` 表示首次写规格块；不允许覆盖块外用户文本。 |
| `close_issue` | `number, state_reason, require_merged_pr?` | `state_reason=completed\|not_planned`。 |
| `add_labels` | `number, labels` | intent 中 labels 按既有规则排序去重。 |
| `request_reviewers` | `pull_number, reviewers, team_reviewers` | intent 中两个集合按既有规则排序。 |
| `merge_pr` | `pull_number, expected_head_sha, merge_method, mode, commit_title?, commit_message?` | `mode=auto`；`merge_method` 沿用 `MergeMethod`；恒需审批。 |
| `rerun_failed_jobs` | `run_id, expected_run_attempt, head_sha` | 保留运行尝试号，不能仅用 run ID 判重复。 |

`upsert_status_comment` 只在 `payload.revision` 及其对应 `intent.revision` 保存修订号，二者相等；不再新增顶层 `revision`。`intent.render_digest` 使用入队时渲染正文的既有摘要规则，与实际渲染结果一致。guardian 从同一交付/目标/slot 的已入队历史最大 revision 继续递增，已取消记录也计入历史（P-61）。其他动作不得带 revision。

**追加与恢复**：仅 `Enqueued.created=true` 时，在插入 Outbox 行的同一事务追加一次事件。`created=false` 返回原行，不追加、不覆盖旧 payload。合并淘汰旧 queued 行与各自的取消事件同事务；不取消 executing 行来做合并。事件超限或校验失败必须回滚入队，不能截掉 payload 后假装保存完整意图。正文先经过既有入口校验与密钥扫描；原始凭据、签名和认证头不进事件，forge 写出前的第二道净化仍保留（I5）。

正例（信封 `subject=act_01JBA1F3Q9TX2N7B5D8H0M4RVC`、`project_id=prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK`；键沿用 `forge-actions §2.1` 的同一组输入）：

```json
{
  "action": {
    "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
    "kind": "create_pr",
    "payload": {
      "repo": "acme/shop-api",
      "head": "gantry/42-fix-pagination",
      "base": "main",
      "title": "Fix pagination off-by-one in /orders",
      "body": "Fixes the off-by-one when page size divides total count.",
      "draft": false,
      "closes": [42],
      "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486",
      "maintainer_can_modify": false
    },
    "target": "pr:gantry/42-fix-pagination->main",
    "intent": {
      "base": "main",
      "head": "gantry/42-fix-pagination",
      "title": "Fix pagination off-by-one in /orders",
      "body": "Fixes the off-by-one when page size divides total count.",
      "draft": false,
      "closes": [42]
    },
    "idempotency_key": "gk1_anbfwcip7r36is6wsqcuxxfd34",
    "requires_approval": null,
    "depends_on": ["act_01JBA1F2N8SW1M6A4C7G9K3QTB"],
    "priority": "high",
    "not_before": null,
    "epoch": 0,
    "coalesce_key": null,
    "enqueued_by": {"module":"engine","causation_id":"evt_01JBA1G8S2WY4Z6B9D1F3H5KNP"}
  }
}
```

| 恢复内容 | 权威来源与责任 | 不能承诺的事 |
| --- | --- | --- |
| 完整入队意图、状态评论 revision | 事件日志；guardian 继续历史计数 | PR 快照重建不得令计数回退；G-06 不是本事件 data。 |
| 当前状态、次数、租约、错误、退避、预写效果与结果 | 持久化 Outbox 主表，一致备份（`architecture §6`、`store §7.6–§7.9`） | 入队事件不能代替主表备份，不凭默认值重建当前执行状态。 |
| 审批消费 | 审批日志与投影；policy 核对 `consumed_by` | 同动作重放不二次消费；消费后失败不退回审批（P-54）。 |
| GitHub 效果 | forge 执行前复核审批，先 probe，未命中才预写和变更 | 远端成功但 `complete` 前崩溃仍先 probe；成功状态、结果与 `outbox.succeeded` 同事务。不由本组三事件声称远端未执行。 |

**`outbox.executing`：`data={attempts,replay}`。**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `attempts` | 是 | 整数 `≥1`，本次 `Claimed.action.attempts` | 是 claim 更新后的值，不是独立累计的事件条数。 |
| `replay` | 是 | 布尔，本次 `Claimed.replay` | queued 普通认领为 `false` 并增加 attempts；过期 executing 租约重放为 `true`，不增加 attempts。 |

每次 claim 返回非空结果，状态/租约更新与事件同事务；返回空不发事件。事件不带 `lease_until` / `started_at`，续租不发 executing 事件。限流 `requeue(refund_attempt=true)` 可退还尝试次数；因此下次普通认领的 attempts 可以与先前事件相同，不能以 `(action_id,attempts)` 去重合法认领（`store §7.6`）。

```json
{"attempts":1,"replay":false}
```

**`outbox.cancelled`：`data={reason}`。** 原因采用当前 `store::CancelReason` 的显式线上词表，不再保留空对象或 `cancel_requested` 对照方案：

| `reason` | 触发与状态 | 原子性 |
| --- | --- | --- |
| `superseded` | 同 coalesce_key 的新状态评论取代旧 queued 行 | 新入队、旧行取消与各自事件同事务；executing 不适用。 |
| `dependency_failed` | queued 行的前置已 failed/cancelled，含多层级联 | 每条实际转移各一事件，与级联取消同事务；前置缺失只阻塞，不冒充失败。 |
| `approval_invalid` | forge 执行前复核失败，取消已认领动作 | 与取消同行提交；本动作已消费的合法重放不适用。 |
| `manual` | 用户或模块显式取消 queued/executing 行 | 沿用实际 `ActorCtx`；不等于撤销已经发出的 GitHub 请求。 |

只在实际转为 cancelled 时追加一次；终态重复取消不再发事件。`last_error.code` 可维持同一原因投影，但不把错误对象塞进事件；不增加 `cancelled_by`，身份由信封 actor 表达。取消不发 `outbox.failed`；临时错误仅回队退避，终态失败才发既有 `outbox.failed`（P-53）。当前 M0 `cancel` 只写占位原因而不发本事件，不能把上述推荐说成已实现。

```json
{"reason":"manual"}
```

**验收（契约与事务层分别验证）**

| 事件/路径 | 正例 | 关键负例 |
| --- | --- | --- |
| `enqueued` | 上例；逐一覆盖 14 个 kind 的既有强类型 payload；状态评论 revision 与 intent 相等 | 仅 `{revision:7}`；G-06 整体冒充 data；kind/payload 不符；target/intent/key 任一不符；缺 `coalesce_key`；必填字段为 null；加入 status/lease/expected_effects；重复入队多发事件 |
| `executing` | 首次 `{attempts:1,replay:false}`；同值 `{attempts:1,replay:true}` 的租约重放；限流退还后普通认领 | attempts 为 0；缺 replay 或为 null；重放增加 attempts；空 claim 发事件；事件提交但行更新回滚；以 attempts 去重吞掉再认领 |
| `cancelled` | manual 正例；合并、前置失败级联、审批无效各覆盖一个真实取消 | `{}`；`other` / `cancel_requested`；superseded 用于 executing；缺失前置被当失败；已消费的同动作重放误取消；终态重复发事件；取消代替临时重试或终态失败 |
| 恢复 | 消费审批后崩溃；预写效果后崩溃；远端成功、complete 前崩溃；从一致备份继续 revision | 单靠 enqueued 重建租约/消费状态；重放重复创建 PR；取消后将未知远端效果当成“未发生” |

确认者：入队与恢复 `F+G+E+S+C`；认领 `F+S+C`；取消 `F+G+E+C`。这些技术推荐统一送契约审查，不重新请求用户逐个选择字段，也不解冻运行时。

#### 2.2.5 Forge 两事件

**推荐规格，待契约审查。** 两事件仍为 `ContractUndefined`；以下字段、白名单和合成边界是收敛推荐，不是已批准契约。

##### 2.2.5.1 `forge.synced`

**信封**：`subject=prj_…`（固定项目级，避免 Issue 绑定多个交付时 subject 漂移）；`project_id` 必填且等于 subject；`correlation_id` 在能解析交付时等于该交付的 `delivery_id`，未绑定时为 `null`（`forge §7.5` 允许未绑定对象停放）。`data` 不重复 `project_id`，不新增 `delivery_id`；投递绑定由信封表达。`actor` 用 `{kind:"forge",id:"forge"}`。`causation_id` 为触发处理的入站事件或 `null`。

**`data` 字段**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `target` | 是 | `{type:"pr"\|"issue", number:u64, node_id:string}` | `number` 和 `node_id` 都必填；这是 `ForgeEventTarget` 的收窄，不表示 branch/installation 可用。`target` 必须属于信封项目，`node_id` 与回读对象一致。 |
| `source` | 是 | `webhook` / `reconcile`，复用 `ForgeEventSource` | `webhook` 只用于因该投递而更新快照；patrol、启动全量、投递恢复、强制回读与 `reconcile_now` 都用 `reconcile`。 |
| `changes` | 是 | `Change[]`，非空 | 每项 `{field, from, to}`；同一 `field` 至多一次；按本节字段表顺序排序。`from` 与 `to` 必须不同。 |
| `synced_at` | 是 | `Timestamp`，快照写入时间 | 等于应用本批变更后的快照 `synced_at`；不是 GitHub 时间戳。事件信封的 `ts` 不得早于 `synced_at`。若由入站 forge 事件触发，`synced_at` 不早于该事件 `occurred_at`（允许时钟容差）；reconcile、startup_sweep、recovery 等无入站事件路径不比较 `occurred_at`。 |

**`changes[].field` 白名单与值类型**

| `field` | 适用 target | `from` / `to` 类型 | 约束 |
| --- | --- | --- | --- |
| `state` | pr / issue | PR：`open` / `closed` / `merged`；Issue：`open` / `closed` | 不得跨 target 使用另一枚举。 |
| `state_reason` | issue | `completed` / `not_planned` / `null` | 这是事件层收窄；现有 `IssueRef.state_reason` 仍是 `Option<String>`，契约 PR 需对齐。`null` 表示没有关闭原因，是真实值；不是“未知”。 |
| `title` | pr / issue | `UntrustedText` | `source` 形如 `github:pr.title` / `github:issue.title`；正文按 `forge.events.max_text_bytes` 截断。 |
| `head_sha` | pr | `GitOid` | 完整 OID，不接受短 SHA。 |
| `base_ref` | pr | 非空字符串 | 分支名；不写 `refs/heads/` 前缀。 |
| `base_sha` | pr | `GitOid` | 完整 OID。 |
| `draft` | pr | bool | — |
| `mergeable_state` | pr | 非空字符串 | 保留 GitHub 原值（`clean` / `dirty` / `blocked` / `behind` / `unstable` / `unknown` 等）；不建封闭枚举。 |
| `in_merge_queue` | pr | bool | — |
| `checks` | pr | `CheckSummary` | `{passed, failed, pending: u32, required_ok: bool}`；不复用 check 正文。 |
| `review_state` | pr | `entities::ReviewState`：`none` / `approved` / `changes_requested` | 不是 `forge::ReviewState`（后者含 `commented`，只用于评审事件）。 |

排除项：`repo` / `number` / `node_id` 是身份，不进入 changes；`synced_at` 由顶层承载；`body` / `labels` / `reviewers` 不在 `IssueRef` / `PullRequestRef` 快照，不造字段。`forge.synced` 只覆盖 PR / Issue 快照；branch、installation 与项目级同步不在 M0 范围，分别由 drift 或既有 `forge.<kind>` 事件表达。

**示例**

```json
{
  "target": {"type":"pr","number":57,"node_id":"PR_kwDOabc123_example"},
  "source": "reconcile",
  "changes": [
    {"field":"mergeable_state","from":"clean","to":"dirty"},
    {"field":"checks","from":{"passed":12,"failed":0,"pending":1,"required_ok":true},"to":{"passed":12,"failed":1,"pending":0,"required_ok":false}}
  ],
  "synced_at": "2026-10-24T12:41:03.120Z"
}
```

**正负例**

| 正例 | 关键负例 |
| --- | --- |
| PR 合并状态与 checks 同批变化；Issue `state` 与 `state_reason` 同批变化；webhook 与 reconcile 各一条 | `changes` 为空；`from` 等于 `to`；未知 `field`；同一 `field` 出现两次；Issue 带 `head_sha`；PR 带 `state_reason`；`target` 缺 `node_id`；`type` 为 branch/installation；webhook 已解析交付却把 `correlation_id` 填成其他值；`synced_at` 与快照不一致；`title` 为裸字符串；`checks` 缺 `required_ok`；`review_state=commented`。 |

##### 2.2.5.2 `forge.drift_detected`

`data` 直接使用 §4 的 drift 记录，不再包一层 `{drift:…}`；具体字段、九种判别、合成边界与正负例见 §4。信封规则与 `forge.synced` 相同：`subject=prj_…`，`project_id` 必填，`actor={kind:"forge",id:"forge"}`；`correlation_id` 在 webhook 触发回读且发现 drift 时等于该投递的 `delivery_id`，否则可为 `null`。`delivery_id` 放在 `data`（可空），因为项目级 drift 没有单一交付。

确认者：`F+G+C`。

#### 2.2.6 预算与策略

**推荐规格，待契约审查。** 本节把 §2.1 的 #13、#14 收敛为可评审的推荐契约，不表示用户批准、契约冻结或已实现。§9.1 的已接受方向只是输入；字段、表、枚举与事务仍待契约审查。以下表名只说明持久职责，当前不存在，不能按已实现处理。

##### 2.2.6.1 `budget.charged`

**信封**

| 项 | 推荐规则 |
| --- | --- |
| `subject` | 只能是 `dlv_<ULID>` 或 `cnv_<ULID>`。`Subject::parse` 只接受 `<prefix>_<ULID>`，不接受 `cnv_…:YYYY-MM-DD`；日期放 `data.ledger.day`。`BudgetLedger::ledger_id()` 的 `cnv_…:YYYY-MM-DD` 是内部/业务键，不是事件 subject。 |
| `project_id` | 必填，不能为 `null`。 |
| `actor` | 引擎自有维度用 `engine`；有 P78 `ActorCtx` 时用 `ctx.actor`。直接调用用 `ctx.actor`，无上下文时用 `system` 且 `id:"models"`。不得新增 `models` / `policy` 两种 `ActorKind`。 |
| `causation_id` | nullable；无来源事件时显式写 `null`，不是省略字段。 |
| `correlation_id` | 账本实体 ID：交付为 `dlv_…`，会话为 `cnv_…`；不带日期。策略事件见 2.2.6.5。 |
| `data` | 不新增与信封重复的顶层 ID 字段。`data.ledger.id` 必须等于信封 `subject` 的实体 ID，`data.ledger.subject` 必须与信封前缀一致；日期只放会话台账的 `day`。 |

**账本对象**

复用 `LedgerRef`，二选一，不使用双 nullable 归属：

```json
{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"}
```

```json
{"subject":"conversation","id":"cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ","day":"2026-10-24"}
```

交付台账：`id` 必填，不带 `day`。会话台账：`id` 和 `day` 都必填；`day` 是首次派发准入时固定的 UTC 日，晚到结算回原账；排队跨日且尚未开始时释放旧预占并重新准入。无项目/交付/会话的调用不启用，不建 `unattributed` 台账。

**`data` 字段**

| 字段 | 类型 | 必填 | 规则 |
| --- | --- | --- | --- |
| `ledger` | `LedgerRef` | 是 | 见上。 |
| `dimension` | 枚举 | 是 | `tokens_by_tier` / `nodes` / `attempts` / `wall_time` / `rounds` / `forge_writes`。`attempts` 需新增 `BudgetDimension::Attempts`，当前枚举没有；未同步前该分支无效。 |
| `delta` | u64 | 是 | 实际增量；规则见下表。 |
| `used_after` | u64 | 是 | 同账本、同维度、同 tier 的事务内累计值；审计便利字段，不是权威来源。 |
| `source` | 判别联合 | 是 | 见下表。 |
| `tier` | 枚举 | 仅 token | `utility` / `planner` / `worker`；其他分支禁止出现。 |
| `usage` | `TokenUsage` | 仅 token | 四类 token 加 `requests`。 |
| `weighted_tokens` | u64 | 仅 token | 必须等于 `delta`。 |
| `weight_audit` | 判别联合 | 仅 token | 见 2.2.6.2。 |

公共字段不设顶层 `config_rev`；分支不适用字段省略，不用 `null` 表示未知；未结清不是事件。

**维度、单位与 `delta`**

| `dimension` | 单位 | `delta` 规则 |
| --- | --- | --- |
| `tokens_by_tier` | 加权 token | `>=0`。完整计量下允许 0；上游失败可有 `requests>0` 但四类 token 都为 0。 |
| `nodes` | 节点数 | 固定 1；无节点不写。 |
| `attempts` | 尝试数 | 固定 1；无尝试不写；无上限。 |
| `wall_time` | 整秒 | 用毫秒测量后**向上取整**；`0` 仅当测量值恰为 0。 |
| `rounds` | 轮次 | 固定 1；评审修订轮或自愈轮各一条。 |
| `forge_writes` | 成功写次数 | 固定 1；只在观察到 `outbox.succeeded` 后写。 |

`concurrency` 是槽位，不是累计消费，不写 `budget.charged`。

**`source`**

| `source.kind` | 必填身份 | 类型/前缀 | 适用 |
| --- | --- | --- | --- |
| `attempt` | `attempt_id` | `att_` + ULID | token、attempts、wall_time |
| `direct_call` | `call_id` | 裸 ULID，无前缀 | token |
| `node` | `node_id` | `nod_` + ULID | nodes |
| `review_round` | `workflow_id`、`round` | `wfl_` + ULID；`round` 为 u32，`>=1` | rounds |
| `heal_round` | `round_id` | 裸 ULID，无前缀 | rounds |
| `outbox_action` | `action_id` | `act_` + ULID | forge_writes |

`round` 的 0 不产生计费；`round_id` 不是任意字符串。`call_id`、`round_id` 都是既有 ULID 空间，不新增前缀。

**示例（token）**

```json
{
  "ledger": {"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},
  "dimension": "tokens_by_tier",
  "tier": "worker",
  "delta": 32600,
  "used_after": 123456,
  "source": {"kind":"attempt","attempt_id":"att_01JBX4Q9ZK3M8T5V2N7RHC6WYD"},
  "usage": {"input_tokens":18200,"output_tokens":2400,"cache_read_tokens":9000,"cache_write_tokens":1200,"requests":1},
  "weighted_tokens": 32600,
  "weight_audit": {
    "basis": "job_token",
    "config_rev": 18240,
    "weights": {"input":1.0,"output":5.0,"cache_read":0.1,"cache_write":1.25}
  }
}
```

```json
{
  "ledger": {"subject":"conversation","id":"cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ","day":"2026-10-24"},
  "dimension": "tokens_by_tier",
  "tier": "utility",
  "delta": 11062,
  "used_after": 21062,
  "source": {"kind":"direct_call","call_id":"01JBX5D2F4H6K8M0P2R4T6V8XZ"},
  "usage": {"input_tokens":6402,"output_tokens":820,"cache_read_tokens":5600,"cache_write_tokens":0,"requests":2},
  "weighted_tokens": 11062,
  "weight_audit": {
    "basis": "svc_records",
    "record_count": 2,
    "first_seq": 90211,
    "last_seq": 90212,
    "weight_revs": [
      {"config_rev":18240,"weighted_tokens":5531},
      {"config_rev":18241,"weighted_tokens":5531}
    ],
    "records_digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
  }
}
```

**示例（非 token）**

```json
[
  {"ledger":{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},"dimension":"nodes","delta":1,"used_after":5,"source":{"kind":"node","node_id":"nod_01JBX7A1B2C3D4E5F6G7H8J9K0"}},
  {"ledger":{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},"dimension":"attempts","delta":1,"used_after":7,"source":{"kind":"attempt","attempt_id":"att_01JBX4Q9ZK3M8T5V2N7RHC6WYD"}},
  {"ledger":{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},"dimension":"wall_time","delta":42,"used_after":184,"source":{"kind":"attempt","attempt_id":"att_01JBX4Q9ZK3M8T5V2N7RHC6WYD"}},
  {"ledger":{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},"dimension":"rounds","delta":1,"used_after":2,"source":{"kind":"review_round","workflow_id":"wfl_01JBX6C2D3E4F5G6H7J8K9M0NP","round":1}},
  {"ledger":{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},"dimension":"rounds","delta":1,"used_after":3,"source":{"kind":"heal_round","round_id":"01JBX6Y7Z8A9B0C1D2E3F4G5H6"}},
  {"ledger":{"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},"dimension":"forge_writes","delta":1,"used_after":9,"source":{"kind":"outbox_action","action_id":"act_01JBA1F3Q9TX2N7B5D8H0M4RVC"}}
]
```

##### 2.2.6.2 权重、去重、预留与结算

**权重审计**

| 字段 | 类型 | 必填 | 规则 |
| --- | --- | --- | --- |
| `basis` | 枚举 | 是 | `job_token` 或 `svc_records`。 |
| `config_rev` | u64 | `job_token` | 该 job token 的权重快照修订。 |
| `weights` | 对象 | `job_token` | 四项正数；不得缺项。 |
| `record_count` | u64 | `svc_records` | 已归集记录数。 |
| `first_seq` / `last_seq` | u64 | `svc_records` | 记录范围；零记录时省略。 |
| `weight_revs` | 数组 | `svc_records` | `{config_rev, weighted_tokens}`；同修订合并，和等于 `delta`。 |
| `records_digest` | `sha256:…` | `svc_records` | 覆盖 `{seq, weight_rev, weighted_tokens}` 的 JCS 摘要。 |

`job_token` 的单一 `config_rev` 是合法的，因为权重在令牌签发时只有一个快照；仍须保存权重值或摘要。`FinalUsage` 当前缺权重快照；推荐随 `FinalUsage` 持久化权重摘要，或从 job token 签发记录取回，不能只存 `config_rev`。`svc_records` 必须逐请求保留 `weight_rev`；不能用一个 `config_rev` 冒充多请求版本。`UsageRecord` 当前缺 `weight_rev`，这是契约增补点；晚到记录不按当前配置重算。

**去重**

规范投影（只含适用字段，缺省即省略，不用 `day?` 之类的伪数组）：

```json
{"v":"gantry/budget/v1","ledger":{"subject":"delivery","id":"dlv_..."},"dimension":"tokens_by_tier","tier":"worker","source":{"kind":"attempt","attempt_id":"att_..."}}
```

会话账本才加 `"day":"YYYY-MM-DD"`；非 token 维度省略 `tier`。`dedup_key = "sha256:" + lowercase_hex(sha256(JCS(projection)))`，`used_after` 不参与。

| 分支 | 身份 |
| --- | --- |
| attempt | `attempt_id`；多次 revoke 是重复。 |
| direct call | 全局 `call_id`；同一调用所有 `seq` 归集。 |
| node | `node_id`。 |
| attempts / wall_time | `attempt_id`。 |
| review round | `workflow_id` + `round`。 |
| heal round | `round_id`。 |
| forge writes | `action_id`；新 `epoch` 新 ID。 |

**预留**

推荐持久职责（当前不存在）：`budget_reservations` 以 `(ledger_key, source_kind, source_id)` 为主键；`source_kind` 为 `attempt` 或 `direct_call`，`source_id` 为 `attempt_id` 或 `call_id`。列含 `ledger_key`、`tier`、`amount`、`state`（`reserved` / `settled` / `released`）、`updated_at`。

- 同一 `source` 只允许绑定一个 `ledger_key` 和 `tier`；重复出现不同值即 fail-closed。
- attempt 的 `amount` 是 token 预占；direct call 没有 Egress job 封顶，`amount=0` 仅用于绑定 `call_id → ledger/tier`，不是预算盾牌。调用前仍按剩余额度检查。
- 派发前在事务内检查 `limit - used - reserved`；不足则不派发。
- 结算：`used += actual`，并**一次性** `reserved -= 原 amount`；实际小于预占的差额随这次释放，不再发负 `delta` 或二次 release。实际大于预占按实际记账，可越过上限。
- 未发送的预占释放，只在确证没有未知发送时允许：未签发令牌，或令牌已关闭且 `FinalUsage` 完整。否则保持 `reserved`。
- `forget` 只在 charge 事务提交后调用；失败重试，不回滚结算。

**`forge_writes` 的写入者**

`outbox.succeeded` 由 `forge` 在主事务中写；`engine` 作为消费者在**自己的事务**里追加 `budget.charged` 并推进 `engine` 消费游标，按 `source.action_id` 去重。不得要求与 `forge` 主事务同事务，也不得让 `forge` 直接写预算。`models` 只写 direct call；engine 只写自己的维度；没有第三个预算写者。

**执行终态与 `Attempt.usage`**

- 执行终态事实先写内部 pending 结算记录；这些事实不可改。
- 公开展示的 `attempt.finished`、`Attempt.status` 终态、`Attempt.usage` 和 `wall_time` charge 在 token 结算事务中一起提交。`Attempt.usage` 是最终 Egress 计量，不是 worker 自报或零占位。
- 不新增 `usage_state`，不把 `Attempt.usage` 改成可空。现有 `attempt.finished.usage` 是可选字段；本节建议 engine 生成的最终事件必须填值，这是新约定，不是对现有可选性的兼容解释。
- `incomplete`：不结算、不 charge、不零结清；保留预占，台账 `metering_blocked`，暂停新派发。执行终态已结束时，收集产物后可销毁沙盒，不占无限沙盒槽；不自动重跑，不自动抹账。控制面令牌和 pending 结算保留到人工处置。
- 保留期丢证据且未结算：保留预占、阻塞、人工处置；已结算则保留本地账，不改账。

**崩溃与零值**

| 崩溃点 | 恢复 |
| --- | --- |
| 预占提交前 | 重新决策。 |
| 预占后、发送前 | 确证未发送才释放。 |
| 发送后、关闭门禁前 | 关闭门禁并 drain，不猜用量。 |
| 门禁已关、inflight 未清 | 重试 drain；不返回固定 FinalUsage。 |
| FinalUsage 已持久、结算事务未提交 | 重放结算，去重键防重。 |
| 结算已提交、forget 前 | 重试 forget，不重复 charge。 |
| direct call 记录已收、游标未推进 | 按 `seq` 重放。 |
| 终结证明未取或记录缺失 | 不结算；分页末尾不是证明。 |
| 证据过期且未结算 | 保留预占、阻塞、升级；不零结清。 |

| 情形 | 处理 |
| --- | --- |
| 完整零 token，`requests>0` | 允许 `delta=0`；记录原请求数。 |
| 确定零请求 | 允许 `delta=0` 的审计事件，释放预占。 |
| 取消且计量完整 | 按实际结算。 |
| 取消且 `incomplete` | 保留预占并阻塞，不 charge。 |
| `capped=true` | 最终值仍可结算，不等于缺失。 |
| `404` / 记录过期 / 无下一页 | 不等于零。 |

##### 2.2.6.3 沙盒关闭与 drain 屏障

`revoke` 不得被解释为“Egress 已给终态”。推荐 job token 状态：`accepting` → `draining` → `closed`，状态持久化。

1. **同一串行点**：请求接受时，Egress 在同一事务/同一串行点登记持久 inflight 并检查门禁；关闭门禁也在该点串行执行。接受先赢则请求计入 inflight；关闭先赢则拒绝新请求。不得先回 200 再登记 inflight。
2. **关闭门禁**：`revoke` 先持久化 `draining`，立即拒绝新请求；已接受的 inflight 继续完成或中断。
3. **drain**：每个 inflight 完成时，在同一事务写用量记录并标记完结。所有 inflight 清零、计量持久化后，才计算并持久化 `FinalUsage`，置 `closed`，返回 200。
4. **超时/连接中断**：结果未知，调用方重试同一 `job_id`；已 `closed` 的幂等返回同一 `FinalUsage`。不得用超时或 404 推零。
5. **事务边界**：等待 drain 不能跨 DB 事务持有；状态机分多次短事务，HTTP 等待发生在事务之外。引擎拿到 `FinalUsage` 后才开结算事务。
6. **崩溃丢计量**：`incomplete=true`，不零结清，不自动重跑；保留控制面证据并升级。

`GET …/usage` 在 `accepting`/`draining` 只给当前快照；不是终结证明。

##### 2.2.6.4 B-01 推荐方案（方向已确认，字段待契约审查）

**状态**：2026-10-26 用户确认本方案方向及墓碑存储代价；下列字段、保留期与 HTTP 形状仍须契约审查。契约批准前不改变 revoke/usage 端点。

**必要性**：现有 `POST /control/v1/svc-tokens/{id}/revoke` 的 `{}` 只整令牌吊销；`GET /control/v1/usage?call_id=…` 只分页。两者都不能证明全局 `call_id` 的未知发送和迟到接收已结束。direct call 的“一调用一条结算”、释放和零判定因此没有可信证据。

**候选写语义**：只在既有 revoke 端点增加最小 call 作用域，不新增路由：

```json
{"scope":"call","call_id":"01JBX5D2F4H6K8M0P2R4T6V8XZ"}
```

`scope` 缺省或 `{}` 保持整令牌吊销；`scope:"call"` 时 `call_id` 必填，其他作用域禁止携带。路径 `svc_token_id` 是端点绑定，但关闭必须按全局 `call_id`；不得用每次调用轮换进程令牌规避该屏障。

**全局屏障与请求登记**

- Egress 维护全局 `call_id` 状态：`accepting` → `draining` → `closed`，以及跨所有 token 的 inflight 登记。
- 请求接受与 inflight 登记、门禁关闭在同一串行点；已关闭/正在 drain 的 call 拒绝新请求。
- 路径 token 关联不能只看已完成记录：token 必须至少有一条**已接受请求**（inflight 或 completed）；若全局没有任何已接受请求，才是零接收。全部请求都在其他 token 时，当前 token 不能关闭，除非用有关联的 token。
- 调用方 `models` 在首次发送前持久化：`call_id`、`ledger`、`tier`、已使用 token 集合、禁止继续发送状态。先持久化“禁止继续发送”，再发起 close；崩溃恢复关闭旧 `call_id`，不用新 ID 丢旧账；重试继续同一 call。
- `POST` 先将全局状态置 `draining`，等待所有 token 的 inflight 结束或中断，计量持久化后计算证明，置 `closed` 并返回 200。等待不跨 DB 事务。
- 超时/响应丢失：重试同一 POST；幂等返回同一证明。不得从已完成记录数或最大 seq 推零/推完整。

**只读终结证明**：`GET /control/v1/usage?call_id=…` 保持只读，响应增加可选 `terminal`（`items` 照旧）：

| `terminal` 字段 | 类型 | 必填 | 校验 |
| --- | --- | --- | --- |
| `state` | 枚举 | 是 | 只能 `closed`。 |
| `closed_at` | timestamp | 是 | 关闭持久化时间。 |
| `last_seq` | u64 | 是 | 最大已记录 seq；零接收为 `0`。 |
| `record_count` | u64 | 是 | 唯一 `seq` 数；必须等于实际记录数。 |
| `svc_token_ids` | string[] | 是 | 有已接受请求的 token；零接收为空数组。 |
| `usage` | `TokenUsage` | 是 | 汇总；必须等于记录逐项之和。 |
| `weighted_tokens` | u64 | 是 | 汇总；必须等于记录之和。 |
| `records_digest` | `sha256:…` | 是 | 覆盖全部 `{seq, weight_rev, weighted_tokens}`。 |
| `incomplete` | bool | 是 | `true` 表示证明不完整，不得结算。 |

完整性校验必须同时满足：唯一 `seq` 数等于 `record_count`，`last_seq` 等于最大 `seq`，`usage` 与 `weighted_tokens` 等于逐条之和，`records_digest` 等于重算值。缺任何记录、空页、仅最大 seq、仅记录数都不算证明。`incomplete=true` 时即使 `closed` 也不能结算。

**墓碑与保留期**

- 关闭后保留最小 `closed` 墓碑（`call_id`、`closed_at`，不含用量正文），不自动遗忘；否则同 `call_id` 可能被重新接收。
- 完整证明只在 usage 记录保留期（现提议 7 天）内可查；过期后空 `items` 不等于零，不结算、不新增 ack 接口。
- 24 h 自动遗忘只适用于 **job token**（`egress-control §4.2`）；svc usage 的保留期是另一条（`§4.5`，7 天提议），不得混用。
- 墓碑存储成本已随 B-01 方向一并确认；保留期与字段仍待契约审查。

##### 2.2.6.5 `policy.denied`

**信封与 actor**

- `actor`：使用 P78 实际调用 `ActorCtx.actor`；不得伪造 `policy` 或新增 `policy` kind。
- `decide_action` / `evaluate_patch` 已有 `ctx`；`decide_proposals` 当前没有 `ActorCtx`，推荐增加该参数，否则 proposal 分支不能诚实填写 actor。
- `subject`：action 有交付时为 `dlv_…`，否则 `prj_…`；patch/proposal 为 `att_…`。
- `project_id` 必填；`causation_id` nullable，显式 `null`；`correlation_id` 为 delivery ID，否则 attempt ID，否则 project ID。
- `policy` 自己写事件，使用当前配置快照的 `config_rev`；不要求 `ProposalDecision::Denied` 返回 `config_rev`。

**来源与字段**

| 字段 | 类型 | 必填 | 适用与规则 |
| --- | --- | --- | --- |
| `origin` | 枚举 | 是 | `action` / `patch` / `proposal`。 |
| `action_id` | `PolicyActionId` | `origin:"action"` | 20 个之一；patch/proposal 禁止出现。 |
| `kind` | `ForgeActionKind` | `origin:"action"` | 与 `action_id` 对应；patch/proposal 禁止出现。 |
| `target` | `PolicyTarget` | `action` / `patch` | 见目标判别；不能只用 `repo` 代替对象身份。 |
| `reason_code` | 字符串 | `action` / `proposal` | 只取本节原因码表；patch 不用。 |
| `reasons` | `PolicyDeniedReason[]` | `patch` | 非空，保留全部命中；每项见补丁 reasons 表。 |
| `proposal_id` | string | `proposal` | 与 `attempt_id` 一起去重。 |
| `proposal_kind` | `ProposalKind` | `proposal` | `open_pr` / `reply_review_thread` / `resolve_review_thread` / `escalate`。 |
| `target_handle` | string | `proposal` 可选 | 只复用已验证的 `Proposal.target` 规则；不是 `UntrustedText`，不额外设 512 上限。 |
| `subject` | `GitOid` \| `Sha256Digest` | 可选 | 这里是 **`data.subject`**，不是信封 `subject`；`GitOid` 用于 head，`Sha256Digest` 用于补丁；不得任意字符串。 |
| `config_rev` | u64 | 是 | policy 当前快照的修订；由 policy 直接写事件时取自已加载快照。 |

公共规则：`Held` / `NeedsApproval` / `Accepted` 不写；patch/proposal 不带 `action_id`；action 不带 `reasons`。`action_id` 为 20 个 `PolicyActionId` 之一，`kind` 为对应的 `ForgeActionKind`；条件伪动作仍保留 `action_id` 的伪动作名，`kind` 用底层的 `ForgeActionKind`。

**目标判别**

`PolicyTarget` 是推荐新增的事件投影类型，不是现有生产类型；目标必须来自 `ActionRequest.target`，不能从 `ActionFacts::Other` 猜。

| `kind` | 字段 | 用途 |
| --- | --- | --- |
| `pr` | `repo`、`number` u64 | merge/update/request_reviewers |
| `branch` | `repo`、`branch` | push/force/outside |
| `review_thread` | `repo`、`number` u64、`thread_node_id` | reply/resolve |
| `comment_target` | `repo`、`target`（`type` 为 `pr` 或 `issue`，另带 `number`）、`slot` | 状态评论；`target` 为 `{type, number}` |
| `comment_creation` | `repo`、`target`（`{type, number}`）、`client_ref` | 新建评论，不要求已创建 ID |
| `issue` | `repo`、`number` u64 | update/close/labels |
| `issue_creation` | `repo`、`client_ref` | 新建 Issue，不要求已创建编号 |
| `run` | `repo`、`run_id` u64 | rerun |
| `pr_creation` | `repo`、`head`、`base` | 新建 PR，不要求 PR 号 |
| `patch` | `repo`、`paths: UntrustedText[]`、`path_digest`、`rules[]` | 补丁拒绝 |

`paths` 线上是 `UntrustedText` 对象 `{text, source, truncated}`，不是裸字符串；`path_digest` 是 `Sha256Digest`，覆盖截断前的完整路径列表，去重不能用截断后的 paths。`target_handle` 复用已验证的 `Proposal.target` 字符串规则，不额外设 512 上限。

**动作目标映射**

| `action_id` | `kind` | `target` |
| --- | --- | --- |
| `merge_pr` | `merge_pr` | `pr` |
| `force_push` | `push_branch` | `branch` |
| `push_outside_prefix` | `push_branch` | `branch` |
| `resolve_review_thread_human` | `resolve_review_thread` | `review_thread` |
| `delete_branch` | — | 无；预留，无执行器 |
| `publish_release` | — | 无；预留，无执行器 |
| `change_repo_settings` | — | 无；预留，无执行器 |
| `push_branch` | `push_branch` | `branch` |
| `create_pr` | `create_pr` | `pr_creation` |
| `update_pr` | `update_pr` | `pr` |
| `upsert_status_comment` | `upsert_status_comment` | `comment_target` |
| `create_comment` | `create_comment` | `comment_creation` |
| `reply_review_thread` | `reply_review_thread` | `review_thread` |
| `resolve_review_thread` | `resolve_review_thread` | `review_thread` |
| `create_issue` | `create_issue` | `issue_creation` |
| `update_issue` | `update_issue` | `issue` |
| `close_issue` | `close_issue` | `issue` |
| `add_labels` | `add_labels` | `issue` |
| `request_reviewers` | `request_reviewers` | `pr` |
| `rerun_failed_jobs` | `rerun_failed_jobs` | `run` |

3 个预留项保留配置与拒绝语义，但没有当前执行路径：`delete_branch` 是破坏性删除，`publish_release` 是发布写，`change_repo_settings` 是仓库设置写；第一版均无执行器，不产生事件，不虚构调用。

**端口来源**：`ActionRequest` 需要新增 `target`（或等价字段），由入队方在入队前从尚未入队的请求 payload 投影；这是契约变更建议，本轮不改生产代码，也不要求已创建编号。当前 `ActionFacts` 不完整：`Other` 只有 `repo`，`ResolveReviewThread` 缺 thread id。未补 `target` 前，缺目标身份的动作 fail-closed，不能退化为 `repo`（否则同仓库不同 PR 会被合并）。

**补丁 `reasons`**

`PolicyDeniedReason` 是推荐新增的线形状，不是现有生产类型。`reasons: PolicyDeniedReason[]`，每项 `{code, path?: UntrustedText, rule_id?: string}`；非空，保留 policy 求值收集的全部命中，不因首个 deny 停止，也不漏原因。它是写入投影：`PatchReason.path` 是 `RepoPath`，写事件时转成 `UntrustedText(source:"sandbox:patch_path")`。

| `code` | 说明 |
| --- | --- |
| `policy.size` | 大小上限 |
| `policy.symlink` | 纵深防御；正常由 repo 机械拒绝 |
| `policy.submodule` | 纵深防御 |
| `policy.binary` | 纵深防御 |
| `policy.path_outside_allow` | 路径不在 allow |
| `policy.path_denied` | 命中 deny |
| `policy.path_protected:workflows` | 固定拒绝 |
| `policy.path_protected:<rule_id>` | 受保护规则；`<rule_id>` 是 `policies.paths` 键 |
| `policy.path_protected:file_mode_change` | OQ-P5 候选，未决 |
| `policy.secret` | 敏感内容 |
| `policy.path_invalid` | 纵深防御路径非法 |
| `policy.dotgit` | 纵深防御 `.git` 路径 |

`<rule_id>` 只来自配置键，不是任意开放串；未列出的码无效。正常机械拒绝仍只写 `patch.rejected`（P69）；policy 复查发现同类事实时可写 `policy.denied`，但不是机械路径的替代。

**原因码**

| `DenyReason` | 候选 `reason_code` |
| --- | --- |
| `ActionDenied` | `policy.action_denied` |
| `ApprovalDenied` | `policy.approval_denied` |
| `ApprovalConsumed` | `policy.approval_consumed` |
| `HeadMismatch` | `policy.head_mismatch` |
| `NoValidatedPatch` | `policy.no_validated_patch` |
| `PatchDenied` | `policy.patch_denied` |
| `MultipleApprovalsRequired` | `policy.multiple_approvals_required` |
| `ApprovalShapeUndefined` | `policy.approval_shape_undefined` |
| `ApprovalChainUnavailable` | `policy.approval_chain_unavailable` |

`policy §7.3` 的 `PatchNotApproved` 不在当前枚举，推荐新增 `PatchNotApproved`，候选 `policy.patch_not_approved`；不得静默折叠。`worker §9` 的 `policy.*` 只是 `failure_detail` 子码，不是完整动作码表。`file_mode_change` 仍是 OQ-P5 候选，不是已定决定。

**去重**

- action：`sha256(JCS({project_id, envelope_subject: subject, data_subject?: data.subject, origin, action_id, target, reason_code, config_rev}))`；`envelope_subject` 是信封 `subject`，`data_subject` 是 `data.subject`（可选）。
- patch：`sha256(JCS({project_id, envelope_subject: subject, data_subject: data.subject, origin, path_digest, rules, reasons_digest, config_rev}))`；`data.subject` 必填且是 `Sha256Digest`，不另造 `patch_sha256`；`reasons_digest` 覆盖全部 reasons，不用截断 path。
- proposal：`sha256(JCS({project_id, envelope_subject: subject, origin, proposal_id, proposal_kind, target_handle?, reason_code, config_rev}))`。
- 重复判定只保留一条；不同 reason/reasons 分别保留。

**正负例**

```json
{"origin":"action","action_id":"push_branch","kind":"push_branch","target":{"kind":"branch","repo":"acme/shop-api","branch":"gantry/42-fix-pagination"},"reason_code":"policy.action_denied","subject":"d7d018f867d6c5b78e82906386420d45a4d84486","config_rev":18240}
```

```json
{"origin":"patch","target":{"kind":"patch","repo":"acme/shop-api","paths":[{"text":"Cargo.lock","source":"sandbox:patch_path","truncated":false}],"path_digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333","rules":["lockfiles"]},"reasons":[{"code":"policy.path_protected:lockfiles","path":{"text":"Cargo.lock","source":"sandbox:patch_path","truncated":false},"rule_id":"lockfiles"}],"config_rev":18240,"subject":"sha256:2222222222222222222222222222222222222222222222222222222222222222"}
```

```json
{"origin":"proposal","proposal_id":"p2","proposal_kind":"reply_review_thread","target_handle":"thread-1","reason_code":"policy.approval_denied","config_rev":18240}
```

- 非法 `actor.kind = models` 或 `policy`：拒绝；直接调用用 `system{id:"models"}`，policy 用实际 ctx actor。
- `data.subject` 不是 GitOid/Sha256Digest：拒绝；不得用自由文本；不得与信封 `subject` 混用。
- `origin:"patch"` 携带 `action_id` 或使用裸字符串 paths：拒绝。
- `origin:"proposal"` 携带 `action_id`：拒绝；`target_handle` 只复用已验证 Proposal 规则。
- 3 个预留动作产生事件：拒绝。
- 机械拒绝补 `policy.denied`：拒绝；policy 复查同类事实可写，不算机械路径。
- 只写首个 deny、漏其余 reasons：拒绝；`reasons` 必须全量。
- settings/device 内部拒绝：不写 `policy.denied`。

**测试矩阵**

| 用例 | 期望 |
| --- | --- |
| direct call 两条不同 `seq`，同一 `call_id` | 一条事件，usage/token 求和。 |
| 重复投递同一 `seq` | 不重复累加。 |
| `requests=2` 但四类 token 全 0，完整计量 | 允许 `delta=0`。 |
| 真实重试同一 `call_id` 新 `seq` | 计入同一结算。 |
| 新 `attempt_id` | 新 charge。 |
| `nodes`/`attempts`/`rounds`/`forge_writes` 的 `delta=0` | 拒绝。 |
| 500 ms wall_time | `delta=1` 秒（向上取整）。 |
| wall_time 0 ms | `delta=0`。 |
| `actor.kind=models` / `policy` | 拒绝。 |
| proposal 无 ActorCtx | 阻塞，不伪造 actor。 |
| patch 多原因 | 一条事件含全部 reasons。 |
| B-01 缺任一记录 / 仅最大 seq | 不结算。 |

##### 2.2.6.6 仍需授权或阻塞

- B-01 方向已确认、字段待审：契约批准前 direct call 最终结算、释放和零判定仍阻塞。
- `decide_proposals` 缺 P78 `ActorCtx`：proposal 分支不能发事件；需加参数。
- `ActionRequest` 缺统一 `target`，`ActionFacts::Other`/`ResolveReviewThread` 缺对象身份；未补前不能发对应 `policy.denied`，不能退化为 `repo`。
- `DenyReason` 缺 `PatchNotApproved`；`file_mode_change` 仍是 OQ-P5 候选。
- `UsageRecord` 缺 `weight_rev`，`FinalUsage` 缺权重快照；权重审计需补契约。
- `BudgetDimension` 缺 `Attempts`；需枚举同步，不新增配置键。
- 现有 schema 仍拒绝新 data，持久表未实现；`Attempt.usage` 保持非空，公开终态延迟到结算。`incomplete` 的长期等待需人工处置，不自动重跑或抹账。

确认者：`E+M+X+C+U`（预算）与 `P+C+U`（策略）。本节是推荐规格，待契约审查，不是用户批准、契约冻结或实施完成。

### 2.3 通用候选约束

以下只是候选评审原则，不是已冻结规则：

- 已定义形状的 `data` 使用 `additionalProperties: false`；未知字段拒绝。
- 来自 GitHub、沙盒或设备的文本使用 `UntrustedText` 或明确的截断字符串；事件 `data` 不出现 secret 值。
- 事件 `subject` 已承载的实体 ID 不重复；需要跨实体关联时再增加字段。
- 判定类事件带 `config_rev`；纯生命周期事件不带。
- 跨字段一致性（例如 `delivery_id` / `conversation_id` 互斥）在 Rust 校验；schema 能用 `if/then` 表达的也表达。
- 优先复用现有实体和枚举；不新增 ID 前缀，除非现有类型无法表达。

### 2.4 其余事件的推荐收敛

以下选择用于减少草案分支，不视为用户逐项确认。优先复用信封与现有类型；没有依据的 ID、长度值、枚举不补猜。

| 事件 | 推荐 | 契约审查仍须完成 |
| --- | --- | --- |
| `guardian.triaged` | 普通分流与 `round_closed` 使用同一事件的判别分支；`event_ref` 区分 Forge 事件、尝试结束和结算检查，不能都强塞成外部事件 | 按 `guardian §7.3 / §7.10` 列封闭字段集；冻结触发引用、分支必填项和轮 ID。不得为草案新增 `rnd_` 前缀 |
| `guardian.heal_requested` | 先冻结 `HealTask` / `ProblemSpec` 序列化，再冻结事件中的摘要和引用；不带材料正文 | 明确 `request_digest` 覆盖范围、`batch_included` 元素及 round_id；保留调用 HealPort 前落盘的顺序（`guardian §7.10`） |
| `guardian.escalated` | `class` / `reason_code` 复用守护封闭表；推荐显式 `suppressed_duplicate`，避免把缺 inbox_id 当作抑制成功 | `Deduped` 的 inbox 引用、可选性和全局归属须明确；不能忽略守护字段表与抑制说明之间的缺口 |
| `approval.invalidated` | 推荐 6 个原因：head 变化、PR 关闭/合并、交付放弃、尝试替代、发起方取消；head 变化带新旧 head，空分支只带 `reason` | 补齐 `pr_closed` / `pr_merged` 的端口映射；`SubjectReplaced` / `Other` 不产生事件，待契约审查是否新增分支 |
| `approval.expired` | 推荐 `{}`，用信封定位审批，依赖审批日志恢复前态 | 不新增 `previous_status` / `expired_at`；区分实体 expires_at 与实际记录过期的时刻 |
| `approval.denied` | 推荐 `{reason?: string}`，由信封记录主体、操作者与时间，不重复 device_id | 备注按不可信文本处理；受 64 KiB 请求体门禁，不借 settings 的 256 KiB 上限 |
| `forge.synced` | §2.2.5.1 已给出 `target` 仅 PR/Issue、`changes` 按字段强类型与完整正负例；不允许任意 JSON 兜底或复制正文 | 契约审查核对字段白名单、可空性和首次快照；项目级变化仍不在 M0 范围，不从 PR 示例推导扩围 |
| `policy.denied` | 动作策略拒绝与补丁策略拒绝分支审计；机械拒绝仍归 `patch.rejected`（P-69）；动作目标按 20 个动作映射 | 完整目标来源、条件伪动作对应关系、subject 与原因词表待审；不套用不完整的 ApprovalTarget，不使用 `other` 或伪造 push_branch 来隐藏缺口 |

验收须覆盖每个最终判别分支、信封与 data 一致性及未知字段拒绝；不因选了最小形状省略实际适用的校验，也不为凑测试增加冗余字段。

## 3. 第二组：`ConfigView.injection`

### 3.1 阻塞表

| 项 | 现有依据 | 缺失决策 | 推荐/确认状态 | 确认者 |
| --- | --- | --- | --- | --- |
| `ConfigView.injection` 非空形状 | `egress-control §4.6` 示例只给出 `"injection": {}`，但已要求 Egress 复核 V10/V11/V13；`§6` 已规定复核失败响应；`config §8.1` 定义 settings 侧字段，`§2.4` 定义 `SecretRef`；当前 Rust `empty_injection` 与 schema `maxProperties: 0` | 完整视图字段映射、校验顺序与错误位置 | 引用结构及四项选择已确认；字段表、校验与正负例见 §3.2，待独立契约 PR 固化。settings 自身 `SecretRef` 读写契约不改。 | `X+C+U` |

**现有约束，不是待决项**：`egress-control §4.6` 已要求 Egress 对视图做 V10/V11/V13 防御性复核（规则见 `config §11`）。复核失败返回 `422 config_invalid`，附 `errors[]`，其中 `pointer` 相对视图；保留旧视图，`health = degraded`。`egress-control §6` 再次规定 `config_invalid`，不能与其他字段校验的 `validation_failed` 二选一。这些规则不需要用户重新批准；§3.2 是据此收敛的推荐规格，仍待独立契约 PR 固化及验证。

### 3.2 推荐规格

引用结构及下列四项选择已确认：字段名与语义沿用 settings 的 `hosts` / `kind` / `header`，但视图使用独立的 `InjectionView` / `InjectionCredentialView`；`credential` 仅含 `{secret_ref}`，不复用接受 `present` 的 `SecretRefDoc`。以下 JSON 与字段表是推荐规格，完整契约仍需独立契约 PR 审查，不是生产契约：

```json
{
  "injection": {
    "npm_private": {
      "hosts": ["npm.corp.example"],
      "kind": "bearer",
      "credential": { "secret_ref": "registry.npm.corp" }
    },
    "cargo_private": {
      "hosts": ["cargo.corp.example"],
      "kind": "header",
      "header": "x-api-key",
      "credential": { "secret_ref": "registry.cargo.corp" }
    }
  }
}
```

#### 3.2.1 字段表

| 字段 | 必填 | 类型与来源 | 约束 |
| --- | --- | --- | --- |
| `injection` | 是 | `{id → InjectionView}`；空对象合法 | `id` 是配置键，不是实体 ID，不新增前缀。推荐 `^[a-z0-9][a-z0-9._-]{0,63}$`，与 `SecretName` 同一规则；依据是该 id 会成为 `/r/<job_token>/<injection_id>/…` 的路径段（`egress-control §4.6`），必须 URL 安全。大小写敏感、不得含 `/`、不得为空。 |
| `injection.<id>.hosts` | 是 | `string[]`，非空 | 每项是 DNS 主机名（无协议、端口、路径）；允许一层前缀通配 `*.example.com`；去重并保留顺序。必须被某个分组的 `hosts` 覆盖（V13 原文）；推荐再收紧为被 `profiles` 引用的分组覆盖。不得含 GitHub 域名族（V10 原文）；IP 字面量、裸 `*`、多层通配由 `egress` G5 与运行时规则禁止。 |
| `injection.<id>.kind` | 是 | `bearer` / `header` | 决定注入头。 |
| `injection.<id>.header` | 条件 | 非空字符串 | `kind=bearer` 时必须省略（包括 `null`）；`kind=header` 时必须提供。使用 HTTP field-name token 语法；大小写不敏感拒绝逐跳/框架头：`Host`、`Content-Length`、`Transfer-Encoding`、`Connection`、`Keep-Alive`、`Proxy-Authenticate`、`Proxy-Authorization`、`TE`、`Trailer`、`Upgrade`。`header` 模式下也拒绝 `Authorization`（推荐增补；V13 原文只列逐跳/框架头），避免与 `bearer` 的隐式头重叠。 |
| `injection.<id>.credential` | 是 | `InjectionCredentialView`：`{secret_ref}` | 只含 `secret_ref`；满足 `^[a-z0-9][a-z0-9._-]{0,63}$`。不得含 `value`、`present` 或字符串引用；视图永不输出 secret 值。`SecretRefDoc` 接受 `present`，因此不能直接复用为视图类型。 |
| `present` | — | 不出现在 ConfigView | settings 的 GET 响应有 `present`；下发视图只含引用。 |
| 模型 provider | — | 不重复出现在 `injection` | 由 `models.providers` 自动派生为只读注入项；`injection` 只放私有包仓库。 |

`kind=bearer` 的隐式头名按 `Authorization` 计；`kind=header` 使用显式头名。`(host, header)` 唯一性按大小写不敏感的最终头名判定，因此同一 host 上不能有两个 `bearer`，也不能有重复的自定义头名。

#### 3.2.2 校验与错误

校验按以下顺序执行，前一步失败不继续推断后一步：

1. **形态**：对象、必填字段、类型、未知字段拒绝（`additionalProperties:false`）。
2. **V10**：`injection.hosts` 不得含 GitHub 域名族（V10 原文）。
3. **`egress` G5**：不得含 IP 字面量、裸 `*`、多层通配。
4. **V13**：每个 host 必须被某个分组覆盖（推荐再要求该分组被 `profiles` 引用）；`(host, header)` 唯一；`header` 不是逐跳/框架头。
5. **引用**：`credential` 只校验 `secret_ref` 格式；不解析、不回显 secret 值。

V11 是契约要求 Egress 对视图 `providers` 执行的复核，不因本节只讨论 `injection` 而免除；当前核心校验尚未实现 V11，属于契约 PR 后的实现工作。

错误映射：

| 情形 | 响应 | 位置 |
| --- | --- | --- |
| 字段缺失、类型错误、未知字段、`header` 与 `kind` 不匹配、`credential` 为字符串或含未知字段 | `422 validation_failed`（settings 写入）或 `422 config_invalid`（视图复核） | `errors[].pointer` 相对被校验对象；视图复核时相对视图，如 `/injection/npm_private/header`。 |
| 视图中的 `credential` 含 `value` 或 `present` | `422 config_invalid` | settings 写入仍允许 `value` / `present`；只在只读下发视图中拒绝。 |
| host 不在分组、`(host, header)` 重复、逐跳/框架头、GitHub 域名族/IP/裸 `*` | `422 config_invalid` | 同上；保留旧视图，`health=degraded`。 |
| `secret_ref` 格式不合法 | `422 validation_failed` / `422 config_invalid` | 指向 `…/credential/secret_ref`。 |
| `secret_ref` 合法但 secret 未激活 | 视图仍可应用 | 状态响应 `unresolved_secret_refs` 列出引用（不列值），`health=degraded`；不是 `config_invalid`。 |

`credential_missing` 仍只用于签发令牌时 provider 凭据未激活（`egress-control §6`），不用于视图校验。

#### 3.2.3 正负例

| 正例 | 关键负例 |
| --- | --- |
| `npm_private` 的 `bearer` 示例；`cargo_private` 的 `header` 示例；两个不同 host 各一个 `bearer` | `injection` 不是对象；id 含 `/`、大写或为空；`hosts` 缺失/空/含 `api.github.com`/`127.0.0.1`/`*`/`**.example.com`/`https://npm.corp.example`/`npm.corp.example:443`；host 不在分组；`kind` 缺失或未知；`kind=bearer` 带 `header`（包括 `null`）；`kind=header` 缺 `header` 或 `header` 为空、为 `Host` / `Content-Length` / `Transfer-Encoding` / `Authorization`；`credential` 缺失、为字符串、带 `value` 或 `present`、`secret_ref` 非法；未知字段；同一 host 两个 `bearer`；同一 `(host, header)` 大小写不同但重复；视图含 secret 值。 |

#### 3.2.4 里程碑与现有约束

- 非空 `injection` 的启用阶段：M0 定义契约与校验类型，不启用代理能力；M1 继续拒绝非空；M2 才启用私有包仓库代理。
- 不新增 `injection_rev`；沿用整个 `ConfigView.rev`。
- 现有生产类型与 schema 仍拒绝非空 `injection`；只有契约批准并另获实施授权后才可能改变。当前 `ConfigView.injection` 用 `#[serde(default)]` 允许缺省为空对象；§3.2.1 将其列为必填是候选收紧，须在契约 PR 明确是否保留缺省。
- `settings` 侧读写契约不改：写入的 `credential` 仍可带 `value` 完成写入/轮换，读取可带 `present`；下发视图只含 `secret_ref`。
- 视图新增 `InjectionView` / `InjectionCredentialView`，不复用 `EgressInjection.credential`（即 `SecretRefDoc`，其反序列化接受 `present`）。settings 的 `EgressInjection` 与 `EgressInjectionPatch` 保持不变。`InjectionView` 与 `InjectionCredentialView` 都必须使用 `deny_unknown_fields` / `additionalProperties:false`，在反序列化和 schema 两层拒绝 `present` / `value`。
- settings GET 的 `present` 与当前 `SecretRefDoc.Serialize` 只输出 `secret_ref` 的差异是既有实现缺口，不因视图只含 `secret_ref` 而改变 settings 契约；由实现 PR 修复，不在本草案把两者混为一谈。
- “凭据只发往列出的主机”及复核失败行为是已有安全约束。本轮未接线，控制面行为须由后续另获授权的实现验证，不能用 serde/schema 校验冒充 HTTP 服务验证。

确认者：`X+C+U`。

## 4. 第三组：drift 9 种 `kind`

### 4.1 推荐公共信封

`forge-actions §7.4` 给出 drift 目录、`head_diverged` 示例和同时合成的事件；`forge.md §7.7` 给出每种 drift 的比较来源；`crates/gantry-core/src/ports/forge.rs::DriftKind` 有 9 个变体；`schemas/forge/drift.json` 与 `schemas/events/forge.drift_detected.json` 当前拒绝一切。以下是把这些依据收敛成的推荐规格，仍待契约审查。

`forge.drift_detected.data` 直接等于 drift 记录，不再包一层 `{drift:…}`；`schemas/forge/drift.json` 与事件 schema 共享同一份 `$defs`，避免双份定义。只保留现有 9 个 `kind`，未来新增只追加。

| 字段 | 必填 | 类型 | 约束 |
| --- | --- | --- | --- |
| `kind` | 是 | 9 值封闭枚举 | `pr_state_diverged` / `head_diverged` / `effect_missing` / `orphan_effect` / `issue_state_diverged` / `review_state_diverged` / `merge_state_diverged` / `default_branch_changed` / `installation_unreachable`。 |
| `delivery_id` | 是 | `DeliveryId` 或 `null` | 可空性按 §4.3 表；不是省略字段。`null` 只用于项目/安装级 drift。 |
| `target` | 是 | `DriftTarget` 判别联合 | 见 §4.2；不得用 `ForgeEventTarget` 代替。 |
| `expected` | 是 | 按 `kind` 判别的对象 | 不得整体为 `null`；分支内字段按 §4.3。 |
| `actual` | 是 | 按 `kind` 判别的对象 | 不得整体为 `null`；分支内字段按 §4.3。 |
| `resolution` | 是 | 封闭枚举 | M0 只有 `github_wins`；未来新增只追加，不能静默接受其他值。 |
| `synthesized_events` | 是 | 字符串数组，可为空 | 元素是不带 `forge.` 前缀的 forge 事件 kind；必须是该 `kind` 的允许集合（§4.3）的子集；空数组表示只记录、不合成。 |
| `detected_by` | 是 | 封闭枚举 | `patrol` / `startup_sweep` / `recovery` / `on_demand` / `webhook`。`recovery` 覆盖投递恢复；`on_demand` 覆盖 `reconcile_now` 与强制回读。 |

### 4.2 `DriftTarget` 判别

`DriftTarget` 是推荐新增的事件投影类型，不是现有生产类型；不能直接复用 `ForgeEventTarget`，因为后者没有 `repo` / `branch` 字段，且 `number` / `node_id` 可选，无法精确表达 drift 的对象身份。

| `target.type` | 字段 | 适用 `kind` |
| --- | --- | --- |
| `pr` | `repo: RepoSlug`、`number: u64` | `pr_state_diverged`、`head_diverged`、`review_state_diverged`、`merge_state_diverged`、`effect_missing`（状态评论 / PR 效果） |
| `issue` | `repo: RepoSlug`、`number: u64` | `issue_state_diverged`、`effect_missing`（状态评论） |
| `branch` | `repo: RepoSlug`、`branch: string` | `effect_missing`（分支效果） |
| `repo` | `repo: RepoSlug` | `default_branch_changed` |
| `installation` | `installation_id: i64` | `installation_unreachable`；与 `Project.installation_id` 的 `i64` 一致 |

`target` 必须来自回读对象或 Outbox 效果索引，不能只凭 `repo` 猜对象。带 `repo` 的 target 的 `repo` 必须属于信封 `project_id`；`installation` target 没有 `repo`，按项目分别发事件。

### 4.3 九种 `kind` 的判别

| `kind` | `target` | `delivery_id` | `expected` | `actual` | 允许的 `synthesized_events` |
| --- | --- | --- | --- | --- | --- |
| `pr_state_diverged` | `pr` | 必填 | `{state:"open"}` | `{state:"closed"\|"merged", merge_commit_sha?: GitOid}`；`state=closed` 时不得带 `merge_commit_sha` | `["pr_closed"]` 或 `["pr_merged"]`；`merged` 但缺 `merge_commit_sha`、或回读 DTO 未定义时 `[]`；`[]` 始终允许。 |
| `head_diverged` | `pr` | 必填 | `{head_sha:GitOid, basis_action:ActionId}` | `{head_sha:GitOid, pushed_by:ForgeActor\|null}` | `["pr_synchronized"]` 仅当 `pushed_by` 非 `null` 且非 self 归因；否则 `[]`。 |
| `effect_missing` | `pr` / `issue` / `branch` | 必填 | 见下方三分支 | 见下方三分支 | `[]`（M0）；状态事件由 `pr_state_diverged` 或 webhook 路径负责，避免双发。 |
| `orphan_effect` | `pr` / `issue` | 可空 | `{outbox_action:null}`；保留字段以明确没有对应 Outbox 行 | `{object:"comment"\|"pull_request"\|"issue", github_id:u64, node_id:string, marker:UntrustedText}` | `[]`；不自动认领或补做远端对象。 |
| `issue_state_diverged` | `issue` | 必填 | `{state:"open"\|"closed", state_reason?:IssueStateReason\|null}` | 见下方三分支 | `["issue_closed"]` 或 `["issue_reopened"]`；删除/转移时 `[]`；`issue_closed` 在回读 DTO 未定义时 `[]`，`issue_reopened` 不受影响；`[]` 始终允许。 |
| `review_state_diverged` | `pr` | 必填 | `{review_state:ReviewSnapshotState}` | `{review_state:ReviewSnapshotState}` | `[]`；汇总状态不足以确定具体 review。 |
| `merge_state_diverged` | `pr` | 必填 | `{in_merge_queue:true}` | `{in_merge_queue:false, reason?:UntrustedText\|null}` | `["pr_dequeued"]` 仅当 `PullRequestReadback.head_sha` 可用；否则 `[]`。 |
| `default_branch_changed` | `repo` | 必须 `null` | `{default_branch:string}` | `{default_branch:string, renamed_from?:string\|null}` | `renamed_from` 是旧默认分支名，不是 `RepoSlug`；`[]`；`repo_changed` 需要 `delivery_guid`，尚无对账幂等来源。 |
| `installation_unreachable` | `installation` | 必须 `null` | `{reachable:true}` | `{reachable:false, code:InstallationUnreachableCode, http_status?:u16\|null}` | `[]`；不可达不等于安装被删除。 |

`ReviewSnapshotState` 复用 `entities::ReviewState`：`none` / `approved` / `changes_requested`。`InstallationUnreachableCode` 是推荐新增的封闭枚举，不是现有生产类型：`token_exchange_failed` / `repo_not_accessible` / `permission_denied` / `transport_error`。

**`effect_missing` 的三个判别分支**

| `expected.effect` | 允许的 `target` | `expected` 其余字段 | 允许的 `actual` |
| --- | --- | --- | --- |
| `status_comment` | `pr` / `issue` | `action_id:ActionId`、`render_digest:Sha256Digest` | `{observation:"missing"}` 或 `{observation:"content_changed", render_digest:Sha256Digest}`。 |
| `branch` | `branch` | `action_id:ActionId` | `{observation:"missing"}`。 |
| `pull_request` | `pr` | `action_id:ActionId`、`state:"open"` | `{observation:"missing"}` 或 `{observation:"state_changed", state:"closed"\|"merged"}`。 |

`render_digest` 只在 `status_comment` 分支出现；`content_changed` 的 `actual.render_digest` 是回读正文的摘要，必须与 `expected.render_digest` 不同。`state_changed` 只用于 `pull_request`；PR 关闭不能写成 `{observation:"missing"}`。`missing` 表示确证对象不存在（排除权限与临时错误），不是“读不到”。

**`issue_state_diverged` 的三个判别分支**

| `actual.observation` | 字段 | 合成 |
| --- | --- | --- |
| `state` | `state:"open"\|"closed"`、`state_reason?:IssueStateReason\|null` | 确证关闭/重开且事件必需字段可从回读补齐时，分别合成 `issue_closed` / `issue_reopened`；否则 `[]`。 |
| `deleted` | 无额外字段 | `[]`；删除需要确证，不得缩成 `closed`。 |
| `transferred` | `to_repo:RepoSlug`、`new_number:u64` | `[]`；转移保留目标身份。 |

`head_diverged` 完整示例（字段名沿用 `forge-actions §7.4`）：

```json
{
  "kind": "head_diverged",
  "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
  "target": {"type":"pr","repo":"acme/shop-api","number":57},
  "expected": {
    "head_sha": "f2c03d84a858156b0121130fd1ec2f391aa90865",
    "basis_action": "act_01JBA1H5T7XA3P9C2E6J8M1QWD"
  },
  "actual": {
    "head_sha": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
    "pushed_by": {"login":"alice","github_id":1234567,"type":"user"}
  },
  "resolution": "github_wins",
  "synthesized_events": ["pr_synchronized"],
  "detected_by": "patrol"
}
```

示例在原 `forge-actions §7.4` 上只补了 `target.repo`，因为 drift 事件是项目级事件，顶层没有 repo 字段；其余字段名保持不变。`ForgeActor.association` 是可选字段，示例省略合法。

### 4.4 一致性与合成规则

- `kind` 与 `target.type` 必须匹配 §4.3；`expected` / `actual` 按 `kind` 的判别分支解析，不能只校验“是对象”。每个已定义的分支对象使用 `additionalProperties:false`，未知字段拒绝。
- `delivery_id` 非 `null` 时，`target` 必须属于该交付单元的 PR / Issue / branch；`default_branch_changed` 与 `installation_unreachable` 的 `delivery_id` 必须是 `null`。
- `synthesized_events` 必须是 §4.3 允许集合的子集；元素不得带 `forge.` 前缀，不得出现未定义的事件 kind。
- 合成事件必须与 `actual` 一致：`resolution=github_wins` 时不得合成与 GitHub 事实相反的事件；缺少事件必需字段时只记录 drift，不列该事件。具体来源见 §4.4.1。
- `detected_by=webhook` 且能解析交付时，信封 `correlation_id` 必须等于该交付 ID；投递未绑定时为 `null`。其他来源可为 `null`。
- `orphan_effect` 的 `marker` 是来自正文的 `UntrustedText`；标记不是执行授权，事件不得把 `marker` 当机器可信字段。
- `effect_missing` 只用于有成功 Outbox 效果的对象。同一 PR 的关闭/合并若已满足 `pr_state_diverged`，只发 `pr_state_diverged`，不再发 `effect_missing(pull_request)`；`effect_missing` 仍保留正文哈希变化与对象缺失两类。
- `review_state_diverged` 只记录汇总状态差异；没有具体 `review_id` / actor 时不合成 `review_submitted` / `review_dismissed`。
- `default_branch_changed` 可以更新 Project 快照，但 M0 不合成 `repo_changed`，因为该事件含 `delivery_guid` 且尚无对账幂等来源；不得伪造 webhook GUID。
- `installation_unreachable` 只证明访问失败，不证明安装被删除或仓库对象不存在；`installation_changed` 需要另有安装状态证据。一个安装影响多个项目时，按受影响项目分别发事件，`project_id` 取该项目；不把跨项目安装状态合并成一条事件。

#### 4.4.1 合成事件字段来源

`drift.expected` / `actual` 只保存判别所需的最小事实；合成事件的其余字段由 forge 从回读对象、快照或本地事件日志补齐。下表是唯一允许的来源，不得用默认值或猜测补齐。任一必需字段不可得时，对应事件不进入 `synthesized_events`。

| 合成事件 | 必需字段 | 来源 | 不可得时 |
| --- | --- | --- | --- |
| `pr_closed` | `number`、`head_sha`、`closed_by` | `number` 取 `target.number`；`head_sha` 取 `PullRequestReadback.head_sha`（回读当时的实际 head）；`closed_by` 取 `PullRequestReadback.closed_by` | `[]` |
| `pr_merged` | `number`、`merge_commit_sha`、`head_sha`、`base_ref`、`merged_by`、`via_merge_queue` | `number` 取 `target.number`；`merge_commit_sha` 取 `actual`；`head_sha`、`base_ref` 取 `PullRequestReadback`；`merged_by`、`via_merge_queue` 取 `PullRequestReadback` | `actual.merge_commit_sha` 缺失或 DTO 字段缺失时 `[]` |
| `pr_synchronized` | `head_sha`、`previous_head_sha`、`head_epoch`、`base_ref` | `head_sha` 取 `actual`；`previous_head_sha` 取 `expected`；`head_epoch` 取本地事件日志中已追加 `forge.pr_synchronized` 的最大 `head_epoch` + 1，无历史时为 `1`（`COALESCE(max,0)+1`，`forge §7.5`）；`base_ref` 取 `PullRequestReadback.base_ref` | `[]` |
| `issue_closed` | `number`、`state_reason`、`closed_by` | `number` 取 `target.number`；`state_reason` 取 `actual` 并转换为 `Option<String>`；`closed_by` 取 `IssueReadback.closed_by` | `[]` |
| `issue_reopened` | `number` | 取 `target.number` | 总是可得 |
| `pr_dequeued` | `number`、`head_sha`、可选 `reason` | `number` 取 `target.number`；`head_sha` 取 `PullRequestReadback.head_sha`；`reason` 取 `actual` 并转换为 `Option<String>` | 缺 `head_sha` 时 `[]` |

**回读来源缺口（推荐契约增补）**：现有 `ForgeReader::pull_request` / `issue` 只返回 `PullRequestRef` / `IssueRef`，两者都没有 `closed_by`、`merged_by`、`via_merge_queue`。因此上表这些列在 M0 现有端口上不可得，按“不可得时 `[]`”处理，不能从事件默认值或本地投影伪造。推荐在 `PR-C3` 与实现契约中新增回读 DTO，由回读适配器填充 actor、head 与 merge queue 事实：

```text
PullRequestReadback { head_sha: GitOid, base_ref: String,
                      closed_by: ForgeActor?, merged_by: ForgeActor?,
                      via_merge_queue: bool? }
IssueReadback       { closed_by: ForgeActor? }
```

`?` 表示按路径填充，缺失即不可合成。也可以给 `PullRequestRef` / `IssueRef` 增加同名可选字段，但不能用默认 actor 或猜测值补齐。推荐给 `ForgeReader` 增加 `pull_request_readback` / `issue_readback`，保留现有 `pull_request` / `issue` 的快照读取签名；若改现有方法返回 readback DTO，必须在契约 PR 同步全部调用方。在契约定义前，`pr_closed` / `pr_merged` / `issue_closed` 的 `synthesized_events` 为 `[]`。`pr_synchronized` 的 `head_epoch` 来自本地事件日志，`base_ref` 来自 `PullRequestReadback`；`pr_dequeued` 的 `head_sha` 也来自 `PullRequestReadback`，避免拿更新前的快照合成旧 head。

`head_diverged` 的 self 归因：`actual.pushed_by` 是 GitHub actor；把 `Project.bot_user_id`（`i64`）与 `ForgeActor.github_id`（`u64`）显式转换后比较，相等或命中 `forge_effect_index` 时为 self，不合成 `pr_synchronized`；非 self 才合成。转换越界或失败按非 self 处理，不因比较错误而抑制 drift。`pushed_by=null` 不合成。

### 4.5 正负例与冻结前验收

| `kind` | 正例 | 关键负例 |
| --- | --- | --- |
| `pr_state_diverged` | 回读 DTO 可用时：本地 open、回读 closed，合成 `["pr_closed"]`；回读 merged 且 `merge_commit_sha` 可用，合成 `["pr_merged"]`。DTO 未定义时 `synthesized_events=[]` | `target.type=issue`；`expected.state=closed`；`actual.state` 缺失；merged 却合成 `pr_closed`；`delivery_id=null`；DTO 未定义却合成。 |
| `head_diverged` | `expected.head_sha` 与 `actual.head_sha` 不同，`pushed_by` 非 self，合成 `["pr_synchronized"]` | 两个 head 相同；`pushed_by=null` 却合成 `pr_synchronized`；`basis_action` 不是 `act_`；短 SHA；`delivery_id=null`。 |
| `effect_missing` | 状态评论正文哈希变化的 `content_changed`；分支 404 确证的 `missing`；PR 不再是 open 的 `state_changed` | `status_comment` 用 `state_changed`；`branch` 用 `content_changed`；PR 关闭写成 `missing`；同一个 `render_digest` 同时作 from/to；与 `pr_state_diverged` 双发。 |
| `orphan_effect` | bot 作者 + 标记、效果索引无行，`actual.object=comment` | 只凭正文标记判 orphan；`marker` 为裸字符串；`expected.outbox_action` 非 null；把标记当执行授权。 |
| `issue_state_diverged` | 关闭、重开各一条 `state` 分支；删除、转移各一条独立分支；回读 actor 可用时关闭分支可合成 `issue_closed` | 删除写成 `state=closed`；转移丢失 `to_repo`/`new_number`；权限不足的 404 当 `deleted`；`delivery_id=null`；回读 actor 缺失却合成 `issue_closed`。 |
| `review_state_diverged` | 快照 `approved`、回读 `changes_requested` | 合成 `review_submitted`；`review_state=commented`；`review_state=none` 被当作未知；`target.type=issue`。 |
| `merge_state_diverged` | 本地在 queue、回读不在，`PullRequestReadback.head_sha` 可用，合成 `["pr_dequeued"]` | 回读仍 `in_merge_queue=true`；`reason` 填猜测值；DTO 缺 `head_sha` 却合成 `pr_dequeued`。 |
| `default_branch_changed` | 默认分支 `main` → `master`，`renamed_from="main"` | `target.type=branch`；合成 `repo_changed`；`delivery_id` 非 null；`renamed_from` 与 expected 矛盾。 |
| `installation_unreachable` | 换令牌失败、`code=token_exchange_failed` | `code` 为未知值；用 drift 宣称安装已删除；`target.type=repo`；`delivery_id` 非 null；`http_status` 为非整数。 |

公共负例：未知 `kind`；`expected` / `actual` 整体为 `null`；`resolution` 非 `github_wins`；`detected_by` 未知；`synthesized_events` 含未定义事件或带 `forge.` 前缀；未知字段；`target` 缺 `repo` / `number` / `branch` / `installation_id`；`target` 与 `kind` 错配。

冻结前验收还须覆盖：权限不足的 403、普通超时、未排除权限问题的 404 不当作删除/关闭；未知操作者时 `pushed_by=null` 不合成；Issue 删除/转移不缩成 closed；对象仍存在但状态变化与正文变化分别落入 `state_changed` / `content_changed`；缺少合成事件必需字段时 `synthesized_events=[]`。

### 4.6 观测证据与文档冲突

`forge-actions §7.2–§7.4` 的当前对账范围不因表中出现仓库/安装而自动扩展为全局巡检。`forge-actions §2.6` 已区分临时错误、权限不足和排除可见性问题后的目标缺失。403、普通超时、未排除权限问题的 404 不能被用作“已删除/已关闭”的事实。访问失败可以触发已有停写/上报或不可达处置，但不能编造另一个交付事实；不得借此修改 I4 或放宽对账范围。

**仍待契约 PR 消解的冲突**：`forge-actions §7.2` 已把正文哈希差异列入 `effect_missing`，`§7.4` 又以“对象不存在”描述并包含“PR 被关”，`forge §7.7` 同时列存在性与正文哈希。§4.3 在同一个 kind 下按事实建分支，保留三类场景；与 `pr_state_diverged` 的重复记录规则按 §4.4 处理。`forge §7.5` 规定仓库/安装事件含 `delivery_guid`、不经对账合成，`forge-actions §7.4` 与 `forge §7.7` 却要求合成它们；§4.3 保留差异检测，但在事实充分且对账幂等来源定义前，`repo_changed` / `installation_changed` 的 `synthesized_events` 为 `[]`。同样，`forge-actions §7.4` 对 `pr_closed` / `pr_merged` / `issue_closed` 的合成承诺，在 §4.4.1 的回读 DTO 定义前由 `synthesized_events=[]` 承接；契约 PR 必须补齐 DTO 或回写该边界。这些差异须由独立契约 PR 回写原文档，不能以接口优先级直接删掉已列场景。

确认者：`F+G+E+C`。

## 5. 继续延期：`ResultBundle` 与四个纯示例/部署片段

这些项目**不计入**本文三组生产协议阻塞，也不应被用来解冻任何事件或 schema。它们继续留在 `fixtures/contracts/unverified.json` 的现有分类中。

| 项 | 依据 | 当前处理 | 为什么不计入阻塞 | 何时再议 |
| --- | --- | --- | --- | --- |
| `ResultBundle` | `worker-protocol §5.2`，标 `[提议]`；清单由服务端生成，`Attempt.result_ref` 指向其哈希 | 继续 `contract-undefined`；不新增类型 | 它是服务端内部结果库清单，不是 worker 产出，也不是 M0 当前跨模块契约；M0 只要求 `Attempt.result_ref` 为内容寻址引用 | M1 需要结果读取/保留期时，单独提案 |
| `G-13` egress stage 示例 | `egress-control §4.4` 的暂存示例 `request_id` 实际为 26 字符，但含 ULID 禁止的 `L` / `U`；现有 gap / unverified 登记的“25 位”说明有误 | 继续 `example`；不作为实现通过证据 | 原文示意错误，不是新的协议形状；本轮只纠正本草案说明 | 文档清理 PR 修正原示例与登记，并重新生成、验证提取夹具 |
| `G-15` Message 示例 | `api §4.5` 的 `Message.id` 后缀为 27 字符，不是合法 26 字符 ULID | 继续 `example`；不作为实现通过证据 | 原文示意错误，不是新的协议形状 | 文档清理 PR 修正示例，并重新生成、验证提取夹具 |
| `github_ref` + `expected_effects` 组合 | `forge-actions §4.2` 把两者放在一个示例里展示 | 继续 `example`；不新增组合类型 | 不是单一生产类型；实际字段分属 `OutboxAction` 的 `expected_effects` 与 `github_ref` | 需要时补 `OutboxAction` 的字段级示例 |
| App Manifest 片段 | `forge-actions §9.6` 供 `deploy` 使用 | 继续 `example`；不纳入 Gantry 协议 schema | 是 GitHub App 的部署输入，不是 Gantry 的线上协议类型 | 部署文档单独维护 |

补充：

- 这五项不进入本文 §2 / §3 / §4 的冻结范围；不要因为“文档里有示例”就生成生产 schema。
- `ResultBundle` 与 `forge-actions §7.4` 的 drift 示例不同：drift 是生产事件 `forge.drift_detected` 的 `data` 候选，属于第三组；`ResultBundle` 不是。
- 四个 `example` 不需要负例；`ResultBundle` 继续由 `unverified.json` 的 `contract-undefined` 记录，而不是新增负例。

## 6. 建议的契约 PR 与实现写集

### 6.1 独立契约 PR（只改文档）

建议拆成 3 个契约 PR。每个 PR 只做一件可独立验证的事；不要与实现、schema 生成或 Mock 修改混在一起。下表是 2026-10-26 获授权创建的写集；授权只覆盖创建待审 PR，不批准契约、不合并、不实施。

**仅改文档也必须保留提取基线**：`crates/gantry-xtask/src/fixtures.rs::DOCUMENTS` 固定提取 `docs/interfaces/` 下的 `api.md`、`config.md`、`egress-control.md`、`forge-actions.md`、`worker-protocol.md`。开围栏行号决定夹具 `id` 与文件名；`manifest` 还记录 `source`、`line`、上文最近的 `#` 标题（`context`）、原始正文的 SHA-256 与字节数。`main.rs::check_drift` 逐字节比较夹具与 `manifest`，并拒绝多余夹具。因此，即使 JSON 值没变，只改这些文档的行号或标题也可能失败，不能要求夹具不动却忽略漂移。

优先采用**独立补充文档 + 原文末尾引用**：原五份受提取文档只允许在文件末尾追加没有 `json` 围栏的章节与引用；原有字节、围栏正文、开围栏行号、上文标题 `context` 全部不变，也不新增 `json` 围栏。其他 `data-model` / 模块文档只同步说明和引用，不另定义字段。补充文档尚不在现有五文档提取范围，须直接检查其内容，不能声称旧夹具已覆盖它们。

| PR | 目标 | 写集（仅文档） | 明确不写 |
| --- | --- | --- | --- |
| `PR-C1` 事件 data | 冻结除 drift 外 13 个事件的 `data` 与适用负例；维护 14 事件目录 | 拟新增 `docs/interfaces/event-data.md`，承载 §2 中除 `forge.drift_detected` 外的事件细节及经裁定的结算语义；`docs/data-model.md` §5.2 事件目录、`docs/modules/guardian.md` §7.10、`docs/modules/policy.md` §7.3 / §7.6 / §7.7、`docs/modules/forge.md` §7.5 / §7.7 / §7.8、`docs/modules/store.md` §6.3 / §7.6、`docs/modules/engine.md` §7.9 / §11、`docs/modules/models.md` §7、`docs/interfaces/worker-protocol.md` 末尾替代范围引用。`forge.drift_detected` 在目录与 `forge.md §7.7` 中只写“由独立 drift 契约定义”，不写链接；不物理改 `config.md` / `egress-control.md` | `schemas/**`、`fixtures/**`、`crates/**`、`tools/mock-api/**`、`apps/**`；`data-model.md` 的 drift 行定义 |
| `PR-C2` `ConfigView.injection` | 冻结非空注入视图的字段映射，沿用 §3.1 既有复核约束 | 拟新增 `docs/interfaces/config-view-injection.md`，补全 `egress-control.md` §4.6 的注入视图细节；仅在 `docs/interfaces/egress-control.md`、`docs/interfaces/config.md` 末尾追加适用范围与引用（对应 config §8.1 / §8.3 / §11，不改 settings 语义） | 同上；`event-data.md`、`forge-drift.md` |
| `PR-C3` drift | 冻结 9 种 drift 的公共信封、`expected` / `actual` 判别与 `forge.drift_detected.data` | 拟新增 `docs/interfaces/forge-drift.md`，承载 `forge-actions.md` §7.2–§7.4 的 drift 形状、事件 data 与经裁定的合成边界；仅在 `docs/interfaces/forge-actions.md` 末尾追加适用范围与引用。不改 `docs/modules/forge.md`（其 §7.7 引用由 `PR-C1` 写入）；`data-model.md` 不改。 | 同上；`data-model.md`、`docs/modules/forge.md` |

补充：

- 14 个事件中的 `forge.drift_detected` 由 `PR-C3` 冻结；`PR-C1` 只在事件目录中保留“由独立 drift 契约定义”的文字引用，不写链接、不定义 `data`，避免两个 PR 重复定义同一个形状。
- 三份写集互不共享文件：`PR-C1` 改事件目录、`data-model.md` 与模块说明（含 `forge.md §7.7` 的文字引用），`PR-C2` 独占 `config.md` / `egress-control.md` 末尾，`PR-C3` 独占 `forge-actions.md` 末尾；`PR-C3` 不改 `forge.md`。若实际排版要求共享文件，停止并合并为 1 个契约 PR，不拆成互相依赖的多个 PR。
- 每份补充及原文末尾引用须列明适用章节、字段与替代范围。补充获批后，范围内原有未定项或候选形状由该补充替代；范围内旧 JSON 示例在实现阶段机械同步前只作历史候选，不是另一套契约。范围外规则及更高优先级的决策、不变式不变。原五份文档需要更新的 JSON 示例留到实现阶段按已批准字段同步，不在那时重新决定语义。
- 如果用户要求一次审核，也可以合并为 1 个契约 PR，但建议保留 3 个独立提交/章节，避免事件、注入和 drift 的决策互相掩盖。
- `docs/decisions.md` 只在用户要求把确认结果登记为“已确定”时修改；不要把本文的候选自动写成硬决策。
- 契约 PR 的验收不生成 schema；直接检查补充文档的 JSON、链接、格式及字段对照，另以现有 `schemas --check` 证明旧基线未漂移，不能把它当作新字段验证。既有检查不删、不跳过，门禁仍为 0 failure、0 warning，运行时仍 fail-closed。
- 若上述排版不能保持旧提取基线，停止并标 `NEEDS_DECISION`，另提最小写集方案；本文不授权扩大到夹具、manifest、注解或实现文件。

### 6.2 后续实现写集（契约批准且另获实施授权后）

| 区域 | 预计文件 | 工作 |
| --- | --- | --- |
| `core` 事件 | `crates/gantry-core/src/events.rs`、`crates/gantry-core/tests/events.rs`、`crates/gantry-core/tests/schema_regressions.rs` | 增加事件 data 结构、`validate_event_data` 分支、跨字段一致性、正负例 |
| `core` egress | `crates/gantry-core/src/egress.rs`、`crates/gantry-core/tests/*` | 在 `egress.rs` 新增 `InjectionView` / `InjectionCredentialView` 并替换 `empty_injection`；按批准字段实现语义校验，沿用 V10/V11/V13；保留 `Debug` 脱敏；serde/schema 结果不代替控制面 HTTP 行为验证 |
| `core` forge/drift | `crates/gantry-core/src/forge.rs`、`crates/gantry-core/src/ports/forge.rs`、`crates/gantry-core/src/entities.rs` | 给 `DriftKind` 增加载荷类型；实现 9 种 `expected` / `actual` 判别；新增 `PullRequestReadback` / `IssueReadback`（放 `ports/forge.rs`）及 `ForgeReader::pull_request_readback` / `issue_readback`，或给 `PullRequestRef` / `IssueRef` 增加可选 actor / merge queue 字段（`entities.rs`）；缺来源时不合成并返回 `[]` |
| `core` 设置 | `crates/gantry-core/src/settings/resource.rs` | 保持 settings 的 `EgressInjection` / `EgressInjectionPatch` 不变；`InjectionView` 归属 `egress.rs`，不复用 `SecretRefDoc`（接受 `present`），避免 settings 与 egress 循环依赖 |
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

- 本轮只修改 `docs/m0-contract-proposal.md` 和根目录 `NEXT-STEPS.md`；`docs/milestones.md`、`docs/collaboration.md` 的前轮修改及其他已有工作保持原样。未跟踪文件也须与修订前副本作全文差异检查，不能只依赖 `git diff`。
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
- 契约获批只使补充成为对应范围的权威文档，运行时继续按 §7 拒绝；未获批字段仍是候选，不得当作已批准契约。

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
| B1：`ConfigView.injection` 非空 | 拒绝；现有负例 `config-view-non-empty-injection` | 仍拒绝 | M0 类型/校验实现后，合法非空通过类型校验，非法非空按 B2 拒绝；M1 控制面仍拒绝非空，M2 才启用私有包仓库代理 | serde、schema、`Debug` 三者一致；后续控制面另验 M1 拒绝与 M2 接受合法非空，不把类型校验当作代理已启用 |
| B2：`ConfigView.injection` 坏形状 | 无细分 | 仍拒绝 | 覆盖对象类型、未知字段和既有 V10/V11/V13；按最终字段映射覆盖缺 `hosts` / `credential`、未知 `kind`、`header` 与 `kind` 不匹配（含下述已确认规则）、`credential.value` 等适用负例 | 不泄漏 secret 值；`Debug` 不输出注入内容；后续控制面实现另验 `422 config_invalid`、`errors[]` 相对视图 pointer、保留旧视图与 `health = degraded` |
| C1：drift 9 种 | 全部拒绝；`drift.json` 与事件 schema 拒绝一切 | 仍拒绝 | 9 种各有正例；按 §4 的推荐契约直接校验整个 `forge.drift_detected.data`，drift schema 与事件 schema 共享 `$defs` | 9 个 `kind` 都能被识别；未知 `kind` 拒绝；`target` 与 `kind` 错配拒绝 |
| C2：drift 坏形状 | 无细分 | 仍拒绝 | 必测非对象与未知字段；按最终契约覆盖未知/缺 `kind`、缺 `expected` / `actual`、分支错配、错误 `target`、非法 `resolution` / `detected_by` / `synthesized_events` 等全部适用规则 | 公共信封与每种判别分支都覆盖；无相应规则须说明不适用原因 |
| D：跨契约混淆 | `outbox.enqueued` 仍拒绝；drift 仍拒绝 | 仍拒绝 | 按最终形状验证 G-06 信封不能替代 `outbox.enqueued.data`（完整动作快照即使复用字段也不等于该信封）；drift 拒绝裸 `kind` 字符串；若 `policy.denied` 冻结动作 target 判别，则覆盖误用 `ApprovalTarget` 的负例 | 防止 G-06、ApprovalTarget、ResultBundle 误用；按最终契约判定，不把候选当硬规则 |
| E：延期项 | 5 项继续 skip/example | 不变 | 不变；不为它们生成生产 schema | `unverified.json` 仍保留 5 项；没有新增生产类型 |

负例细节建议（各族附加例子均按最终契约条件适用，不预先冻结候选）：

- **事件 A2**：对象类型和 `unknown_field` 必测；逐事件列出 `missing`、`enum`、判别分支、`consistency` 的适用规则并全部覆盖。没有相应规则就说明“不适用”，不为满足固定数量增加字段。例如最终选 `approval.expired = {}` 时无缺必填、枚举或一致性负例；`approval.denied = {reason?}` 没有必填字段，若也没有枚举/一致性要求，则这三项不适用，但仍须验证 `reason` 出现时的类型等实际约束。判别联合若有分支必填字段，覆盖“判别值正确但分支字段缺失”；若判别值封闭，覆盖未知值。
- **各事件族的附加例子**：按推荐规格，覆盖 `attempt.progress` 的非法 `type`、`seq` 非递增、超长 `detail`；`guardian.*` 的未知 `class` / `action` / `reason_code`；`approval.invalidated` 的 `head_changed` 缺新旧 head、非 head 分支错配；`outbox.*` 的 `revision` 用于非状态评论动作、`outbox.executing` 缺 `attempts` / `replay`、`outbox.cancelled` 缺 `reason` 或原因不在封闭词表；`forge.synced` 的未知 `field` / `source`；`policy.denied` 的 `target.kind` 与 `action_id` 不匹配。`forge.drift_detected` 与 C2 共用。`budget.charged` 按 §2.2.6 的 `LedgerRef` 判别测试缺归属、交付分支带 `day`、会话分支缺 `day`、台账与信封 subject 错配及 source ID 错配；§9.1 已确认无归属调用暂不启用，不再把它列为待选默认。直接调用的 `call_id` 重放/补记不得重复扣账；沙盒按收尾后稳定 `FinalUsage` 验证一次结算的幂等、晚到补记与跨日归属，不重复扣账或因晚到丢用量。上述推荐仍须契约批准；若审查改变字段，同步本矩阵，不沿用已舍弃的空对象候选。
- **injection B2**：既有 V10/V11/V13 的安全规则不得删减；按最终视图映射构造 host 不在分组、GitHub 域名/IP 字面量等禁止主机、provider 主机覆盖缺失、重复 `(host, header)`、逐跳/框架 header 等负例。按已确认组合规则，覆盖 `kind = bearer` 带 `header`（包括 `null`）、`kind = header` 缺头名的拒绝。所有错误都不能回显凭据值；按已确认引用结构，`credential` 必须为仅含 `secret_ref` 的对象，字符串引用、`present` 及 `value`（包括 `null`）均须拒绝，不为凑路径新增其他形状。
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

## 8. 冻结前审查清单

三个方向、四项注入选择及 §9.1 六项取舍方向均已确认，不再重复询问；其余条目由契约维护者按本文推荐完成契约审查并送审，不要求用户逐个决定字段名。未获批字段仍是候选，不得当作已批准契约；若细化发现必须改变已确认方向，再提出新的取舍。模块负责人和本文均不能代为批准契约。

1. **总范围**：本文件只是草案，维持 §7 的拒绝行为，不做生产 schema、Rust 或 Mock 接线。
2. **事件族**：覆盖全部 14 个 `data`，不把某项接近可冻结误当作已批准。`guardian.escalated` 的抑制表达按 §2.4 送审。
3. **守护事件**：核对普通分流与 `round_closed` 的分支、触发引用、封闭词表及轮 ID；详见 §2.2.2 / §2.4。
4. **治疗请求**：先审定 `HealTask` / `ProblemSpec` 序列化，再冻结摘要、引用及 `request_digest` 覆盖范围。
5. **审批事件**：核对失效的所有适用原因及非 head 主体；最小过期/拒绝形状与信封分工按 §2.4 送审。
6. **Outbox 事件**：完整入队意图方向已确认；按 §2.2.4 字段来源与恢复责任表审定包装、一致性及认领/取消审计，不以入队事件替代主表备份。
7. **同步与 drift**：同步字段按 §2.2.5.1 的类型与白名单核对；drift 按 §4.2 / §4.3 / §4.4 逐种审定身份、事实和合成字段，按 §9.1 已确认方向固化范围与系统事件合成规则。
8. **预算**：沙盒稳定 `FinalUsage` 一次结算与直接调用分工不重选；按 §2.2.6、§9.1 已确认方向补齐权重审计、归属字段、结算/遗忘协议与崩溃补记，经独立契约 PR 固化为权威文本。
9. **策略拒绝**：按 §2.4 核对 20 个动作、条件伪动作和补丁拒绝；主体及原因必须能追溯，不用 `other` 或虚构动作兜底。
10. **注入视图**：§3.2 已给出字段映射、校验顺序、错误位置与正负例；仍需契约审查确认。settings 读写契约和 V10/V11/V13 不改，类型校验与控制面启用分开验收。
11. **延期项**：保持 §5 五项现有登记，不以本轮草案关闭缺口或生成类型。
12. **PR 拆分**：沿用 §6.1 三份独立补充的建议；拟议写集包含经裁定的结算与合成语义回写。PR 已获授权创建；契约批准与实现仍须单独进行。
13. **尝试进度**：按 §2.2.1 的白名单投影推荐核对 `warn` 范围、采样序号、文本与时间语义，不转发工具正文，不驱动预算或终态。

确认后，契约维护者再把答案回写到对应文档；本文不自动改变任何生产契约或 schema。

## 9. 建议状态与冻结前条件

2026-10-26 用户确认前三行的三个方向及 §3.2 的四项注入视图选择，随后答复“接受”，确认 §9.1 六项推荐方向。下表区分已确认方向与剩余候选；字段仍须经 §8 审查，不因方向确认改变权威契约、运行时和 §7 的拒绝行为。

| 主题 | 现有依据 | 建议与确认状态 | 仍缺的字段或验证条件 |
| --- | --- | --- | --- |
| Outbox | [data-model.md](data-model.md) 的 `OutboxAction`；[forge.md](modules/forge.md) §7.8、P-61 | **已确认方向**：完整入队意图，保留评论 `revision`。本轮补齐 §2.2.4 字段来源与恢复责任表，推荐 `action` 子集、认领 `{attempts, replay}`、取消 `{reason}`；具体字段未批准。 | 审定身份、可空性、派生值一致性与原因词表；覆盖去重、租约重放、审批已消费与远端成功后崩溃。入队日志不替代主表备份，不用 G-06 替代 data。 |
| `budget.charged` | [egress-control.md](interfaces/egress-control.md) §3.1 / §4.2–§4.5；[engine.md](modules/engine.md) §7.9；[models.md](modules/models.md) §7.3 / §7.8 | **已确认方向**：沙盒收尾后按稳定 `FinalUsage` 一次结算；直接调用由 models 记账，引擎不重复扣账；§9.1 第 1–4 项取舍方向已确认。 | 按已确认方向审定统一 source、维度、归属及版本表达和完成证据；验证真实重试不漏计、重复记录不多计、取消/崩溃补记和跨日，不能以稳定值承诺掩盖 `incomplete`。 |
| `ConfigView.injection` | [config.md](interfaces/config.md) §2.4 / §8.1；[egress-control.md](interfaces/egress-control.md) §4.6；[milestones.md](milestones.md) M2 的 `exec` 泳道 | **2026-10-26 已确认引用结构及四项选择**，详见 §3.2；settings 自身 `SecretRef` 读写契约不改。§3.2 已给出字段映射、校验顺序、错误位置与正负例，仍待契约审查。 | 保持 V10/V11/V13、`422 config_invalid`、保留旧视图与 `health = degraded`，后续实现另验。M0 类型校验与 M1/M2 控制面启用边界按 §6.4 分别验收；当前仍按 §7 拒绝，不新增配置开关、路由或错误码。 |
| drift（部分方向已确认） | [forge-actions.md](interfaces/forge-actions.md) §2.6、§7.2–§7.4；[forge.md](modules/forge.md) §7.5 / §7.7 | §9.1 第 5–6 项的事实分支与有条件合成方向已确认；§4 推荐共用记录、data 直接承载，仍待契约审查，不因方向确认获批。 | 按 §4.2 / §4.3 / §4.4 逐种审定身份、判别字段、未知值与去重来源。权限/可见性失败不当删除，PR 关闭不当对象消失，Issue 删除/转移不缩成 closed。 |
| `attempt.progress`（未确认） | [worker-protocol.md](interfaces/worker-protocol.md) §7.1–§7.4、§10.1 A4 | §2.2.1 推荐白名单投影 `stage/tool/usage`，保留 worker 序号并允许采样跳号，不转发工具正文，不驱动预算或终态。 | 审定 warn、时间及文本边界；验证丢行/乱序、伪造完成与用量、文本注入。已有 200 字符限制不扩借给其他字段。 |
| 守护、策略及其余事件（未确认） | 本文 §2；[guardian.md](modules/guardian.md) §7.10；[policy.md](modules/policy.md) §7.3 / §7.6 / §7.7 | §2.4 推荐封闭分支与信封复用；治疗请求先定上游类型；不以 `ApprovalTarget` 或 `other` 假装覆盖。 | 核对轮 ID、抑制表达、非 head 审批、同步字段与全部策略目标；各分支适用负例齐全后再送契约审查。 |

### 9.1 六项取舍确认记录

2026-10-26 用户答复“接受”，确认下表六项推荐方向。状态为**方向已确认，待契约固化**；本表保留当时的冲突依据、取舍及代价，未采用的替代方案仅供追溯，不再请求同一方向的确认。具体字段、完成证据、事务与恢复协议仍须审查；不是完整契约批准，不自动生效。

| # | 原冲突或缺口（权威文档待回写） | 已确认方向 | 代价或未采用的替代方案 |
| --- | --- | --- | --- |
| 1 | 权重时点：`config §2.6` 按记账时，`egress-control §4.5` 按 job 签发快照 / svc 请求时 | 以 Egress 为实际请求算出的加权值结算，晚到不重算；同调用多请求不强塞一个配置版本 | 热改权重不追溯已发生请求；若选记账时重算，须同时重定义封顶、对账与版本审计，不能只改事件字段 |
| 2 | 会话跨日与无归属：已有 UTC 日台账，但首次绑定时点未定；models OQ-3 暂用两种归属都空 | 首次派发前按原子准入固定业务键和 UTC 日，排队跨日若重新准入则先释放未使用预留；开始后的重试/补记沿用原账。无归属调用在其额度与审计规则定前不启用，不虚构全局台账 | 跨日进行中的调用继续占原日预算；需要全局调用时另定额度，不默认无上限放行 |
| 3 | 计量不可用、自报降级、`incomplete` 与并发预留（`models §7.3 / §7.8`、`engine §7.9`） | 不以自报或零值结清；保留未结清预留，暂停受影响台账新增派发，继续对账或转人工处置；共享台账原子预留，不无证据承诺“最多超支一次调用” | 牺牲计量故障时的可用性，缺失记录可能需要人工裁定；若允许估算后继续，必须单独定义风险上限与修正语义，不能静默采用 |
| 4 | 每调用一次结算与逐记录记账冲突；取消、轮换后仍可能有在途记录；保留期会截断补查 | 按 seq 去重并持久化归集，确认同 call_id 全部请求终结后结算；终结证据、游标事务、跨令牌关联和遗忘条件一起冻结。取消不免除实际费用 | 需要可恢复的归集与完成证据，现有查询分页不足以证明完成；若改为逐记录扣账，须显式调整“一调用一条事件”，不能 call_id 去重吞掉后续真实费用 |
| 5 | `effect_missing` 在接口内同时指对象缺失、PR 关闭和正文哈希不符 | 同一 kind 按事实建分支，保留三类已列场景；与 pr_state_diverged 的重复记录规则单独冻结 | 判别字段稍多，但不丢场景；若缩为对象不存在，须明确其他两类归属，不能删除现有承诺 |
| 6 | 系统事件：`forge §7.5` 禁止对账合成且需 delivery_guid，接口 §7.4 与模块 §7.7 又要求合成 repo_changed / installation_changed | 保留已列差异检测；仅在所需事实充分且有定义好的对账幂等来源时合成。不可达只证明访问失败，不能推断安装被删；不伪造 webhook GUID | 必须同步合成分支与去重契约；若不支持合成，须明确用 drift、快照更新和既有停写/上报承接哪些消费者，不能静默漏通知 |

后续按这些方向完成 §8 的字段与冻结条件审查，不把其他候选一并标成已确认；§6.1 的三份契约 PR 已获授权创建，但尚未批准。方向确认不批准完整字段；契约批准后实施仍需单独授权，不启动 M1 或协作扩展。
