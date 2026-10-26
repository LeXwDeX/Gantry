# 前端接入说明（M0 Mock API）

> 目标读者：`desktop-shell`、`desktop-ui`、`mobile` 的前端实现者。M0 只提供录制数据与脚本响应，真实鉴权、引擎、审批和持久化尚未实现。本文说明怎样启动、怎样请求、哪些行为是真实契约、哪些只是 Mock 边界。

## 1. 启动与测试

要求 Node.js ≥ 22。Mock 使用 Node 原生 `http` 与 `node:test`，没有 Web 框架。

```bash
cd tools/mock-api
npm ci
npm test
```

从仓库根目录运行：

```bash
npm ci --prefix tools/mock-api
npm test --prefix tools/mock-api
```

从仓库根目录启动：

```bash
npm start --prefix tools/mock-api
# 默认监听 http://127.0.0.1:8787

MOCK_PORT=8788 npm start --prefix tools/mock-api
MOCK_SSE_HEARTBEAT_MS=1000 npm start --prefix tools/mock-api
```

- 只监听 `127.0.0.1`，不提供 CORS，不接受外部网卡绑定。Mock 用同一个监听器承载 `/v1/pair` 和其余路由；它**不**模拟独立的配对监听器，也不模拟 mTLS 握手。
- 没有 `/health`；用 `GET /v1/me` 检查可用性。
- Mock 是单进程内存状态；重启后恢复到 `fixtures/api/` 的初始值。
- 测试可以启动随机端口，不依赖 `8787`。

## 2. 契约与 OpenAPI

`schemas/openapi.yaml` 是 OpenAPI 3.1 文档；内容使用 JSON（合法 YAML 子集），Node 直接 `JSON.parse`。它覆盖契约中的 **48 条路由**：47 条 mTLS API 路由 + 1 条配对路由。`GET /v1/results/{sha256}` 等结果读取路由没有加入，原因是 `docs/interfaces/api.md` OQ-11 尚未裁定。

Mock 的运行时行为由同一份 OpenAPI 驱动：

- 方法和路径从 `paths` 发现；
- 请求体按 operation 的 `requestBody.content["application/json"].schema` 校验；
- 响应按 operation 的响应 schema 校验；
- 数据来自 `fixtures/api/`，operation 的 `x-gantry-fixture` 扩展标出使用哪个夹具；
- 扩展字段：
  - `x-gantry-role`：最低角色或 `settings_dynamic`；
  - `x-gantry-mobile`：`false` / `true` / `limited`；
  - `x-gantry-idempotency`：`required` / `optional` / `not_applicable`；
  - `x-gantry-rate`：`read` / `write` / `message` / `sign` / `pair_ip`。

OpenAPI 中所有实体表示都含 `id`、`created_at`、`updated_at`（API 约定）；`data-model.md` 为了简洁省略了后两者，夹具已按 API 约定补齐。`IssueRef` / `PullRequestRef` 是嵌入快照，不是顶层实体，因此没有独立时间字段。

补全依据：

| 夹具补上的字段 | 依据 |
| --- | --- |
| 所有顶层实体的 `created_at` / `updated_at` | `api.md` §1：“所有实体表示都含 `id`、`created_at`、`updated_at`” |
| `PairResponse.device` 的时间字段 | 同上；配对的 `device` 是 `Device` 表示 |
| `Approval.consumed_by` | `data-model.md` §3 Approval 字段表 |
| `Device.approval_pubkey_fpr` | `data-model.md` §3 Device；只返回指纹，不返回公钥 |
| `BudgetLedger` 的 `day` / `template` / `created_at` / `updated_at` | `data-model.md` §3 BudgetLedger + §1 实体时间约定 |
| `Workflow` / `Node` / `Attempt` / `Worktree` 的时间字段 | `data-model.md` §3 + §1 实体时间约定 |
| 录制事件的完整信封 | `data-model.md` §5.1；`log_epoch` 不进信封，只进 heartbeat 与响应头 |

## 3. 固定夹具与 ID

### 3.1 设备与当前身份

| 内容 | ID / 值 |
| --- | --- |
| 当前桌面设备 | `dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM`（macOS，approver） |
| 手机设备 | `dev_01JB6Z4M8S3T6X9N2Q5V7W0YZN`（iOS，operator，无审批公钥指纹） |
| 当前档案 | `desktop` |
| 服务端时间（冻结） | `2026-10-24T12:41:03.456Z` |
| 证书续期时刻 | `2026-11-13T12:00:00.000Z` |

