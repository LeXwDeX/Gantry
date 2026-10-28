# 调用级终结（B-01）

> 状态：2026-10-27 批准（#82 Q1–Q7 答复）。本文是 [egress-control.md](egress-control.md) §4.3 / §4.5 的补充，与之同级；两者冲突时提 issue，不自行取舍。
> 来源：方向来自 [M0 契约冻结提案 §2.2.6.4](../m0-contract-proposal.md)（2026-10-26 确认方向及墓碑存储代价）；字段、HTTP 形状、错误码与保留期按 #82 的答复定稿。
> 范围：在既有 revoke / usage 端点上增加调用级关闭与只读终结证明，不新增路由、不新增错误码、不新增 ack 接口。
> 端口：`core::ports::egress::EgressControl::close_call` / `call_terminal`；线上类型在 `core::egress`（`RevokeSvcTokenRequest`、`CloseCallResponse`、`CallTerminal`）。

## 1. 必要性

整令牌吊销（`POST /control/v1/svc-tokens/{id}/revoke`，请求体 `{}`）和按 `call_id` 分页查询（`GET /control/v1/usage?call_id=…`）都不能证明一个全局 `call_id` 的未知发送和迟到接收已经结束。没有这个证明，直接调用的"一调用一条结算"、释放和零判定就没有可信证据。

## 2. 写语义：调用级关闭

在既有 revoke 端点增加最小 call 作用域：

```json
{"scope":"call","call_id":"01JBX5D2F4H6K8M0P2R4T6V8XZ"}
```

| 规则 | 说明 |
| --- | --- |
| 作用域 | `scope` 缺省（请求体 `{}`）保持整令牌吊销，响应不变；`scope` 目前只有 `"call"`；`scope: "call"` 时 `call_id` 必填；不带 `scope` 时禁止携带 `call_id`；未知字段或未知作用域 → `422 validation_failed` |
| 路径 token | 路径 `svc_token_id` 是端点绑定，必须是 Egress 签发过的 `svc_token`（已吊销的也可以，崩溃恢复要用旧令牌关闭旧调用）；不存在 → `404 not_found` |
| 关闭对象 | 按全局 `call_id` 关闭，跨所有 token；不得用每次调用轮换进程令牌来规避该屏障 |
| 关联校验 | 路径 token 必须对该 `call_id` 至少有一条**已接受请求**（inflight 或 completed）；全局没有任何已接受请求时视为零接收，任一路径 token 都可以关闭；该调用的已接受请求全部属于别的 token → `409 conflict`，须改用有关联的 token |
| 响应 | `200`，响应体 `{call_id, terminal}`（`CloseCallResponse`，见 §4）；`terminal` 形状同 §3 |
| 幂等 | 重复调用（含超时、响应丢失后的重试）返回同一份 `terminal`；保留期过后重复调用同样返回该份证明，但 `incomplete` 按 §5 置 `true` |

响应示例（一条已接受请求、证明完整）：

```json
{
  "call_id": "01JBX5D2F4H6K8M0P2R4T6V8XZ",
  "terminal": {
    "state": "closed",
    "closed_at": "2026-10-24T12:41:05.002Z",
    "last_seq": 90211,
    "record_count": 1,
    "svc_token_ids": ["svt_01JBX5A2C4E6G8J1L3N5Q7S9UW"],
    "usage": { "input_tokens": 3201, "output_tokens": 410, "cache_read_tokens": 2800, "cache_write_tokens": 0, "requests": 1 },
    "weighted_tokens": 5531,
    "records_digest": "sha256:c22348d8fde83362ae748e3b7ad96fa64dc2ee6073faa41681ddb4addb00e3a2",
    "incomplete": false
  }
}
```

### 2.1 全局屏障与请求登记

- Egress 维护全局 `call_id` 状态：`accepting` → `draining` → `closed`，以及跨所有 token 的 inflight 登记。
- 请求接受与 inflight 登记、门禁关闭在同一串行点；数据面上 `call_id` 处于 `draining` / `closed` 时的请求一律拒绝，返回 `403 token_invalid`（与 egress-control §5 的令牌拒绝同一响应，不给探测线索），不转发、不计量。
- `POST` 先将全局状态置 `draining`，等待所有 token 的 inflight 结束或中断，计量持久化后计算证明，置 `closed` 并返回 200。等待不跨 DB 事务。
- 超时 / 响应丢失：重试同一 POST，幂等返回同一证明。不得从已完成记录数或最大 `seq` 推零、推完整。

