# 客户端 API 契约（`api`）

> 状态：设计 v0.1 · 里程碑：M0 冻结契约，M1 起实现 · 泳道：`sec`（鉴权）、`be-engine`（路由实现）、`fe-desktop`、`fe-mobile` · 机器可读版本：`schemas/openapi.yaml`（由本文派生，冲突以本文为准）

客户端（桌面端、手机端）与服务端之间的全部通道：HTTPS + mTLS 的 JSON API、SSE 事件流，外加一个单向 TLS 的配对端点。实现 `api` 模块、`desktop-shell` / `desktop-ui`、`mobile`、Mock API 服务的 agent 只读本文即可互相对得上。

**约定与权威关系**

- 实体字段与共用值类型（`Ref`、`Card`、`UntrustedText`、`CheckSummary`、`Level`）以 [data-model.md](../data-model.md) 为准（§1.1），本文不新增实体字段。
- 标注 **[提议]** 的是骨架没定、本文给的默认值；全部数值默认值集中在 [§3.7](#37-默认参数)；骨架没定又无法给默认的，写在 §10，**不要自行拍板**。
- 所有实体表示都含 `id`、`created_at`、`updated_at`（data-model §3 省略了，示例为节省篇幅也省略）。
- 同批编写的配套契约：[config.md](config.md)（settings 的 resource、键、取值、合并 / 收紧、校验、角色细则；**settings 的 HTTP 形状以本文为准**）、[forge-actions.md](forge-actions.md)（Forge 动作的执行）、[data-model.md](../data-model.md#审批动作的-target-与-subject_digest)（审批 `action` / `target` / `subject_digest` 的全部形状，P-54）、[worker-protocol.md](worker-protocol.md)（与客户端无关；沙盒与服务端之间没有网络端点，decisions P-15）、[egress-control.md](egress-control.md)（与客户端无关；settings 里的 secret 经它转交 Egress）。配对与证书的实现见 [modules/pki.md](../modules/pki.md)；尚未存在：`schemas/events/*.json`。
- 桌面端的 HTTP 请求**只由 `desktop-shell` 发出**（它持有 mTLS 密钥与证书，architecture §4）；`desktop-ui` 经 IPC 调用，不直连服务端。下文"客户端"指 shell 的网络层或 `mobile` 的网络层。

---

## 1. 传输与版本

### 1.1 入口一览

| 入口 | 访问者 | 传输 | 鉴权 | 归属文档 |
| --- | --- | --- | --- | --- |
| mTLS API `/v1/*` | 桌面端、手机端 | HTTPS，强制客户端证书 | 设备证书 → Device → role | 本文 |
| 配对端点 `POST /v1/pair`（独立监听器） | 未配对设备 | HTTPS，**单向 TLS** | 一次性 token + CA 指纹固定 | 本文 §1.3 |
| Webhook `/webhook`（及 App 创建期间的 `/setup/github/callback`） | GitHub | 回环地址上的明文 HTTP；对公网一律经终止 TLS 的反向代理或隧道（P-65） | HMAC | forge / deploy 规格，**不属于本 API** |

- Webhook 不在 `/v1` 下、不在 API 监听器上；API 监听器对 `/webhook` 返回 404。反向代理 / 隧道只转发 `/webhook`（与创建 App 期间的 `/setup/github/callback`）到回环上的 webhook 监听器，因此 mTLS API 不会经它暴露。
- 沙盒与服务端之间没有任何网络端点（只有文件与 stdout，P-15），所以这里没有面向沙盒的入口。
- 客户端不会、也不应调用 Webhook（I6）。

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
| 服务端身份 | 配对串携带**离线根 CA** 的指纹（P-64）；客户端**不用系统信任库**，只接受链到该根的服务端证书（防中间人）。中间 CA 轮换不影响已配对设备 |
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
  "approval_pubkey": "BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7ZaASo8h6hMNHwYgitHzpH-S9kLN8ImOTHNk0q-vuB-sTk",
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
    "approval_pubkey_fpr": "sha256:7a1c3e5f90b2d4f6a8c0e2a4b6d8f0a2c4e6a8b0d2f4a6c8e0b2d4f6a8c0e2a4",
    "role": "approver",
    "status": "active",
    "last_seen": null
  },
  "certificate_chain": [
    "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----\n",
    "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----\n",
    "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----\n"
  ],
  "api_base_url": "https://gantry.example.com:8443",
  "ca_fingerprint": "sha256:3b7e0c52a91d6f48e2b05c7a9d1f3e64b8a20c5d7e91f3a6b4c8d0e2f1a39b57",
  "confirmation_code": "482913"
}
```

- `token` 是不透明字符串（示例仅为示意）。客户端本地生成**两把**密钥（P-47），**私钥都不出设备**：mTLS 客户端密钥（CSR 用它；后台连接无提示使用）与审批签名密钥（Secure Enclave / TPM，不可导出，每次使用必须用户在场：Touch ID / Windows Hello）。CSR 的 subject 由服务端忽略并重写（`CN = device.id`），密钥算法要求见 modules/pki.md（本文按 ECDSA P-256）。
- `approval_pubkey`：审批签名公钥，P-256 未压缩点的 base64url（无填充）；服务端登记在 Device 上，此后所有审批签名都用它验签（§6.3）。必须与 CSR 的公钥不同，否则 `422 validation_failed`。桌面档案（`platform` ∈ `windows` / `macos`）必带，缺失 → `422 validation_failed`；手机档案（`ios`）可省略，省略则 Device 不登记审批公钥、响应里 `approval_pubkey_fpr` 为 `null`（P-76）。
- `certificate_chain` 为 `[叶子, 中间 CA, 根]`（P-64）；客户端必须验证根证书的指纹等于配对串里固定的离线根 CA 指纹、链完整，再存入系统钥匙串 / 凭据存储。`ca_fingerprint` 是根 CA 指纹。
- `confirmation_code`（6 位）与服务端控制台显示的一致，由用户人工核对（architecture §7.1 步骤 5）。核对失败的处置、设备在核对前是否已 `active`：OQ-12。
- token 无效、已用、过期、被作废**统一**返回 `401 pair_token_invalid`，不区分原因（不给枚举 oracle）。响应丢失后 token 已被消耗，只能重新生成 token 再配对（本端点不支持 `Idempotency-Key`）。
- 配对成功写 `device.paired` 事件。新设备的 `role`：第一台配对设备为 `approver`（所有者），之后配对的设备默认 `operator`（P-23）；示例是第一台设备。手机档案设备至多 `operator`，即使是第一台（P-76）；第一台是手机时，安装引导期持续到第一台桌面设备配对，由它成为 approver（decisions D-18）。

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
| 枚举 | 小写字符串；`verdict` 用大写（`ACCEPT` 等）。`Level` 线上取值为 `"None"` / `"Low"` / `"Medium"` / `"High"` / `"Max"` / `"Ultimate"`（data-model §1） |
| `UntrustedText` | `{ "text": string, "source": string, "truncated": boolean }`（data-model §1.1） |
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
- 证书短期有效；客户端在证书剩余 1/3 有效期时（建议值）经 `POST /v1/devices/self/renew` 自动续期（§4.10，P-64）。

### 2.2 角色语义（层级：`approver` ⊃ `operator` ⊃ `viewer`）

| 角色 | 可以做 |
| --- | --- |
| `viewer` | 读取除 settings 之外的所有资源；订阅事件流 |
| `operator` | viewer + 发消息、调档 / 暂停 / 继续 / 放弃交付单元、重试失败节点、解除收件箱项、工作树人工锁、**读 settings 并写其中的中性 / 收紧类改动**、重命名设备、为内部动作发起审批请求（§6.6） |
| `approver` | operator + 获取审批 challenge 并签名 / 拒绝审批；带签名的写入：放宽类、"仅 approver 键"与带 secret `value` 的 settings 写入（config.md §2.5）、改设备角色、吊销设备（§6.6，P-18、P-23） |

第一台配对设备为 approver，之后配对的设备默认 operator（P-23）。手机档案设备永远不能成为 approver（P-76，见 §2.3）。

### 2.3 客户端档案与手机端子集

角色回答"能做哪类事"，**档案**回答"这个客户端是什么形态"。档案由 `Device.platform` 推出：

| 档案 | 判定 | 可用路由 |
| --- | --- | --- |
| `desktop` | `platform` ∈ {`windows`, `macos`} | 全部，按角色 |
| `mobile` | `platform` = `ios` | 仅下表 13 条（D-11：手机端只给 agent 发消息） |

> 角色只有三级，无法表达"只做对话"，所以另设档案（P-23）。`platform` 取值见 data-model Device。

**手机端角色至多 `operator`**（P-23、P-76）：发消息需要 operator；手机档案设备**永远不能**成为 approver——配对时角色至多 `operator`（即使是第一台），`PUT /v1/devices/{id}/role` 与主机上的 `gantry device recover --promote` 把它设为 approver 一律 `409 conflict`（`reason: mobile_cannot_approve`）。审批路由对手机档案一律 `403 forbidden_profile`。

手机端可用路由：

| 路由 | 用途 |
| --- | --- |
| `GET /v1/me` | 角色、档案、服务端时间 |
| `GET /v1/projects`、`GET /v1/projects/{project_id}` | 选项目 |
| `GET /v1/projects/{project_id}/conversations` | 找到对话 |
| `GET /v1/conversations/{conversation_id}/messages` | 读消息 |
| `POST /v1/conversations/{conversation_id}/messages` | 发消息（可带 `refs`；手机档案带的 `level_cap` 被服务端忽略，固定使用项目默认上限，P-35） |
| `GET /v1/inbox`、`GET /v1/inbox/{inbox_id}` | 读收件箱（不能 dismiss） |
| `GET /v1/inbox/summary` | 按项目的 open 计数（项目列表的"需要你"徽标，P-63） |
| `POST /v1/inbox/{inbox_id}/choose` | 对 `decision_needed` 与 `budget_exhausted` 选择选项（`budget_exhausted` 的 `raise_cap` 只在桌面；升档用 `escalation`，与审批一样只在桌面；`spec_needed` 没有选项，用发消息答复，P-79） |
| `POST /v1/devices/self/renew` | 续期本设备的客户端证书（P-64、P-75；所有档案都需要） |
| `PUT /v1/devices/self/push-token` | 注册 / 更新本设备的 APNs 推送令牌（M4，P-77） |
| `GET /v1/events` | 事件流；服务端只下发 `project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked` [提议] |

手机端不提供：交付单元、工作树、工作流、预算、审批、settings、设备管理（续期本设备证书与注册推送令牌除外）。

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
| 403 | `signature_required` | 写入需要审批签名但未带 `Gantry-Approval` 头（settings、改角色、吊销，§6.6）；附 `signing`（`action`、`target`、`subject_digest`） | 按 §6.6 发起审批、签名后带头重发 |
| 404 | `not_found` | 资源不存在，或路由不存在 | 刷新列表 |
| 405 | `method_not_allowed` | 路径存在但方法不对；附 `Allow` 头 | 客户端 bug |
| 409 | `conflict` | 状态冲突；附 `reason`（如 `self_role_change`、`invalid_state`） | 刷新后让用户重新决定 |
| 409 | `idempotency_in_progress` | 同一 `Idempotency-Key` 的首次请求仍在处理 | 退避后用同一 key 重试 |
| 409 | `worktree_locked` | 工作树锁被其他持有者占用；附 `lock` | 展示持有者与 `lease_until` |
| 409 | `approval_not_pending` | 审批已非 `pending`；附 `approval_status` | 刷新审批，展示最终状态 |
| 409 | `challenge_invalid` | nonce 不匹配 / 已用 / 已过期 / 本设备没有 challenge；附 `reason` | 重新 `GET challenge`，再签 |
| 409 | `stale_head` | PR head 已变，审批被置为 `invalidated`；附 `current_head_sha` | 展示"代码已更新，审批失效"，等待重新请求 |
| 409 | `approval_mismatch` | `Gantry-Approval` 引用的审批不可用于本次写入；附 `reason`（`not_granted` / `consumed` / `expired` / `action_mismatch` / `target_mismatch` / `digest_mismatch` / `base_rev_mismatch`） | 按 §6.6 重新发起审批 |
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
| 必带 | `POST …/messages`、`POST /v1/approvals`、`POST …/grant`、`POST …/deny`、`POST /v1/inbox/{inbox_id}/choose`、控制路由 `POST /v1/deliveries/{delivery_id}/pause`、`POST /v1/deliveries/{delivery_id}/resume`、`POST /v1/nodes/{node_id}/retry`（P-56）（缺失 → `400 idempotency_key_required`） |
| 可带 | 其余 `POST` 状态流转。它们本身按"目标状态幂等"：已在目标状态则 `200` 返回当前表示（abandon 已放弃的、dismiss 已 dismissed 的、revoke 已吊销的） |
| 不适用 | `GET` / `PUT` / `PATCH` / `DELETE`（天然幂等或用 `If-Match`；含 `PUT /v1/devices/self/push-token`，同值重放无副作用）；`POST /v1/pair` |
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

- **每个响应**（含错误响应与事件流的响应头）带头 `X-Log-Epoch: <log_epoch>`（P-67）：事件日志的纪元号，每次服务端从备份恢复加一。客户端保存它；发现与本地值不同就丢弃全部本地缓存与游标，按 §5.3 从头全量同步。
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
| 审批有效期（`pending → expired`，含内部动作） | `policies.approval.ttl_s`，默认 86400 s（24 小时，建议值；config §5.4） |
| 客户端证书自动续期 | 剩余有效期 ≤ 1/3 时（建议值，P-64） |
| 工作树人工锁租约 | 默认 1800 s，最长 14400 s |
| 请求体上限 | 64 KiB；settings 256 KiB |
| `message.text` / `refs` | ≤ 16 KiB / ≤ 20 条 |
| 限流（每设备） | 读 600/min；写 120/min；发消息 30/min；challenge 与发起审批 30/min；配对 5/min/源 IP |
| 客户端时钟偏差告警 | 与 `/v1/me` 的 `server.time` 相差 > 60 s |
| 吊销后事件流断开 | 心跳内的吊销检查（M1），**上限 60 s**（建议值，P-30） |

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
| 26 | POST | `/v1/approvals` | operator | — | 为内部动作发起审批（幂等键：必） | §6.6 |
| 27 | GET | `/v1/approvals/{approval_id}/challenge` | approver | — | 取签名 challenge | §6.2 |
| 28 | POST | `/v1/approvals/{approval_id}/grant` | approver | — | 提交签名放行（幂等键：必） | §6.4 |
| 29 | POST | `/v1/approvals/{approval_id}/deny` | approver | — | 拒绝（幂等键：必） | §6.1 |
| 30 | GET | `/v1/settings/{resource}` | operator | — | 读全局设置（`?rev=N`） | §4.9 |
| 31 | PUT | `/v1/settings/{resource}` | operator* | — | 写全局设置（If-Match；`?dry_run`） | §4.9 |
| 32 | GET | `/v1/projects/{project_id}/settings/{resource}` | operator | — | 读项目覆盖 / 生效值 | §4.9 |
| 33 | PUT | `/v1/projects/{project_id}/settings/{resource}` | operator* | — | 写项目覆盖（If-Match；`?dry_run`） | §4.9 |
| 34 | DELETE | `/v1/projects/{project_id}/settings/{resource}` | operator* | — | 清除项目覆盖（If-Match） | §4.9 |
| 35 | GET | `/v1/settings/catalog` | operator | — | 设置页表单目录 | §4.9 |
| 36 | GET | `/v1/settings/status` | operator | — | 各模块已应用的 rev 与健康 | §4.9 |
| 37 | GET | `/v1/devices` | viewer | — | 设备列表 | §4.10 |
| 38 | PATCH | `/v1/devices/{device_id}` | operator | — | 重命名 | §4.10 |
| 39 | PUT | `/v1/devices/{device_id}/role` | approver | — | 改角色（带签名，§6.6） | §4.10 |
| 40 | POST | `/v1/devices/{device_id}/revoke` | approver | — | 吊销（带签名，§6.6） | §4.10 |
| 41 | GET | `/v1/events` | viewer | ✓ | SSE 事件流 | §5 |
| 42 | POST | `/v1/inbox/{inbox_id}/choose` | operator | ✓* | 选择上报项的一个选项（幂等键：必） | §4.8 |
| 43 | POST | `/v1/deliveries/{delivery_id}/pause` | operator | — | 暂停交付单元（幂等键：必，P-56） | §4.4 |
| 44 | POST | `/v1/deliveries/{delivery_id}/resume` | operator | — | 继续交付单元（幂等键：必，P-56） | §4.4 |
| 45 | POST | `/v1/nodes/{node_id}/retry` | operator | — | 重试失败节点（幂等键：必，P-56） | §4.7 |
| 46 | GET | `/v1/inbox/summary` | viewer | ✓ | 按项目的 open 计数（P-63） | §4.8 |
| 47 | POST | `/v1/devices/self/renew` | viewer | ✓ | 提交新 CSR 续期本设备证书（P-64） | §4.10 |
| 48 | PUT | `/v1/devices/self/push-token` | operator | ✓ | 注册 / 更新本设备的 APNs 推送令牌（M4，P-77） | §4.10 |

合计 **48 条**：mTLS API 47 条 + 配对 1 条；手机档案 13 条（#2 #3 #4 #10 #11 #12 #21 #22 #41 #42 #46 #47 #48）。`✓*`：手机档案只能对 `decision_needed` 与 `budget_exhausted` 选择，且 `budget_exhausted` 的 `raise_cap` 返回 `403 forbidden_profile`；`escalation`（升档等会增加花费的选择一律用它，P-63）与 `approval_needed` 返回 `403 forbidden_profile`（P-35）；`spec_needed` 没有选项（P-79），任何档案对它 `choose` 都是 `422`。

`*` settings 的"最低角色"是**路由下限**：服务端按改动**内容**判定实际所需角色与签名（config.md §2.5：中性 / 收紧 → operator，无签名；放宽、"仅 approver 键"、任何带 secret `value` 的写入 → approver + 审批签名，§6.6），`dry_run` 先告知 `required_role` / `requires_signature`（§4.9）。读类 settings 路由 operator 起，viewer 不能读 settings。

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
    "approval_pubkey_fpr": "sha256:7a1c3e5f90b2d4f6a8c0e2a4b6d8f0a2c4e6a8b0d2f4a6c8e0b2d4f6a8c0e2a4",
    "role": "approver",
    "status": "active",
    "last_seen": "2026-10-24T12:41:03.120Z"
  },
  "profile": "desktop",
  "certificate": { "not_after": "2026-11-23T12:00:00.000Z", "renew_after": "2026-11-13T12:00:00.000Z" },
  "server": { "api_version": "v1", "version": "0.1.0", "time": "2026-10-24T12:41:03.456Z", "log_epoch": 1 }
}
```