Device 响应只有 `pubkey_fpr` / `approval_pubkey_fpr`，**永远不返回 `approval_pubkey`**。配对请求可以携带 `approval_pubkey`；配对响应不包含它。

### 3.2 项目与交付

| 内容 | ID |
| --- | --- |
| 项目 `acme/web` | `prj_01JB7K2M9Q4X8V3N5R6T1W0YZA` |
| 项目 `acme/api` | `prj_01JB7K2N0R5X9V4P6S7T2W1YZB` |
| 进行中的交付单元 | `dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB` |
| 已关闭的交付单元 | `dlv_01JB7M5P9R3X7V2N6Q4T8W1YZD` |
| 预算台账 | `bdg_01JB7M4Q1T5X9V2N6R8K3W7YZC` |
| 工作树（engine 锁） | `wtr_01JB7M8Q2X5V9N3R6T1K4W0YZE` |
| 工作树（无锁） | `wtr_01JB7M8R3X6V0N4R7T2K5W0YZF` |
| 工作流 | `wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF` |
| build 节点 | `nod_01JB7M7V4X2N9R5T8Q3K6W1YZG` |
| verify 节点 | `nod_01JB7M7W5X3N9R6T8Q2K4V1YZH` |
| verify 的尝试 | `att_01JB7M9X5N3R8T2Q6K1V4W7YZH` |

进行中的交付单元：`level_cap = High`、`level_effective = Medium`、`stage = review`、`stage_status = running`，关联 PR `acme/web#12`，PR head 为 `9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345`。

### 3.3 对话与消息

| 内容 | ID |
| --- | --- |
| 主对话 | `cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC` |
| 用户消息 | `msg_01JB7N3R5T8X2V6M9Q4K1W7YZD` |
| agent 审批卡片消息 | `msg_01JB7N8T2X6V9M3Q5R1K4W7YZP` |
| 事件行 | `msg_01JB7NB4K8R2X6V9M3Q5T1W7YZ` |

`GET /v1/conversations/{id}/messages` 默认 `order=desc`；界面按 `asc` 展示时由客户端反转。

### 3.4 收件箱与审批

| 内容 | ID |
| --- | --- |
| `approval_needed` 上报项 | `inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ` |
| `escalation` 上报项（选项 `upgrade` / `narrow` / `abandon`） | `inb_01JB7P3M7S1T4X6N9Q2V5W8YZK` |
| 已 dismissed 的 `spec_needed` | `inb_01JB7P4N8T2X5V7P9Q3W6X0YZM` |
| pending 的 `merge_pr` 审批 | `apr_01JB7P4M8T2X6N9R3Q5K1V7WZK` |

### 3.5 设置 rev

| resource | 当前 rev |
| --- | --- |
| `models` | 18200 |
| `levels` | 18238 |
| `policies` | 18200 |
| `guardian` | 18150 |
| `sandbox` | 18100 |
| `egress` | 18050 |
| `notify` | 17900 |
| `github` | 17800 |

项目 `prj_01JB7K2M9Q4X8V3N5R6T1W0YZA` 有 `levels` 覆盖（`high.budget.nodes = 20`，rev 18150）和 `sandbox` 覆盖（`limits.mem_mb = 2048`，rev 18120）。

### 3.6 事件游标

`fixtures/api/events.json` 记录 7 条完整事件，`seq` 从 **18234** 到 **18240**，`head = 18240`，`log_epoch = 1`。事件类型：

- `delivery.stage_changed`（18234）
- `message.posted`（18235、18236、18240）
- `inbox.raised`（18237、18239）
- `settings.updated`（18238，全局）

每条 `data` 都是完整的 core 信封内容中的 `data` 对象。`settings.updated` 的 `value` 是已剥离 secret 的稀疏文档。没有把形状未定的 `approval.*`、`delivery.paused`、`device.*` 等事件写成录制数据。

录制窗口从 18234 开始；settings 表里早于 18234 的 rev 没有对应事件录制，属于窗口外历史，不代表夹具不一致。当前窗口内只有 `levels` 的一次 `settings.updated`（18238）。

## 4. 示例命令

