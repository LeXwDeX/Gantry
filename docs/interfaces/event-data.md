# 事件 data 契约

> **状态：已批准（2026-10-27，用户统一批准）。** 原为送审草案；正文中「推荐规格，待契约审查」「候选」等措辞是送审时的原文，批准范围即正文所写内容，不另改字段或语义。
>
> 本文件只定义下列 13 个事件的 `data` 候选规格，供契约审查使用。它不修改生产契约、schema、fixture、Rust、Mock 或前端，也不表示任何字段、枚举或一致性规则已经冻结。运行时按 `core` 事件目录的实际落地情况校验：已落地 data 校验的事件按本契约校验，尚未落地（`ContractUndefined`）的仍按 fail-closed（失败闭合）拒绝。
>
> 本文件的来源、理由与取舍记录是 [docs/m0-contract-proposal.md](../m0-contract-proposal.md) §2。这里只把该节中 13 个事件的推荐规格独立成送审文档，不新增取舍、字段或枚举；该提案的待决状态不因本文件而改变。
>
> 权威优先级：`decisions.md` > `architecture.md` 的不变式 > `interfaces/` > `modules/` > 其余文档。冲突时以更高优先级的权威文档为准。本文件位于 `interfaces/`。

## 1. 范围与排除

本文件覆盖 13 个事件：`attempt.progress`、`guardian.triaged`、`guardian.heal_requested`、`guardian.escalated`、`approval.invalidated`、`approval.expired`、`approval.denied`、`outbox.enqueued`、`outbox.executing`、`outbox.cancelled`、`forge.synced`、`budget.charged`、`policy.denied`。另有批准后经契约 PR 增补的 `budget.limit_raised`（§2.2.6.6，#129）。

`forge.drift_detected` 不在本文件范围内；它由独立 drift 契约定义。本文件不定义该事件的 `data`、信封、九种 `kind` 的判别、`DriftTarget` 或合成规则。

## 2. 事件 data 推荐规格

以下 §2.1–§2.4 大体沿用 [m0-contract-proposal.md](../m0-contract-proposal.md) §2 的编号与正文顺序，便于逐项对照；因排除 `forge.drift_detected` 与独立契约的端点细节，个别小节编号重排。

### 2.1 阻塞总表

> 本表沿用提案编号；#12 `forge.drift_detected` 由独立 drift 契约定义，不在本文件。其余 13 行是候选状态，不是批准记录。