`certificate.renew_after` 是客户端应发起自动续期的时刻（剩余 1/3 有效期，建议值，P-64）；`server.log_epoch` 同响应头 `X-Log-Epoch`（§3.6）。`device` 对象里只返回两把公钥的指纹，不返回公钥本身。

### 4.3 projects

`Project` 表示（`settings_override` **不在此返回**，只能经 settings 路由读写，I7）：

```json
{
  "id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "repo": "acme/web",
  "installation_id": 48213907,
  "bot_user_id": 9988776,
  "default_branch": "main",
  "allowed_merge_methods": ["squash", "merge"],
  "merge_queue_enabled": false
}
```

- `GET /v1/projects` → `{ "items": [Project…], "next_cursor": null }`。
- `GET /v1/projects/{project_id}` → `Project`。
- 后三项是仓库设置快照（只读）。新任务的默认档位不是 Project 字段，而是配置 `levels.default_level`（项目可覆盖，P-22），读写走 §4.9。

### 4.4 deliveries

`Delivery` 表示（`issues[]` 为 `IssueRef`，`pr` 为 `PullRequestRef`，均为 GitHub 快照，`synced_at` 是最近对账时间，**交付事实以 GitHub 为准**，I4）。`level_effective` 的初值由引擎在规格阶段按风险评估给出（≤ `level_cap`），之后只在上限内升（P-57）；`stage_status` ∈ `running` / `waiting_human` / `blocked` / `paused` / `done`，转移规则见 data-model §4.1：

