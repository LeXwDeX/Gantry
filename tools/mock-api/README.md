# Mock API

`schemas/openapi.yaml` 的内存模拟实现，只监听 `127.0.0.1`。用于前端联调和 UI 层 E2E（desktop-ui §12）。不做 TLS、鉴权和 CORS，也不校验签名。

```sh
npm ci
npm start              # MOCK_PORT（默认 8787）；MOCK_SSE_HEARTBEAT_MS（默认 15000）
npm test
```

## 行为

- **路由与形状**：来自 `openapi.yaml`，初始数据来自 `fixtures/api/`。每个响应体都先按 OpenAPI 校验，不合法就返回 500。
- **写请求产生事件**：每个成功的写请求都追加对应事件（data-model §5），推进 `head`，并实时推给已建立的 `/v1/events` 连接。事件 `data` 按 `schemas/events/<type>.json` 校验，信封按 OpenAPI 的 `EventEnvelope` 校验，不合法就返回 500。响应头 `X-Event-Seq` 已包含本次写入产生的事件；幂等重放不会再产生事件。

  | 写操作 | 事件 |
  | --- | --- |
  | 发消息 | `message.posted` |
  | 暂停 / 继续 / 放弃 / 改上限 | `delivery.paused` / `delivery.resumed` / `delivery.abandoned` / `delivery.level_changed`（`kind = cap`） |
  | 重试节点 | `node.retry_requested` |
  | 选择 / 忽略上报项 | `inbox.resolved`（`chosen`）/ `inbox.dismissed` |
  | 发起 / 批准 / 拒绝审批 | `approval.requested` / `approval.granted` / `approval.denied` |
  | 设置 PUT / 项目覆盖 DELETE | `settings.updated`（凭据只记 `secret_changes`，不含值）；签名写入另发 `approval.consumed` |
  | 设备改名 / 改角色 / 吊销 | `device.updated` / `device.revoked`；吊销本机时立即断开事件流 |
  | 工作树人工锁 | `worktree.locked` / `worktree.unlocked` |

- **凭据写入需要签名**（config.md §13）：请求体里带 `secret_ref` 和 `value` 的设置写入，`dry_run` 返回 `requires_signature: true`、`required_role: approver` 和 `signing`。正式 PUT 必须带 `Gantry-Approval`，指向一个已 `granted`、且动作、目标、`subject_digest` 都与本次请求一致的审批，否则返回 `403 signature_required`（附 `signing`）。审批用过一次即变为 `consumed`。Mock 不计算 `relaxes`，放宽类改动不会要求签名。

## 场景控制面 `/__mock/*`

只在 Mock 上存在，不属于 api.md 契约，也不在 OpenAPI 里。

| 路由 | 作用 |
| --- | --- |
| `POST /__mock/reset` | 以夹具重建全部状态，清空幂等记录、challenge 和请求日志，并断开事件流 |
| `POST /__mock/events` | `{ type, subject, data, project_id?, correlation_id?, actor? }`：按 schema 校验后追加并实时推送；不合法返回 400 |
| `POST /__mock/drop-streams` | 断开已建立的事件流（客户端按游标重连并拿到回放） |
| `POST /__mock/mutate` | `{ collection, id, patch }` 合并到 `state[collection].items` 中的匹配项；`{ collection, push }` 追加；`{ set, value }` 设置顶层标量（如 `logEpoch`） |
| `POST /__mock/clock` | `{ advance_s }` 或 `{ time }`：推进或设置服务端时间（challenge 过期依赖它） |
| `POST /__mock/fail-next` | `{ operation_id, status, code, detail? }`：下一次该操作返回指定 problem |
| `GET` / `DELETE /__mock/requests` | 读取 / 清空请求日志 `[{ method, path, operationId }]` |

`mutate` 不产生事件。要让客户端感知变化，需要再调用 `/__mock/events` 注入对应事件，与真实服务端“状态变化必有事件”的约定保持一致。