| # | 事件 | 现有依据 | 缺失决策 | 候选/确认状态（草） | 确认者 |
| --- | --- | --- | --- | --- | --- |
| 1 | `attempt.progress` | `data-model §5.2` 只用于展示；`worker-protocol §7.1` 定义 worker 进度事件，`§7.2` 说“转发给客户端的是采样节流后的 `attempt.progress` 事件”；`sandbox.md §11` 只列采样后的 stage / detail / 工具摘要 / 自报用量 | 最终白名单、`warn` 范围、时间与文本字段的边界 | 推荐 A：服务端白名单投影并净化；B 的包装仅作对照。见 §2.2.1 | `W+E+C` |
| 2 | `guardian.triaged` | `guardian §7.10` 给出 `{event_ref, class, action, delivery_id, rule, reason_code?, fingerprint?, also_matched[], config_rev, round_id?, inbox_id?, action_ids[]}`；`§7.3` 给出处置词汇；`§7.6` 给出指纹 | 审定 §2.2.2 推荐的触发引用判别、封闭 action、`round_closed` 字段、轮 ID 与分支必填规则 | 判别联合：普通分流一支、`round_closed` 一支。见 §2.2.2 | `G+C` |
| 3 | `guardian.heal_requested` | `guardian §7.10` 给出字段列表；`§6` 给出 `HealTask` 与 `ProblemSpec` | `task` 的序列化；`problems[].kind` 词表；`batch_included[]` 元素；`request_digest` 覆盖什么、是什么类型 | 推荐字段表、`request_digest` 投影与分支负例。见 §2.2.2 | `G+E+C` |
| 4 | `guardian.escalated` | `guardian §7.10` 给出 `{delivery_id, class, reason_code, inbox_id?, fingerprint?, round_id?}`；`§7.9` 给出 `reason_code` 表；`m0-contract-gaps.md` 称它是 14 个中唯一可安全补全的候选 | `reason_code` 的封闭集；`suppressed_duplicate` 如何表达；`round_id` 类型；`Deduped` 时是否省略 `inbox_id` | 最小 JSON 候选。见 §2.2.2；仍不是冻结 | `G+U+C` |
| 5 | `approval.invalidated` | `policy §7.6` 第 5 步说 `data` 含新旧 head；`§7.7` 还列出 `pr_closed`、`pr_merged`、`delivery.abandoned`、head 变化等失效原因；`data-model §4.4` 定义审批状态机 | 端口映射已由 #89 Q2 补齐（`InvalidateReason` 与 6 个 reason 一一对应） | 推荐 `reason` 判别联合（6 值，含 `owner_cancelled`）与映射；见 §2.2.3 | `P+F+E+C` |
| 6 | `approval.expired` | `data-model §5.2` 只有事件名；`policy §7.5` / `§7.7` 定义懒过期与清扫；审批实体已有 `expires_at`、`status` | 确认已推荐的 `data={}` 是否冻结；不新增 `previous_status` / `expired_at`；不重复 `approval_id` | 推荐 `data={}`；见 §2.2.3 | `P+C` |
| 7 | `approval.denied` | `api §6.1` 的 `deny` 请求体是 `{reason?}`；`policy §6.4` 的 `deny(id, device, reason)`；`§7.5` 定义 `pending → denied` | 确认已推荐的 `{reason?: string}` 与空对象是否冻结；纯文本处理与体积门禁 | 推荐 `{reason?: string}`，受 64 KiB 请求体门禁；见 §2.2.3 | `P+API+C` |
| 8 | `outbox.enqueued` | `m0-contract-gaps.md` 说只定义 `revision` 一项；`core §7.5` 的 `upsert_status_comment` 带 `revision`；`forge §7.8` 用 `revision` 保持幂等；`data-model` 的 `OutboxAction` 有完整动作字段 | 字段来源与恢复责任；哪些动作写及如何取值；是否内嵌 payload/依赖/优先级/`enqueued_by` 及包装；不能单靠入队事件恢复后续租约、重试或预写效果，G-06 信封不能替代 | 已确认方向：记录完整入队意图，而非仅 `revision` 审计；保留状态评论 `revision`。候选 A/B 仅作字段对照；B 的完整快照不是已批准包装。见 §2.2.4 | `F+G+E+S+C` |
| 9 | `outbox.executing` | `forge §7.8` 说 claim 时追加；`data-model §5.2` 只有事件名；`core §7.5` 未列 | 审计字段及其与 claim 结果的一致性 | 推荐 `{attempts, replay}`；空对象与租约快照仅作对照。见 §2.2.4 | `F+S+C` |
| 10 | `outbox.cancelled` | `forge §7.8` 的状态机列出 `cancel`、前置失败、`superseded`、执行前审批失败；`m0-contract-gaps.md` 说 coalesce 时追加；`OutboxAction` 没有 `reason` 字段 | 4 值取消原因；不新增 `cancelled_by`；不塞入 `last_error` 对象；不允许空对象 | 推荐 `{reason}`，4 值复用 `store::CancelReason`；见 §2.2.4 | `F+G+E+C` |
| 11 | `forge.synced` | `forge.md §7.5` 第 6 点给出提议形状 `{target, source, changes[{field, from, to}], synced_at}`；`guardian §4` 以该提议为输入；`data-model §5.2` 只有名称 | target 范围、字段白名单、可空性及 source 枚举 | 推荐 `target` 仅 PR/Issue、`changes` 按 field 强类型、`subject=prj_…`；字段与正负例见 §2.2.5.1 | `F+G+C` |
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
- 与 `core::ports::policy::InvalidateReason` 一一对应（#89 Q2，契约 PR 已落地）：`HeadMoved { from, to }` → `head_changed`（`from` / `to` 即 `previous_head_sha` / `current_head_sha`），`PrClosed` → `pr_closed`，`PrMerged` → `pr_merged`，`DeliveryAbandoned` → `delivery_abandoned`，`AttemptSuperseded { from, to }` → `attempt_superseded`（`previous_attempt_id` / `current_attempt_id`），`OwnerCancelled` → `owner_cancelled`；`core` 提供 `From<&InvalidateReason> for ApprovalInvalidatedData`。原 `SubjectReplaced`、`Other(String)` 没有调用方，已删除：主体替换按具体原因（head 变化、尝试替代）表达，封闭 `reason` 不接受任意字符串。
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