```bash
# 当前设备
curl -sS -D - http://127.0.0.1:8787/v1/me

# 分页
curl -sS 'http://127.0.0.1:8787/v1/projects?limit=1'

# 消息
curl -sS 'http://127.0.0.1:8787/v1/conversations/cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC/messages'

# 发消息：Idempotency-Key 必带
curl -sS -D - -X POST \
  'http://127.0.0.1:8787/v1/conversations/cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC/messages' \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: demo-message-0000001' \
  -d '{"text":"继续处理登录超时","level_cap":"High","refs":[{"kind":"issue","repo":"acme/web","number":5}]}'

# 设置 dry_run（levels 当前 ETag）
curl -sS -D - -X PUT \
  'http://127.0.0.1:8787/v1/settings/levels?dry_run=true' \
  -H 'Content-Type: application/json' \
  -H 'If-Match: "18238"' \
  -d '{"value":{"high":{"budget":{"nodes":20}}}}'

# SSE：从头回放（无游标时首帧是 heartbeat，不回放；这里显式使用 cursor）
curl -N \
  -H 'Accept: text/event-stream' \
  -H 'Last-Event-ID: 18233' \
  'http://127.0.0.1:8787/v1/events?types=message.*,inbox.*&project_id=prj_01JB7K2M9Q4X8V3N5R6T1W0YZA'

# 配对（Mock 不验证 token / CSR；仅演示形状）
curl -sS -X POST http://127.0.0.1:8787/v1/pair \
  -H 'Content-Type: application/json' \
  -d '{
    "token":"gp1_demo",
    "csr":"-----BEGIN CERTIFICATE REQUEST-----\nMOCK\n-----END CERTIFICATE REQUEST-----\n",
    "approval_pubkey":"BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7ZaASo8h6hMNHwYgitHzpH-S9kLN8ImOTHNk0q-vuB-sTk",
    "device":{"name":"Demo Mac","platform":"macos"}
  }'
```

所有响应带 `Cache-Control: no-store`、`X-Request-Id`、`X-Log-Epoch`、`RateLimit-*`，成功响应另带 `X-Event-Seq`。设置响应带 `ETag`。错误响应为 `application/problem+json; charset=utf-8`。

## 5. SSE 的具体行为

严格按 `api.md` §5 实现：

- `Last-Event-ID` **优先**于查询参数 `after`；两者都存在时只使用头。
- 游标是十进制整数，允许 `0`；不允许负数、前导零、小数或超过 `Number.MAX_SAFE_INTEGER`。非整数头返回 `400 invalid_cursor`。
- 无游标：不重放，首帧是 `heartbeat`，`id` 为当前 head（18240）。
- 有游标：回放 `(游标, head]` 内通过过滤的事件，再发一个 `heartbeat`。
- `heartbeat` 的 `id` 始终是**已扫描到的最大 seq**（包括被过滤的事件），所以过滤流也能推进游标；Mock 的 head 固定为 18240。
- `heartbeat.data` 含 `seq`、`ts`、`log_epoch`。
- 过滤：
  - `project_id` 可重复；事件信封 `project_id = null` 的全局事件始终下发。`settings.updated` 全局事件带 `project_id: null`，因此即使请求了项目过滤也会收到。
  - `types` 是逗号分隔的精确类型或 `前缀.*`。
- 游标大于 head 返回 `409 cursor_ahead`（普通 problem 响应，不开流）。
- 每 15 秒心跳；`MOCK_SSE_HEARTBEAT_MS` 可改，测试用短间隔。
- Mock **不**模拟手机档案的 `settings.*` 可见性，也**不**模拟角色过滤；它固定按桌面 approver 视角提供录制事件。真实 `api` 必须按 `api.md` §5.4 再做档案 / 角色门禁。
- Mock **不**发送 `close` 帧；没有真实吊销、证书到期或服务端关机事件。
- 写路由**不追加**新事件；录制流只回放固定事件。需要观察写入后的状态时重新 GET 资源。
- SSE 的 `data:` 行为完整 core 信封的单行 JSON；已录制事件仅使用 `delivery.stage_changed`、`message.posted`、`inbox.raised`、`settings.updated` 四种形状明确的事件。

## 6. 写路由与 Mock 边界

### 6.1 已实现的通用语义

- 请求体使用 OpenAPI request schema 严格校验；未知字段、类型、枚举、范围错误返回 `422 validation_failed`，`errors[].pointer` 相对请求体。
- 非 `application/json` 返回 `415 unsupported_media_type`；超过 64 KiB（设置 256 KiB）返回 `413 payload_too_large`。
- 必带 `Idempotency-Key` 的路由缺少头返回 `400 idempotency_key_required`；格式必须为 16–64 个 `[A-Za-z0-9_-]`。同 key 同内容重放首次响应并加 `Idempotent-Replayed: true`；同 key 不同内容返回 `422 idempotency_key_reuse`。
- 设置 `PUT` / 项目 `DELETE` 强制 `If-Match`：缺失 `428 precondition_required`，不匹配 `412 precondition_failed`（带当前 `ETag`）。
- 分页为 keyset 游标，`cursor` 不透明；默认 50、最大 200。
- 未知查询参数或查询类型错误返回 `400 invalid_request`；未知路径 `404 not_found`；路径存在但方法不对 `405 method_not_allowed` 且带 `Allow`。
- 所有错误响应带 `X-Request-Id`；所有响应带 `X-Log-Epoch`、`RateLimit-*`；无 CORS 响应头。

