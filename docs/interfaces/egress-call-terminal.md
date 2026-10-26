# 调用级终结（B-01 候选，送审草案）

> 状态：送审，尚未批准，非权威；只定义候选控制面行为，不实施。
> 来源：以下正文逐字摘自 [M0 契约冻结提案 §2.2.6.4](../m0-contract-proposal.md)，含原编号。
> 范围：只定义既有 revoke / usage 端点的候选控制面行为，不新增路由。

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

## 非目标（送审说明）

- 不把用量查询改为写接口；`GET /control/v1/usage` 保持只读。
- 不新增 ack 接口。
- 不新增错误码。
- 不实施端点；本文件只定义候选行为，契约批准前不改变 revoke / usage 端点。