#### 2.2.5 Forge 事件

**推荐规格，待契约审查。** 本文件只定义 `forge.synced`；它仍为 `ContractUndefined`；以下字段与白名单是收敛推荐，不是已批准契约。`forge.drift_detected` 由独立 drift 契约定义，不在本文件。

##### 2.2.5.1 `forge.synced`

**信封**：`subject=prj_…`（固定项目级，避免 Issue 绑定多个交付时 subject 漂移）；`project_id` 必填且等于 subject；`correlation_id` 在能解析交付时等于该交付的 `delivery_id`，未绑定时为 `null`（`forge §7.5` 允许未绑定对象停放）。`data` 不重复 `project_id`，不新增 `delivery_id`；投递绑定由信封表达。`actor` 用 `{kind:"forge",id:"forge"}`。`causation_id` 为触发处理的入站事件或 `null`。

**`data` 字段**

| 字段 | 必填 | 类型/来源 | 约束 |
| --- | --- | --- | --- |
| `target` | 是 | `{type:"pr"\|"issue", number:u64, node_id:string}` | `number` 和 `node_id` 都必填；这是 `ForgeEventTarget` 的收窄，不表示 branch/installation 可用。`target` 必须属于信封项目，`node_id` 与回读对象一致。 |
| `source` | 是 | `webhook` / `reconcile`，复用 `ForgeEventSource` | `webhook` 只用于因该投递而更新快照；patrol、启动全量、投递恢复、强制回读与 `reconcile_now` 都用 `reconcile`。 |
| `changes` | 是 | `Change[]` | 每项 `{field, from, to}`；同一 `field` 至多一次；按本节字段表顺序排序。`from` 与 `to` 必须不同。不带 `initial` 时必须非空；带 `initial` 时必须为空数组。 |
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

**首次快照（`initial`，可选；#92 / #150 拍板）**：本地第一次看到某个 PR / Issue 时没有 `from` 可比。此时发一条 `forge.synced`，`changes` 为空数组，`initial` 一次给出该 target 适用的**全部**白名单字段，值类型与上表相同：

| `initial.type` | 必须且只能有的字段 |
| --- | --- |
| `pr` | `state` `title` `head_sha` `base_ref` `base_sha` `draft` `mergeable_state` `in_merge_queue` `checks` `review_state` |
| `issue` | `state` `state_reason`（可为 `null`，表示没有关闭原因） `title` |