### 6.2 只做脚本响应，不做真实业务

Mock 的写路由只更新内存夹具并返回契约形状。它**不**实现：

- 真实 mTLS、证书校验、设备鉴权、角色 / 档案判定；
- 真实限流（只返回静态 `RateLimit-*` 头）；
- 引擎状态机、队列、重试调度；
- `policy` / Outbox / 真实审批签名验证；
- SSE 吊销、证书到期或 `close`；
- `merge_pr` 等 GitHub 动作。

具体限制：

- `pair` 不验证 token、CSR 或公钥；桌面档案缺 `approval_pubkey` 时返回 422（前端形状检查），但不验证其与 CSR 公钥不同。
- `grant` 只要求非空 `nonce` 与 86 字符 base64url 签名，不验签；`deny` 不要求签名。
- 设备改角色 / 吊销不要求 `Gantry-Approval`；手机设备设 approver 与 self role change 返回 409（脚本规则）。
- 设置 `dry_run` 始终返回 `valid: true`、`required_role: "operator"`、`requires_signature: false`；**没有实现** config.md V01–V22 的语义校验、放宽差分、签名判定或真实角色检查。
- 设置 `view=effective` 只做简单的 JSON 对象合并（数组整体替换），**没有** clamp / `clamped[]` 计算；夹具里始终为空数组。
- 设置 `?rev=N` 只返回当前文档，不提供真实历史。
- 任何 secret `value` 在写入时被剥离，读响应、事件、日志不包含它；Mock 不写入真实密钥存储。
- 推送令牌只存在进程内，不出现在任何读响应或事件中；设备吊销时清除。
- 发消息只把 user 消息追加到内存；不触发意图解析、agent 回复或交付单元。
- 暂停 / 继续 / 放弃 / 调档 / 忽略 / 选择 / 工作树锁只更新简单状态；不派生事件。
- `GET /v1/events` 只回放 `fixtures/api/events.json`，不读取写操作状态。

因此，Mock 可用于前端布局、列表分页、详情、错误渲染、幂等重试和 SSE 续传的联调；不能用来证明安全边界、审批正确性或真实引擎行为。

## 7. 明确的数据与契约缺口

| 项 | 处理 |
| --- | --- |
| `warnings[]` 元素形状 | `api.md` / `config.md` 只给出空数组示例，未定义元素字段。OpenAPI 只声明为数组；Mock 固定返回空数组。不要据此实现警告 UI 的字段。 |
| `settings/status` 的 `detail` | config.md 没有给形状；Mock 留作可选，夹具只给字符串示例。 |
| 事件 `data` 形状 | `core.md` §7.5 仍有未定项。Mock 只为 data 已明确的四种事件提供录制回放；未定事件不伪造。完整事件契约未冻结。 |
| 结果读取路由 | `OQ-11` 未定；没有 `GET /v1/results/...`。 |
| `Approval` 的 `subject_digest` | 不同动作形状不同（40 位 head SHA 或 `sha256:…`），OpenAPI 保留为字符串；不要在前端做统一长度假设。 |
| settings 的 `value` 判定 | OpenAPI 用八个 resource schema 的联合；Mock 按路径 resource 选择夹具。前端应按 resource 使用生成的类型，不要把 `value` 当无类型 Map。 |
| 客户端时间 | Mock 时钟冻结，`server.time` 不会前进；前端不应依赖它判断“刚刚发生”。 |
| mTLS / CORS | Mock 无 TLS、无鉴权、无 CORS；真实 API 强制 mTLS，且同样不提供 CORS。 |

## 8. 接入真实服务时替换什么

1. Base URL 从 `http://127.0.0.1:8787` 改为配对响应的 `api_base_url`。
2. 由 `desktop-shell` 加载 mTLS 客户端证书与私钥；`desktop-ui` 不直连。
3. 保留请求形状、`Idempotency-Key`、`If-Match`、错误按 `code` 分支的处理。
4. SSE 继续使用 `Last-Event-ID` 或 `after`，并保存 `X-Log-Epoch` / `heartbeat.log_epoch`；真实服务会发送 `close` 并执行角色 / 档案过滤。
5. 移除任何依赖 Mock 冻结时间、内存状态或宽松干跑结果的测试断言。