### 2.2 调用方（`models`）的持久化顺序

- 首次发送前持久化：`call_id`、`ledger`、`tier`、已使用 token 集合、禁止继续发送状态。
- 先持久化"禁止继续发送"，再发起 close。
- 崩溃恢复关闭旧 `call_id`（用已使用 token 集合里的任一枚做路径 token），不用新 ID 丢旧账；重试继续同一 call。

## 3. 只读终结证明

`GET /control/v1/usage?call_id=…` 保持只读，响应在 `UsagePage` 上增加可选 `terminal`（`items` 照旧）。`terminal` 缺省表示该调用**尚未关闭**；出现时恒为 `state: "closed"`。

| `terminal` 字段 | 类型 | 必填 | 校验 |
| --- | --- | --- | --- |
| `state` | 枚举 | 是 | 只能 `closed`。 |
| `closed_at` | timestamp | 是 | 关闭持久化时间。 |
| `last_seq` | u64 | 是 | 最大已记录 `seq`；零接收为 `0`。 |
| `record_count` | u64 | 是 | 唯一 `seq` 数；必须等于实际记录数。 |
| `svc_token_ids` | string[] | 是 | 有已接受请求的 token，按字典序升序、去重；零接收为空数组。 |
| `usage` | `TokenUsage` | 是 | 汇总；必须等于记录逐项之和。 |
| `weighted_tokens` | u64 | 是 | 汇总；必须等于记录之和。 |
| `records_digest` | `sha256:…` | 是 | 见 §3.1。 |
| `incomplete` | bool | 是 | `true` 表示证明不完整，不得结算；触发条件见 §3.2。 |

完整性校验必须同时满足：唯一 `seq` 数等于 `record_count`，`last_seq` 等于最大 `seq`（零条时为 `0`），`usage` 与 `weighted_tokens` 等于逐条之和，`records_digest` 等于重算值。缺任何记录、空页、仅最大 `seq`、仅记录数都不算证明。`incomplete = true` 时即使 `closed` 也不能结算。`core::egress::CallTerminal::verify(call_id, &items)` 实现这组校验（另检查每条记录都属于该 `call_id`、`svc_token_ids` 等于记录里的 token 集合）。

查询示例（与 §2 的关闭响应对应）：

```json
{
  "items": [
    {
      "seq": 90211,
      "at": "2026-10-24T12:41:03.120Z",
      "caller": { "kind": "svc", "svc_token_id": "svt_01JBX5A2C4E6G8J1L3N5Q7S9UW", "call_id": "01JBX5D2F4H6K8M0P2R4T6V8XZ" },
      "project_id": null,
      "ledger": { "subject": "conversation", "id": "cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ", "day": "2026-10-24" },
      "tier": "utility",
      "provider": "anthropic",
      "model": "example-utility-model",
      "usage": { "input_tokens": 3201, "output_tokens": 410, "cache_read_tokens": 2800, "cache_write_tokens": 0, "requests": 1 },
      "weighted_tokens": 5531,
      "weight_rev": 7,
      "incomplete": false
    }
  ],
  "next_cursor": "90211",
  "terminal": {
    "state": "closed",
    "closed_at": "2026-10-24T12:41:05.002Z",
    "last_seq": 90211,
    "record_count": 1,
    "svc_token_ids": ["svt_01JBX5A2C4E6G8J1L3N5Q7S9UW"],
    "usage": { "input_tokens": 3201, "output_tokens": 410, "cache_read_tokens": 2800, "cache_write_tokens": 0, "requests": 1 },
    "weighted_tokens": 5531,
    "records_digest": "sha256:c22348d8fde83362ae748e3b7ad96fa64dc2ee6073faa41681ddb4addb00e3a2",
    "incomplete": false
  }
}
```

### 3.1 `records_digest`