`initial.type` 必须等于 `target.type`；除 `state_reason` 外不得为 `null`；`initial` 出现时不得为 `null`，不需要时省略。之后同一 target 的同步只发 `changes`，不再带 `initial`。投影（store.md §7.3 `pr_snapshots` / `issue_snapshots`）用 `initial` 建行，用 `changes` 更新，快照列只从 `forge.synced` 折叠；`forge.<kind>`（如 `pr_opened`）照常同批追加，只驱动业务语义。同批里 `forge.synced` 排在 `forge.<kind>` 之后。

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
| PR 合并状态与 checks 同批变化；Issue `state` 与 `state_reason` 同批变化；webhook 与 reconcile 各一条；PR 与 Issue 的首次快照（`initial`）各一条 | `changes` 为空且无 `initial`；`initial` 与非空 `changes` 同时出现；`initial` 缺字段 / 多字段 / 字段为 `null`（`state_reason` 除外）/ 整体为 `null`；`initial.type` 与 `target.type` 不同；Issue 的 `initial.state=merged`；`from` 等于 `to`；未知 `field`；同一 `field` 出现两次；Issue 带 `head_sha`；PR 带 `state_reason`；`target` 缺 `node_id`；`type` 为 branch/installation；webhook 已解析交付却把 `correlation_id` 填成其他值；`synced_at` 与快照不一致；`title` 为裸字符串；`checks` 缺 `required_ok`；`review_state=commented`。 |

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
| `correlation_id` | 账本实体 ID：交付为 `dlv_…`，会话为 `cnv_…`；不带日期。策略事件见 2.2.6.4。 |
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

`job_token` 的单一 `config_rev` 是合法的，因为权重在令牌签发时只有一个快照；仍须保存权重值或摘要。`FinalUsage` 当前缺权重快照；推荐随 `FinalUsage` 持久化权重摘要，或从 job token 签发记录取回，不能只存 `config_rev`。`svc_records` 必须逐请求保留 `weight_rev`；不能用一个 `config_rev` 冒充多请求版本。`UsageRecord.weight_rev`（u64，必填，计量时生效的权重修订）已由 #82 补入（egress-control §4.5）；晚到记录不按当前配置重算。

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

`direct_call` 结算以 egress 控制面契约的 `B-01` 调用级终结为前提；未取得时不结算。`B-01` 字段仍待契约审查，端点细节属于另一份契约 PR。

##### 2.2.6.3 沙盒关闭与 drain 屏障

`revoke` 不得被解释为“Egress 已给终态”。推荐 job token 状态：`accepting` → `draining` → `closed`，状态持久化。

1. **同一串行点**：请求接受时，Egress 在同一事务/同一串行点登记持久 inflight 并检查门禁；关闭门禁也在该点串行执行。接受先赢则请求计入 inflight；关闭先赢则拒绝新请求。不得先回 200 再登记 inflight。
2. **关闭门禁**：`revoke` 先持久化 `draining`，立即拒绝新请求；已接受的 inflight 继续完成或中断。
3. **drain**：每个 inflight 完成时，在同一事务写用量记录并标记完结。所有 inflight 清零、计量持久化后，才计算并持久化 `FinalUsage`，置 `closed`，返回 200。
4. **超时/连接中断**：结果未知，调用方重试同一 `job_id`；已 `closed` 的幂等返回同一 `FinalUsage`。不得用超时或 404 推零。
5. **事务边界**：等待 drain 不能跨 DB 事务持有；状态机分多次短事务，HTTP 等待发生在事务之外。引擎拿到 `FinalUsage` 后才开结算事务。
6. **崩溃丢计量**：`incomplete=true`，不零结清，不自动重跑；保留控制面证据并升级。

在 `accepting` / `draining` 阶段读取的用量只是当前快照，不是终结证明。

##### 2.2.6.4 `policy.denied`

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

`PolicyTarget` 已是 core 类型（`events::PolicyTarget`）。目标必须来自请求本身：`MergePr` / `PushBranch` / `ResolveReviewThread` 取 `ActionFacts` 里的对象身份，其余动作取 `ActionFacts::Other.target`（#89 Q1）；不能从仓库名猜。

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

**端口来源**（#89 Q1 已决）：`ActionFacts::ResolveReviewThread` 带 `{repo, number, thread_node_id, thread_has_human_comment}`；`ActionFacts::Other` 带 `target: PolicyTarget`，由入队方在入队前从待入队载荷投影。`target.kind` 与动作 id 不符时按 fail-closed 处理，不能退化为 `repo`（否则同仓库不同 PR 会被合并）。

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

