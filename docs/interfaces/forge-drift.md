# Forge drift 契约（送审）

> **状态：送审（under review），尚未批准（not approved），非权威（non-authoritative）。**
>
> 本文把 [M0 契约冻结提案](../m0-contract-proposal.md) §4 的 drift 推荐规格整理为独立待审契约，供契约审查。它不是已批准契约，也不表示任何 schema 已冻结。
>
> 当前运行时保持 fail-closed：`schemas/forge/drift.json` 与 `schemas/events/forge.drift_detected.json` 仍拒绝一切；`validate_event_data` 继续返回 `ContractUndefined`，`store` 继续拒绝追加。只有契约批准并另获实现授权后，才落地 schema、夹具、Rust、Mock 或前端。

## 1. 范围

本文定义以下待审内容：

- 9 种 `drift.kind` 的公共信封与 `DriftTarget` 判别。
- 每种 `kind` 的 `expected` / `actual` 判别字段及字段来源。
- 一致性与合成规则、合成事件的字段来源和去重依据。
- `forge.drift_detected.data` 与 drift 记录的关系。

`forge.drift_detected.data` 直接承载 drift 记录，不包一层 `{drift:…}`，也不使用 `G-06` 入队信封。`G-06`（`OutboxEnqueueEnvelope`）是 `forge-actions §2.1` 的入队信封，不是 `outbox.enqueued.data`，也不是本事件 `data` 的包装。实现时 `schemas/forge/drift.json` 与事件 schema 共用同一份 `$defs`。

`§4.1` 的字段表是 drift 记录自身的字段全集。草案没有 `detected_at`，本文不新增该字段；时间取事件信封的 `ts`。`correlation_id` 也属于事件信封，不属于 drift 记录。

命名以草案 §4.3 的实际九个 `kind` 为准：`pr_state_diverged`、`head_diverged`、`effect_missing`、`orphan_effect`、`issue_state_diverged`、`review_state_diverged`、`merge_state_diverged`、`default_branch_changed`、`installation_unreachable`。`repo_changed`、`installation_changed`、`base_advanced` 是既有事件 kind，不是 `drift.kind`。本文不新增 `kind`；未来只允许追加。

## 2. 已确认方向（草案 §9.1 第 5–6 项）

以下是方向确认，不是字段级批准：

- `effect_missing` 保留三个事实分支：对象缺失、PR 已关闭、正文哈希变化。与 `pr_state_diverged` 的重复记录规则见 §4.4。
- 只有事实充分且对账幂等来源已定义时才合成事件；缺少来源时不合成，不用默认 actor 或猜测值补齐。
- `installation_unreachable` 只证明访问失败，不推断安装被删除或仓库对象不存在；`installation_changed` 需要另有安装状态证据。
- `default_branch_changed` 可以更新 Project 快照，但 M0 不合成 `repo_changed`，因为该事件含 `delivery_guid` 且尚无对账幂等来源；不得伪造 webhook GUID。
- `repo_changed` / `installation_changed` 的合成分支必须与去重契约同步；在来源未定义前以 `synthesized_events=[]` 承接，不能静默漏通知。

## 3. 依据与待审边界

- 依据：`forge-actions §7.2`–`§7.4`、`forge.md §7.5` / `§7.7`、`crates/gantry-core/src/ports/forge.rs::DriftKind`、`schemas/forge/drift.json`、`schemas/events/forge.drift_detected.json`。
- `DriftTarget`、`InstallationUnreachableCode`、`PullRequestReadback`、`IssueReadback` 都是推荐新增类型，不是现有生产类型。
- 以下 §4.1–§4.6 承载草案 §4.1–§4.6；只调整了指向具体 PR 编号的措辞，不改变字段与规则。§4.4.2 是补充的去重来源记录，草案未单列。

## 4. 推荐规格（逐字承载草案 §4.1–§4.6）

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

**回读来源缺口（推荐契约增补）**：现有 `ForgeReader::pull_request` / `issue` 只返回 `PullRequestRef` / `IssueRef`，两者都没有 `closed_by`、`merged_by`、`via_merge_queue`。因此上表这些列在 M0 现有端口上不可得，按“不可得时 `[]`”处理，不能从事件默认值或本地投影伪造。推荐在后续契约中新增回读 DTO，由回读适配器填充 actor、head 与 merge queue 事实：

```text
PullRequestReadback { head_sha: GitOid, base_ref: String,
                      closed_by: ForgeActor?, merged_by: ForgeActor?,
                      via_merge_queue: bool? }
IssueReadback       { closed_by: ForgeActor? }
```

`?` 表示按路径填充，缺失即不可合成。也可以给 `PullRequestRef` / `IssueRef` 增加同名可选字段，但不能用默认 actor 或猜测值补齐。推荐给 `ForgeReader` 增加 `pull_request_readback` / `issue_readback`，保留现有 `pull_request` / `issue` 的快照读取签名；若改现有方法返回 readback DTO，必须在契约变更时同步全部调用方。在契约定义前，`pr_closed` / `pr_merged` / `issue_closed` 的 `synthesized_events` 为 `[]`。`pr_synchronized` 的 `head_epoch` 来自本地事件日志，`base_ref` 来自 `PullRequestReadback`；`pr_dequeued` 的 `head_sha` 也来自 `PullRequestReadback`，避免拿更新前的快照合成旧 head。

`head_diverged` 的 self 归因：`actual.pushed_by` 是 GitHub actor；把 `Project.bot_user_id`（`i64`）与 `ForgeActor.github_id`（`u64`）显式转换后比较，相等或命中 `forge_effect_index` 时为 self，不合成 `pr_synchronized`；非 self 才合成。转换越界或失败按非 self 处理，不因比较错误而抑制 drift。`pushed_by=null` 不合成。