- 输入：该调用全部记录按 `seq` 升序排列的数组 `[{"seq": …, "weight_rev": …, "weighted_tokens": …}, …]`；`weight_rev` 即 `UsageRecord.weight_rev`（egress-control §4.5）。
- 规范化：RFC 8785 JCS（与 [event-data.md](event-data.md) 中 `budget.charged` 的 `records_digest` 一致，`core::canon::jcs`）。
- 摘要：对 JCS 字节串取 SHA-256，写作 `sha256:<64 位小写十六进制>`。
- 零条记录：对 `[]` 取摘要，不另设固定值。
- 中断请求（`UsageRecord.incomplete = true`）照常进入摘要。

### 3.2 `incomplete`

`terminal.incomplete = true` 当且仅当下列任一成立：

1. 该调用存在 `UsageRecord.incomplete = true` 的记录（中断请求）。汇总时中断请求按已计量到的 `usage` 与 `weighted_tokens` 计入，照常进入 `records_digest`。
2. 证明已过保留期（§5），明细不再可查。

## 4. 端口

```rust
// core::ports::egress::EgressControl
async fn close_call(&self, svc_token_id: &SvcTokenId, call_id: &CallId, ctx: &ActorCtx)
    -> Result<CallClosed, EgressError>;   // = CloseCallResponse；POST …/svc-tokens/{id}/revoke {"scope":"call",…}
async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError>; // GET usage?call_id=
// CallStatement { call_id, items, terminal: Option<CallTerminal> }
```

| 端口形状 | 含义 |
| --- | --- |
| `CallStatement { items, terminal: None }` | 未关闭；不得结算、不得零判定 |
| `CallStatement { terminal: Some(t) }`，`t.incomplete = false` 且 `t.verify(call_id, &items)` 通过 | 已关闭、证明完整，可以结算（`CallStatement::settleable_terminal()`） |
| `CallStatement { terminal: Some(t) }`，`t.incomplete = true` | 已关闭但证明不完整（中断请求或已过保留期）；不得结算 |

错误映射：`409 conflict` → `EgressError::Conflict`；`404 not_found` → `EgressError::NotFound`；`422 validation_failed` → `EgressError::Rejected`。`call_terminal` 对从未出现过的 `call_id` 返回 `items: []`、`terminal: None`，不返回 `404`。

## 5. 墓碑与保留期

- 关闭后保留最小 `closed` 墓碑（`call_id`、`closed_at`、证明汇总字段，不含用量明细正文），永久保留、不自动遗忘；否则同一 `call_id` 可能被重新接收。
- 完整证明只在 usage 记录保留期（egress-control §4.5，7 天 **[提议]**）内可查。过了保留期，`GET usage?call_id=` 的 `terminal` 仍为 `state: "closed"`，同时 `incomplete: true`，`items` 可以为空；空 `items` 不等于零，不据此结算。
- job 登记与最终计量按 [egress-control §4.2 / §8](egress-control.md#8-测试与验收) 保留至结算与终结提交后显式删除，不按 24 h 自动遗忘（D-22）；svc usage 的保留期仍按该文 §4.5，不得混用。

## 6. 非目标

- 不把用量查询改为写接口；`GET /control/v1/usage` 保持只读。
- 不新增路由、ack 接口或错误码。
- 整令牌吊销（`{}`）的请求与响应不变。

## 7. 测试与验收

`EgressStub` 与真实 Egress 同样运行：

- [ ] 关闭后同一 `call_id` 再请求数据面 → `403 token_invalid`，无新用量记录；关闭前的记录不变。
- [ ] 重复关闭幂等：同一 `terminal` 原样返回；`call_terminal` 可重复读取、不改变状态。
- [ ] 零接收：从未发送的 `call_id` 关闭 → `record_count = 0`、`last_seq = 0`、`svc_token_ids = []`、`records_digest` 等于 `[]` 的 JCS 摘要。
- [ ] 关联校验：调用的请求全部属于 token A，用 token B 关闭 → `409 conflict`；用 A 关闭成功；用已吊销的 A 关闭同样成功。
- [ ] `terminal.verify(call_id, &items)` 通过；删掉一条记录、改动 `weighted_tokens` 或 `weight_rev` 任一项 → 校验失败。
- [ ] 中断记录或过保留期 → `incomplete = true`，`state` 仍为 `closed`。
- [ ] 请求体 `{"scope":"call"}`（缺 `call_id`）、`{"call_id":"…"}`（缺 `scope`）→ `422 validation_failed`。