##### 2.2.6.5 仍需授权或阻塞

- ~~B-01 方向已确认、字段待审~~：2026-10-27 已批准并补端口（#82，[egress-call-terminal.md](egress-call-terminal.md)）；direct call 的最终结算、释放和零判定以其 `terminal` 证明为据。
- `decide_proposals` 缺 P78 `ActorCtx`：proposal 分支不能发事件；需加参数。
- ~~`ActionRequest` 缺统一 `target`，`ActionFacts::Other`/`ResolveReviewThread` 缺对象身份~~：#89 Q1 已补（`Other.target`、`ResolveReviewThread.{number, thread_node_id}`）。
- `DenyReason` 缺 `PatchNotApproved`；`file_mode_change` 仍是 OQ-P5 候选。
- ~~`UsageRecord` 缺 `weight_rev`~~（#82 已补）；`FinalUsage` 缺权重快照；权重审计仍需补契约。
- `BudgetDimension` 缺 `Attempts`；需枚举同步，不新增配置键。
- 现有 schema 仍拒绝新 data，持久表未实现；`Attempt.usage` 保持非空，公开终态延迟到结算。`incomplete` 的长期等待需人工处置，不自动重跑或抹账。

确认者：`E+M+X+C+U`（预算）与 `P+C+U`（策略）。本节是推荐规格，待契约审查，不是用户批准、契约冻结或实施完成。

##### 2.2.6.6 `budget.limit_raised`（#129，decisions D-19）

用户对 `budget_exhausted` 上报项选 `raise_cap` 后，引擎给耗尽的那个维度追加一份该档预算模板的原值（D-19），并写本事件记录这次追加。本节在本文件批准之后由 #129 增补，不属于原送审的 13 个事件。

**信封与 actor**

- 写入方只有 `engine`：在处理 `inbox.resolved(chosen, option_id = raise_cap)` 的同一决策事务里追加；`actor` 取该决策的 `ActorCtx.actor`（引擎），`causation_id` 指向那条 `inbox.resolved`。
- `subject`：被追加的交付单元台账（`dlv_…`），与触发它的 `budget.exhausted` 相同。

**data**