```json
{
  "id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "title": "登录接口超时改为 30 秒",
  "level_cap": "High",
  "level_effective": "Medium",
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
    "base_sha": "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
    "draft": false,
    "mergeable_state": "blocked",
    "in_merge_queue": false,
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
| `PATCH /v1/deliveries/{delivery_id}` | `{ "level_cap": "Max" }`（仅此字段；`stage` 为 `closed` / `abandoned` 时 `409 conflict`，`reason: invalid_state`） | `200 Delivery`；产生 `delivery.level_changed`。调高上限只放宽引擎可加码的范围，`level_effective` 不会因此自动上升；调低到 `level_effective` 以下时 `level_effective` 随之下调，但已触发的闸门不撤销。两种情况都**不改变**创建时按 `level_cap` 固定的预算模板（P-57；预算耗尽后的追加见 decisions D-19）。桌面端消息带 `level_cap` 且 `@` 已有交付时等同本路由（P-82，§4.5） |
| `POST /v1/deliveries/{delivery_id}/abandon` | `{ "reason": "不做了" }`（`reason` 可选，≤ 500 字符） | `200 Delivery`（`stage: abandoned`，`stage_status: done`）；产生 `delivery.abandoned` |
| `POST /v1/deliveries/{delivery_id}/pause` | `{}`；`Idempotency-Key` 必带 | `200 Delivery`（`stage_status: paused`）；产生 `delivery.paused`。引擎不再派发新节点，运行中的尝试跑到安全点；已是 `paused` 原样 `200`；终态 → `409 conflict`（`invalid_state`）（P-56） |
| `POST /v1/deliveries/{delivery_id}/resume` | `{}`；`Idempotency-Key` 必带 | `200 Delivery`（`stage_status` 回到暂停前的值，期间上报项已处理则为 `running`）；产生 `delivery.resumed`。不是 `paused` → 已在目标状态则原样 `200`，终态 → `409 conflict` |
| `GET /v1/deliveries/{delivery_id}/budget` | — | `BudgetLedger` |

`BudgetLedger`（只读的运行状态；改预算只能走 settings，且只对之后创建的交付单元生效——config.md §2.6：创建时固化，I7）：

```json
{
  "id": "bdg_01JB7M4Q1T5X9V2N6R8K3W7YZC",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "subject": "delivery",
  "template": "high.multi_review",
  "limits": {
    "tokens_by_tier": { "utility": 400000, "planner": 300000, "worker": 8000000 },
    "nodes": 40,
    "rounds": 4,
    "wall_time": 10800,
    "forge_writes": 120,
    "concurrency": 3
  },
  "used": {
    "tokens_by_tier": { "utility": 18400, "planner": 52100, "worker": 611000 },
    "nodes": 4,
    "attempts": 5,
    "rounds": 1,
    "wall_time": 940,
    "forge_writes": 3
  }
}
```

> 计数口径见 data-model BudgetLedger：token 以 Egress 计量为准，按 `levels.token_weights` 加权（P-70）；`wall_time` 为秒、尝试运行时长之和；`forge_writes` 是累计数（按小时限速是配置项，不在台账里）。示例交付单元的 `level_cap` 是 High，所以模板与限额来自 `levels.high`，创建时固定（P-57），与 `level_effective`（Medium）无关。None 档问答的会话台账（`subject: conversation`，P-58）不经本路由暴露。

### 4.5 conversations / messages

`Conversation` 表示：

```json
{ "id": "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC", "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA" }
```

`GET /v1/projects/{project_id}/conversations` → 列表，目前恰好一项：每个项目一个主对话，在项目绑定时创建（`conversation.created`，P-22）；保留列表形状以便将来扩展。

`Message` 表示：

```json
{
  "id": "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD",
  "conversation_id": "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC",
  "role": "user",
  "agent_kind": null,
  "tone": null,
  "text": "把登录接口的超时改成 30 秒，参考 #5",
  "level_cap": "High",
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
  "tone": null,
  "text": "PR #12 已就绪，合并前需要你的审批。",
  "level_cap": null,
  "refs": [{ "kind": "pr", "repo": "acme/web", "number": 12 }],
  "cards": [{ "kind": "approval", "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK" }],
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
}
```

一条 `role: event` 的事件行（守护静默修复 CI，由 `guardian.triaged` 渲染）：

```json
{
  "id": "msg_01JB7NB4K8R2X6V9M3Q5T1W7YZ",
  "conversation_id": "cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC",
  "role": "event",
  "agent_kind": null,
  "tone": "ok",
  "text": "后台已修复 PR #12 的 lint 失败（f2c03d8）",
  "level_cap": null,
  "refs": [{ "kind": "pr", "repo": "acme/web", "number": 12 }],
  "cards": [],
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
}
```

| 字段 | 规则 |
| --- | --- |
| `role` | `user` / `agent` / `event`（`event` 是折叠显示的后台事件行） |
| `tone` | 仅 `role: event`：`info` / `ok` / `run` / `warn`，决定事件行圆点颜色；其他角色为 `null`。客户端不认识的值按 `info` 显示 |
| `agent_kind` | `planner` / `worker` / `utility` / `null`；只决定气泡边框颜色（D-12：不显示头像） |
| `refs[]` | `Ref`（data-model §1.1）：`{"kind":"issue","repo":"o/r","number":5}`、`{"kind":"pr","repo":"o/r","number":12}`、`{"kind":"worktree","id":"wtr_…"}`、`{"kind":"delivery","id":"dlv_…"}`。客户端只发上述最小字段，`node_id` / 快照由服务端解析；解析不到 → `422 validation_failed` |
| `cards[]` | `Card`（data-model §1.1）：`plan` / `approval` / `escalation` / `choice` / `summary`（`summary` 卡为 `items[]`，每项 `{text, ref}`）；`kind` 集合开放，**未知 `kind` 必须降级为不显示或显示"请升级客户端"**。详情总是再取资源，卡片不内嵌可签名内容 |
| `level_cap` | 用户消息附带的**任务级**档位上限，`null` = 用配置 `levels.default_level` 的生效值；`"None"` = 纯对话不建交付单元（architecture §8；读代码的问答记会话台账，P-58）。不是配置（§8.2）。**手机档案**发出的消息，服务端忽略该字段，固定使用项目默认上限（P-35），回执消息复述实际生效的上限。**桌面档案**的消息带 `level_cap` 且 `refs` 里 `@` 的是已有交付单元：视为调整该交付单元的上限，等同 `PATCH /v1/deliveries/{id}`（产生 `delivery.level_changed`，预算模板不变；值与当前上限相同则不变），客户端发送前提示影响（P-82） |
| `delivery_id` | 服务端异步关联；`POST` 响应里通常是 `null`，客户端从 `delivery.created` 等事件得知，不要轮询它 |

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/conversations/{conversation_id}/messages` | 查询：分页（`order` 默认 `desc`） | 列表 `Message` |
| `POST /v1/conversations/{conversation_id}/messages` | 见下；`Idempotency-Key` 必带 | `201 Message`（`role: user`），头 `X-Event-Seq` = `message.posted` 的 `seq` |

请求：

```json
{
  "text": "把登录接口的超时改成 30 秒，参考 #5",
  "level_cap": "High",
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

`Workflow` 表示（`GET /v1/workflows/{workflow_id}`，含当前修订的节点；`?include_superseded=true` 额外返回被图改写替换的节点，供"图修订历史"）。计划是独立阶段（P-55）：实现阶段的图是 `build → verify`（Low 为 `single`），`plan` 在计划阶段的图里。None 档读代码的临时图 `delivery_id` 与 `stage` 为 `null`、`conversation_id` 非空（P-58）：

```json
{
  "id": "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "conversation_id": null,
  "stage": "implement",
  "graph_rev": 1,
  "status": "succeeded",
  "template": "medium.implement",
  "nodes": [
    {
      "id": "nod_01JB7M7V4X2N9R5T8Q3K6W1YZG",
      "workflow_id": "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF",
      "role": "build",
      "tier": "worker",
      "exec": "sandbox",
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
      "exec": "sandbox",
      "depends_on": ["nod_01JB7M7V4X2N9R5T8Q3K6W1YZG"],
      "required": true,
      "status": "succeeded",
      "verdict": "ACCEPT",
      "superseded_by": null
    }
  ]
}
```

`Attempt` 表示（`status`、`failure_class` 枚举与 `usage` 口径见 data-model；`usage` 是 Egress 计量值；`result_ref` 是内容寻址对象，结果内容的读取路由未定，OQ-11）：

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
  "failure_detail": null,
  "usage": { "input_tokens": 41200, "output_tokens": 6800, "cache_read_tokens": 30500, "cache_write_tokens": 2100, "requests": 9 }
}
```

| 路由 | 查询 | 响应 |
| --- | --- | --- |
| `GET /v1/deliveries/{delivery_id}/workflows` | 分页；`stage` | 列表 `Workflow`（不含 `nodes`，取详情再展开） |
| `GET /v1/workflows/{workflow_id}` | `include_superseded` | `Workflow`（含 `nodes`） |
| `GET /v1/nodes/{node_id}/attempts` | 分页（默认 `order=asc`，按 `n`） | 列表 `Attempt` |
| `GET /v1/attempts/{attempt_id}` | — | `Attempt` |
| `POST /v1/nodes/{node_id}/retry` | `{}`；`Idempotency-Key` 必带（P-56） | `202`，体为该节点（`status` 仍为当前值）；产生 `node.retry_requested`，引擎随后新建尝试（`attempt.started`）。只对 `failed` 且未被替换的节点有效，否则 `409 conflict`（`invalid_state`）；交付单元 `paused` 或终态 → `409 conflict`；重试受预算约束，预算不足时引擎上报 `budget_exhausted` 而不是新建尝试 |