#### 4.4.2 去重来源（待审）

草案没有为 drift 记录定义独立 `dedup_key`。当前可核对的去重依据只有：

- `delivery_id` 非 `null` 时绑定到具体交付单元；`null` 只用于项目/安装级记录（§4.1、§4.3）。
- 同一 PR 的关闭/合并只发 `pr_state_diverged`，不再发 `effect_missing(pull_request)`（§4.4）。
- `pr_synchronized` 的 `head_epoch` 取本地事件日志中已追加的最大值 + 1，事件去重沿用该日志（§4.4.1）。
- `default_branch_changed` / `installation_unreachable` 在幂等来源定义前不合成，且 `delivery_id=null`；不得伪造 webhook GUID。

记录级去重键、重复巡检窗口和跨项目安装状态的合并方式仍未定义。本文不擅自新增键或配置；这些必须在契约审查中决定。

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

## 5. 回读数据对象与合成门禁

**字段来源总则**：`expected` 取本地期望（快照、Project 或 Outbox 效果）；`actual` 取回读对象；`installation_unreachable` 的 `actual` 取令牌交换或仓库访问结果。`orphan_effect` 的 `expected.outbox_action` 固定为 `null`，表示效果索引没有对应行；`actual` 取 GitHub 对象。每种 `kind` 的判别字段见 §4.3；合成事件字段来源见 §4.4.1。未列来源的字段不得补齐。

### 5.1 推荐回读 DTO（送审）

```text
PullRequestReadback { head_sha: GitOid, base_ref: String,
                      closed_by: ForgeActor?, merged_by: ForgeActor?,
                      via_merge_queue: bool? }
IssueReadback       { closed_by: ForgeActor? }
```

`?` 表示按路径填充；缺失即不可合成。

| 字段 | 来源 | 使用 | 缺失时 |
| --- | --- | --- | --- |
| `PullRequestReadback.head_sha` | 回读 PR 的实际 head | `pr_closed` / `pr_merged` / `pr_synchronized` / `pr_dequeued` | 相关事件不合成，`[]` |
| `PullRequestReadback.base_ref` | 回读 PR 的 base ref | `pr_merged` / `pr_synchronized` | 相关事件不合成，`[]` |
| `PullRequestReadback.closed_by` | 回读 PR 的关闭 actor | `pr_closed` | `pr_closed` 不合成，`[]` |
| `PullRequestReadback.merged_by` | 回读 PR 的合并 actor | `pr_merged` | `pr_merged` 不合成，`[]` |
| `PullRequestReadback.via_merge_queue` | 回读 PR 的 merge queue 事实 | `pr_merged` | `pr_merged` 不合成，`[]` |
| `IssueReadback.closed_by` | 回读 Issue 的关闭 actor | `issue_closed` | `issue_closed` 不合成，`[]` |

### 5.2 合成门禁

- 无回读来源时 `synthesized_events=[]`，不默认、不猜测。
- 当前 `ForgeReader::pull_request` / `issue` 返回 `PullRequestRef` / `IssueRef`，不含上述字段；M0 现有端口下 `pr_closed` / `pr_merged` / `issue_closed` 的合成列表为 `[]`。
- `pr_synchronized` 的 `head_epoch` 来自本地事件日志；`base_ref` 来自回读。`pr_dequeued` 的 `head_sha` 来自回读。
- `issue_reopened` 只取 `target.number`，不依赖回读 actor。
- 新增 `pull_request_readback` / `issue_readback` 或扩展 ref 属于实现阶段，不在本文实施；本文只定义推荐 DTO 与门禁，现有端口签名不变。

## 6. 冻结前验收清单

- [ ] 9 种 `kind` 的 §4.3 判别分支全部有正例和关键负例。
- [ ] §4.5 公共负例全部拒绝；未知 `kind`、未知字段、`expected` / `actual` 整体为 `null` 都必须失败。
- [ ] `effect_missing` 三支分开验证；`status_comment` 的 `content_changed` 摘要必须与 `expected.render_digest` 不同；PR 关闭不得写成 `missing`；不得与 `pr_state_diverged` 双发。
- [ ] `head_diverged` 的 self 归因与非 self 归因分别验证；`pushed_by=null` 不合成。
- [ ] 权限不足的 403、普通超时、未排除权限问题的 404 不当作删除或关闭；Issue 删除/转移不缩成 `closed`。
- [ ] 缺少 `PullRequestReadback` / `IssueReadback` 来源时 `synthesized_events=[]`；不得用默认 actor 或猜测值补齐。
- [ ] `default_branch_changed` / `installation_unreachable` 在幂等来源未定义前不合成 `repo_changed` / `installation_changed`，不伪造 `delivery_guid`。
- [ ] `forge.drift_detected.data` 与 `schemas/forge/drift.json` 使用同一份 `$defs`；`data` 不包 `{drift:…}` 或 `G-06` 信封。
- [ ] 通过后仍需契约批准；schema、夹具、Rust、Mock 与前端实现另获授权。

## 7. 未实施与后续边界

- 本文只提交文档送审；未修改 `schemas/**`、`fixtures/**`、`crates/**`、Mock 或前端。
- 本文未新增 `drift.kind`、事件名、ID 前缀或配置键。
- `forge-actions.md` 只增加指向本文的范围说明；其既有正文、示例和 §7.2–§7.4 的 drift 形状不变。
- `docs/modules/forge.md` 与 `docs/data-model.md` 未改；§4.6 记录的冲突仍须独立契约变更回写。
- 端口回读方法、schema `$defs`、夹具与 Rust 校验属于实现阶段，不在本文实施。