```json
{"dimension": "rounds", "from": 3, "to": 6, "by": 3}
```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dimension` | `BudgetDimension` 字符串（与 `budget.exhausted.dimension` 同一词表，如 `nodes`、`rounds`、`tokens_by_tier.worker`） | 被追加的维度，即耗尽的那个维度 |
| `from` | `u64` | 追加前该维度的上限 |
| `by` | `u64` | 追加量 = 该档预算模板在该维度的原值（模板在交付单元创建时按 `level_cap` 固定，P-57） |
| `to` | `u64` | 追加后的上限 |

**一致性（`core` 校验）**

- `by > 0`，否则拒绝，指针 `/by`；
- `to = from + by`，且不溢出 `u64`，否则拒绝，指针 `/to`；
- 未知字段拒绝（`deny_unknown_fields`）；四个字段都必填。

**语义**

- 引擎在预算台账投影里折叠：`limits[dimension] = to`，`used` 不变，不写 `budget.charged`；该维度不再满足耗尽条件后解除 `waiting_human` 并继续派发（engine.md §7.9）。折叠的实现不在本契约内（#73 或之后的切片）。
- 每选一次 `raise_cap` 追加一次；追加与处理该 `inbox.resolved` 的决策同事务，重放不重复追加（engine.md §7.14.1）。

**测试矩阵**

| 用例 | 期望 |
| --- | --- |
| `{rounds, 3, 6, 3}` | 通过。 |
| `by = 0` | 拒绝（`/by`）。 |
| `to ≠ from + by` | 拒绝（`/to`）。 |
| `from + by` 溢出 `u64` | 拒绝（`/to`）。 |
| 多余字段（如 `reason`） | 拒绝。 |
| 缺 `from` | 拒绝。 |

### 2.3 通用候选约束（契约审查须确认）

以下只是候选评审原则，不是已冻结规则：

- 已定义形状的 `data` 使用 `additionalProperties: false`；未知字段拒绝。
- 来自 GitHub、沙盒或设备的文本使用 `UntrustedText` 或明确的截断字符串；事件 `data` 不出现 secret 值。
- 事件 `subject` 已承载的实体 ID 不重复；需要跨实体关联时再增加字段。
- 判定类事件带 `config_rev`；纯生命周期事件不带。
- 跨字段一致性（例如 `delivery_id` / `conversation_id` 互斥）在 Rust 校验；schema 能用 `if/then` 表达的也表达。
- 优先复用现有实体和枚举；不新增 ID 前缀，除非现有类型无法表达。

### 2.4 其余事件的推荐收敛（契约审查须确认）

以下选择用于减少草案分支，不视为用户逐项确认。优先复用信封与现有类型；没有依据的 ID、长度值、枚举不补猜。

| 事件 | 推荐 | 契约审查仍须完成 |
| --- | --- | --- |
| `guardian.triaged` | 普通分流与 `round_closed` 使用同一事件的判别分支；`event_ref` 区分 Forge 事件、尝试结束和结算检查，不能都强塞成外部事件 | 按 `guardian §7.3 / §7.10` 列封闭字段集；冻结触发引用、分支必填项和轮 ID。不得为草案新增 `rnd_` 前缀 |
| `guardian.heal_requested` | 先冻结 `HealTask` / `ProblemSpec` 序列化，再冻结事件中的摘要和引用；不带材料正文 | 明确 `request_digest` 覆盖范围、`batch_included` 元素及 round_id；保留调用 HealPort 前落盘的顺序（`guardian §7.10`） |
| `guardian.escalated` | `class` / `reason_code` 复用守护封闭表；推荐显式 `suppressed_duplicate`，避免把缺 inbox_id 当作抑制成功 | `Deduped` 的 inbox 引用、可选性和全局归属须明确；不能忽略守护字段表与抑制说明之间的缺口 |
| `approval.invalidated` | 推荐 6 个原因：head 变化、PR 关闭/合并、交付放弃、尝试替代、发起方取消；head 变化带新旧 head，空分支只带 `reason` | 已落地（#89 Q2）：`InvalidateReason` 与 6 个 reason 一一对应，`SubjectReplaced` / `Other` 已删除 |
| `approval.expired` | 推荐 `{}`，用信封定位审批，依赖审批日志恢复前态 | 不新增 `previous_status` / `expired_at`；区分实体 expires_at 与实际记录过期的时刻 |
| `approval.denied` | 推荐 `{reason?: string}`，由信封记录主体、操作者与时间，不重复 device_id | 备注按不可信文本处理；受 64 KiB 请求体门禁，不借 settings 的 256 KiB 上限 |
| `forge.synced` | §2.2.5.1 已给出 `target` 仅 PR/Issue、`changes` 按字段强类型与完整正负例；不允许任意 JSON 兜底或复制正文 | 契约审查核对字段白名单、可空性和首次快照；项目级变化仍不在 M0 范围，不从 PR 示例推导扩围 |
| `policy.denied` | 动作策略拒绝与补丁策略拒绝分支审计；机械拒绝仍归 `patch.rejected`（P-69）；动作目标按 20 个动作映射 | 完整目标来源、条件伪动作对应关系、subject 与原因词表待审；不套用不完整的 ApprovalTarget，不使用 `other` 或伪造 push_branch 来隐藏缺口 |

验收须覆盖每个最终判别分支、信封与 data 一致性及未知字段拒绝；不因选了最小形状省略实际适用的校验，也不为凑测试增加冗余字段。