尝试不可变（architecture §2）：同一 `Attempt` 的重复读取，终态后内容恒定。重试不修改旧尝试，只追加新尝试。

### 4.8 inbox

```json
{
  "id": "inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "kind": "approval_needed",
  "severity": "blocker",
  "summary": "PR #12 可以合并，需要你的审批。",
  "payload": { "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK" },
  "status": "open",
  "dedup_key": "approval:apr_01JB7P4M8T2X6N9R3Q5K1V7WZK"
}
```

带选项的升档上报（`escalation`，P-63；手机档案对它 `choose` 返回 `403 forbidden_profile`）：

```json
{
  "id": "inb_01JB7P3M7S1T4X6N9Q2V5W8YZK",
  "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "kind": "escalation",
  "severity": "warn",
  "summary": "改动触及 auth 路径，需要升到 Max 才能插入对抗评审。",
  "payload": {
    "reason_code": "level_cap_exceeded",
    "node_id": null,
    "attempt_id": null,
    "evidence": [{ "kind": "pr", "repo": "acme/web", "number": 12 }],
    "options": [
      { "id": "upgrade", "label": "升到 Max 并继续", "primary": true },
      { "id": "narrow", "label": "保持当前，缩小范围" },
      { "id": "abandon", "label": "放弃" }
    ]
  },
  "status": "open",
  "dedup_key": "escalation:dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB:level_cap_exceeded:Max"
}
```

`GET /v1/inbox/summary` 响应（只数 `status = open`；全局事项的 `project_id` 为 `null`，P-63）：

