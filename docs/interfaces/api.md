# 客户端 API 契约（`api`）

> 状态：设计 v0.1 · 里程碑：M0 冻结契约，M1 起实现 · 泳道：`sec`（鉴权）、`be-engine`（路由实现）、`fe-desktop`、`fe-mobile` · 机器可读版本：`schemas/openapi.yaml`（由本文派生，冲突以本文为准）

客户端（桌面端、手机端）与服务端之间的全部通道：HTTPS + mTLS 的 JSON API、SSE 事件流，外加一个单向 TLS 的配对端点。实现 `api` 模块、`desktop-shell` / `desktop-ui`、`mobile`、Mock API 服务的 agent 只读本文即可互相对得上。

**约定与权威关系**

- 实体字段以 [data-model.md](../data-model.md) 为准，本文不新增实体字段；响应里 data-model 没给出线上形态的类型（`Ref`、`Card`、`UntrustedText`、`CheckSummary` 等）按本文示例的形态，并登记在 [§10 开放问题](#10-开放问题) OQ-10，待回写 data-model。
- 标注 **[提议]** 的是骨架没定、本文给的默认值；全部数值默认值集中在 [§3.7](#37-默认参数)；骨架没定又无法给默认的，写在 §10，**不要自行拍板**。
- 所有实体表示都含 `id`、`created_at`、`updated_at`（data-model §3 省略了，示例为节省篇幅也省略）。
- 同批编写的配套契约：[config.md](config.md)（settings 的 resource、键、取值、合并 / 收紧、校验、角色细则；**settings 的 HTTP 形状以本文为准**）、[forge-actions.md](forge-actions.md)（审批里 `action` 的取值）、[worker-protocol.md](worker-protocol.md)（与客户端无关，仅界定 Worker 端点不属于本 API）。尚未存在：`modules/pki.md`、`schemas/events/*.json`。
- 桌面端的 HTTP 请求**只由 `desktop-shell` 发出**（它持有 mTLS 密钥与证书，architecture §4）；`desktop-ui` 经 IPC 调用，不直连服务端。下文"客户端"指 shell 的网络层或 `mobile` 的网络层。

---

## 1. 传输与版本

### 1.1 入口一览

| 入口 | 访问者 | 传输 | 鉴权 | 归属文档 |
| --- | --- | --- | --- | --- |
| mTLS API `/v1/*` | 桌面端、手机端 | HTTPS，强制客户端证书 | 设备证书 → Device → role | 本文 |
| 配对端点 `POST /v1/pair`（独立监听器） | 未配对设备 | HTTPS，**单向 TLS** | 一次性 token + CA 指纹固定 | 本文 §1.3 |
| Webhook `/webhook` | GitHub | HTTPS（公网直连，或只转发 `/webhook` 的隧道，P-13） | HMAC | forge / deploy 规格，**不属于本 API** |
| Worker 协议端点 | 沙盒内 worker | Unix socket / 内网 | 每沙盒短期 token | interfaces/worker-protocol.md，**不属于本 API** |

- Webhook 与 Worker 端点不在 `/v1` 下、不在 API 监听器上；API 监听器对 `/webhook` 返回 404。内网部署的隧道只转发 `/webhook`，因此 mTLS API 不会经隧道暴露。
- 客户端不会、也不应调用 Webhook 与 Worker 端点（I6）。

### 1.2 mTLS API 监听器

| 项 | 规则 |
| --- | --- |
| TLS | 仅 TLS 1.3 [提议，来源 design.md]；`client_auth = required`：无证书或证书无效 → 握手失败，**不产生任何 HTTP 响应** |
| 握手校验 | 证书链到服务端 CA、在有效期内、`cert_serial` 不在吊销表（吊销表由服务端握手时查询，无 CRL / OCSP） |
| HTTP | HTTP/1.1 与 HTTP/2；请求与响应 `application/json; charset=utf-8`；事件流 `text/event-stream`；错误 `application/problem+json` |
| CORS | 不提供。客户端是原生进程；WebView 不直连服务端 |
| 地址 | 来自配对串 / 配对响应的 `api_base_url`；端口由部署配置决定，本文不固定 |
| 缓存 | 默认 `Cache-Control: no-store`；设置资源例外（§4.9，ETag 条件请求） |

### 1.3 配对端点（唯一的单向 TLS 端点）

| 项 | 规则 |
| --- | --- |
| 监听器 | **与 mTLS API 分开的监听端口**，只路由 `POST /v1/pair` 一条；其他路径一律 404 |
| 服务端身份 | 配对串携带 CA 指纹；客户端**不用系统信任库**，只接受链到该指纹的服务端证书（防中间人） |
| 开启时机 | 仅在 `gantry pair` 的有效窗口内开启监听 [提议]；窗口外端口关闭（OQ-12） |
| 限流 | 按源 IP（§3.5）；token 单次有效、短 TTL、失败若干次作废（design.md，细节见 modules/pki.md） |

**为什么用独立端口而不是同端口的另一个路径：**

1. **fail-closed**：API 监听器的 `client_auth` 为 required，握手阶段就挡住所有未配对者，任何一条路由忘记写鉴权也不可能被未配对者触达。同端口放行配对路径意味着客户端证书"可选"，鉴权从握手层下沉到应用层，一个漏检就 fail-open；且 rustls 的一个 `ServerConfig` 只能配一种客户端验证策略。
2. **暴露面可收缩**：配对监听器可以只在配对窗口内存在；API 端口常开。
3. **未认证面可审计**：单向 TLS 面上恰好 1 条路由，评审和模糊测试范围固定。
4. **限流、日志、告警分开**：配对按源 IP、API 按设备；配对失败是安全信号，应单独告警。

**`POST /v1/pair`**（无角色；鉴权 = token）

请求：

```json
{
  "token": "gp1_4Ww9rX0b2sQ7hYk1",
  "csr": "-----BEGIN CERTIFICATE REQUEST-----\nMIIB...\n-----END CERTIFICATE REQUEST-----\n",
  "device": { "name": "Susan 的 MacBook", "platform": "macos" }
}
```

响应 `201`：

```json
{
  "device": {
    "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
    "name": "Susan 的 MacBook",
    "platform": "macos",
    "cert_serial": "5f3a9c01d27be4468a1c",
    "pubkey_fpr": "sha256:9fe698148e3e8234105e25bf598c9689640aba943b8275348470152799f1dd42",
    "role": "approver",
    "status": "active",
    "last_seen": null
  },
  "certificate_chain": [
    "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----\n",
    "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----\n"
  ],
  "api_base_url": "https://gantry.example.com:8443",
  "ca_fingerprint": "sha256:3b7e0c52a91d6f48e2b05c7a9d1f3e64b8a20c5d7e91f3a6b4c8d0e2f1a39b57",
  "confirmation_code": "482913"
}
```

- `token` 是不透明字符串（示例仅为示意）。客户端本地生成密钥对，**私钥不出设备**；CSR 的 subject 由服务端忽略并重写（`CN = device.id`），密钥算法要求见 modules/pki.md（本文按 ECDSA P-256，OQ-6）。
- `certificate_chain` 为 `[叶子, 中间 CA]`；客户端必须验证链到配对串里的 CA 指纹，再存入系统钥匙串 / 凭据存储。
- `confirmation_code`（6 位）与服务端控制台显示的一致，由用户人工核对（architecture §7.1 步骤 5）。核对失败的处置、设备在核对前是否已 `active`：OQ-12。
- token 无效、已用、过期、被作废**统一**返回 `401 pair_token_invalid`，不区分原因（不给枚举 oracle）。响应丢失后 token 已被消耗，只能重新生成 token 再配对（本端点不支持 `Idempotency-Key`）。
- 配对成功写 `device.paired` 事件。新设备的 `role` 取值规则：OQ-1。

### 1.4 版本与兼容

| 规则 | 说明 |
| --- | --- |
| 路径前缀 | `/v1`。破坏性变更只能发 `/v2`，两个主版本并行至少一个里程碑 |
| v1 内允许 | 新增路由、新增可选请求字段、新增响应字段、新增枚举值（data-model §1："只追加"）、新增事件类型、新增 Card `kind` |
| 客户端必须 | **忽略**未知响应字段、未知枚举值（按"未知"降级显示）、未知事件类型、未知 Card `kind`；不得因此崩溃 |
| 服务端必须 | 请求体严格校验：未知字段 → `422 validation_failed`（不是静默忽略，I7 要用到，见 §8.2） |
| 探测 | `GET /v1/me` 返回 `server.api_version` 与服务端时间 |

### 1.5 数据约定

| 项 | 规则 |
| --- | --- |
| ID | 带前缀的 ULID 字符串（data-model §2），客户端视为不透明 |
| 时间 | UTC，RFC 3339，毫秒精度，`Z` 结尾 |
| 枚举 | 小写字符串；`verdict` 沿用 data-model 的大写（`ACCEPT` 等）。`Level` 线上取值用小写 `none / low / medium / high / max / ultimate`（OQ-10） |
| `UntrustedText` | `{ "text": string, "source": string, "truncated": boolean }`（形态见 OQ-10） |
| 不可信文本渲染 | `UntrustedText`、`Message.text`（`role` 为 `agent` / `event`）、`InboxItem.summary`、Issue / PR 标题一律当**数据**：按纯文本或经消毒的受限 Markdown 渲染；不渲染 HTML；不自动加载远程资源；链接只在系统浏览器打开并先显示目标主机。服务端错误的 `detail` 同理，只写日志，不直接展示（展示按 `code` 本地化） |
| JSON | 无注释、无尾逗号；整数不超过 2^53−1 |

---

## 2. 鉴权与角色

### 2.1 每个请求的判定链

| 步骤 | 判定 | 失败 |
| --- | --- | --- |
| 1. 握手 | 客户端证书有效、未过期、序列号未吊销（§1.2） | 握手失败（无 HTTP 响应） |
| 2. 映射 | `cert_serial` → `Device`；`Device.status = active` | `401 device_revoked`（长连接上途中被吊销时） |
| 3. 角色 | `Device.role` ≥ 路由最低角色（实时查库，**不缓存**：改角色、吊销对新请求立即生效） | `403 forbidden_role` |
| 4. 档案 | 路由在该设备客户端档案的可用子集内（§2.3） | `403 forbidden_profile` |
| 5. 限流 | §3.5 | `429 rate_limited` |
| 6. 处理 | 写操作的事件 `actor` 记为 `{ "kind": "device", "id": "dev_…" }` | — |

- 骨架没有"用户"实体（Q-01 待决），操作主体就是 Device，审计也按 Device 记（OQ-15）。
- 握手失败对客户端表现为 TLS 层错误，与断网无法直接区分。客户端连续多次得到 TLS 告警（而非超时）时，应提示"设备可能已被吊销或证书已过期"，由用户决定是否清除本地密钥并重新配对。
- 证书短期有效；续期路由骨架未定（OQ-12）。

### 2.2 角色语义（层级：`approver` ⊃ `operator` ⊃ `viewer`）

| 角色 | 可以做 |
| --- | --- |
| `viewer` | 读取除 settings 之外的所有资源；订阅事件流 |
| `operator` | viewer + 发消息、调档 / 放弃交付单元、解除收件箱项、工作树人工锁、**读 settings 并写其中的中性 / 收紧类改动**、重命名设备 |
| `approver` | operator + 获取审批 challenge 并签名 / 拒绝审批、放宽类、"仅 approver 键"与带 secret `value` 的 settings 写入（config.md §2.5，[提议]）、改设备角色、吊销设备 |

> 骨架只给了角色名；语义取自 design.md"只读 / 控制 / 可审批危险动作"，该文档是历史记录，不是规格。OQ-1。

### 2.3 客户端档案与手机端子集

角色回答"能做哪类事"，**档案**回答"这个客户端是什么形态"。档案由 `Device.platform` 推出：

| 档案 | 判定 | 可用路由 |
| --- | --- | --- |
| `desktop` | `platform` ∈ {`windows`, `macos`} | 全部，按角色 |
| `mobile` | `platform` = `ios` | 仅下表 9 条（D-11：手机端只给 agent 发消息） |

> 角色只有三级，无法表达"只做对话"，所以另设档案（OQ-2：也可改为新增角色，需改 data-model）。`platform` 取值集合是假设（OQ-10）。

**手机端默认角色 `operator`** [提议]：发消息需要 operator；M4 要求"默认不可审批"，所以不是 approver。即使设备被改成 approver，手机档案下审批路由仍是 `403 forbidden_profile`（OQ-2）。

手机端可用路由：

| 路由 | 用途 |
| --- | --- |
| `GET /v1/me` | 角色、档案、服务端时间 |
| `GET /v1/projects`、`GET /v1/projects/{project_id}` | 选项目 |
| `GET /v1/projects/{project_id}/conversations` | 找到对话 |
| `GET /v1/conversations/{conversation_id}/messages` | 读消息 |
| `POST /v1/conversations/{conversation_id}/messages` | 发消息（可带 `level_cap`、`refs`） |
| `GET /v1/inbox`、`GET /v1/inbox/{inbox_id}` | 读收件箱（只读，不能 dismiss） |
| `GET /v1/events` | 事件流；服务端只下发 `project.*`、`message.*`、`inbox.*`、`device.revoked` [提议] |

手机端不提供：交付单元、工作树、工作流、预算、审批、settings、设备管理。离线推送令牌的注册路由骨架未定（M4 的 `notify` APNs 实现，OQ-2）。

---

## 3. 通用约定

### 3.1 错误：`application/problem+json`（RFC 9457）

所有非 2xx 响应（含 404、405、429、5xx 与路由不存在）都用该格式：

```json
{
  "type": "urn:gantry:problem:stale_head",
  "title": "Approval head is stale",
  "status": 409,
  "code": "stale_head",
  "detail": "PR head moved from 9f2c4e7a1b3d to 41ab93c0d2e4; the approval was invalidated.",
  "instance": "/v1/approvals/apr_01JB7P4M8T2X6N9R3Q5K1V7WZK/grant",
  "request_id": "01JB7P9Z3K5R8T2X6N1Q4V7WYA",
  "current_head_sha": "41ab93c0d2e4f6a81b3c5d7e9f0a2b4c6d8e0f13",
  "approval_status": "invalidated"
}
```

| 字段 | 规则 |
| --- | --- |
| `type` | `urn:gantry:problem:<code>` |
| `code` | 稳定的机器码，客户端据此分支与本地化；只增不改 |
| `detail` | 面向日志的英文说明，不展示给用户 |
| `request_id` | 同时出现在响应头 `X-Request-Id` |
| 扩展成员 | 按 `code` 附加（如 `required_role`、`errors`、`current_head_sha`、`lock`），见下表 |
| `errors` | 仅 `validation_failed`：`[{ "pointer": "/refs/0/number", "code": "required \| type \| enum \| range \| unknown_field", "message": "…" }]`；`pointer` 为 JSON Pointer（相对请求体；settings 相对 `value`，跨 resource 校验的错误指向本次写入的 resource）。settings 的 `code` 在上述之外追加 config.md §11 的语义码（如 `secret_ref_missing`、`override_loosens_global`） |

| HTTP | `code` | 触发 | 客户端动作 |
| --- | --- | --- | --- |
| 400 | `invalid_request` | JSON 语法错误、查询参数类型错、未知查询参数 | 视为客户端 bug，不重试 |
| 400 | `idempotency_key_required` | 必带 `Idempotency-Key` 的路由缺头 | 同上 |
| 400 | `invalid_cursor` | 分页游标无法解析 | 丢弃游标重新从头取 |
| 401 | `device_revoked` | 长连接途中设备被吊销 | 停止所有请求，清除本地密钥与证书，提示重新配对 |
| 401 | `pair_token_invalid` | 仅配对端点 | 提示用户重新生成配对串 |
| 403 | `forbidden_role` | 角色不足；附 `required_role` | 隐藏 / 禁用入口，提示需要的角色 |
| 403 | `forbidden_profile` | 路由不在客户端档案子集内 | 客户端 bug，不重试 |
| 403 | `signature_invalid` | 审批签名验证失败 | 重新取 challenge，重签；反复失败则报告 |
| 403 | `signature_required` | 写入需要设备签名但未提供（settings，OQ-16） | 按 `dry_run` 的 `requires_signature` 先走签名 |
| 404 | `not_found` | 资源不存在，或路由不存在 | 刷新列表 |
| 405 | `method_not_allowed` | 路径存在但方法不对；附 `Allow` 头 | 客户端 bug |
| 409 | `conflict` | 状态冲突；附 `reason`（如 `self_role_change`、`invalid_state`） | 刷新后让用户重新决定 |
| 409 | `idempotency_in_progress` | 同一 `Idempotency-Key` 的首次请求仍在处理 | 退避后用同一 key 重试 |
| 409 | `worktree_locked` | 工作树锁被其他持有者占用；附 `lock` | 展示持有者与 `lease_until` |
| 409 | `approval_not_pending` | 审批已非 `pending`；附 `approval_status` | 刷新审批，展示最终状态 |
| 409 | `challenge_invalid` | nonce 不匹配 / 已用 / 已过期 / 本设备没有 challenge；附 `reason` | 重新 `GET challenge`，再签 |
| 409 | `stale_head` | PR head 已变，审批被置为 `invalidated`；附 `current_head_sha` | 展示"代码已更新，审批失效"，等待重新请求 |
| 409 | `cursor_ahead` | `Last-Event-ID` 大于服务端 head（如服务端从备份恢复） | 丢弃本地缓存，重拉快照后无游标订阅 |
| 410 | `cursor_expired` | 游标早于事件保留窗口（预留，OQ-13） | 同 `cursor_ahead` |
| 412 | `precondition_failed` | `If-Match` 与当前 ETag 不符；响应头带当前 `ETag` | 重新 GET，让用户在新值上重做 |
| 413 | `payload_too_large` | 超过 §3.7 的体积上限 | 不重试 |
| 415 | `unsupported_media_type` | 非 `application/json` | 客户端 bug |
| 422 | `validation_failed` | 语义 / schema 校验失败，含未知字段；附 `errors` | 在表单上标出 |
| 422 | `not_overridable` | 项目级覆盖了 config.md 标为不可覆盖的 resource（`notify`、`github`） | 禁用该入口 |
| 422 | `idempotency_key_reuse` | 同一 key 配了不同的请求内容 | 客户端 bug：换新 key |
| 428 | `precondition_required` | 写 settings 缺 `If-Match` | 客户端 bug |
| 429 | `rate_limited` | 超限；`Retry-After` 秒数 | 按 `Retry-After` 退避，不要立即重试 |
| 500 | `internal` | 服务端未预期错误 | 退避重试（读）；写操作用同一 `Idempotency-Key` 重试 |
| 503 | `unavailable` | 启动中 / 存储不可用；`Retry-After` | 同 429 |

### 3.2 分页（游标）

- 列表响应：`{ "items": [ … ], "next_cursor": "opaque-or-null" }`。`next_cursor` 为 `null` 表示没有更多。
- 请求参数：`limit`（默认 50，最大 200）、`cursor`（上一页的 `next_cursor`）、`order`（`asc` / `desc`）。
- 游标是**不透明**的 keyset 游标，基于 ULID `id` 排序，不随插入漂移；客户端不得解析、不得跨资源复用。
- 默认排序 `id desc`（新的在前）。`messages` 默认 `desc`：先取最新一页，向上滚动时用 `next_cursor` 取更早的；界面按 `asc` 展示由客户端自己反转。
- 筛选参数各路由单独列出；未知参数 → `400 invalid_request`。

### 3.3 幂等：`Idempotency-Key`

| 项 | 规则 |
| --- | --- |
| 头 | `Idempotency-Key: <16–64 字符 [A-Za-z0-9_-]>`，建议 ULID / UUID，由客户端为每次**用户意图**生成一次，重试沿用 |
| 必带 | `POST …/messages`、`POST …/grant`、`POST …/deny`（缺失 → `400 idempotency_key_required`） |
| 可带 | 其余 `POST` 状态流转。它们本身按"目标状态幂等"：已在目标状态则 `200` 返回当前表示（abandon 已放弃的、dismiss 已 dismissed 的、revoke 已吊销的） |
| 不适用 | `GET` / `PUT` / `PATCH` / `DELETE`（天然幂等或用 `If-Match`）；`POST /v1/pair` |
| 作用域 | `(device_id, key)`；服务端保存请求指纹（方法 + 路径 + 正文哈希）与首次响应 |
| 重放 | 同 key 同指纹：原样返回首次响应（状态码 + 正文），加头 `Idempotent-Replayed: true` |
| 冲突 | 同 key 不同指纹 → `422 idempotency_key_reuse`；首次仍在处理 → `409 idempotency_in_progress` |
| 失败 | 首次响应是 5xx 时不记录，允许同 key 重试 |
| 保留 | 24 小时 [提议] |

这是 API 层去重，**与 Outbox 的 `idempotency_key`（data-model §3 OutboxAction，由交付单元 / 动作 / 目标 / 内容哈希派生）是两回事**：前者防"同一次点击发两条消息"，后者防"同一个 GitHub 动作执行两次"（I2）。

### 3.4 并发控制：ETag / If-Match（settings）

- 每个 `(scope, resource)` 独立：ETag 是引号包裹的修订号 `"<rev>"`，`rev` = 该 `(scope, resource)` 最近一次 `settings.updated` 事件的 `seq`，从未写过为 `0`（config.md §2.3）。`GET` 响应头返回；客户端视为不透明。写响应的头 `X-Event-Seq` 等于新 `rev`。
- `PUT` / `DELETE` 必须带 `If-Match: "<etag>"`：缺失 → `428 precondition_required`；不符 → `412 precondition_failed`（响应头带当前 ETag）。成功响应头带新 ETag。
- `GET` 支持 `If-None-Match`，命中返回 `304`。
- settings 以外的资源不用 ETag：状态由服务端状态机串行化，冲突以 `409` 表达。

### 3.5 限流

- 按设备（配对端点按源 IP）令牌桶，服务端静态配置，**不经 API 修改**（不是 settings，I7 不适用）。
- 响应头（所有响应）：`RateLimit-Limit`、`RateLimit-Remaining`、`RateLimit-Reset`（秒）；超限 `429` + `Retry-After`。
- 分类与默认值见 §3.7。事件流重连风暴同样会被 `429`，客户端必须遵守 `Retry-After` 并加抖动。

### 3.6 读一致性：`X-Event-Seq`

- **每个成功响应**带头 `X-Event-Seq: <seq>`：读响应 = 该快照已反映到的最大事件 `seq`（服务端须在同一读事务里读投影与 `seq`）；写响应 = 本次写产生的最后一个事件的 `seq`。
- 客户端用它衔接"快照 + 事件流"：拉快照 → 取 `X-Event-Seq` → 以它作为游标订阅 `/v1/events`（§5.3）。
- 一次初始化要拉多个资源 / 多页时，取其中**最小**的 `X-Event-Seq` 作游标；重叠的事件靠 `seq` 幂等丢弃。

### 3.7 默认参数

以下全是 **[提议]** 默认值（OQ-14），实现时做成服务端静态配置常量，不要散落在代码里。

| 参数 | 默认 |
| --- | --- |
| 事件流心跳间隔 / 客户端判定断线 | 15 s / 45 s 无任何字节 |
| 重连退避 | 1 s 起指数增长，上限 30 s，全抖动 |
| 每设备并发事件流 | 3 |
| 分页 `limit` | 默认 50，最大 200 |
| 幂等键保留 | 24 h |
| 审批 challenge TTL | 120 s（不超过 `Approval.expires_at`） |
| 工作树人工锁租约 | 默认 1800 s，最长 14400 s |
| 请求体上限 | 64 KiB；settings 256 KiB |
| `message.text` / `refs` | ≤ 16 KiB / ≤ 20 条 |
| 限流（每设备） | 读 600/min；写 120/min；发消息 30/min；challenge 30/min；配对 5/min/源 IP |
| 客户端时钟偏差告警 | 与 `/v1/me` 的 `server.time` 相差 > 60 s |
| 吊销后事件流断开 | 目标立即，**上限 60 s**（milestones M3 退出标准） |

---

## 4. 资源与路由

### 4.1 路由总表

路径均带 `/v1` 前缀。"手机"列：✓ = 手机档案可用。"幂等键"：必 = 必带 `Idempotency-Key`。

| # | 方法 | 路径 | 最低角色 | 手机 | 说明 | 详述 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | POST | `/v1/pair`（配对监听器） | —（token） | — | 提交 CSR 换证书 | §1.3 |
| 2 | GET | `/v1/me` | viewer | ✓ | 本设备、角色、档案、服务端时间 | §4.2 |
| 3 | GET | `/v1/projects` | viewer | ✓ | 项目列表 | §4.3 |
| 4 | GET | `/v1/projects/{project_id}` | viewer | ✓ | 项目详情 | §4.3 |
| 5 | GET | `/v1/deliveries` | viewer | — | 交付单元列表 | §4.4 |
| 6 | GET | `/v1/deliveries/{delivery_id}` | viewer | — | 交付单元详情 | §4.4 |
| 7 | PATCH | `/v1/deliveries/{delivery_id}` | operator | — | 调整档位上限 `level_cap` | §4.4 |
| 8 | POST | `/v1/deliveries/{delivery_id}/abandon` | operator | — | 放弃 | §4.4 |
| 9 | GET | `/v1/deliveries/{delivery_id}/budget` | viewer | — | 预算台账（只读） | §4.4 |
| 10 | GET | `/v1/projects/{project_id}/conversations` | viewer | ✓ | 项目内对话列表 | §4.5 |
| 11 | GET | `/v1/conversations/{conversation_id}/messages` | viewer | ✓ | 读消息 | §4.5 |
| 12 | POST | `/v1/conversations/{conversation_id}/messages` | operator | ✓ | 发消息（幂等键：必） | §4.5 |
| 13 | GET | `/v1/worktrees` | viewer | — | 工作树列表 | §4.6 |
| 14 | GET | `/v1/worktrees/{worktree_id}` | viewer | — | 工作树详情 | §4.6 |
| 15 | PUT | `/v1/worktrees/{worktree_id}/lock` | operator | — | 取得 / 续租人工锁 | §4.6 |
| 16 | DELETE | `/v1/worktrees/{worktree_id}/lock` | operator | — | 释放人工锁 | §4.6 |
| 17 | GET | `/v1/deliveries/{delivery_id}/workflows` | viewer | — | 交付单元的各阶段图 | §4.7 |
| 18 | GET | `/v1/workflows/{workflow_id}` | viewer | — | 图详情（含节点） | §4.7 |
| 19 | GET | `/v1/nodes/{node_id}/attempts` | viewer | — | 节点的尝试列表 | §4.7 |
| 20 | GET | `/v1/attempts/{attempt_id}` | viewer | — | 尝试详情 | §4.7 |
| 21 | GET | `/v1/inbox` | viewer | ✓ | 收件箱列表 | §4.8 |
| 22 | GET | `/v1/inbox/{inbox_id}` | viewer | ✓ | 收件箱项详情 | §4.8 |
| 23 | POST | `/v1/inbox/{inbox_id}/dismiss` | operator | — | 忽略 | §4.8 |
| 24 | GET | `/v1/approvals` | viewer | — | 审批列表 | §6.1 |
| 25 | GET | `/v1/approvals/{approval_id}` | viewer | — | 审批详情 | §6.1 |
| 26 | GET | `/v1/approvals/{approval_id}/challenge` | approver | — | 取签名 challenge | §6.2 |
| 27 | POST | `/v1/approvals/{approval_id}/grant` | approver | — | 提交签名放行（幂等键：必） | §6.4 |
| 28 | POST | `/v1/approvals/{approval_id}/deny` | approver | — | 拒绝（幂等键：必） | §6.1 |
| 29 | GET | `/v1/settings/{resource}` | operator | — | 读全局设置（`?rev=N`） | §4.9 |
| 30 | PUT | `/v1/settings/{resource}` | operator* | — | 写全局设置（If-Match；`?dry_run`） | §4.9 |
| 31 | GET | `/v1/projects/{project_id}/settings/{resource}` | operator | — | 读项目覆盖 / 生效值 | §4.9 |
| 32 | PUT | `/v1/projects/{project_id}/settings/{resource}` | operator* | — | 写项目覆盖（If-Match；`?dry_run`） | §4.9 |
| 33 | DELETE | `/v1/projects/{project_id}/settings/{resource}` | operator* | — | 清除项目覆盖（If-Match） | §4.9 |
| 34 | GET | `/v1/settings/catalog` | operator | — | 设置页表单目录 | §4.9 |
| 35 | GET | `/v1/settings/status` | operator | — | 各模块已应用的 rev 与健康 | §4.9 |
| 36 | GET | `/v1/devices` | viewer | — | 设备列表 | §4.10 |
| 37 | PATCH | `/v1/devices/{device_id}` | operator | — | 重命名 | §4.10 |
| 38 | PUT | `/v1/devices/{device_id}/role` | approver | — | 改角色 | §4.10 |
| 39 | POST | `/v1/devices/{device_id}/revoke` | approver | — | 吊销 | §4.10 |
| 40 | GET | `/v1/events` | viewer | ✓ | SSE 事件流 | §5 |

合计 **40 条**：mTLS API 39 条 + 配对 1 条；手机档案 9 条（#2 #3 #4 #10 #11 #12 #21 #22 #40）。

`*` settings 的"最低角色"是**路由下限**：服务端按改动**内容**判定实际所需角色（config.md §2.5：中性 / 收紧 → operator；放宽、"仅 approver 键"、任何带 secret `value` 的写入 → approver，放宽类另需设备签名 [提议]），`dry_run` 先告知 `required_role` / `requires_signature`（§4.9）。读类 settings 路由 operator 起，viewer 不能读 settings。

> 补充项：#9（预算只读）不在原清单里，但 milestones M3 的"预算面板"需要它；项目与交付单元没有"创建"路由——项目绑定见 OQ-8，交付单元只由对话 / 引擎创建。

### 4.2 `GET /v1/me`

```json
{
  "device": {
    "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
    "name": "Susan 的 MacBook",
    "platform": "macos",
    "cert_serial": "5f3a9c01d27be4468a1c",
    "pubkey_fpr": "sha256:9fe698148e3e8234105e25bf598c9689640aba943b8275348470152799f1dd42",
    "role": "approver",
    "status": "active",
    "last_seen": "2026-10-24T12:41:03.120Z"
  },
  "profile": "desktop",
  "server": { "api_version": "v1", "version": "0.1.0", "time": "2026-10-24T12:41:03.456Z" }
}
```

### 4.3 projects

`Project` 表示（`settings_override` **不在此返回**，只能经 settings 路由读写，I7）：

```json
{
  "id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "repo": "acme/web",
  "installation_id": 48213907,
  "default_branch": "main",
  "default_level": "medium"
}
```

- `GET /v1/projects` → `{ "items": [Project…], "next_cursor": null }`。
- `GET /v1/projects/{project_id}` → `Project`。
- `default_level` 在此只读，是 `levels.default_level` 的投影（config.md 诉求 A9，data-model 需同步，OQ-3）；要改就 `PUT` `levels` resource。`settings_override` 的读写走 §4.9。

### 4.4 deliveries

`Delivery` 表示（`issues[]` 为 `IssueRef`，`pr` 为 `PullRequestRef`，均为 GitHub 快照，`synced_at` 是最近对账时间，**交付事实以 GitHub 为准**，I4）：

```json
{
  "id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "title": "登录接口超时改为 30 秒",
  "level_cap": "high",
  "level_effective": "medium",
  "stage": "review",
  "stage_status": "running",
  "issues": [
    {
      "repo": "acme/web",
      "number": 5,
      "node_id": "I_kwDOAbCdEf5",
      "title": { "text": "Login request times out too early", "source": "github:issue.title", "truncated": false },
      "state": "open",
      "state_reason": null,
      "synced_at": "2026-10-24T12:40:58.000Z"
    }
  ],
  "pr": {
    "repo": "acme/web",
    "number": 12,
    "node_id": "PR_kwDOAbCdEs4",
    "title": { "text": "fix: raise login timeout to 30s", "source": "github:pr.title", "truncated": false },
    "state": "open",
    "head_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
    "base_ref": "main",
    "checks": { "passed": 6, "failed": 0, "pending": 1, "required_ok": false },
    "review_state": "none",
    "synced_at": "2026-10-24T12:40:58.000Z"
  },
  "worktree_id": "wtr_01JB7M8Q2X5V9N3R6T1K4W0YZE",
  "parent_id": null,
  "depends_on": [],
  "origin": "conversation"
}
```

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/deliveries` | 查询：`project_id`、`stage`（可重复）、`stage_status`、`active=true`（排除 `closed` / `abandoned`，供左侧"进行中"列表）、分页 | 列表 `Delivery` |
| `GET /v1/deliveries/{delivery_id}` | — | `Delivery` |
| `PATCH /v1/deliveries/{delivery_id}` | `{ "level_cap": "max" }`（仅此字段；`stage` 为 `closed` / `abandoned` 时 `409 conflict`，`reason: invalid_state`） | `200 Delivery`；产生 `delivery.level_changed`。降档不撤销已触发的闸门（architecture §8），是否生效见响应里的 `level_effective` |
| `POST /v1/deliveries/{delivery_id}/abandon` | `{ "reason": "不做了" }`（`reason` 可选，≤ 500 字符） | `200 Delivery`（`stage: abandoned`）；产生 `delivery.abandoned` |
| `GET /v1/deliveries/{delivery_id}/budget` | — | `BudgetLedger` |

`BudgetLedger`（只读的运行状态；改预算只能走 settings，且只对之后创建的交付单元生效——config.md §2.6：创建时固化，I7）：

```json
{
  "id": "bdg_01JB7M4Q1T5X9V2N6R8K3W7YZC",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "template": "medium",
  "limits": {
    "tokens_by_tier": { "utility": 200000, "planner": 400000, "worker": 2000000 },
    "nodes": 12,
    "rounds": 3,
    "wall_time": 7200,
    "forge_writes": 30
  },
  "used": {
    "tokens_by_tier": { "utility": 18400, "planner": 52100, "worker": 611000 },
    "nodes": 4,
    "rounds": 1,
    "wall_time": 940,
    "forge_writes": 3
  }
}
```

> `wall_time` 单位为秒，其余子字段的单位 / 形态是示意（OQ-10）。

### 4.5 conversations / messages

`Conversation` 表示：

```json
{ "id": "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC", "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA" }
```

`GET /v1/projects/{project_id}/conversations` → 列表。对话由服务端在项目绑定时创建；项目与对话的基数（ER 图 1:n，D-10 暗示"主对话"）见 OQ-7。

`Message` 表示：

```json
{
  "id": "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD",
  "conversation_id": "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC",
  "role": "user",
  "agent_kind": null,
  "text": "把登录接口的超时改成 30 秒，参考 #5",
  "level_cap": "high",
  "refs": [{ "kind": "issue", "repo": "acme/web", "number": 5 }],
  "cards": [],
  "delivery_id": null
}
```

一条 `role: agent` 且带审批卡片的消息：

```json
{
  "id": "msg_01JB7N8T2X6V9M3Q5R1K4W7YZP",
  "conversation_id": "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC",
  "role": "agent",
  "agent_kind": "planner",
  "text": "PR #12 已就绪，合并前需要你的审批。",
  "level_cap": null,
  "refs": [{ "kind": "pr", "repo": "acme/web", "number": 12 }],
  "cards": [{ "kind": "approval_request", "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK" }],
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
}
```

| 字段 | 规则 |
| --- | --- |
| `role` | `user` / `agent` / `event`（`event` 是折叠显示的后台事件行） |
| `agent_kind` | `planner` / `worker` / `utility` / `null`；只决定气泡边框颜色（D-12：不显示头像） |
| `refs[]` | 线上形态（OQ-10）：`{"kind":"issue","repo":"o/r","number":5}`、`{"kind":"pr","repo":"o/r","number":12}`、`{"kind":"worktree","id":"wtr_…"}`、`{"kind":"delivery","id":"dlv_…"}`。客户端只发上述最小字段，`node_id` / 快照由服务端解析；解析不到 → `422 validation_failed` |
| `cards[]` | 带 `kind` 的联合体；`kind` 集合开放，**未知 `kind` 必须降级为不显示或显示"请升级客户端"**。已定义：`approval_request`（`approval_id`）；详情总是再取资源，卡片不内嵌可签名内容（OQ-10） |
| `level_cap` | 用户消息附带的**任务级**档位上限，`null` = 用项目 `default_level`；`none` = 纯对话不建交付单元（architecture §8）。不是配置（§8.2） |
| `delivery_id` | 服务端异步关联；`POST` 响应里通常是 `null`，客户端从 `delivery.created` 等事件得知，不要轮询它 |

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/conversations/{conversation_id}/messages` | 查询：分页（`order` 默认 `desc`） | 列表 `Message` |
| `POST /v1/conversations/{conversation_id}/messages` | 见下；`Idempotency-Key` 必带 | `201 Message`（`role: user`），头 `X-Event-Seq` = `message.posted` 的 `seq` |

请求：

```json
{
  "text": "把登录接口的超时改成 30 秒，参考 #5",
  "level_cap": "high",
  "refs": [{ "kind": "issue", "repo": "acme/web", "number": 5 }]
}
```

- 请求体只允许 `text`（必填）、`level_cap`、`refs`；**没有** `model` 或预算字段（I7）。
- `201` 只表示消息已持久化。意图解析、agent 回复、交付单元创建都是异步的，经事件流到达；客户端用 `id` 去重，不要把 `201` 当成"agent 已处理"。
- `POST` 的 `actor` 是发送设备；Message 本身不记设备（data-model 无此字段），审计看 `message.posted` 事件的 `actor`。

### 4.6 worktrees

`Worktree` 表示（`lock` 即 data-model 的 `{holder, delivery_id, acquired_at, lease_until}`，无锁为 `null`）：

```json
{
  "id": "wtr_01JB7M8Q2X5V9N3R6T1K4W0YZE",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "branch": "gantry/dlv-12-login-timeout",
  "base_ref": "main",
  "path": "/var/lib/gantry/worktrees/acme-web/dlv-12",
  "lock": {
    "holder": "engine",
    "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
    "acquired_at": "2026-10-24T12:38:00.000Z",
    "lease_until": "2026-10-24T13:08:00.000Z"
  }
}
```

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/worktrees` | 查询：`project_id`、`delivery_id`、分页 | 列表 `Worktree` |
| `GET /v1/worktrees/{worktree_id}` | — | `Worktree` |
| `PUT /v1/worktrees/{worktree_id}/lock` | `{ "lease_seconds": 1800 }`（可选，默认 1800，最长 14400） | `200 Worktree`（`lock.holder = "human"`）。已是 human 持有则续租；被 `engine` / `guardian` 持有 → `409 worktree_locked`（附 `lock`）。产生 `worktree.locked` |
| `DELETE /v1/worktrees/{worktree_id}/lock` | — | `200 Worktree`（`lock: null`）。只能释放 `human` 锁，否则 `409 worktree_locked`。产生 `worktree.unlocked` |

- 人要在本地编辑某分支时先取人工锁，避免与后台修复同时写（design.md"分支所有权 / 锁"）；租约到期自动释放（data-model §3）。人看代码的方式是 `git fetch` 该分支（P-01），API 只提供分支名；`path` 是服务端路径，仅作展示。
- 不提供创建 / 删除工作树的路由：工作树的生命周期由交付单元和引擎管理（`worktree.created` / `removed` 的 actor 不是客户端）。

### 4.7 workflows / nodes / attempts（只读）

`Workflow` 表示（`GET /v1/workflows/{workflow_id}`，含当前修订的节点；`?include_superseded=true` 额外返回被图改写替换的节点，供"图修订历史"）：

```json
{
  "id": "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "stage": "implement",
  "graph_rev": 1,
  "status": "finished",
  "template": "medium.implement",
  "nodes": [
    {
      "id": "nod_01JB7M7V4X2N9R5T8Q3K6W1YZG",
      "workflow_id": "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF",
      "role": "plan",
      "tier": "planner",
      "depends_on": [],
      "required": true,
      "status": "succeeded",
      "verdict": null,
      "superseded_by": null
    },
    {
      "id": "nod_01JB7M7W5X3N9R6T8Q2K4V1YZH",
      "workflow_id": "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF",
      "role": "verify",
      "tier": "worker",
      "depends_on": ["nod_01JB7M7V4X2N9R5T8Q3K6W1YZG"],
      "required": true,
      "status": "succeeded",
      "verdict": "ACCEPT",
      "superseded_by": null
    }
  ]
}
```

`Attempt` 表示（`result_ref` 是内容寻址对象，结果内容的读取路由骨架未定，OQ-11）：

```json
{
  "id": "att_01JB7M9X5N3R8T2Q6K1V4W7YZH",
  "node_id": "nod_01JB7M7W5X3N9R6T8Q2K4V1YZH",
  "n": 1,
  "sandbox_id": "sbx_01JB7M9Y2T6X4N8R3Q5K1V7WZJ",
  "status": "succeeded",
  "started_at": "2026-10-24T12:30:10.000Z",
  "ended_at": "2026-10-24T12:34:52.000Z",
  "result_ref": "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9",
  "failure_class": null,
  "usage": { "input_tokens": 41200, "output_tokens": 6800 }
}
```

| 路由 | 查询 | 响应 |
| --- | --- | --- |
| `GET /v1/deliveries/{delivery_id}/workflows` | 分页；`stage` | 列表 `Workflow`（不含 `nodes`，取详情再展开） |
| `GET /v1/workflows/{workflow_id}` | `include_superseded` | `Workflow`（含 `nodes`） |
| `GET /v1/nodes/{node_id}/attempts` | 分页（默认 `order=asc`，按 `n`） | 列表 `Attempt` |
| `GET /v1/attempts/{attempt_id}` | — | `Attempt` |

尝试不可变（architecture §2）：同一 `Attempt` 的重复读取，终态后内容恒定。

### 4.8 inbox

```json
{
  "id": "inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "kind": "approval_needed",
  "severity": "high",
  "summary": "PR #12 可以合并，需要你的审批。",
  "payload": { "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK" },
  "status": "open"
}
```

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/inbox` | 查询：`project_id`、`status`（默认 `open`）、`kind`、分页 | 列表 `InboxItem` |
| `GET /v1/inbox/{inbox_id}` | — | `InboxItem` |
| `POST /v1/inbox/{inbox_id}/dismiss` | `{ "note": "已知晓" }`（可选） | `200 InboxItem`（`status: dismissed`）；已是 `dismissed` 则原样 `200`；`resolved` 的 → `409 conflict`（`reason: invalid_state`） |

- 上报项通过它所指的事情被处理而 `resolved`（批准审批、在对话里答复、调档），由服务端发 `inbox.resolved`；客户端**没有**通用 resolve 路由（OQ-9）。
- `summary` 可能含不可信内容，按 §1.5 渲染。`payload` 的结构随 `kind` 变化，未定义（OQ-9）。

### 4.9 settings

**唯一的配置读写入口**（I7）。HTTP 形状（路径、方法、头、响应外壳、错误）以本文为准；设置**文档的内容**——有哪些 resource、键、取值、默认、合并 / 收紧规则、校验规则 V01–V22、谁能改什么、热加载——以 [interfaces/config.md](config.md)（及 `schemas/config/*.json`）为准，本文不重复。`{resource}` 是 config.md 的顶层键，目前八个：`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`、`notify`、`github`（任务清单只点名了前三个）。读写粒度是**整个 resource**。下面示例里的 `value` 只截取个别键示意。

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/settings/{resource}[?rev=N]` | — | `200` 全局文档，头 `ETag: "<rev>"`；`?rev=N` 取该 resource 在历史 rev 时的文档（可追溯；"回滚"= 读旧版再 `PUT` 回去，没有专门的回滚路由） |
| `PUT /v1/settings/{resource}[?dry_run=true]` | 头 `If-Match`；体 `{ "value": … }`，整体替换 | `200` 新文档 + `warnings[]`，头新 `ETag` 与 `X-Event-Seq`；`dry_run=true` 只校验、不落库、不发事件，返回 `{valid, errors[], warnings[], relaxes[], required_role, requires_signature}` |
| `GET /v1/projects/{project_id}/settings/{resource}[?rev=N][&view=effective]` | — | `200`；默认返回该项目的**覆盖**（无覆盖则 `value: null`，ETag `"0"`）；`view=effective` 返回逐 resource 合并并经收紧约束后的生效值，附 `clamped[]`（只读，其 ETag 不可用于 `If-Match`） |
| `PUT /v1/projects/{project_id}/settings/{resource}[?dry_run=true]` | 同全局 | 同全局；`notify`、`github` 不可覆盖 → `422 not_overridable`；越界键 → `422 validation_failed`（`override_not_allowed` / `override_loosens_global`） |
| `DELETE /v1/projects/{project_id}/settings/{resource}` | 头 `If-Match` | `200`，`value: null`（回退到全局） |
| `GET /v1/settings/catalog` | — | 设置页表单所需的目录（模型能力、图模板名、动作 id、分流类含锁定标记、各键所需角色），结构由 config.md 定 |
| `GET /v1/settings/status` | — | 各模块 `applied_rev` 与健康，供"某模块尚未应用 rev N"的提示 |

`catalog` 与 `status` 是字面路径，不会被 `{resource}` 吞掉（`{resource}` 只取上述八个）。

`GET /v1/settings/levels` 响应（`ETag: "18102"`）：

```json
{
  "resource": "levels",
  "scope": "global",
  "rev": 18102,
  "value": { "high": { "budget": { "nodes": 30 } } }
}
```

`PUT /v1/settings/levels`（头 `If-Match: "18102"`）把 `high` 档的节点预算从 30 收紧到 20：

```json
{ "value": { "high": { "budget": { "nodes": 20 } } } }
```

响应 `200`（头 `ETag: "18236"`、`X-Event-Seq: 18236`；`rev` 与 ETag 一致）：

```json
{
  "resource": "levels",
  "scope": "global",
  "rev": 18236,
  "value": { "high": { "budget": { "nodes": 20 } } },
  "warnings": []
}
```

`PUT …?dry_run=true` 响应：

```json
{
  "valid": true,
  "errors": [],
  "warnings": [],
  "relaxes": [],
  "required_role": "operator",
  "requires_signature": false
}
```

写入模型凭据：`PUT /v1/settings/models` 请求体里的凭据字段是 `SecretRef` 对象，`value` 只写；响应里同一字段只剩 `secret_ref` 与只读的 `present`：

```json
{
  "value": {
    "providers": {
      "anthropic": {
        "credential": {
          "type": "api_key",
          "secret_ref": "model.anthropic.main",
          "value": "example-value-not-a-real-key"
        }
      }
    }
  }
}
```

```json
{
  "resource": "models",
  "scope": "global",
  "rev": 18240,
  "value": {
    "providers": {
      "anthropic": {
        "credential": { "type": "api_key", "secret_ref": "model.anthropic.main", "present": true }
      }
    }
  },
  "warnings": []
}
```

校验失败（`422`；`pointer` 相对 `value`）：

```json
{
  "type": "urn:gantry:problem:validation_failed",
  "title": "Settings validation failed",
  "status": 422,
  "code": "validation_failed",
  "request_id": "01JB7PA2M5R8T3X6N1Q4V9WZYB",
  "errors": [
    {
      "pointer": "/providers/anthropic/credential/secret_ref",
      "code": "secret_ref_missing",
      "message": "secret_ref \"model.anthropic.main\" does not exist and no value was supplied"
    }
  ]
}
```

项目范围：该项目尚无覆盖（`ETag: "0"`），以及生效值视图（`?view=effective`）：

```json
{
  "resource": "levels",
  "scope": "project",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "rev": 0,
  "value": null
}
```

```json
{
  "resource": "levels",
  "scope": "effective",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "rev": 18236,
  "value": { "high": { "budget": { "nodes": 20 } } },
  "clamped": []
}
```

`GET /v1/settings/status` 响应（字段名为示意，config.md 只写"各模块 applied_rev 与健康"）：

```json
{ "modules": { "egress": { "applied_rev": 18102, "healthy": true } } }
```

| 规则 | 说明 |
| --- | --- |
| 整体替换 | `PUT` 替换整个 resource 的 `value`，不合并；先 `GET` 再改再 `PUT`。全局 `PUT {"value": {}}` 回到内置默认；项目覆盖用 `DELETE` 清除。存储是稀疏的、合并与"仅收紧"约束见 config.md §2.1–§2.2 |
| 并发 | `rev` / `ETag` / `If-Match` 见 §3.4；两个设置页同时保存同一 resource，后到者得到 `412`，重新 `GET` 后在新值上重做 |
| 角色与签名 | 路由下限 operator；服务端按内容判定实际所需角色（config.md §2.5）。不足 → `403 forbidden_role`（附 `required_role`）；提交前用 `dry_run` 取得 `required_role` / `requires_signature`。需要签名而未带 → `403 signature_required`。**签名的载体与规范化两份文档都没定义（OQ-16）：在此之前，需要签名的写入（含 `models.providers` 与全部带 `value` 的凭据写入）无法实现，而 M1 的"设置页·模型"要用到它** |
| 凭据字段 | 以 config.md §2.4 为准：凭据字段是 `SecretRef`（`secret_ref`、只读的 `present`、只写的 `value`）。`GET` 永不返回 `value`；`PUT` 里 `value` 省略 = 保持不变，字符串 = 写入或轮换，`null` 一律 `422`（`secret_clear_in_use`）；`present` 被接受并忽略。值与 `settings.updated` 原子写入。客户端发送后立即清除内存、不得持久化或记日志 |
| 校验 | 先 schema 再语义；失败 → `422 validation_failed`，`errors[].code` 取 `required` / `type` / `enum` / `range` / `unknown_field` 与 config.md §11 的语义码；`warning` 不阻止写入，随 `200` / `dry_run` 返回 |
| 事件 | `settings.updated`（config.md §2.3 提议，须回写 data-model §5.2）：`subject` 为字面量 `settings`（全局）或 `prj_…`（项目覆盖）；**项目覆盖变更也发 `settings.updated`，不再发 `project.updated`**；只下发给桌面档案且角色 ≥ operator 的事件流（§5.4）。客户端收到后重新 `GET` 该 resource |
| 生效时机 | 判定类键下一次判定起生效；`levels.*` 只对之后创建的交付单元生效；以 `status` 查看各模块是否已应用（config.md §2.6） |
| 不在 settings 里的 | 设备与证书（§4.10）、本机外观（`desktop-shell` 本地存储）、任务级参数（`Message.level_cap`） |

### 4.10 devices

`Device` 表示同 §4.2 的 `device` 对象。

| 路由 | 请求 | 响应 / 规则 |
| --- | --- | --- |
| `GET /v1/devices` | 分页 | 列表 `Device`（含已吊销，`status` 区分；`last_seen` 为节流更新） |
| `PATCH /v1/devices/{device_id}` | `{ "name": "书房 Mac" }`（仅 `name`，1–64 字符） | `200 Device` |
| `PUT /v1/devices/{device_id}/role` | `{ "role": "operator" }`；**不能改自己**（`409 conflict`，`reason: self_role_change`） | `200 Device`；产生角色变更事件（OQ-4：目录里没有该类型）。新角色对该设备的**下一个请求**立即生效 |
| `POST /v1/devices/{device_id}/revoke` | 无体 | `200 Device`（`status: revoked`）。可吊销自己；已吊销则原样 `200`。产生 `device.revoked`，并断开该设备的全部事件流（§5.5） |

- 吊销不可逆；要恢复必须重新配对（新设备记录）。
- 改角色与吊销是否应当也要设备签名：OQ-5。

---

## 5. 事件流（SSE）

### 5.1 请求与帧

```http
GET /v1/events?project_id=prj_01JB7K2M9Q4X8V3N5R6T1W0YZA&types=delivery.*,message.* HTTP/1.1
Accept: text/event-stream
Last-Event-ID: 18233
```

响应 `200`，头 `Content-Type: text/event-stream; charset=utf-8`、`Cache-Control: no-store`、`X-Accel-Buffering: no`。帧：

```text
retry: 3000

id: 18234
event: delivery.stage_changed
data: {"id":"evt_01JB7P6Q9T3X2N5R8K1V4W7YZN","seq":18234,"ts":"2026-10-24T12:41:03.120Z","type":"delivery.stage_changed","subject":"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB","project_id":"prj_01JB7K2M9Q4X8V3N5R6T1W0YZA","actor":{"kind":"engine","id":"engine"},"causation_id":"evt_01JB7P6M3K5R8T2X9N1Q4V7WZA","correlation_id":"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB","data":{"from":"implement","to":"propose"}}

event: heartbeat
id: 18240
data: {"seq":18240,"ts":"2026-10-24T12:41:18.000Z"}

```

| 帧 | 规则 |
| --- | --- |
| 业务事件 | `id:` = 事件 `seq`（十进制）；`event:` = 事件 `type`；`data:` = **data-model §5.1 信封的单行 JSON，原样不改**。`data` 字段内部结构以 `schemas/events/<type>.json` 为准，上例的 `from` / `to` 仅为示意 |
| `heartbeat` | 每 15 s 一次；`id:` = 本连接**已扫描**到的最大 `seq`（含被过滤掉的），所以过滤后的流也能推进游标；不是日志事件，没有信封 |
| `close` | 服务端主动关闭前尽力发送：`event: close`，`data: {"reason":"device_revoked"}`；`reason` ∈ `device_revoked` / `cert_expired` / `server_shutdown` / `lagging`。发完即关连接 |
| `retry:` | 连接开头一次，`3000`（毫秒）；自定义客户端仍按 §3.7 的退避，以更大者为准 |

### 5.2 游标与续传语义

| 项 | 规则 |
| --- | --- |
| 游标 | `Last-Event-ID` = 事件 `seq`；也可用查询参数 `after=<seq>`（头优先）。语义：下发 `seq` **大于**游标的事件 |
| 无游标 | 只推订阅之后的新事件（不回放）；首帧是带当前 head 的 `heartbeat` |
| 顺序与重复 | 同一连接内 `seq` 严格递增、不重复；过滤会让 `seq` 出现空洞，属正常。跨重连至少一次：客户端按 `seq` 幂等，`seq` ≤ 已应用的直接丢弃 |
| 回放 | 服务端先从事件日志回放 `(游标, head]`，再无缝切到实时，不丢不重（订阅须先于回放读取，或以 `seq` 去重衔接） |
| 客户端推进游标 | **先应用事件、后持久化游标**；游标取"最后已应用事件的 `seq`"与"最后收到的 `heartbeat` 的 `id`"中较大者 |
| 游标超前 | `Last-Event-ID` > 服务端 head（如服务端从备份恢复）→ **`409 cursor_ahead`**（普通 problem 响应，不开流） |
| 游标过旧 | `410 cursor_expired` 预留，骨架未定义事件保留期（OQ-13） |
| 慢消费者 | 服务端缓冲超限 → 发 `close`（`lagging`）并断开；因为可从日志回放，客户端重连即可，不丢事件 |

### 5.3 快照 + 事件流（客户端的标准衔接）

1. 并发拉取需要的快照（`/v1/projects`、`…/messages`、`/v1/deliveries?active=true`、`/v1/inbox` 等），记下每个响应的 `X-Event-Seq`。
2. 取其中最小值 `S`，连接 `GET /v1/events`（`Last-Event-ID: S`，**一个客户端一条流**，不按项目各开一条；手机端也同样）。
3. 收到事件后用 `seq` 去重并应用到本地状态；断线后用最后已应用的 `seq` 续传。
4. 遇到 `409 cursor_ahead` / `410 cursor_expired`：丢弃本地缓存，从第 1 步重来（这一次不带游标）。

事件按需带出的实体（如 `message.posted`）若 `data` 内容不足以更新界面，客户端按事件的 `subject` 重新 `GET` 对应资源，不要猜。

### 5.4 过滤

| 参数 | 规则 |
| --- | --- |
| `project_id` | 可重复；只下发这些项目的事件。**没有 `project_id` 的全局事件**（如 `device.*`）始终下发（信封里该字段对全局事件为 `null`，OQ-13） |
| `types` | 逗号分隔，精确类型或 `前缀.*`（如 `message.*`）；缺省 = 全部 |
| 手机档案 | 服务端再与白名单取交集：`project.*`、`message.*`、`inbox.*`、`device.revoked` [提议]；请求了白名单外的类型不报错，只是收不到 |
| `settings.*` | 只下发给**桌面档案且角色 ≥ operator** 的流（事件 `data` 含设置内容；config.md 诉求 A7）[提议]；按**发送时刻**的角色判定 |

### 5.5 吊销、过期与连接上限

| 情形 | 行为 |
| --- | --- |
| 设备被吊销 | 服务端在**同一进程内**按 `device_id` 找到该设备全部事件流，不再下发任何事件，尽力发 `close`（`device_revoked`）后立即关闭；后续新连接在握手时被拒。兜底：每次心跳检查 `Device.status`，因此**最迟 15 s，上限 60 s**（M3 退出标准）。客户端收到 `close(device_revoked)` 或随后的握手失败，按 §3.1 `device_revoked` 处理 |
| 证书到期 | 流在证书 `notAfter` 时关闭（`cert_expired`）；客户端应在到期前续期（路由见 OQ-12） |
| 并发上限 | 每设备 3 条；超出 → `429 rate_limited`（`Retry-After`） |
| 角色变化 | 不断流；除 `settings.*` 外，事件可见范围不随角色变化（viewer 起都能订阅），只受档案白名单影响；降到 viewer 后不再下发 `settings.*` |

> milestones 把"吊销对已建立事件流连接生效"放在 M3，而配对与吊销本身在 M1；本契约要求 M1 的 `api` 就实现心跳兜底，M3 做进程内即时断开的优化（OQ-13）。

---

## 6. 审批签名

Gantry 审批（architecture §7.4）：用户用**设备私钥**对"动作 + 目标 + head SHA"签名，服务端验签后由 `policy` 放行（I3）。GitHub 上的 PR review approve 是另一件事，**不走本 API**；需要时客户端只展示跳转链接（`https://github.com/{repo}/pull/{number}`，由 `repo` + `number` 拼出，在系统浏览器打开）。

### 6.1 审批资源与读、拒绝

`Approval` 表示（`pending` 时 `device_id` / `signature` 为 `null`）：

```json
{
  "id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "action": "merge_pr",
  "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
  "head_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
  "device_id": null,
  "signature": null,
  "status": "pending",
  "expires_at": "2026-10-24T14:41:00.000Z"
}
```

- `status`：`pending` / `granted` / `invalidated` / `expired` / `denied`（data-model §3；§4.4 还有 `consumed`，两处不一致，OQ-6）。
- `action`、`target`、`head_sha` 的取值以 [forge-actions.md §2.5](forge-actions.md) 为准：`merge_pr` → `target = {kind:"pr", repo, number}`，`head_sha` = PR 的 head；`force_push` / `push_branch` → `target = {kind:"branch", repo, branch, …}`，`head_sha` = 将要推送的提交。`delete_branch` / `publish_release` / `change_repo_settings` 预留，无 head 的动作如何绑定 SHA 未定（OQ-6）。本文示例用 `merge_pr`。
- `GET /v1/approvals`：查询 `status`、`delivery_id`、分页；`GET /v1/approvals/{approval_id}`：`Approval`。viewer 起可读。
- `POST /v1/approvals/{approval_id}/deny`：`{ "reason": "先别合" }`（可选），`Idempotency-Key` 必带，`approver` 起，**不需要签名**（拒绝不授予任何权限，OQ-6）→ `200 Approval`（`denied`），发 `approval.denied`。非 `pending` → `409 approval_not_pending`。
- 审批请求由 `policy` / 引擎创建（`approval.requested`），客户端经事件、收件箱（`approval_needed`）和消息卡片得知；**客户端不能创建审批**。

### 6.2 `GET /v1/approvals/{approval_id}/challenge`

`approver` 起；`Cache-Control: no-store`。服务端为 `(approval, 当前设备)` 生成或返回仍有效的 challenge：同一设备在有效期内重复 `GET` 返回**同一个** challenge（重试安全）；用掉或过期后，下一次 `GET` 换新 nonce。

```json
{
  "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
  "payload": {
    "action": "merge_pr",
    "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
    "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
    "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
    "expires_at": "2026-10-24T12:52:00.000Z",
    "head_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
    "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
    "target": { "kind": "pr", "number": 12, "repo": "acme/web" },
    "v": 1
  },
  "signing": {
    "alg": "ES256",
    "canonicalization": "JCS",
    "prefix": "gantry/approval/v1\n"
  }
}
```

- `payload` 就是**要签名的全部内容**，`expires_at` 是 **challenge** 的过期时间（≤ `Approval.expires_at`，默认 TTL 120 s），不是审批的。`nonce` 是服务端为本次签发生成的 32 字节随机数（base64url，无填充）。
- `signing` 是自描述；客户端若不认识 `alg` / `canonicalization` / `prefix` / `v`，**必须中止**并提示升级，不得猜。
- 非 `pending` → `409 approval_not_pending`；本设备不是 `approver` → `403 forbidden_role`。

### 6.3 规范化与签名

**待签字节** = `UTF-8("gantry/approval/v1\n")` ‖ `canonical(payload)`，其中 `canonical` 是 RFC 8785（JCS）并限定如下（payload 只含字符串、整数、对象）：

| 规则 | 说明 |
| --- | --- |
| 编码 | UTF-8，无 BOM |
| 空白 | 无任何非必要空白（无缩进、无换行、冒号和逗号后无空格） |
| 键顺序 | 每一层对象的键按 UTF-16 码元升序排序（payload 的键全是 ASCII，等价于字节序） |
| 键唯一 | 不允许重复键 |
| 字符串 | 按 JCS 转义：只转义 `"` `\` 与 U+0000–U+001F（用 `\b \t \n \f \r` 或 `\u00xx`），其余字符（含非 ASCII）原样输出 UTF-8，不转义为 `\uXXXX` |
| 数字 | **只允许整数**（`v`、`target.number`），十进制，无前导零、无小数点、无指数；不允许浮点和 `null` |
| 时间 | RFC 3339 UTC，**恰好三位毫秒**，`Z` 结尾（如 `2026-10-24T12:52:00.000Z`） |
| 大小写 | `head_sha`、`nonce` 等按 challenge 原样，客户端不做任何规范化 |

**算法**：`alg = ES256`，即 ECDSA P-256 + SHA-256，签名编码为**原始 `r ‖ s` 共 64 字节**（IEEE P1363，非 DER），再 base64url 无填充（86 字符）。系统 API 若返回 DER（Secure Enclave / Keychain），由 `desktop-shell` / `mobile` 转换。签名密钥 = 设备证书对应的私钥（`Device.pubkey_fpr`），私钥不可导出；服务端用证书里的公钥验签（算法与"签名密钥即 TLS 密钥"待 modules/pki.md 确认，OQ-6）。

**测试向量**（Mock API 与两端实现都必须用它自测）：

| 项 | 值 |
| --- | --- |
| 公钥（P-256 未压缩点，base64url） | `BNn_Gz6KrjbLH0rq59xX_HvHW9bNbotAo2XP8VhMcelALHKuuyz5Ni_97g5sqgb5amxT-Q_FOhS7DbwENKfdR34` |
| `canonical(payload)`（即上面 §6.2 的 `payload`，单行） | `{"action":"merge_pr","approval_id":"apr_01JB7P4M8T2X6N9R3Q5K1V7WZK","delivery_id":"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB","device_id":"dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM","expires_at":"2026-10-24T12:52:00.000Z","head_sha":"9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345","nonce":"lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js","target":{"kind":"pr","number":12,"repo":"acme/web"},"v":1}` |
| 待签字节的 SHA-256（hex） | `1324554867b4194bfdbc33b49f5156d9b4384883a0e2a97ff736ca3cf628e36c` |
| 一个有效签名（base64url） | `UoCqGyJjlVc45Yz6uIAdkg6iT9Xyh8wuXoOILCNq1TGr-fTHTvziH2DdtZyIrcsMxa6Te-kWzAHuwwJYzkwdmQ` |

验签通过 = 用上述公钥验证"有效签名"对"待签字节"成立。ECDSA 签名不确定，所以不同实现各自签出的值会不同；canonical 字节与它的 SHA-256 必须逐字节一致。

### 6.4 `POST /v1/approvals/{approval_id}/grant`

`approver` 起；`Idempotency-Key` 必带。请求只带 nonce 与签名，**不带 payload**——服务端从自己保存的 challenge 重建 payload 并验签，客户端无法让服务端验证"另一份内容"：

```json
{
  "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
  "signature": "UoCqGyJjlVc45Yz6uIAdkg6iT9Xyh8wuXoOILCNq1TGr-fTHTvziH2DdtZyIrcsMxa6Te-kWzAHuwwJYzkwdmQ"
}
```

响应 `200 Approval`：

```json
{
  "id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "action": "merge_pr",
  "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
  "head_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
  "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
  "signature": "UoCqGyJjlVc45Yz6uIAdkg6iT9Xyh8wuXoOILCNq1TGr-fTHTvziH2DdtZyIrcsMxa6Te-kWzAHuwwJYzkwdmQ",
  "status": "granted",
  "expires_at": "2026-10-24T14:41:00.000Z"
}
```

**服务端校验顺序**（任一步失败立即返回，不进入下一步）：

| # | 校验 | 失败 |
| --- | --- | --- |
| 1 | 设备 `active`、角色 ≥ approver、桌面档案 | `401 device_revoked` / `403 forbidden_role` / `403 forbidden_profile` |
| 2 | `Approval.status = pending` | `409 approval_not_pending`（附 `approval_status`） |
| 3 | 本设备持有该审批的 challenge，`nonce` 相符、未用、未过期 | `409 challenge_invalid`（`reason`：`expired` / `used` / `mismatch` / `none`） |
| 4 | 用本设备证书公钥，对服务端保存的 payload 重建的待签字节验签 | `403 signature_invalid` |
| 5 | **head 一致性**：payload 的 `head_sha` 等于 forge 对账后的当前 PR head（必要时先强制对账一次）。不等 → 审批置 `invalidated`，发 `approval.invalidated` | `409 stale_head`（附 `current_head_sha`） |
| 6 | 通过：写 `Approval`（`device_id`、`signature`、`granted`），发 `approval.granted`；nonce 作废 | `200` |

- 第 5 步只是**提前失败**。权威的 head 检查在**消费审批时**（`policy` 放行 → Outbox 执行 merge 之前，I3）：`granted` 只在 head 未变时可被消费，之后任何推送都使它失效。`200` 不代表已合并；合并由 Outbox 异步执行，进展经 `outbox.*` / `forge.synced` 事件到达。
- 同一设备凭同一 `Idempotency-Key` 重试 → 重放首次 `200`（响应丢失的补救）；换 key 重发已用 nonce → `409 challenge_invalid`（`used`），再 `GET approval` 看最终状态。
- 多个 approver 设备并发：先到先得，其余得到 `409 approval_not_pending`。

### 6.5 界面展示必须与签名载荷一致

目标：用户看到并确认的内容 = 设备实际签名的内容。威胁：展示层（WebView 里的 `desktop-ui`）被注入文本欺骗，或实现者把"展示用数据"和"签名用数据"分开取。

**界面必须展示的字段**（全部来自同一个已解析的 `payload`）：

| payload 字段 | 展示要求 |
| --- | --- |
| `action` | 动作的本地化名称，旁边小字显示原始 `action` 值；**不认识的 `action` 拒绝签名**并提示升级客户端 |
| `target` | `repo` + 类型 + 编号 / ref 完整展示（如 `acme/web · PR #12`），不得缩写或只显示标题 |
| `head_sha` | 前 12 位醒目显示，可展开完整 40 位，并提供"在 GitHub 打开该 commit"的核对入口 |
| `expires_at` | 倒计时 + 本地时区绝对时间；过期后禁用确认按钮并提示重新获取 |
| `device_id` | "将使用本设备（设备名）的私钥签名"，设备名取自 `/v1/me` |
| `delivery_id` | 展示交付单元标题作**上下文**（来自 `Delivery` 资源，见下），同时显示 id |
| `approval_id`、`nonce`、`v` | 折叠的"技术细节"里可见 |

**一致性规则**：

1. **单一来源**：`desktop-shell` 取得 challenge → 解析 → 生成 canonical 字节；界面所用的展示数据由 shell 从**同一个已解析对象**生成并经 IPC 下发。`desktop-ui` 回传给 shell 的只有 `{ approval_id, confirmed: true }`，**不得回传任何待签内容**；shell 只签自己持有的那份 canonical 字节。
2. **未签名信息要标明**：PR 标题、改动行数、Issue 摘要等不在 payload 内，且来自 `UntrustedText`，必须放在与签名字段**视觉分离**的区域并标注"未签名信息，仅供参考"，不得影响"确认"按钮的文案。
3. **展示前自检**，任一失败则中止、不弹确认：`challenge.approval_id` 等于所请求的 id；`payload.device_id` 等于本设备；`payload.v` 已知；`signing` 三项均受支持；`payload.expires_at` 未过期（用 `/v1/me` 的 `server.time` 校正时钟偏差）；`payload` 的 `action` / `target` / `head_sha` / `delivery_id` 与 `GET /v1/approvals/{id}` 返回的 `Approval` 一致，否则报告"服务端数据不一致"。
4. **系统级确认**：解锁私钥必须经 Touch ID / Windows Hello（milestones M2）；系统弹窗的提示文案应包含由 payload 生成的 `action`、`repo#number` 和 `head_sha` 前 8 位（平台允许自定义文案时），因为它在 WebView 之外渲染。
5. **以服务端结果为准**：提交后界面只按响应里的 `Approval.status` 更新；不做"已批准"的乐观显示；`409 stale_head` 要明确告诉用户"代码在你确认期间已更新，审批已失效"。
6. **一次一审**：一次确认只对应一份 payload；不提供"批量批准"，也不缓存 challenge 跨过其 `expires_at`。

---

## 7. 路由与错误对照（实现自检）

| 场景 | 期望 |
| --- | --- |
| 无证书 / 吊销证书连 API 端口 | 握手失败，无 HTTP 响应 |
| 在 API 端口请求 `/v1/pair`，或在配对端口请求 `/v1/me` | `404 not_found`（problem+json） |
| viewer 调 `POST …/messages` | `403 forbidden_role`，`required_role: "operator"` |
| `ios` 设备调 `GET /v1/deliveries` | `403 forbidden_profile`（即使是 approver） |
| 同一 `Idempotency-Key` 重放 `POST …/messages` | 只产生 1 条 `message.posted`，第二次响应带 `Idempotent-Replayed: true` |
| `PUT /v1/settings/models` 缺 `If-Match` / 旧 `If-Match` | `428` / `412`（带当前 `ETag`） |
| 带 `credential.value` 写入后，`GET` 同一 resource / 读 `settings.updated` 事件 / 查日志 | 都没有 `value`，只有 `secret_ref` 与 `present: true` |
| viewer 或 `ios` 设备读任意 settings 路由；viewer 订阅事件流 | `403 forbidden_role` / `403 forbidden_profile`；流里没有 `settings.*` |
| 事件流断开后带 `Last-Event-ID` 重连 | 不丢、不重；过滤流的游标由 `heartbeat` 推进 |
| `Last-Event-ID` 大于 head | `409 cursor_ahead` |
| 吊销设备 | 其事件流 ≤ 60 s 关闭（目标立即），新握手被拒 |
| `GET challenge` → 篡改 nonce → `POST grant` | `409 challenge_invalid` |
| 签名 payload 被改一个字节 | `403 signature_invalid` |
| 签名后 PR 有新推送再 `POST grant` | `409 stale_head`，审批变 `invalidated`，发 `approval.invalidated` |
| 测试向量 | 公钥验证通过；canonical 字节的 SHA-256 吻合 |
| 请求体带 `model` 或预算字段（如 `POST …/messages`） | `422 validation_failed`，`errors[].code = unknown_field` |

---

## 8. 不变式落点

### 8.1 I6：客户端不执行、不持凭据

| I6 的要求 | 本 API 的体现 |
| --- | --- |
| 客户端不执行 | 全部写路由只有六类：发消息（数据）、改配置（settings）、审批签名 / 拒绝、状态流转（调档 / 放弃 / dismiss）、工作树人工锁、设备管理。**没有任何路由接收代码、命令、补丁、脚本，也没有"让服务端替客户端运行某物"的路由**；执行只发生在服务端 → 引擎 → 沙盒，GitHub 写只经 Outbox（I1、I2） |
| 不持 GitHub 凭据 | 没有任何响应包含 installation token、App 私钥、webhook secret；客户端不调用 GitHub API，PR / Issue 信息来自服务端快照，跳转链接由 `repo` + `number` 拼出并在系统浏览器打开 |
| 不持模型凭据 | 模型凭据只作为 `SecretRef.value` 经 `PUT /v1/settings/{resource}` 写入且**只写不读**，设置文档里只有 `secret_ref`（§4.9）；客户端提交后不得持久化、不得写日志；任何读路由、事件、日志都不含值 |
| 客户端持有的只有 | 设备私钥（不可导出，只用于 mTLS 与审批签名）、服务端 CA 指纹、设备证书。签名内容里没有任何凭据 |
| 同时的相关不变式 | I3：审批只由 `policy` 放行，API 只转交签名并重建 payload 验签（§6.4）；I4：快照带 `synced_at`，界面可标注新鲜度，事实以 GitHub 为准；I5：§1.5 渲染规则 |

### 8.2 I7：配置只在 settings 路由

| 做法 | 说明 |
| --- | --- |
| 入口唯一 | 模型、档位预算、策略等配置的读写只有 §4.9 的 `/v1/settings/{resource}` 与 `/v1/projects/{project_id}/settings/{resource}`（另有只读的 `catalog`、`status`），均 operator 起（放宽类 / 凭据写入 approver），手机档案不可用 |
| 工作路由拒绝配置字段 | 所有非 settings 路由的请求体 `additionalProperties: false`（§1.4）：消息里带 `model`、预算、策略字段 → `422`，而不是静默忽略 |
| 工作路由只出运行状态 | `Delivery.level_cap` / `level_effective`、`BudgetLedger.used`、节点状态都是运行态；`BudgetLedger.limits` 只读展示，改预算只能经 settings |
| 任务级档位不是配置 | `Message.level_cap`、`PATCH /deliveries` 的 `level_cap` 是**单个任务的上限**，由 architecture §7.2 / §8 明确允许在工作界面设置；与"档位模板的预算数值"（配置）区分 |
| 项目字段 | `Project.settings_override` 不在 Project 表示里返回，读写走 `/v1/projects/{project_id}/settings/{resource}`；`default_level` 在 Project 里只读，是 `levels.default_level` 的投影（OQ-3） |
| 凭据 | 只写不读（§4.9），配置页以外无处暴露，设置文档里也只有 `secret_ref` / `present` |

---

## 9. 客户端接入顺序

| 步骤 | 做什么 |
| --- | --- |
| 1 | 加载设备证书与私钥 → `GET /v1/me`：读 `role`、`profile`、`server.time`（偏差 > 60 s 警告）；握手失败按 §2.1 处理 |
| 2 | 按档案裁剪界面：`mobile` 只显示对话与收件箱；按 `role` 隐藏 / 禁用入口（服务端仍会 `403`，客户端裁剪只为体验） |
| 3 | 并发拉快照（项目、当前项目的对话与最新消息页、进行中交付单元、收件箱），记 `X-Event-Seq` 取最小值 |
| 4 | 订阅 `/v1/events`（一条流）；事件按 `seq` 幂等应用；游标先应用后保存 |
| 5 | 写操作：为每次用户意图生成 `Idempotency-Key`；网络失败 / 5xx 沿用同一 key 重试；settings 带 `If-Match` |
| 6 | 错误统一按 `code` 分支（§3.1 表），`detail` 不展示 |
| 7 | 审批：事件 / 收件箱 → 打开审批界面 → `GET challenge` → 按 §6.5 展示 → 系统生物识别 → 签名 → `POST grant` |

---

## 10. 开放问题

骨架没定的、需要用户或对应模块规格拍板的。**"本文的临时处理"只是让文档能自洽，不构成决定。**

| # | 问题 | 本文的临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | 角色边界与新设备的初始角色：谁、如何指定（`gantry pair` 参数？首台设备是否自动 `approver`？）；各路由最低角色是本文按 design.md 推出的 | §2.2 表；`POST /v1/pair` 响应里直接给 `role` | modules/pki.md、全部路由的权限 |
| OQ-2 | 手机端：角色无法表达"只做对话"，本文另设"客户端档案"；`platform` 取值集合；M4"默认不可审批"是否暗示可配置（与 D-11 的"只发消息"关系）；APNs 推送令牌的注册路由未定义 | 档案由 `platform=ios` 推出，固定 9 条路由；默认角色 `operator`；无推送令牌路由 | fe-mobile、notify、data-model Device |
| OQ-3 | settings 与 config.md 的衔接：config.md 的诉求 A1–A9 已在本文采纳（八个 resource、`dry_run`、`?rev=N`、语义错误码、`catalog` / `status`、`SecretRef` 语义、`settings.updated` 及其可见范围、写权限按内容判定、`default_level` 投影）。仍待定：(a) `Project.default_level` 改为 `levels.default_level` 的只读投影需要回写 data-model；(b) `catalog`、`status` 的响应结构由 config.md 补全；(c) resource 从任务清单点名的三个扩到八个，是否接受；(d) 没有专门的回滚 / 历史列表路由（读 `?rev=N` 再 `PUT`），设置页的"历史"如何列出版本 | 照 config.md 当前文本实现；(d) 暂以事件流中的 `settings.updated` 作历史来源 | config.md、data-model、desktop-ui 设置页 |
| OQ-4 | 事件目录缺口：`settings.updated`（config.md §2.3 提议，须回写 data-model §5.2）；设备改名 / 改角色；`approval.expired` / `consumed`（forge-actions.md §14 也提出）；`inbox` 被 dismiss（只有 `inbox.resolved`）；对话创建 | 设置事件按 config.md 提议；其余缺口客户端靠重新拉取发现；改角色路由（§4.10）处标了缺口 | data-model §5.2、多客户端一致性 |
| OQ-5 | 改设备角色、吊销设备是否也应带设备签名（防会话劫持）；settings 的放宽类写入已按 config.md §2.5 由内容判定为 approver（本文的路由下限仍是任务要求的 operator） | 改角色 / 吊销为 approver 起、不签名 | I3、policy、pki |
| OQ-6 | 审批：`Approval.status` 缺 `consumed`（§4.4 有）；challenge / nonce 的存放位置与是否进事件日志；无 head 的预留动作（删分支、发 release、改仓库设置）`head_sha` 取什么（`merge_pr` 等已由 forge-actions.md §2.5 定义）；设备密钥算法（本文按 ES256）与"签名密钥 = TLS 密钥"；`deny` 是否也要签名；审批有效期默认值 | §6 全部按 ES256 + `merge_pr` / PR 示例；`deny` 不签名；challenge 存放不规定 | policy、pki、forge-actions.md、data-model |
| OQ-7 | Conversation：ER 图是 Project 1:n Conversation，D-10 暗示每项目一个主对话；谁创建、有无"主对话"标记、`Conversation` 字段 | 只提供列表路由，对话由服务端在项目绑定时创建；表示里只有 `id` / `project_id` | conversation、桌面端项目 tab |
| OQ-8 | 项目绑定 / 解绑（`project.added`）由什么触发：GitHub App 安装回调还是 API；Delivery 的调档 / 放弃是否应直达 API，还是只经对话意图 | 无绑定路由；调档、放弃提供直达路由（#7 #8） | forge、deploy、conversation |
| OQ-9 | InboxItem：`payload` 按 `kind` 的结构、`severity` 枚举、需要用户做选择的 `decision_needed` 的选项如何表达、是否需要显式 resolve 路由 | 只有 dismiss；`payload` 与 `severity` 示例是示意 | notify、desktop-ui 收件箱 |
| OQ-10 | data-model 未给线上形态的类型，需回写：`Level` 大小写、`UntrustedText`、`Ref`、`Card`、`CheckSummary`、`Workflow.status`、`Attempt.status` / `usage`、`Device.platform`、`cert_serial` / `pubkey_fpr` 格式、`BudgetLedger` 子字段单位 | 本文示例的形态（均为示意） | core、schemas/、两端的类型生成 |
| OQ-11 | `Attempt.result_ref`（内容寻址对象）的读取路由与格式，依赖 worker-protocol.md 的结果结构 | 不提供；检视器暂只能显示尝试元数据 | desktop-ui 检视器 |
| OQ-12 | 配对与证书：配对监听器的端口与是否按需开启；`confirmation_code` 核对失败的处置（设备核对前是否已 `active`、有无 abort 路由）；**证书续期路由**（design.md 提到"在已有 mTLS 通道上自动续期"，骨架未定） | 配对 1 条路由；无续期路由 | pki、deploy、desktop-shell |
| OQ-13 | 事件：保留期与 `410 cursor_expired`；全局事件信封里 `project_id` 是否为 `null`；吊销断流 M1（心跳兜底）与 M3（即时）的拆分 | `project_id` 对全局事件为 `null`；保留不限 | store、api、milestones |
| OQ-14 | §3.7 全部数值默认值（心跳、限流、TTL、体积上限、幂等保留） | 照表 | api 实现与限流配置 |
| OQ-15 | Q-01（个人 / 团队）：骨架没有"用户"实体，主体就是 Device；若将来做团队，`actor`、角色、审计都要重做 | 主体 = Device | 全局 |
| OQ-16 | **settings 写入的设备签名**：config.md §2.5 [提议] 放宽类写入需设备签名，带 secret `value` 的写入归 approver，是否也要签名未明；但签名放在哪个头 / 字段、签什么（`scope + resource + base_rev + 内容哈希`？）、规范化与域分隔前缀、首次写入（`base_rev = 0`）与凭据轮换如何防重放，两份文档都没定。M1 的"设置页·模型"要配置 provider 与凭据，所以这是 **M1 阻塞项** | 只定义了 `dry_run` 的 `requires_signature` 与 `403 signature_required`；不规定载体 | pki、policy、desktop-shell、config.md §2.5 |
