# 第二轮裁决：模块规格书暴露的跨文档不一致（2026-10-24）

> 20 份模块规格书写作时，各 agent 报告了骨架、契约与相邻规格之间的不一致。以下是产品负责人代理的裁决，记入 decisions.md P-47 起（编号见各条）。所有文档按本文对齐；标识符以本文为准。

| # | 裁决 | 决策号 |
| --- | --- | --- |
| C1 | **双密钥**：桌面端有两把密钥。mTLS 客户端密钥用于无提示的后台连接；**审批签名密钥**单独生成，必须用户在场（Touch ID / Windows Hello，Secure Enclave / TPM，不可导出）。配对请求 `POST /v1/pair` 同时提交 CSR 与 `approval_pubkey`；所有审批签名都用审批签名密钥，服务端用登记的 `approval_pubkey` 验签。 | P-47 |
| C2 | **Gantry 不修改 `.github/workflows/`**：App 不申请 `workflows` 权限；补丁触及 workflow 文件时 `policy` 拒绝（`failure_detail = policy.path_protected:workflows`），上报 `decision_needed`（`payload.kind = manual_action`），附补丁供用户手动应用。依赖清单与 lockfile 仍按 P-11/P-19：M1 拒绝，M2 起需审批。M2 退出标准中"workflow 补丁审批后推送"改为"workflow 补丁被拒绝并附补丁上报"。 | P-48 |
| C3 | **沙盒身份用每个尝试一枚的能力令牌**：服务端为每个 Attempt 签发 `job_token`，放在模型 API base URL 路径中（`http://<egress>/j/<job_token>/<provider>/…`），只对该尝试有效、尝试结束即作废、只能从沙盒网段使用；Egress 校验令牌并按尝试计量。沙盒即使读到令牌也只能给自己的尝试记账。取代 P-16 中"沙盒网络身份"的说法。 | P-49 |
| C4 | **Egress 控制面契约**：新增 `docs/interfaces/egress-control.md`。`gantryd` 经主机本地 Unix socket `/run/gantry/egress.sock` 调用 Egress 控制面：签发 / 吊销 `job_token`、写入 secret（只写不读）、查询 secret 是否存在（`present`）、拉取用量。服务端 `models` 的模型调用走 Egress 的回环 HTTP 反向代理，使用服务令牌 `svc_token`（与 `job_token` 同一计量口径，计入会话台账或交付台账）。统一名称：控制面 `egress.sock`；不再使用 `models_uds`。 | P-50 |
| C5 | **共享端口与替身**：所有跨模块端口 trait 定义在 `core::ports`（M0 交付）：`ForgeReader`、`OutboxPort`、`SandboxService`、`ModelProvider`、`RepoService`、`PolicyPort`、`InboxPort`、`EgressControl`。M0 替身补齐：`store` 的内存实现 `Store::open_in_memory`、`FakePolicy`、`FakeInbox`、HTTP 级 `FakeGitHub`（供 `forge` 测试）、`EgressStub`。 | P-51 |
| C6 | **沙盒端口分两层**：`SandboxService`（引擎面：提交 JobSpec、等待结果、取消）与 `SandboxDriver`（容器面：rootless Podman + gVisor 的具体操作）。engine 只依赖 `SandboxService`。 | P-52 |
| C7 | **Outbox 失败语义**：临时错误留在 `queued`，以 `not_before` 退避重试，不进入 `failed`；重试用尽后进入终态 `failed`，只在终态发 `outbox.failed`；人工重试创建新动作（新 `epoch`）。data-model §4.3 去掉 `failed → queued`。 | P-53 |
| C8 | **审批动作标识与形状**：`Approval.action` ∈ `ForgeActionKind` ∪ {`settings_update`、`device_role_change`、`device_revoke`、`force_push`、`push_outside_prefix`、`resolve_review_thread_human`、`patch_apply_protected`}。每个动作的 `target` 与 `subject_digest` 形状在 data-model 中列表定义；未定义形状的动作一律 fail-closed。**审批在执行开始前消费**（`consumed`，记录 Outbox 动作 ID）；执行终态失败不退回，需要新审批。 | P-54 |
| C9 | **阶段机补全**：任何非终态阶段 → `abandoned`（用户放弃、PR 关闭未合并、Issue `not_planned`）；`REJECT` 后选择"重新规划" → `plan`；`merge` 阶段 PR 被关闭 → `abandoned`（重新打开则回到 `review`）。`stage_status`：`running` / `waiting_human` / `blocked` / `paused` / `done`，转移规则写进 data-model。**准入只在 High 及以上**：Medium 为 `spec → plan`（免准入），Low 为 `spec → implement`（免准入、无计划阶段）。**计划是独立阶段**：计划阶段图 = `plan`（Max 起加 `reasoner`）；实现阶段图 = `build → verify`（Low 为 `single`）。architecture §8 表格按"计划阶段 / 实现阶段 / 评审阶段"三列重写；Low 的阶段列表包含 `review`（只等 CI）。 | P-55 |
| C10 | **控制路由与事件**：新增 `POST /v1/deliveries/{delivery_id}/pause`、`POST /v1/deliveries/{delivery_id}/resume`、`POST /v1/nodes/{node_id}/retry`（operator，幂等键必带）；事件 `delivery.paused`、`delivery.resumed`、`node.retry_requested`。 | P-56 |
| C11 | **档位实际值**：`level_effective` 初值由引擎在规格阶段按风险评估给出（≤ `level_cap`），之后只能在上限内升，不自动降；已触发的闸门永不撤销。预算模板在交付单元创建时按 `level_cap` 固定。api 示例改为与此一致。 | P-57 |
| C12 | **None 档记账**：None 档问答不建交付单元。需要读代码时创建一个挂在会话上的临时图：`Workflow.delivery_id` 可空，此时必须有 `conversation_id`；用量记入每个项目的**会话台账**（`BudgetLedger.subject = conversation`），每日上限是配置项 `levels.None.daily_tokens`。 | P-58 |
| C13 | **模块依赖修正**：`engine` 依赖 `forge`（只用 `ForgeReader` 读取 Issue / PR 正文与评论，按需拉取、作为不可信文本，不进快照）与 `notify`；`conversation` 依赖 `notify`；`forge` 依赖 `policy`（执行时验审批）与 `notify`；`guardian` 依赖 `core`、`store`、`notify`；`notify` 依赖 `models`（可选，打杂模型生成通知文案）与 `secrets`（M4 APNs 密钥）；`forge`、`egress` 的依赖列加 `secrets`。architecture §4 表与依赖图同步。 | P-59 |
| C14 | **守护事件与范围**：新增事件 `guardian.triaged`（`event_ref`、`class`、`action`）、`guardian.heal_requested`、`guardian.escalated`；会话服务把它们渲染为事件行。分流规则以 `modules/guardian.md` 的决策表为唯一来源（含"无需动作"的显式行），architecture §7.3 与 config `guardian` 小节与之对齐；规则表外的事件一律上报。`reply_review_thread` 提前到 M2；M2 的 `be-engine` 泳道交付 `request_heal` 与 `classify` 端口。守护静默自愈不改变交付阶段；需要改代码且超出自愈范围时才走 `review → implement`。 | P-60 |
| C15 | **状态清单评论幂等**：`upsert_status_comment` 的幂等键含单调递增的 `revision`；`revision` 与 `head_epoch` 计数保存在本地事件日志中（随日志备份），不依赖可重建的 PR 快照。 | P-61 |
| C16 | **forge 不读策略**：是否需要审批由入队方在入队时调用 `policy` 决定，结果写入 `OutboxAction.requires_approval`；`forge` 只在执行时调用 `policy` 验证审批。修正 config §1.3 与 forge-actions §11A。 | P-62 |
| C17 | **上报项**：`InboxItem` 增加 `project_id` 与 `dedup_key`；`escalation`、`spec_needed`、`decision_needed` 的 `payload.options[]` 为 `{id, label, primary?}`；升档一律用 `escalation`（手机档案不可 `choose`，P-35）。新增 `GET /v1/inbox/summary`（按项目的 open 计数，手机可用）。桌面系统通知由 `inbox.raised`（`severity ≥ warn`）触发，不新增事件。`notify` 归 `be-engine` 泳道（M1 上报与计数），APNs 归 M4 `ops`。 | P-63 |
| C18 | **证书链与续期**：配对时固定的是**离线根 CA** 指纹；服务端下发链 `[叶子, 中间 CA, 根]`，客户端校验根指纹与固定值一致；中间 CA 轮换不影响已配对设备。新增 `POST /v1/devices/self/renew`（mTLS，提交新 CSR），M1 交付；客户端在证书剩余 1/3 有效期时自动续期（建议值）。 | P-64 |
| C19 | **webhook 入口与 App 创建**：webhook 监听只在回环地址上以明文 HTTP 提供；对公网暴露一律经终止 TLS 的反向代理或隧道（公网证书）。首次部署顺序：先配置 webhook 入口，再用 App Manifest 创建 App；`gantry github setup` 生成一次性的 Manifest 提交页并经同一入口的 `/setup/github/callback` 接收 `code`（细节需核实）。architecture §7.1 与 deploy 按此修正。 | P-65 |
| C20 | **安装引导期**：第一台设备配对之前，主机命令（需 root）可以写入 `github.*` 与 `sandbox.base_image` 等"仅 approver"配置，写审计事件；之后改为需要审批签名。`sandbox.base_image` 默认使用随版本发布、固定摘要的官方 worker 镜像。 | P-66 |
| C21 | **备份恢复**：事件日志有 `log_epoch`，每次从备份恢复加一；事件流与快照响应携带 `log_epoch`，客户端发现变化就全量重同步。设备吊销同时追加写入日志之外的 `revocations.journal`（只追加），恢复时重放晚于备份的吊销。 | P-67 |
| C22 | **二进制文件**：第一版补丁中的二进制文件一律拒绝（与 worker-protocol 不生成 `--binary` 一致），撤销 P-41 中的 `binary_allowlist`；符号链接一律拒绝。 | P-68 |
| C23 | **补丁校验分工**：`repo` 做机械检查并产出 `PatchFacts`（能否干净应用、文件列表、大小、二进制、符号链接、子模块、权限位）；`policy` 基于 `PatchFacts` 和配置做规则判定（路径规则、大小上限）。应用失败等机械拒绝由 `repo` 发 `patch.rejected`（`reason`）；`policy.denied` 只用于策略判定。提交由 `repo` 创建；提交身份来自配置 `github.commit_identity`（App 创建时写入）。 | P-69 |
| C24 | **token 加权**：保留 P-44；config 增加 `levels.token_weights`（`input`、`output`、`cache_read`、`cache_write`，默认值为建议值，标"待按价格调优"）；config 原 OQ-7 关闭。 | P-70 |
| C25 | **reasoner 无判定**：worker-protocol 中 `reasoner` 的结构化输出去掉可选 `verdict`（P-45）。 | — |
| C26 | **契约小修**：api §2.3 手机路由数改为实际数；§3.3 必带幂等键清单加 `choose` 与新增控制路由；§6.3 规范化规则允许数组；设置状态字段统一为 `applied_rev`；`Workflow.status` 示例改为 `succeeded`；适配器标识统一为 `claude-code`；`ForgeEvent.occurred_at` 带毫秒；`Card.summary` 改为 `items[]`（每项含 `text` 与 `ref`）；对话事件行消息增加 `tone`（`info` / `ok` / `run` / `warn`）；手机档案发消息时服务端忽略 `level_cap`，使用项目默认上限（P-35）；`expected_effects.matched_delivery_ids` 改名为 `matched_webhook_deliveries`。 | — |
| C27 | **显示约定**：短 SHA 统一 7 位；生物识别提示里用 12 位；审批单显示完整 40 位。审批有效期（`pending → expired`）为 `policies.approval.ttl_s`，默认 24 小时（建议值）；challenge 有效期 120 秒。设置保存只在凭据或放宽类改动时需要签名（prototype.md 与原型文案同步）。 | — |
| C28 | **里程碑调整**：M1 `fe-desktop` 包含档位上限选择器、配对向导、"设备与证书"页（列表、改名、吊销）；"外观"页放 M3。私有包仓库反向代理归 M2 `exec`。M1 `ops` 包含 webhook 反向代理 / 隧道配置与 `gantry github setup`。 | — |
| C29 | **预算默认值**：调整 config 中各档 `nodes` 建议值，使其不先于 `rounds` 耗尽（例如 Medium ≥ 20、High ≥ 40，仍标"建议值，待调优"）。 | — |