```json
{
  "total_open": 3,
  "by_project": [
    { "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA", "open": 2, "max_severity": "blocker" },
    { "project_id": null, "open": 1, "max_severity": "warn" }
  ]
}
```

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/inbox` | 查询：`project_id`、`status`（默认 `open`）、`kind`、分页 | 列表 `InboxItem` |
| `GET /v1/inbox/{inbox_id}` | — | `InboxItem` |
| `GET /v1/inbox/summary` | — | 按项目的 open 计数（上例）；供项目 tab 与手机项目列表的徽标，客户端收到 `inbox.*` 事件后可本地增减，也可重新拉取 |
| `POST /v1/inbox/{inbox_id}/dismiss` | `{ "note": "已知晓" }`（可选） | `200 InboxItem`（`status: dismissed`），产生 `inbox.dismissed`；已是 `dismissed` 则原样 `200`；`resolved` 的 → `409 conflict`（`reason: invalid_state`） |
| `POST /v1/inbox/{inbox_id}/choose` | `{ "option_id": "upgrade", "note": "可选的补充说明" }`；`Idempotency-Key` 必带 | `200 InboxItem`（`status: resolved`），产生 `inbox.resolved`（`data.option_id`）；选项不在 `payload.options` 中（含没有选项的 `spec_needed`）→ `422`；已不是 `open` → `409 conflict`；手机档案的限制见 §4.1 `✓*` |

- 带选项的上报项（`decision_needed`、`escalation` 的 `level_cap_exceeded`、`budget_exhausted`）在 `payload.options[]` 里给出 `{id, label, primary?}`（`primary` 标记默认 / 推荐项，P-63）；`reason_code` 与选项 id 的词表固定在 [data-model](../data-model.md#上报项的-reason_code-与选项词表)（P-79），客户端按 id 渲染、不自行增减；对话里的选择卡片与收件箱**引用同一个 `inbox_id`**，按钮都调 `choose`（P-40）。升档一律是 `escalation`，只能在桌面选择（P-35）。
- `project_id` 与 `dedup_key`（P-63）：服务端用 `dedup_key` 去重——同一键已有 `open` 项时不再新建；客户端视 `dedup_key` 为不透明字符串，只用于展示"重复出现"。
- `decision_needed` 的 `payload.kind = manual_action`（如 workflow 补丁，P-48）表示需要用户在 Gantry 之外手动完成；`payload.patch.ref` 指向的补丁内容读取路由与 `result_ref` 一样尚未定义（OQ-11）。
- 桌面系统通知：桌面端收到 `inbox.raised` 且 `severity` 为 `warn` / `blocker` 时弹系统通知（P-63），不另设事件或路由。
- 自由文本答复（`spec_needed`，如补充验收标准）走发消息路由，消息带 `refs` 指向该交付单元；会话服务据此处理后由服务端 `resolve` 上报项（P-79）。
- 审批类上报项（`approval_needed`）只能经审批路由（§6）处理，`choose` 对它返回 `409`。
- `summary` 可能含不可信内容，按 §1.5 渲染。`severity`（`info` / `warn` / `blocker`）与按 `kind` 的 `payload` 形状见 data-model InboxItem。

### 4.9 settings

**唯一的配置读写入口**（I7）。HTTP 形状（路径、方法、头、响应外壳、错误）以本文为准；设置**文档的内容**——有哪些 resource、键、取值、默认、合并 / 收紧规则、校验规则 V01–V22、谁能改什么、热加载——以 [interfaces/config.md](config.md)（及 `schemas/config/*.json`）为准，本文不重复。`{resource}` 是 config.md 的顶层键，目前八个：`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`、`notify`、`github`（任务清单只点名了前三个）。读写粒度是**整个 resource**。下面示例里的 `value` 只截取个别键示意。

| 路由 | 请求 | 响应 |
| --- | --- | --- |
| `GET /v1/settings/{resource}[?rev=N]` | — | `200` 全局文档，头 `ETag: "<rev>"`；`?rev=N` 取该 resource 在历史 rev 时的文档（可追溯；"回滚"= 读旧版再 `PUT` 回去，没有专门的回滚路由） |
| `PUT /v1/settings/{resource}[?dry_run=true]` | 头 `If-Match`；需要签名时另带 `Gantry-Approval`（§6.6）；体 `{ "value": … }`，整体替换 | `200` 新文档 + `warnings[]`，头新 `ETag` 与 `X-Event-Seq`；`dry_run=true` 只校验、不落库、不发事件，返回 `{valid, errors[], warnings[], relaxes[], required_role, requires_signature, signing?}`（`signing` 仅当需要签名时出现，即 §6.6 发起审批所需的 `{action, target, subject_digest}`） |
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
  "value": { "high": { "budget": { "nodes": 40 } } }
}
```

`PUT /v1/settings/levels`（头 `If-Match: "18102"`）把 `high` 档的节点预算从 40 收紧到 30：

```json
{ "value": { "high": { "budget": { "nodes": 30 } } } }
```

响应 `200`（头 `ETag: "18236"`、`X-Event-Seq: 18236`；`rev` 与 ETag 一致）：

```json
{
  "resource": "levels",
  "scope": "global",
  "rev": 18236,
  "value": { "high": { "budget": { "nodes": 30 } } },
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

写入凭据时 `dry_run` 的响应（`requires_signature: true`，`signing` 原样用于 `POST /v1/approvals`，§6.6）：

```json
{
  "valid": true,
  "errors": [],
  "warnings": [],
  "relaxes": [],
  "required_role": "approver",
  "requires_signature": true,
  "signing": {
    "action": "settings_update",
    "target": { "kind": "settings", "scope": "global", "resource": "models", "base_rev": 18236 },
    "subject_digest": "sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a"
  }
}
```

写入模型凭据：`PUT /v1/settings/models`（头 `If-Match: "18236"`、`Gantry-Approval: apr_…`）请求体里的凭据字段是 `SecretRef` 对象，`value` 只写；响应里同一字段只剩 `secret_ref` 与只读的 `present`：

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
  "value": { "high": { "budget": { "nodes": 30 } } },
  "clamped": []
}
```

`GET /v1/settings/status` 响应（字段名以 config.md §2.8 为准；版本字段统一叫 `applied_rev`，第二轮裁决 C26）：

```json
{ "latest_rev": 18240, "modules": { "egress": { "applied_rev": 18102, "health": "ok", "unresolved_secret_refs": [] } } }
```

| 规则 | 说明 |
| --- | --- |
| 整体替换 | `PUT` 替换整个 resource 的 `value`，不合并；先 `GET` 再改再 `PUT`。全局 `PUT {"value": {}}` 回到内置默认；项目覆盖用 `DELETE` 清除。存储是稀疏的、合并与"仅收紧"约束见 config.md §2.1–§2.2 |
| 并发 | `rev` / `ETag` / `If-Match` 见 §3.4；两个设置页同时保存同一 resource，后到者得到 `412`，重新 `GET` 后在新值上重做 |
| 角色与签名 | 路由下限 operator；服务端按内容判定实际所需角色与签名（config.md §2.5）。不足 → `403 forbidden_role`（附 `required_role`）；提交前用 `dry_run` 取得 `required_role` / `requires_signature` / `signing`。需要签名而未带 `Gantry-Approval` → `403 signature_required`；审批不匹配 → `409 approval_mismatch`。签名机制见 §6.6（M1 起实现，P-18） |
| 凭据字段 | 以 config.md §2.4 为准：凭据字段是 `SecretRef`（`secret_ref`、只读的 `present`、只写的 `value`）。`GET` 永不返回 `value`；`PUT` 里 `value` 省略 = 保持不变，字符串 = 写入或轮换，`null` 一律 `422`（`secret_clear_in_use`）；`present` 被接受并忽略。值与 `settings.updated` 原子写入：模型 key 与私有包仓库凭据经 Egress 控制面两段式转交（暂存 → 事件提交后激活，失败则丢弃，[egress-control.md §4.4](egress-control.md#44-密钥只写不读)），`present` 也由控制面查询；`api` 不留存值。客户端发送后立即清除内存、不得持久化或记日志 |
| 校验 | 先 schema 再语义；失败 → `422 validation_failed`，`errors[].code` 取 `required` / `type` / `enum` / `range` / `unknown_field` 与 config.md §11 的语义码；`warning` 不阻止写入，随 `200` / `dry_run` 返回 |
| 事件 | `settings.updated`（data-model §5.2）：`subject` 为字面量 `settings`（全局）或 `prj_…`（项目覆盖）；**项目覆盖变更也发 `settings.updated`，不再发 `project.updated`**；只下发给桌面档案且角色 ≥ operator 的事件流（§5.4）。客户端收到后重新 `GET` 该 resource |
| 生效时机 | 判定类键下一次判定起生效；`levels.*` 只对之后创建的交付单元生效；以 `status` 查看各模块是否已应用（`applied_rev`，config.md §2.6、§2.8） |
| 安装引导期 | 第一台设备配对之前，没有 HTTP 客户端；"仅 approver 键"（`github.*`、`sandbox.base_image` 等）只能由主机命令（需 root）写入，产生 `actor.kind = system` 的 `settings.updated`；第一台设备配对之后，同样的写入只能经本路由并带审批签名（P-66） |
| 不在 settings 里的 | 设备与证书（§4.10）、本机外观（`desktop-shell` 本地存储）、任务级参数（`Message.level_cap`） |

### 4.10 devices

`Device` 表示同 §4.2 的 `device` 对象。

| 路由 | 请求 | 响应 / 规则 |
| --- | --- | --- |
| `GET /v1/devices` | 分页 | 列表 `Device`（含已吊销，`status` 区分；`last_seen` 为节流更新） |
| `PATCH /v1/devices/{device_id}` | `{ "name": "书房 Mac" }`（仅 `name`，1–64 字符） | `200 Device`；产生 `device.updated` |
| `PUT /v1/devices/{device_id}/role` | `{ "role": "operator" }`；头 `Gantry-Approval`（动作 `device_role_change`，§6.6）；**不能改自己**（`409 conflict`，`reason: self_role_change`） | `200 Device`；产生 `device.updated`。新角色对该设备的**下一个请求**立即生效 |
| `POST /v1/devices/{device_id}/revoke` | `{}`；头 `Gantry-Approval`（动作 `device_revoke`，§6.6） | `200 Device`（`status: revoked`）。可吊销自己；已吊销则原样 `200`。产生 `device.revoked`，同时追加到事件日志之外的 `revocations.journal`（P-67），并断开该设备的全部事件流（§5.5） |
| `PUT /v1/devices/self/push-token` | `{ "token": "<APNs device token，hex>", "environment": "production" }`（`environment` ∈ `production` / `sandbox`）；operator 起，所有档案可调用（第一版只有手机档案使用：`notify` 的 APNs 渠道只给 `ios` 设备推送），M4（P-77） | `204`。令牌存于 `notify` 的私有表，按设备覆盖；**不进事件日志、不出现在任何读响应里**；设备吊销时清除。令牌格式非法 → `422 validation_failed`；限流同写操作 |
| `POST /v1/devices/self/renew` | `{ "csr": "-----BEGIN CERTIFICATE REQUEST-----\n…" }`；经现有 mTLS 连接（证书仍有效），viewer 起，所有档案可用（P-64） | `200 { "certificate_chain": [叶子, 中间 CA, 根], "not_after": "…" }`；客户端校验根指纹与配对时固定的一致后替换本地证书，旧证书到期前仍可用。CSR 可沿用或更换 mTLS 客户端密钥，**不涉及**审批签名密钥（`approval_pubkey` 不变）。CSR 非法 → `422 validation_failed`；设备已吊销 → 握手失败 / `401 device_revoked`；限流同写操作 |

- 吊销不可逆；要恢复必须重新配对（新设备记录）。`revocations.journal` 只追加，每次写入同步复制到备份目标（复制失败则吊销失败），事务回滚不删条目；服务端从备份恢复时重放其中时间戳晚于备份的吊销，被吊销的设备不会因恢复而复活（P-67、P-80）。
- 手机档案设备不能成为 approver：对 `platform = ios` 的设备 `PUT …/role` 设 `approver` → `409 conflict`（`reason: mobile_cannot_approve`，P-76）。
- 续期由客户端在证书剩余 1/3 有效期时自动发起（建议值），用户无感；续期失败时顶栏提示"证书将过期"。
- 改角色与吊销都需要 approver 签名（P-23）。

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
data: {"seq":18240,"ts":"2026-10-24T12:41:18.000Z","log_epoch":1}

```

| 帧 | 规则 |
| --- | --- |
| 业务事件 | `id:` = 事件 `seq`（十进制）；`event:` = 事件 `type`；`data:` = **data-model §5.1 信封的单行 JSON，原样不改**。`data` 字段内部结构以 `schemas/events/<type>.json` 为准，上例的 `from` / `to` 仅为示意 |
| `heartbeat` | 每 15 s 一次；`id:` = 本连接**已扫描**到的最大 `seq`（含被过滤掉的），所以过滤后的流也能推进游标；`data.log_epoch` 是事件日志纪元号（P-67）；不是日志事件，没有信封 |
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
| 纪元变化 | 响应头 `X-Log-Epoch` 或 `heartbeat` 的 `log_epoch` 与客户端保存的值不同（服务端从备份恢复后，`seq` 可能回退并被复用，单看游标发现不了）→ 客户端立即断开、丢弃缓存，按 §5.3 全量重同步（P-67） |
| 游标过旧 | `410 cursor_expired` 预留，骨架未定义事件保留期（OQ-13） |
| 慢消费者 | 服务端缓冲超限 → 发 `close`（`lagging`）并断开；因为可从日志回放，客户端重连即可，不丢事件 |

### 5.3 快照 + 事件流（客户端的标准衔接）

1. 并发拉取需要的快照（`/v1/projects`、`…/messages`、`/v1/deliveries?active=true`、`/v1/inbox` 等），记下每个响应的 `X-Event-Seq`。
2. 取其中最小值 `S`，连接 `GET /v1/events`（`Last-Event-ID: S`，**一个客户端一条流**，不按项目各开一条；手机端也同样）。
3. 收到事件后用 `seq` 去重并应用到本地状态；断线后用最后已应用的 `seq` 续传。
4. 遇到 `409 cursor_ahead` / `410 cursor_expired`，或 `log_epoch` 与本地保存的不同（P-67）：丢弃本地缓存，从第 1 步重来（这一次不带游标），并保存新的 `log_epoch`。

事件按需带出的实体（如 `message.posted`）若 `data` 内容不足以更新界面，客户端按事件的 `subject` 重新 `GET` 对应资源，不要猜。

### 5.4 过滤

| 参数 | 规则 |
| --- | --- |
| `project_id` | 可重复；只下发这些项目的事件。**没有 `project_id` 的全局事件**（如 `device.*`、全局 `settings.updated`）始终下发（信封里该字段对全局事件为 `null`，data-model §5.1） |
| `types` | 逗号分隔，精确类型或 `前缀.*`（如 `message.*`）；缺省 = 全部 |
| 手机档案 | 服务端再与白名单取交集：`project.*`、`conversation.*`、`message.*`、`inbox.*`、`device.revoked` [提议]；请求了白名单外的类型不报错，只是收不到 |
| `settings.*` | 只下发给**桌面档案且角色 ≥ operator** 的流（事件 `data` 含设置内容；config.md 诉求 A7）[提议]；按**发送时刻**的角色判定 |

### 5.5 吊销、过期与连接上限

| 情形 | 行为 |
| --- | --- |
| 设备被吊销 | M1：每次心跳检查 `Device.status`，发现已吊销即不再下发任何事件，尽力发 `close`（`device_revoked`）后关闭，因此**通常 15 s 内，上限 60 s**（建议值，P-30）。M3 优化为进程内按 `device_id` 即时断开。后续新连接在握手时被拒。客户端收到 `close(device_revoked)` 或随后的握手失败，按 §3.1 `device_revoked` 处理 |
| 证书到期 | 流在证书 `notAfter` 时关闭（`cert_expired`）；客户端应在剩余 1/3 有效期时经 `POST /v1/devices/self/renew` 续期（P-64），续期后用新证书重连 |
| 并发上限 | 每设备 3 条；超出 → `429 rate_limited`（`Retry-After`） |
| 角色变化 | 不断流；除 `settings.*` 外，事件可见范围不随角色变化（viewer 起都能订阅），只受档案白名单影响；降到 viewer 后不再下发 `settings.*` |

---

## 6. 审批签名

Gantry 审批（architecture §7.4）：用户用设备上的**审批签名密钥**（与 mTLS 客户端密钥分开，Secure Enclave / TPM，不可导出，每次使用必须用户在场，P-47）对"动作 + 目标 + 主体摘要（`subject_digest`）"签名，服务端用 Device 上登记的 `approval_pubkey` 验签后由 `policy` 放行（I3）。GitHub 上的 PR review approve 是另一件事，**不走本 API**；需要时客户端只展示跳转链接（`https://github.com/{repo}/pull/{number}`，由 `repo` + `number` 拼出，在系统浏览器打开）。

**统一签名原语**（P-18）：所有需要设备签名的操作都是一个 `Approval`，都走同一条链路——取 challenge（§6.2）→ 规范化载荷（§6.3）→ 设备签名 → 服务端验签（§6.4）。动作分两类：

| 类 | `action` | 谁创建审批 | 审批之后谁执行 |
| --- | --- | --- | --- |
| Forge 动作与条件伪动作 | `merge_pr`；`force_push`、`push_outside_prefix`、`resolve_review_thread_human`（[forge-actions §2.5](forge-actions.md#25-审批复核i3)） | 入队方（引擎 / 守护）调用 `policy` 后创建（`approval.requested`），客户端不能创建 | `forge` 在 Outbox 动作**执行开始前**消费（`approval.consumed`，记 `consumed_by = act_…`，P-54） |
| 补丁闸门 | `patch_apply_protected`（补丁命中处置为 `approve` 的路径规则，M2 起） | 引擎创建，客户端不能创建 | `repo` 在应用补丁、创建提交之前消费 |
| 内部动作 | `settings_update`、`device_role_change`、`device_revoke` | 客户端 `POST /v1/approvals`（§6.6） | 客户端带 `Gantry-Approval` 头重发原请求，服务端在同一事务里消费并执行 |

`Approval.action` 的取值集合与每个动作的 `target` / `subject_digest` 形状以 [data-model](../data-model.md#审批动作的-target-与-subject_digest) 为准（P-54）；形状未定义的动作一律 fail-closed，不会出现在本 API 中。

### 6.1 审批资源与读、拒绝

`Approval` 表示（`pending` 时 `device_id` / `signature` 为 `null`）：

```json
{
  "id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "action": "merge_pr",
  "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
  "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
  "device_id": null,
  "signature": null,
  "status": "pending",
  "expires_at": "2026-10-24T14:41:00.000Z"
}
```

- `status`：`pending` / `granted` / `consumed` / `invalidated` / `expired` / `denied`（data-model §4.4）。
- `action`、`target`、`subject_digest` 的取值（摘自 data-model，冲突以 data-model 为准）：

| `action` | `target` | `subject_digest` |
| --- | --- | --- |
| `merge_pr` | `{kind:"pr", repo, number}` | PR 的 head SHA（40 位十六进制） |
| `force_push` / `push_outside_prefix` | `{kind:"branch", repo, branch, expected_remote_oid}` | 将要推送的提交 SHA |
| `resolve_review_thread_human` | `{kind:"review_thread", repo, number, thread_node_id}` | 线程当前评论 ID 列表的 `sha256:…` |
| `patch_apply_protected`（M2 起） | `{kind:"patch", repo, attempt_id, rules:[…], paths:[…]}` | 补丁内容的 `sha256:…`（`ValidatedPatch.sha256`） |
| `settings_update` | `{kind:"settings", scope, project_id?, resource, base_rev}` | 请求体的 `sha256:…`（§6.6） |
| `device_role_change` / `device_revoke` | `{kind:"device", id}` | 请求体的 `sha256:…`（§6.6） |

`delete_branch` / `publish_release` / `change_repo_settings` 第一版不实现（P-27），不会出现。workflow 补丁不能经任何审批放行（P-48），也不会出现。内部动作的审批没有 `delivery_id`。审批有效期（`pending → expired`）是 `policies.approval.ttl_s`，默认 24 小时（建议值）。本文示例用 `merge_pr`。
- `GET /v1/approvals`：查询 `status`、`delivery_id`、分页；`GET /v1/approvals/{approval_id}`：`Approval`。viewer 起可读。
- `POST /v1/approvals/{approval_id}/deny`：`{ "reason": "先别合" }`（可选），`Idempotency-Key` 必带，`approver` 起，**不需要签名**（拒绝不授予任何权限，OQ-6）→ `200 Approval`（`denied`），发 `approval.denied`。非 `pending` → `409 approval_not_pending`。
- Forge 动作的审批由 `policy` / 引擎创建（`approval.requested`），客户端经事件、收件箱（`approval_needed`）和消息卡片得知，**客户端不能创建**；客户端只能为内部动作创建审批（§6.6）。

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
    "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
    "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
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

- `payload` 就是**要签名的全部内容**，字段对所有动作相同：`action`、`approval_id`、`delivery_id`（有交付单元的动作才有：Forge 动作、条件伪动作、`patch_apply_protected`；内部动作省略该键）、`device_id`、`expires_at`、`nonce`、`subject_digest`、`target`、`v`。`expires_at` 是 **challenge** 的过期时间（≤ `Approval.expires_at`，默认 TTL 120 s），不是审批的。`nonce` 是服务端为本次签发生成的 32 字节随机数（base64url，无填充）。
- `signing` 是自描述；客户端若不认识 `alg` / `canonicalization` / `prefix` / `v`，**必须中止**并提示升级，不得猜。
- 非 `pending` → `409 approval_not_pending`；本设备不是 `approver` → `403 forbidden_role`。

### 6.3 规范化与签名

**待签字节** = `UTF-8("gantry/approval/v1\n")` ‖ `canonical(payload)`，其中 `canonical` 是 RFC 8785（JCS）并限定如下（payload 只含字符串、整数、对象、数组）：

| 规则 | 说明 |
| --- | --- |
| 编码 | UTF-8，无 BOM |
| 空白 | 无任何非必要空白（无缩进、无换行、冒号和逗号后无空格） |
| 键顺序 | 每一层对象的键按 UTF-16 码元升序排序（payload 的键全是 ASCII，等价于字节序） |
| 键唯一 | 不允许重复键 |
| 字符串 | 按 JCS 转义：只转义 `"` `\` 与 U+0000–U+001F（用 `\b \t \n \f \r` 或 `\u00xx`），其余字符（含非 ASCII）原样输出 UTF-8，不转义为 `\uXXXX` |
| 数字 | **只允许整数**（`v`、`target.number`、`target.base_rev`），十进制，无前导零、无小数点、无指数；不允许浮点和 `null`（没有值的键直接省略） |
| 数组 | 允许（如 `patch_apply_protected` 的 `target.rules`、`target.paths`）：**保持原序**，不排序、不去重；元素按同样规则规范化（字符串、整数、对象），不得为 `null`。数组的顺序由服务端在创建审批时确定（路径按字节序升序），客户端原样签 |
| 时间 | RFC 3339 UTC，**恰好三位毫秒**，`Z` 结尾（如 `2026-10-24T12:52:00.000Z`） |
| 大小写 | `subject_digest`、`nonce` 等按 challenge 原样，客户端不做任何规范化 |

**算法**：`alg = ES256`，即 ECDSA P-256 + SHA-256，签名编码为**原始 `r ‖ s` 共 64 字节**（IEEE P1363，非 DER），再 base64url 无填充（86 字符）。系统 API 若返回 DER（Secure Enclave / Keychain），由 `desktop-shell` / `mobile` 转换。签名密钥 = 设备的**审批签名密钥**（P-47），**不是** mTLS 客户端密钥：配对时单独生成、不可导出、每次使用必须用户在场（Touch ID / Windows Hello）；服务端用配对时登记的 `Device.approval_pubkey` 验签，不用证书里的公钥。

**测试向量**（Mock API 与两端实现都必须用它自测）：

| 项 | 值 |
| --- | --- |
| 审批签名公钥（P-256 未压缩点，base64url；即 §1.3 示例的 `approval_pubkey`） | `BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7ZaASo8h6hMNHwYgitHzpH-S9kLN8ImOTHNk0q-vuB-sTk` |
| `canonical(payload)`（即上面 §6.2 的 `payload`，单行） | `{"action":"merge_pr","approval_id":"apr_01JB7P4M8T2X6N9R3Q5K1V7WZK","delivery_id":"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB","device_id":"dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM","expires_at":"2026-10-24T12:52:00.000Z","nonce":"lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js","subject_digest":"9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345","target":{"kind":"pr","number":12,"repo":"acme/web"},"v":1}` |
| 待签字节的 SHA-256（hex） | `be72c31cfcdc6b5eed579c8d77def70ea64afa994f89205904cd05ba07fd1d7a` |
| 一个有效签名（base64url） | `QAzQTiBfxzxWOP2G_q8M4i405Cr8GOZvAFZlbZGCC96ICBp04L9nG2vAWEbWF-jQAkPYVrwZjbBeJGf8nUdDLg` |

验签通过 = 用上述公钥验证"有效签名"对"待签字节"成立。ECDSA 签名不确定，所以不同实现各自签出的值会不同；canonical 字节与它的 SHA-256 必须逐字节一致。

### 6.4 `POST /v1/approvals/{approval_id}/grant`

`approver` 起；`Idempotency-Key` 必带。请求只带 nonce 与签名，**不带 payload**——服务端从自己保存的 challenge 重建 payload 并验签，客户端无法让服务端验证"另一份内容"：

```json
{
  "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
  "signature": "QAzQTiBfxzxWOP2G_q8M4i405Cr8GOZvAFZlbZGCC96ICBp04L9nG2vAWEbWF-jQAkPYVrwZjbBeJGf8nUdDLg"
}
```

响应 `200 Approval`：

```json
{
  "id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
  "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
  "action": "merge_pr",
  "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
  "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
  "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
  "signature": "QAzQTiBfxzxWOP2G_q8M4i405Cr8GOZvAFZlbZGCC96ICBp04L9nG2vAWEbWF-jQAkPYVrwZjbBeJGf8nUdDLg",
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
| 4 | 用本设备登记的 `approval_pubkey`（P-47），对服务端保存的 payload 重建的待签字节验签 | `403 signature_invalid` |
| 5 | **主体一致性**（仅 PR 类动作）：payload 的 `subject_digest` 等于 forge 对账后的当前 PR head（必要时先强制对账一次）。不等 → 审批置 `invalidated`，发 `approval.invalidated`。补丁与内部动作的主体在消费时核对 | `409 stale_head`（附 `current_head_sha`） |
| 6 | 通过：写 `Approval`（`device_id`、`signature`、`granted`），发 `approval.granted`；nonce 作废 | `200` |

- 第 5 步只是**提前失败**。权威的主体检查在**消费审批时**（Outbox 动作执行开始前由 `forge` 经 `policy` 验证并消费，I3、P-54；补丁审批由 `repo` 在提交前消费；内部动作见 §6.6）：`granted` 只在 `subject_digest` 仍与当前主体一致时可被消费，消费后为 `consumed`（`approval.consumed`），不能再用；执行终态失败不退回审批，重做需要新审批；之后任何推送都使绑定 head 的审批失效。`200` 不代表已合并；合并由 Outbox 异步执行，进展经 `outbox.*` / `forge.*` 事件到达。
- 同一设备凭同一 `Idempotency-Key` 重试 → 重放首次 `200`（响应丢失的补救）；换 key 重发已用 nonce → `409 challenge_invalid`（`used`），再 `GET approval` 看最终状态。
- 多个 approver 设备并发：先到先得，其余得到 `409 approval_not_pending`。

### 6.5 界面展示必须与签名载荷一致

目标：用户看到并确认的内容 = 设备实际签名的内容。威胁：展示层（WebView 里的 `desktop-ui`）被注入文本欺骗，或实现者把"展示用数据"和"签名用数据"分开取。

**界面必须展示的字段**（全部来自同一个已解析的 `payload`）：

| payload 字段 | 展示要求 |
| --- | --- |
| `action` | 动作的本地化名称，旁边小字显示原始 `action` 值；**不认识的 `action` 拒绝签名**并提示升级客户端 |
| `target` | `repo` + 类型 + 编号 / ref 完整展示（如 `acme/web · PR #12`），不得缩写或只显示标题 |
| `subject_digest` | 审批单显示**完整值**（SHA 为全部 40 位，等宽可复制）；PR 类动作提供"在 GitHub 打开该 commit"的核对入口；补丁审批提供补丁内容（由 shell 从服务端取回并校验 sha256 后展示）；`settings_update` 展示改动差异（由 shell 从本机待发请求体生成）。审批单以外的界面用 7 位短 SHA（第二轮裁决 C27） |
| `expires_at` | 倒计时 + 本地时区绝对时间；过期后禁用确认按钮并提示重新获取 |
| `device_id` | "将使用本设备（设备名）的私钥签名"，设备名取自 `/v1/me` |
| `delivery_id` | 展示交付单元标题作**上下文**（来自 `Delivery` 资源，见下），同时显示 id |
| `approval_id`、`nonce`、`v` | 折叠的"技术细节"里可见 |

**一致性规则**：

1. **单一来源**：`desktop-shell` 取得 challenge → 解析 → 生成 canonical 字节；界面所用的展示数据由 shell 从**同一个已解析对象**生成并经 IPC 下发。`desktop-ui` 回传给 shell 的只有 `{ approval_id, confirmed: true }`，**不得回传任何待签内容**；shell 只签自己持有的那份 canonical 字节。
2. **未签名信息要标明**：PR 标题、改动行数、Issue 摘要等不在 payload 内，且来自 `UntrustedText`，必须放在与签名字段**视觉分离**的区域并标注"未签名信息，仅供参考"，不得影响"确认"按钮的文案。
3. **展示前自检**，任一失败则中止、不弹确认：`challenge.approval_id` 等于所请求的 id；`payload.device_id` 等于本设备；`payload.v` 已知；`signing` 三项均受支持；`payload.expires_at` 未过期（用 `/v1/me` 的 `server.time` 校正时钟偏差）；`payload` 的 `action` / `target` / `subject_digest` / `delivery_id` 与 `GET /v1/approvals/{id}` 返回的 `Approval` 一致，否则报告"服务端数据不一致"；内部动作另核对 `subject_digest` 等于 shell 对本机待发请求体算出的摘要。
4. **用户在场**：审批签名密钥从 M1 起就要求用户在场才能使用（Touch ID / Windows Hello，P-47）；系统弹窗的提示文案应包含由 payload 生成的 `action`、目标（如 `repo#number`）和 `subject_digest` 前 **12** 位（平台允许自定义文案时），因为它在 WebView 之外渲染。
5. **以服务端结果为准**：提交后界面只按响应里的 `Approval.status` 更新；不做"已批准"的乐观显示；`409 stale_head` 要明确告诉用户"代码在你确认期间已更新，审批已失效"。
6. **一次一审**：一次确认只对应一份 payload；不提供"批量批准"，也不缓存 challenge 跨过其 `expires_at`。

### 6.6 内部动作的签名写入（settings、改角色、吊销）

适用：config.md §2.5 判定需要签名的 settings 写入（涉及凭据或放宽安全）、`PUT /v1/devices/{id}/role`、`POST /v1/devices/{id}/revoke`。收紧类与中性 settings 写入不走本节。

| 步 | 客户端 | 服务端 |
| --- | --- | --- |
| 1 | settings：`PUT …?dry_run=true`，取得 `requires_signature` 与 `signing`；设备路由：自行组装 `signing`（`target = {kind:"device", id}`） | 校验并返回 `signing = {action, target, subject_digest}`，不落库 |
| 2 | `POST /v1/approvals`，体 = `signing`，`Idempotency-Key` 必带（operator 起） | 建 `pending` 审批（`expires_at` = 创建时刻 + `policies.approval.ttl_s`，§3.7），发 `approval.requested` → `201 Approval` |
| 3 | approver 设备：`GET challenge` → §6.5 展示与自检 → 签名 → `POST grant` | 同 §6.2–§6.4（第 5 步不适用） |
| 4 | 原样重发原请求，加头 `Gantry-Approval: apr_…`（settings 写入同时带 `If-Match`） | 见下方校验；通过则在**同一事务**里执行写入、把审批置 `consumed`（`approval.consumed`）并写业务事件 |

**`subject_digest`** = `"sha256:"` + 小写十六进制 `SHA-256(JCS(请求体))`。JCS 即 RFC 8785；请求体按 JSON 解析后规范化，所以空白、键顺序不影响摘要。无请求体的路由按 `{}` 计算（吊销的请求体就是 `{}`）。带 secret `value` 的请求体也按原样计算：摘要覆盖整个请求体，不单独对 secret 取哈希；服务端不保存请求体本身。

**第 4 步校验**（任一失败 → 不执行写入）：

| # | 校验 | 失败 |
| --- | --- | --- |
| 1 | 带 `Gantry-Approval` 头 | `403 signature_required`（附 `signing`） |
| 2 | 审批 `status = granted`、未过期 | `409 approval_mismatch`（`not_granted` / `consumed` / `expired`） |
| 3 | `action` 与路由一致，`target` 与路径参数一致（settings 的 `scope` / `project_id` / `resource`） | `409 approval_mismatch`（`action_mismatch` / `target_mismatch`） |
| 4 | `SHA-256(JCS(本次请求体)) == subject_digest` | `409 approval_mismatch`（`digest_mismatch`） |
| 5 | settings：`target.base_rev` 等于 `If-Match` 的 rev，且等于该 `(scope, resource)` 的当前 rev | `409 approval_mismatch`（`base_rev_mismatch`）；rev 已前移则 `412` 优先 |
| 6 | 发请求的设备角色满足该写入的 `required_role` | `403 forbidden_role` |

防重放：nonce 单次有效（§6.4）；审批只能消费一次；`subject_digest` 绑定请求内容，`base_rev` 绑定写入时的版本，所以同一签名既不能用于另一份内容，也不能在设置被他人改过之后再次套用；challenge 有短 TTL（120 s），审批有 `policies.approval.ttl_s`。发起审批的设备与签名的 approver 设备可以不同（operator 设备发起、approver 设备批准），但第 4 步的请求仍要满足 #6 的角色。

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
| 吊销设备 | 其事件流在下一次心跳检查时关闭（≤ 60 s），新握手被拒 |
| 带 secret `value` 的 `PUT /v1/settings/models` 不带 `Gantry-Approval` | `403 signature_required`，附 `signing` |
| 用为内容 A 批准的审批提交内容 B / 第二次使用同一审批 | `409 approval_mismatch`（`digest_mismatch` / `consumed`），未写入 |
| 改角色、吊销不带 `Gantry-Approval` | `403 signature_required` |
| `GET challenge` → 篡改 nonce → `POST grant` | `409 challenge_invalid` |
| 签名 payload 被改一个字节 | `403 signature_invalid` |
| 签名后 PR 有新推送再 `POST grant` | `409 stale_head`，审批变 `invalidated`，发 `approval.invalidated` |
| 测试向量 | 公钥验证通过；canonical 字节的 SHA-256 吻合 |
| 请求体带 `model` 或预算字段（如 `POST …/messages`） | `422 validation_failed`，`errors[].code = unknown_field` |
| 用 mTLS 客户端密钥（而非审批签名密钥）签 challenge | `403 signature_invalid` |
| `ios` 设备发消息并带 `level_cap: "Max"` | `201`，消息的 `level_cap` 为项目默认上限 |
| `ios` 设备对 `escalation` 调 `choose`；对 `budget_exhausted` 选 `raise_cap` | `403 forbidden_profile` |
| 任何设备对 `spec_needed` 调 `choose` | `422`（没有选项，P-79） |
| 把 `ios` 设备改为 `approver`（`PUT …/role`） | `409 conflict`，`reason: mobile_cannot_approve`（P-76） |
| `ios` 设备配对时不带 `approval_pubkey`；`macos` 设备不带 | `201`，`approval_pubkey_fpr: null`；`422 validation_failed` |
| `ios` 设备 `PUT /v1/devices/self/push-token` 后读 `GET /v1/devices`、事件流 | 都不含推送令牌（P-77） |
| 桌面设备发消息带 `level_cap: "Max"` 且 `refs` 指向 `level_cap` 为 High 的交付 | `201`，并产生 `delivery.level_changed`（High → Max）；预算模板不变（P-82） |
| `POST …/pause`、`…/resume`、`/v1/nodes/{id}/retry`、`…/choose` 缺 `Idempotency-Key` | `400 idempotency_key_required` |
| 服务端从备份恢复后客户端带旧游标重连 | 响应头 `X-Log-Epoch` 变化，客户端全量重同步；备份之后吊销的设备握手仍被拒 |
| 证书剩余不足 1/3 时 `POST /v1/devices/self/renew` | `200`，新证书链的根指纹与配对时一致 |

---

## 8. 不变式落点

### 8.1 I6：客户端不执行、不持凭据

| I6 的要求 | 本 API 的体现 |
| --- | --- |
| 客户端不执行 | 全部写路由只有六类：发消息（数据）、改配置（settings）、审批签名 / 拒绝、状态流转（调档 / 暂停 / 继续 / 放弃 / 重试失败节点 / dismiss / choose）、工作树人工锁、设备管理（含证书续期与推送令牌注册）。**没有任何路由接收代码、命令、补丁、脚本，也没有"让服务端替客户端运行某物"的路由**；执行只发生在服务端 → 引擎 → 沙盒，GitHub 写只经 Outbox（I1、I2） |
| 不持 GitHub 凭据 | 没有任何响应包含 installation token、App 私钥、webhook secret；客户端不调用 GitHub API，PR / Issue 信息来自服务端快照，跳转链接由 `repo` + `number` 拼出并在系统浏览器打开 |
| 不持模型凭据 | 模型凭据只作为 `SecretRef.value` 经 `PUT /v1/settings/{resource}`（带审批签名，§6.6）写入且**只写不读**，服务端经 Egress 控制面转交 Egress 保存（P-16、P-50），设置文档里只有 `secret_ref`（§4.9）；客户端提交后不得持久化、不得写日志；任何读路由、事件、日志都不含值 |
| 客户端持有的只有 | 两把不可导出的设备密钥（mTLS 客户端密钥，只用于连接；审批签名密钥，只用于审批签名且每次需用户在场，P-47）、离线根 CA 指纹、设备证书。签名内容里没有任何凭据 |
| 同时的相关不变式 | I3：审批只由 `policy` 放行，API 只转交签名并重建 payload 验签（§6.4）；I4：快照带 `synced_at`，界面可标注新鲜度，事实以 GitHub 为准；I5：§1.5 渲染规则 |

### 8.2 I7：配置只在 settings 路由

| 做法 | 说明 |
| --- | --- |
| 入口唯一 | 模型、档位预算、策略等配置的读写只有 §4.9 的 `/v1/settings/{resource}` 与 `/v1/projects/{project_id}/settings/{resource}`（另有只读的 `catalog`、`status`），均 operator 起（放宽类 / 凭据写入 approver + 签名），手机档案不可用 |
| 工作路由拒绝配置字段 | 所有非 settings 路由的请求体 `additionalProperties: false`（§1.4）：消息里带 `model`、预算、策略字段 → `422`，而不是静默忽略 |
| 工作路由只出运行状态 | `Delivery.level_cap` / `level_effective`、`BudgetLedger.used`、节点状态都是运行态；`BudgetLedger.limits` 只读展示，改预算只能经 settings |
| 任务级档位不是配置 | `Message.level_cap`、`PATCH /deliveries` 的 `level_cap` 是**单个任务的上限**，由 architecture §7.2 / §8 明确允许在工作界面设置；与"档位模板的预算数值"（配置）区分 |
| 项目字段 | `Project.settings_override` 不在 Project 表示里返回，读写走 `/v1/projects/{project_id}/settings/{resource}`；默认档位是配置 `levels.default_level`，不在 Project 上 |
| 凭据 | 只写不读（§4.9），配置页以外无处暴露，设置文档里也只有 `secret_ref` / `present` |

---

## 9. 客户端接入顺序

| 步骤 | 做什么 |
| --- | --- |
| 1 | 加载设备证书与 mTLS 客户端密钥 → `GET /v1/me`：读 `role`、`profile`、`server.time`（偏差 > 60 s 警告）、`server.log_epoch`；握手失败按 §2.1 处理 |
| 2 | 按档案裁剪界面：`mobile` 只显示对话与收件箱；按 `role` 隐藏 / 禁用入口（服务端仍会 `403`，客户端裁剪只为体验） |
| 3 | 并发拉快照（项目、当前项目的对话与最新消息页、进行中交付单元、收件箱），记 `X-Event-Seq` 取最小值 |
| 4 | 订阅 `/v1/events`（一条流）；事件按 `seq` 幂等应用；游标先应用后保存 |
| 5 | 写操作：为每次用户意图生成 `Idempotency-Key`；网络失败 / 5xx 沿用同一 key 重试；settings 带 `If-Match` |
| 6 | 错误统一按 `code` 分支（§3.1 表），`detail` 不展示 |
| 7 | 审批：事件 / 收件箱 → 打开审批界面 → `GET challenge` → 按 §6.5 展示 → 用户在场确认（审批签名密钥）→ 签名 → `POST grant`；内部动作按 §6.6 先发起审批、批准后带 `Gantry-Approval` 重发 |
| 8 | 维护：保存 `X-Log-Epoch`，变化即全量重同步（§5.3）；按 `/v1/me` 的 `certificate.renew_after` 自动续期证书（§4.10） |

---

## 10. 开放问题

骨架没定的、需要用户或对应模块规格拍板的。**"本文的临时处理"只是让文档能自洽，不构成决定。**

| # | 问题 | 本文的临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | ~~角色边界与新设备的初始角色~~ | 已决：见 decisions.md P-23（§1.3、§2.2） | — |
| OQ-2 | ~~手机端 APNs 推送令牌的注册路由~~ | 已决：见 decisions.md P-77（`PUT /v1/devices/self/push-token`，§4.10） | — |
| OQ-3 | settings 与 config.md 的衔接：(a) `default_level` 已决：见 decisions.md P-22；`status` 的版本字段统一为 `applied_rev`（第二轮裁决 C26）；仍待定：(b) `catalog`、`status` 的响应结构是否够用（config.md §2.8 已给出）；(c) resource 从任务清单点名的三个扩到八个，是否接受；(d) 没有专门的回滚 / 历史列表路由（读 `?rev=N` 再 `PUT`），设置页的"历史"如何列出版本 | 照 config.md 当前文本实现；(d) 暂以事件流中的 `settings.updated` 作历史来源 | config.md、desktop-ui 设置页 |
| OQ-4 | ~~事件目录缺口~~ | 已决：见 decisions.md P-21 | — |
| OQ-5 | ~~改设备角色、吊销是否带设备签名~~ | 已决：见 decisions.md P-18、P-23（§6.6） | — |
| OQ-6 | 审批（`consumed`、无 head 的预留动作、`subject_digest` 已决：见 decisions.md P-20、P-27；签名密钥独立于 TLS 密钥已决：见 decisions.md P-47；动作集合、形状与消费时点已决：见 decisions.md P-54；审批有效期默认 24 小时已决：见第二轮裁决 C27）。仍待定：challenge / nonce 的存放位置与是否进事件日志；设备密钥算法（本文按 ES256）；`deny` 是否也要签名 | §6 按 ES256；`deny` 不签名；challenge 存放不规定 | policy、pki |
| OQ-7 | ~~Conversation 基数~~ | 已决：见 decisions.md P-22（每个项目一个主对话） | — |
| OQ-8 | 项目绑定 / 解绑（`project.added`）由什么触发：GitHub App 安装回调还是 API（控制类直达路由已决：见 decisions.md P-37、P-56，调档、暂停、继续、重试、放弃都有直达路由） | 无绑定路由 | forge、deploy、conversation |
| OQ-9 | ~~InboxItem 的选项回传、计数~~ | 已决：选项经 `choose` 回传，见 decisions.md P-40；`project_id`、`dedup_key`、`options[]` 形状与 `GET /v1/inbox/summary` 见 decisions.md P-63。是否需要显式 resolve 路由仍待定（目前只有 dismiss 与 choose） | notify、desktop-ui 收件箱 |
| OQ-10 | 线上形态（`Level`、`UntrustedText`、`Ref`、`Card`、`CheckSummary`、`Attempt.status` / `usage`、`Device.platform`、`BudgetLedger` 单位已决：见 decisions.md P-22；`Workflow.status` 已决：见 data-model，示例用 `succeeded`，第二轮裁决 C26；`stage_status` 已决：见 decisions.md P-55）。仍待定：`cert_serial` / `pubkey_fpr` / `approval_pubkey_fpr` 格式 | 本文示例的形态 | core、schemas/ |
| OQ-11 | `Attempt.result_ref`（内容寻址对象）与 `decision_needed.payload.patch.ref`（workflow 补丁，P-48）的读取路由与格式，依赖 worker-protocol.md 的结果结构（已转为 decisions.md Q-12，推荐 M2 新增 `GET /v1/results/{sha256}`） | 不提供；检视器暂只能显示尝试元数据；手动应用的补丁暂无下载入口 | desktop-ui 检视器 |
| OQ-12 | 配对与证书（证书续期路由已决：见 decisions.md P-64，`POST /v1/devices/self/renew`；固定根指纹已决：同上）。仍待定：配对监听器的端口与是否按需开启；`confirmation_code` 核对失败的处置（设备核对前是否已 `active`、有无 abort 路由）；证书已过期后的恢复（只能重新配对？） | 配对 1 条路由 | pki、deploy、desktop-shell |
| OQ-13 | 事件保留期与 `410 cursor_expired`（全局事件 `project_id` 与吊销断流已决：见 decisions.md P-21、P-30；备份恢复后的客户端重同步已决：见 decisions.md P-67，`log_epoch`） | 保留不限 | store、api |
| OQ-14 | §3.7 全部数值默认值（心跳、限流、TTL、体积上限、幂等保留） | 照表 | api 实现与限流配置 |
| OQ-15 | Q-01（个人 / 团队）：骨架没有"用户"实体，主体就是 Device；若将来做团队，`actor`、角色、审计都要重做 | 主体 = Device | 全局 |
| OQ-16 | ~~settings 写入的设备签名放在哪、签什么、如何防重放~~ | 已决：见 decisions.md P-18（§6.6） | — |
| OQ-17 | ~~唯一的 approver 设备丢失时无人能签~~ | 已决：服务端主机上的应急命令 `gantry device recover`（需主机 root，写审计事件），见 decisions.md P-39 | pki、deploy |
