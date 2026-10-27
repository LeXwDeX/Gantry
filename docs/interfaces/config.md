# 配置（设置）schema 与语义

> 状态：设计 v0.1 · 契约文档 · 机器可读版本放在 `schemas/config/`（M0 由 `contract` 泳道据本文生成；两者冲突时以本文为准，直到回写）。
> 与 [api.md §4.9](api.md#49-settings) 配套：路由、ETag、错误体归 api.md；resource 划分、键、校验、角色、事件、热加载归本文。本文对 api.md 的修改诉求集中在 §2.7。动作 id、`guardian.patrol_interval_s`、`github.*` 等被 [forge-actions.md §11](forge-actions.md) 引用；沙盒与补丁限额来自 [worker-protocol.md §8.6](worker-protocol.md) 的"须回写 config.md"清单。
> 读者：实现 `api`（settings 路由）、`models`、`engine`、`policy`、`guardian`、`sandbox`、`egress`、`notify`、`forge`、`desktop-ui`（设置页）的 agent。

本文定义服务端**运行期设置**（settings）：有哪些键、取值与默认、全局与项目如何叠加、怎样存储与追溯、谁能改、改了何时生效。它是 D-08 / D-09 / I7 的落点：模型、档位预算、策略只在设置里改，工作界面只显示运行状态。

## 0. 约定

| 标记 | 含义 |
| --- | --- |
| （无标记） | 骨架文档直接规定或可直接推出；括号注明出处（如 §8、I3、P-11） |
| **[提议]** | 本文为了能实现而提出的默认，骨架没定，可被推翻 |
| **[建议值，待调优]** | 数值默认，只是起点，未经真实任务验证，不得当作定论 |
| **OQ-n** | 骨架没定的问题，列在 §14；实现者不得自行拍板 |

- **收紧 / 放宽**：收紧 = 有效配置更保守（更少权限、更低上限、更严处置）；放宽 = 相反。每个"仅收紧"键在表中给出放宽方向。
- 键路径用点号书写（`models.worker.model`）；校验错误里用 JSON Pointer（`/models/worker/model`）。
- 表中枚举值用 ` / ` 分隔。

## 1. 范围与总览

### 1.1 不属于本文的内容

| 内容 | 归属 | 说明 |
| --- | --- | --- |
| 引导配置：API 监听地址与端口、数据目录、`secrets/` 路径、webhook 监听与公网入口 / 隧道形态 | `deploy` | 文件配置，重启生效，不进事件日志。引导配置的键可能与 settings resource 同名前缀（如 `sandbox.workspaces_dir`、`egress.sandbox_cidrs`），但它们写在引导配置文件里，**不是** settings 键；写进 settings 文档按未知键拒绝（V01） |
| `forge.*` 内部参数（webhook 载荷上限、对账批大小、Outbox 退避、推送传输、限流余量等） | `forge` / `deploy` | forge-actions §11 B / C：引导配置或内部常量，不是设置。例外见 §6.2、§10 |
| 凭据的值 | 密钥存储（`secrets/`，加密） | 本文只出现引用 `secret_ref`，见 §2.4 |
| 任务级参数：消息上的档位上限、`@` 引用 | `conversation` / `Message.level_cap` | 属于工作输入，不是配置（I7） |
| Project 实体字段：`repo`、`installation_id`、`bot_user_id`、`default_branch`、`allowed_merge_methods`、`merge_queue_enabled` | [data-model §3](../data-model.md#project) | 本文只定义 `Project.settings_override` 的内容。新任务的默认档位是 `levels.default_level`（项目可覆盖，decisions P-22），不是 Project 字段 |
| 设备、证书、吊销表 | `pki` | 设置页"设备与证书"操作的是这些实体，不是 settings 文档，见 §13 |
| 本机外观与本机密钥 | `desktop-shell` 本地存储 | 见 §13 |

### 1.2 顶层键（= settings resource）

| 键 | 内容 | 项目覆盖 | 主要消费者 |
| --- | --- | --- | --- |
| `models` | 提供方、三类模型（打杂 / 大脑 / 干活） | 部分，自由（§3） | `models`、`engine`、`conversation`、`worker`、`egress` |
| `levels` | 默认档位、档位 → 预算模板 + 图模板名、token 权重、None 档每日上限 | 部分：默认档位自由，预算仅收紧 | `engine`、`models`、`forge`（写速率）、`egress`（权重） |
| `policies` | 危险动作、路径规则、自动加码、审批、补丁校验 | 仅收紧 | `policy`、`engine`、`guardian` |
| `guardian` | 分流覆盖、无进展阈值、状态清单、对账巡检周期 | 部分，仅收紧 | `guardian`、`forge` |
| `sandbox` | 驱动、默认资源限制、超时、worker 协议 I/O 上限、预热、devcontainer | 仅 `limits` / `devcontainer`，仅收紧 | `sandbox`、`engine` |
| `egress` | 白名单分组、档案、私有包仓库的反向代理注入 | 仅收窄档案的分组 | `egress`、`sandbox` |
| `notify` | 通知渠道与路由 | 否 | `notify` |
| `github` | GitHub App 标识与凭据引用、提交身份 | 否 | `forge`、`repo`（`commit_identity`） |

### 1.3 各模块读哪里

| 模块 | 读取 | 责任 |
| --- | --- | --- |
| `core` | — | `Settings` / `EffectiveSettings` 类型、merge、"收紧比较"、JSON Schema 校验；纯函数、无 IO [提议] |
| `store` | — | 追加 `settings.updated` 事件；投影出当前文档与按 rev 的历史 |
| `api` | 全部 | settings 路由（api.md §4.9）：合并、校验、角色检查、`dry_run`、secret `value` 的剥离与转存、事件写入 |
| `models` | `models.*`、`levels.token_weights`、`levels.none.daily_tokens` | 解析三类模型、重试与降级、经 Egress 回环反向代理调用（带 `svc_token`，P-50）、用量记账（交付台账或会话台账，P-58） |
| `engine` | `levels.*`、`policies.escalation`、`models.worker`、`sandbox.limits` | 创建交付单元时按 `level_cap` 固化预算与图模板（P-57）；自动加码；Attempt 前把 worker 配置解析进 JobSpec（`execution.*`、`constraints.limits`，见 worker-protocol §4）；入队 Outbox 前调用 `policy` 决定 `requires_approval`（P-62） |
| `policy` | `policies.*` | 判定（被入队方在入队时调用；被 `forge` 在执行时调用以验证并消费审批；被 `repo` 调用以判定 `PatchFacts`）；每个判定记录 `config_rev` |
| `repo` | `github.commit_identity`；补丁限额经 `policy` 读取 | 以该身份创建提交（P-69） |
| `guardian` | `guardian.*`；路径规则经 `policy` 求值 | 分流；入队前调用 `policy` 决定 `requires_approval`（P-62） |
| `sandbox` | `sandbox.*`、`egress`（档案名） | 建沙盒时固化限制；执行 `sandbox.io.*` 上限 |
| `egress` | `egress.*`、`models.providers`、`levels.token_weights`、被引用的 secret | 独立进程，只读 `gantryd` 经控制面下发的这一份视图（[egress-control.md](egress-control.md)），不读其他键 |
| `notify` | `notify.*` | 路由推送；可选用 `models.utility` 生成通知文案；M4 APNs 密钥经 `secrets`（P-59） |
| `forge` | `github.app.*`、`guardian.patrol_interval_s`、`levels.*.budget.forge_writes_per_hour` | App 鉴权与 webhook 验签、对账巡检周期、Outbox 速率闸。**不读 `policies.*`**：是否需要审批由入队方决定并写入 `requires_approval`，`forge` 只在执行时经 `policy` 验证并消费（P-62）；`guardian.status_checklist` 由入队方读取 |
| `desktop-ui` | `catalog`、JSON Schema | 设置页表单由 schema 与 catalog 驱动（§13） |

## 2. 分层、存储、权限与热加载

**resource**：每个顶层键就是一个 settings resource——`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`、`notify`、`github`——即 [api.md §4.9](api.md#49-settings) 的 `{resource}`。api.md 暂定只有前三个，本文把集合扩为八个（须同步 api.md，OQ-25）。读写粒度是**整个 resource**：`PUT` 整体替换（`{"value": …}`），`If-Match` 按 `(scope, resource)` 的 ETag；本文的 JSON 示例是把八个 resource 拼在一起的**聚合视图**，实际按 resource 分别读写。

### 2.1 优先级与合并

优先级（高 → 低）：**项目覆盖 > 全局设置 > 内置默认**（内置默认随二进制）。逐 resource 计算：

```
effective(R, project) = clamp( merge(builtin(R), global(R), override(R, project)),  against = effective(R, global) )
```

| 规则 | 说明 |
| --- | --- |
| merge 语义 | RFC 7386 JSON Merge Patch 的合并规则：对象递归合并，数组**整体替换** |
| 规则集合用"以 id 为键的对象" | 路径规则、加码规则、egress 分组 / 档案 / 注入、providers、通知路由都不用数组，这样项目可按 id 增补或加强，而不必重述全局内容 |
| 存储的文档是**稀疏**的 | 全局与项目覆盖都只含显式设置过的键，其余取下一层 [提议，OQ-20]；存储文档里不出现 `null` |
| 如何清除 | 全局：`PUT {"value": {}}` 回到内置默认；项目：`DELETE`（api.md §4.9）回退到全局 |
| 可覆盖的 resource | `models`、`levels`、`policies`、`guardian`、`sandbox`、`egress` 可；`notify`、`github` 不可（api.md `422 not_overridable`） |
| 可覆盖的键 | resource 内还要看 §2.2 逐键矩阵，越界 → `422 validation_failed`（`override_not_allowed`） |
| clamp | 对"仅收紧"键，项目值比全局有效值更宽松时取全局值。写入时直接拒绝（`override_loosens_global`）；clamp 只会在**全局后来被收紧**、使既有项目覆盖变宽松时出现，`view=effective` 的响应里列出 `clamped[]` |
| 跨 resource 校验 | 以其他 resource 的**当前已存值**为准（如 `models` 的 provider 主机必须已在 `egress.groups.model_api` 里）。需要同时改两个 resource 时，按"被引用方先改"的顺序分步提交 |
| 任务级参数 | 不在此链上（§1.1） |

### 2.2 项目覆盖矩阵

| 键子树 | 项目覆盖 | 放宽方向（用于"仅收紧"比较与角色判定） |
| --- | --- | --- |
| `models.providers`、`models.retry` | 否 | —（项目可改选已存在的 provider id；需要独立计费就在全局建一个独立 key 的 provider） |
| `models.utility / planner / worker` 的所有字段 | **自由** | 无方向；§11 校验仍适用 |
| `levels.default_level` | **自由**（decisions P-22） | 无方向（只是任务的默认上限，单个任务仍可在消息里指定） |
| `levels.*.graph_template` | 否 [提议，OQ-11] | 方向无法比较，视同敏感键 |
| `levels.*.budget.*`、`levels.none.daily_tokens` | 仅收紧 | 数值变大 = 放宽 |
| `levels.token_weights` | 否（仅全局） | 任一权重变小 = 放宽（同样的用量计得更少） |
| `policies.actions.*`、`policies.unlisted_action` | 仅收紧 | 严格度 `auto` < `approve` < `deny`，变小 = 放宽 |
| `policies.allow_globs` | 否 | 范围变宽 = 放宽（glob 集合无法机械比较大小，故仅全局） |
| `policies.paths.<id>` | 仅收紧，可新增 id | `globs` 变少、处置变弱、删除某 id = 放宽 |
| `policies.escalation` | 仅收紧，可新增规则 id | 关闭、删除规则、缩小 `when`、降低 `raise_to` = 放宽 |
| `policies.approval.ttl_s`、`policies.patch.*` | 仅收紧 | 数值变大 = 放宽 |
| `guardian.triage.*` | 仅收紧 | `escalate` → 任何静默值 = 放宽 |
| `guardian.no_progress.*` | 仅收紧 | 数值变大 = 放宽 |
| `guardian.patrol_interval_s` | 否 | 数值变大 = 放宽（仅全局） |
| `guardian.status_checklist.*` | **自由** | 无方向（关闭只影响 PR 上的可见性，审计不受影响） |
| `sandbox.limits.*` | 仅收紧 | 数值变大 = 放宽 |
| `sandbox.devcontainer.mode` | 仅收紧 | `safe_subset` → 更宽的模式 = 放宽 |
| `sandbox.driver / runtime / base_image / prewarm / max_concurrent_sandboxes / idle_timeout_s / kill_grace_s / io.*` | 否 | `runsc` → `runc`、各数值变大 = 放宽 |
| `egress.groups`、`egress.injection`、`egress.default_profile` | 否 | 加主机 = 放宽 |
| `egress.profiles.<name>.groups` | **仅收窄**（项目集合必须是全局集合的子集） | 加分组 = 放宽（OQ-11） |
| `notify.*`、`github.*` | 否（整个 resource 不可覆盖） | — |

### 2.3 存储：事件、版本、追溯

设置的唯一写路径是事件日志（I7；`Project.settings_override` 是投影，不得直接写）。事件类型（data-model §5.2）：

| 事件类型 | 触发 | `subject` | `data` |
| --- | --- | --- | --- |
| `settings.updated` | 某 `(scope, resource)` 写入成功（全局 `PUT`、项目 `PUT` / `DELETE`） | 全局：字面量 `settings`（`project_id` 为 `null`）；项目：`prj_…`（同时填 `project_id`） | `scope`（`global` / `project`）、`resource`、`schema_version`（服务端盖章，当前 `schemas/config/` 的版本）、`base_rev`、`value`（写入后的稀疏文档；项目 `DELETE` 时为 `null`；**已剥离 secret 的 `value`**）、`relaxes[]`（本次放宽的键路径，由校验器算出）、`secret_changes[]`（`{ref, op: set / rotate / delete, version}`，**无值、无哈希**） |

- `actor` = `{kind: "device", id: "dev_…"}`；安装引导期的主机命令写入为 `{kind: "system", id: "host_bootstrap"}`（§2.5，P-66）。
- **项目覆盖变更改发 `settings.updated`，不再用 `project.updated`**：后者在手机端可见的 `project.*` 事件族内，会把覆盖内容带给只发消息的客户端（api.md §2.3）。`settings.*` 只下发给桌面档案且角色 ≥ operator 的事件流 [提议]。
- **rev / ETag**：`(scope, resource)` 的 rev = 其最近一次 `settings.updated` 的 `seq`，从未写过为 `0`；ETag 即 `"<rev>"`（对客户端不透明）。写响应的 `X-Event-Seq` 等于新 rev。
- **`config_rev(project)`**：该项目生效配置的版本 = 所有适用于它的 `settings.updated`（全局任意 resource + 该项目任意 resource）的最大 `seq`。消费者在做决定时记下它；按该值回放事件即可复原当时的有效配置。
- **可追溯**：`GET …?rev=N` 返回该 `(scope, resource)` 在 rev N 时的文档（诉求 A3）；`store` 按 rev 保留文档，事件永久保留。
- **回滚**：没有专门路由。客户端读出旧 rev 的文档再 `PUT` 回去，它是一次普通写入，同样要过全部校验与角色检查。
- **历史列表**：来源是事件流里的 `settings.updated`（带游标回放，operator 起可见）；是否需要独立的版本列表路由见 OQ-25。
- **多客户端一致**：别的客户端靠 `settings.updated` 事件或 `412` 发现变化（补上 api.md OQ-4 的缺口）。
- 备份：随事件日志备份；`secrets/` 单独加密备份（[architecture §6](../architecture.md#6-状态归属)）。

### 2.4 凭据：`secret_ref`

设置文档里只有**引用**，值不进文档、不进事件日志、不进任何读响应、不下发到客户端（I6）。凭据字段统一为 `SecretRef` 对象（schema 上用 `x-gantry-secret: true` 注解，api.md §4.9 的建议）：

| 字段 | `GET` | `PUT` |
| --- | --- | --- |
| `secret_ref` | 返回 | 必填，匹配 `^[a-z0-9][a-z0-9._-]{0,63}$` |
| `present` | 返回：该 ref 是否已有值 | 接受并**忽略**（便于客户端 GET 后原样改了再 PUT） |
| `value` | **永不返回** | 省略 = 保持不变；字符串 = 写入或轮换（存入 `secrets/` 的该 ref）；`null` 不接受（见下） |

| 项 | 规定 |
| --- | --- |
| 出现位置 | `models.providers.<id>.credential`、`egress.injection.<id>.credential`、`github.app.private_key`、`github.app.webhook_secret`、`github.app.webhook_secret_previous` |
| 值的位置 | 模型 key 与私有包仓库凭据只在 Egress 的密钥存储（仅 Egress 进程用户可读）；App 私钥与 webhook secret 在 `secrets/`（[architecture §6](../architecture.md#6-状态归属)）。`api` 写入时只转交、不留存：Egress 的凭据经控制面 `egress.sock` 两段式转交（暂存 → `settings.updated` 提交后激活，失败则丢弃，[egress-control.md §4.4](egress-control.md#44-密钥只写不读)），`present` 也经控制面查询；值不出现在设置文档、事件日志、日志、通知、备份的明文部分 |
| 原子性 | 同一次 `PUT` 里的 secret 写入与 `settings.updated` 要么都成功要么都不成功；不会出现"配置指向了不存在的 ref" |
| 共用 | 多个字段可引用同一个 ref，值只写一次 |
| 缺失 | `PUT` 结果里引用的 ref，既不在 `secrets/` 里、本次又没带 `value` → 422（`secret_ref_missing`）。启动加载时缺失 → 不崩溃，相关功能置 `unhealthy`，在 `status` 里显示 |
| 清除 | 与 api.md §4.9 的"`null` = 清除"**不同**：值只在被引用时有意义，`null` 一律 422（`secret_clear_in_use`）。要移除凭据就移除引用它的对象；不再被任何 `(scope, resource)` 引用的 ref，服务端回收其值（`secret_changes` 里记 `op: delete`）[提议] |
| 轮换 | 同名 ref 写入新 `value`，`version` 递增；消费者在下一次使用时取新值；`egress` 收到重载信号后换新（M3 密钥轮换） |
| 客户端 | `desktop-shell` 不得持久化 `value`，发送后立即清除 [提议]；不写日志（api.md §4.9） |
| 解析者 | `egress` 解析模型 key 与私有包仓库凭据——所有模型调用（沙盒内带 `job_token`，`models` 的直接调用经回环反向代理带 `svc_token`）都经 Egress（decisions P-16、P-49、P-50）；`forge` 解析 App 私钥与 webhook secret |
| 防误填 | 除 `value` 之外的任何字符串若形似密钥（`sk-` 前缀、`ghp_` / `github_pat_`、`-----BEGIN`、高熵长串）→ 422（`inline_secret_detected`） |
| 登记 | 服务端密钥由 `secrets` 模块存储（decisions P-38，模块地图已登记） |

### 2.5 修改权限（设备角色）

设备角色沿用 [data-model Device.role](../data-model.md#device--budgetledger)，语义取自 [api.md §2.2](api.md#22-角色语义层级approver--operator--viewer)（`approver ⊃ operator ⊃ viewer`；viewer **不能读** settings）。签名机制统一见 [api.md §6.6](api.md#66-内部动作的签名写入settings改角色吊销)（decisions P-18）。

| 操作 | 最低角色 | 签名 |
| --- | --- | --- |
| 读 settings（任何 resource、`view=effective`、`?rev=N`、`catalog`、`status`） | operator | — |
| 写：中性改动，或任何**收紧** | operator | 不需要 |
| 写：任何**放宽**（§2.2 方向）；"仅 approver 键"；任何带 secret `value` 的写入 | approver | **需要**审批设备签名：`action = settings_update`，`target = {kind:"settings", scope, project_id?, resource, base_rev}`，`subject_digest` = 请求体摘要；请求带 `Gantry-Approval` 头 |
| 手机档案 | 无 settings | — |
| **安装引导期**：第一台设备配对之前的主机命令（需 root） | — | 不需要：可写入"仅 approver 键"中的 `github.*` 与 `sandbox.base_image` 等引导所需的键，产生 `actor.kind = system` 的 `settings.updated`（即审计事件）。第一台设备配对之后，主机命令不再能写入，这些键只能经 settings 路由 + 审批签名修改（P-66） |

"仅 approver 键"：`models.providers`、`levels.*.graph_template`、`policies.allow_globs`、`github.*`、`sandbox.driver / runtime / base_image / max_concurrent_sandboxes`、`egress.groups / injection / default_profile`——它们要么决定凭据去向、攻击面或放行范围，要么方向无法比较。

签名从 M1 起就要求（M1 设置页要写模型凭据，decisions P-18），没有"只查角色不签名"的过渡期；签名用设备的审批签名密钥（每次需用户在场，P-47）。首台配对设备是 approver（decisions P-23）。设置页只在凭据或放宽类改动时要求签名，中性与收紧类改动直接保存（第二轮裁决 C27）。

校验器对 `PUT` 的新旧值做差，算出 `relaxes[]`，据此决定 `required_role` 与 `requires_signature`。角色不足 → `403 forbidden_role`（附 `required_role`）；需要签名而未带 → `403 signature_required`。`PUT …?dry_run=true` 返回 `{valid, errors[], warnings[], relaxes[], required_role, requires_signature, signing?}` 且不落库，设置页据此在提交前提示"此改动需要 approver 签名"并直接用 `signing` 发起审批（诉求 A2）。

### 2.6 热加载语义

总则：**判定类键每次判定时读当前 rev；创建类键在对象创建时固化。** 配置变更从不暂停、重启或改写运行中的交付单元与图。每个模块在两次操作之间原子切换到新的 `EffectiveSettings`，不在一次操作中途切换。这同时回答 api.md OQ-3 的"改预算对进行中的交付单元是否生效"：**不生效**。

| 子树 | 求值时点 | 对运行中的交付单元 |
| --- | --- | --- |
| `policies.actions / paths / escalation / approval / patch` | 每次判定 | **立即生效**（收紧与放宽皆然），不追溯已作出的判定。已 `granted` 的审批在**消费时重新过 policy**，新规则为 `deny` 则阻止（I3）；已开的 InboxItem 不自动关闭 |
| `guardian.triage / no_progress` | 每次分流、每轮结束 | 立即；不重新分流已处理的事件。`escalate` 一旦生效，进行中的自愈循环在下一步检查时转为上报 |
| `guardian.status_checklist`、`patrol_interval_s` | 下次更新 / 下个巡检周期 | 立即 |
| `levels.*`（预算、图模板名） | **交付单元创建时**：`engine` 按 `level_cap` 解析并写入 `BudgetLedger`（模板名 + limits），同时记下 `config_rev`（P-57） | **不变**。已有交付单元继续用创建时的值（依据 BudgetLedger 存的是 `template` 与 `limits`）；之后调整 `level_cap` 也不改变它。自动加码时的图模板从该 `config_rev` 的快照解析 |
| `levels.token_weights` | 每次记账（`budget.charged`）时 | 下一笔用量起；已记的不重算 |
| `levels.none.daily_tokens` | 每次派发 None 档问答前 | 立即；当天会话台账的上限随之变化 |
| `levels.default_level` | 创建交付单元（解析默认上限）时 | 不影响已有交付单元 |
| `models.utility / planner` | 每次调用 | 下一次调用起；进行中的请求不中断 |
| `models.worker` | Attempt 创建时解析进 JobSpec | 下一个 Attempt；进行中的 Attempt 不变。实际使用的 provider / model / effort 写入 `attempt.started`，供审计与计费 |
| `sandbox.*` | 沙盒创建时固化（`Sandbox.limits`、`image_digest`） | 只对新沙盒；存量沙盒受自身 `wall_time` 约束自然淘汰 |
| `egress.*` | 沙盒创建时固化 `egress_profile`（档案名 + 解析出的分组集合）；连接时求值 | 有效白名单 = **创建时快照 ∩ 当前配置**：收紧立即作用于存量沙盒，放宽只作用于新沙盒 [提议] |
| `notify.*` | 每次发送 | 立即 |
| `github.*` | `forge` 重载，App token 缓存失效 | 下一次取 token；webhook secret 变更后按 `webhook_secret_previous` 过渡 |

- **决策记录 rev**：`policy.denied`、`approval.requested`、`attempt.started`、预算台账创建都带上做决定时的 `config_rev`。
- **未生效可见**：`status`（§2.8）返回各模块的 `applied_rev` 与健康；设置页显示"某模块尚未应用 rev N"。模块应用失败不回滚事件（事件是事实），但必须上报 `unhealthy`，并按 fail-closed 行事（例如 `egress` 不可用 → 沙盒无网络）。
- **未配置模型**：`models.*.model` 无默认值；未配置时不能创建交付单元，会话只提示"去设置页配置"，不接受在聊天里改设置（I7）。

### 2.7 对 api.md 的诉求

settings 路由归 [api.md §4.9](api.md#49-settings)（5 条：全局 `GET` / `PUT`，项目 `GET` / `PUT` / `DELETE`，`?view=effective`，`If-Match` / ETag）。本文**不新增路由**，只提出下列需要 api.md 配合的修改。api.md 当前文本已采纳 A1–A9（见其 OQ-3），下表保留作对照；仍待定的见 OQ-25。

| # | 诉求 | 原因 |
| --- | --- | --- |
| A1 | `{resource}` 扩为 §2 开头的八个 | 本文 §1.2 |
| A2 | `PUT` 支持 `?dry_run=true`，响应形状见 §2.5 | 设置页提示所需角色 / 签名 |
| A3 | `GET` 支持 `?rev=N` | 可追溯、回滚 |
| A4 | `errors[].code` 在 `required / type / enum / range / unknown_field` 之外，追加 §11 的语义码；`errors[].pointer` 相对 `value`，跨 resource 校验的错误指向本次写入的 resource | §11 |
| A5 | 新增只读路由 `GET /v1/settings/catalog` 与 `GET /v1/settings/status`，operator 起；响应结构见 §2.8 | 设置页表单；未生效可见 |
| A6 | 凭据字段语义按 §2.4（`present` 只读、`value` 只写、`null` 不接受） | 与 §4.9 凭据行有差异 |
| A7 | 事件目录加 `settings.updated`；项目覆盖不再发 `project.updated`；`settings.*` 不进 viewer 与手机档案的事件流 | api.md OQ-4 |
| A8 | settings 写权限按 §2.5：放宽类与带 secret `value` 的写入需 approver + 签名；签名流程见 api.md §6.6 | 已采纳 |
| A9 | 默认档位只在 `levels.default_level` | 已决（decisions P-22）：Project 不再有该字段 |

### 2.8 `catalog` 与 `status` 的响应结构

api.md §4.9 把这两个只读路由的响应结构交给本文。字段名即契约；operator 起可读。

`GET /v1/settings/catalog`——设置页表单需要的"服务端才知道的枚举与约束"，前端不得硬编码：

| 字段 | 内容 |
| --- | --- |
| `schema_version` | 整数 |
| `models.capabilities` | `{"<provider_id>/<model>": {tools: bool, reasoning_efforts: [..], context_window?: int}}`，来源见 OQ-3 |
| `models.adapters` | `{"claude-code": {provider_kinds: [..], supports_fallback: bool}}`（适配器标识统一为 `claude-code`，与 worker-protocol `execution.agent` 一致） |
| `levels.graph_templates` | `{"<level>": ["<模板名>", …]}`，来自 `engine` 登记 |
| `policies.actions` | `{"<动作 id>": {default, allowed: [..], dangerous: bool, reserved: bool}}` |
| `guardian.triage` | `{"<类 id>": {default, allowed: [..], locked: bool}}`；锁定类 `allowed` 为空并带 `value` |
| `egress.suggested_groups` | §8.2 的内置分组 |
| `egress.forbidden_hosts` | §8.3 的硬性拒绝模式，供表单即时提示 |
| `roles.approver_keys` | §2.5 的"仅 approver 键"路径模式 |
| `notify.channels` | `{"desktop": {available: true}, "mobile_push": {available: false, reason: "reserved"}}` |

```json
{
  "schema_version": 1,
  "models": {
    "capabilities": {
      "anthropic/example-worker-model": { "tools": true, "reasoning_efforts": ["none", "low", "medium", "high", "max"] }
    },
    "adapters": { "claude-code": { "provider_kinds": ["anthropic"], "supports_fallback": false } }
  },
  "policies": {
    "actions": {
      "merge_pr": { "default": "approve", "allowed": ["approve", "deny"], "dangerous": true, "reserved": false }
    }
  },
  "guardian": {
    "triage": {
      "nit": { "default": "batch", "allowed": ["batch", "escalate"], "locked": false },
      "security_finding": { "locked": true, "value": "escalate", "allowed": [] }
    }
  },
  "roles": { "approver_keys": ["models.providers", "levels.*.graph_template", "github.*"] }
}
```

`GET /v1/settings/status`——"改了但还没生效"的可见性（§2.6）：

| 字段 | 内容 |
| --- | --- |
| `latest_rev` | 当前最大的 `settings.updated` 的 `seq` |
| `modules.<name>` | `{applied_rev, health, detail?}`（版本字段统一叫 `applied_rev`，第二轮裁决 C26）；`<name>` 取 `models` / `engine` / `policy` / `guardian` / `sandbox` / `egress` / `notify` / `forge`；`health` 取 `ok / degraded / unhealthy` |
| `modules.<name>.unresolved_secret_refs` | 该模块读到但在 `secrets/` 里找不到的 ref 列表（§2.4"缺失"） |
| "未生效" | `applied_rev` 小于该模块所读 resource 的最近一次 `settings.updated.seq` |

## 3. `models`

三类模型的定位与调用方来自 [architecture §9](../architecture.md#9-三类模型)。

| 类别 | 键 | 用途 | 调用方 | 工具 | 推理默认 |
| --- | --- | --- | --- | --- | --- |
| 打杂 | `utility` | `classify`：CI 日志分类、事件摘要、状态问答、提交说明、状态清单、通知文案 | `models` 直接调用 | 无 | 低（`low`） |
| 大脑 | `planner` | 直接调用：`intent`、`admission`、`summarize`；沙盒只读：`plan`、`reasoner`、`arbiter` | `models` 直接调用；沙盒内 agent CLI | **必须支持**（沙盒角色要用） | 高（`high`），可设 `by_level` |
| 干活 | `worker` | 沙盒内 `build`、`verify`、`single`、`review:<视角>`、`heal`、`answer` | 沙盒内 agent CLI | **必须支持** | 随档位（`by_level`） |

角色、层级、执行位置的搭配见 [architecture §9](../architecture.md#9-三类模型)（decisions P-17）。**所有调用都经 Egress 反向代理**（decisions P-16）：沙盒内的 agent CLI 把模型 base URL 设为 `http://<egress>/j/<job_token>/<provider_id>`（P-49）；`models` 经 Egress 的回环 HTTP 反向代理调用，路径带 `svc_token`（P-50）；provider 的真实 `base_url` 只有 Egress 使用，由它注入 key 并计量（[egress-control.md](egress-control.md)）。沙盒内运行的 planner 角色沿用 `models.worker` 的 `adapter`、`attempt_wall_s`、`max_turns` **[提议]**。

### 3.1 `models.providers.<id>`（仅全局）

| 键 | 类型 / 取值 | 默认 | 说明 |
| --- | --- | --- | --- |
| `kind` | `anthropic` | — | `ModelProvider` 实现（[architecture §11](../architecture.md#11-扩展点)）；第一版只有 Anthropic API |
| `base_url` | https URL | 该 kind 的官方地址 | 其主机必须属于 `egress.groups.model_api`（V11）；改它等于改 key 的去向，所以是 approver 键 |
| `credential` | `{type, secret_ref}`（`SecretRef`，§2.4） | — | `type` 第一版只有 `api_key`；订阅登录未建模（Q-03，OQ-4） |

### 3.2 `models.utility` / `planner` / `worker`

| 键 | 类型 / 取值 | 默认 | 适用 | 说明 |
| --- | --- | --- | --- | --- |
| `provider` | providers 的 id | 无 | 全部 | |
| `model` | string | 无 | 全部 | 必须无默认；示例中的 id 是占位符 |
| `reasoning_effort` | `none / low / medium / high / max`；planner 与 worker 另可为 `by_level`（推理深度随档位，decisions P-17） | utility `low`；planner `high`；worker `by_level` | 全部 | 抽象枚举，由 `ModelProvider` 映射到具体参数；模型不支持的取值 → 422（V05） |
| `effort_by_level` | `{low, medium, high, max}`（小写档位键，P-71），每项 `{default, critical?}` | 见 §3.3 | planner / worker，且仅当 `reasoning_effort = by_level` | `critical` 由图模板标记的节点使用（OQ-6）；Ultimate 的子交付单元按 Max 解析（[architecture §8](../architecture.md#8-档位阶段与图)），所以没有 `ultimate` 项 |
| `fallback` | 数组，最多 3 项，每项 `{provider, model, reasoning_effort?}` | `[]` | 全部 | 见 §3.4 |
| `adapter` | `claude-code` | `claude-code` | worker | `WorkerAdapter` 实现（P-07），与 worker-protocol `execution.agent` 同一标识；其他适配器后续追加 |
| `request_timeout_s` | 整数，5–1800 | utility 60；planner 300；worker 600 **[建议值，待调优]** | 全部 | 单次模型请求的空闲超时（流式以"无数据"计） |
| `total_timeout_s` | 整数 | utility 120；planner 900 **[建议值，待调优]** | utility / planner | 一次调用含重试与降级的总上限 |
| `attempt_wall_s` | 整数 | 3600 **[建议值，待调优]** | worker | 单个 Attempt 的墙钟上限。**有效值 = min(本键, `sandbox.limits.wall_time_s`, 预算剩余 `wall_time_s`)**；超出记 `failure_class = timeout` |
| `max_turns` | 整数 ≥ 1 | 60 **[建议值，待调优]** | worker | 传给 agent CLI 的轮数上限，写入 JobSpec `constraints.limits.max_turns`（worker-protocol §4） |

`models.retry`（仅全局）：`max_attempts`（默认 3）、`backoff_ms`（默认 1000）**[建议值，待调优]**，作用于 `models` 直接发起的调用；worker 的重试由 agent CLI 自己负责。

`engine` 在创建 Attempt 时把上表解析进 JobSpec（[worker-protocol §4](worker-protocol.md)）：`execution.model` / `reasoning_effort` 来自本节（按 `level_effective` 与节点是否 `critical` 取值），`constraints.limits.wall_time_s` = min(`attempt_wall_s`, `sandbox.limits.wall_time_s`, 预算剩余 `wall_time_s`)，`idle_timeout_s` 来自 `sandbox.idle_timeout_s`（§7），`max_turns` 来自本节，`tokens_total` 是从 BudgetLedger 预留的份额（预留规则见 worker-protocol OQ-14）。

### 3.3 `effort_by_level` 建议默认

**[建议值，待调优]**，依据只是 [design.md](../design.md) 的档位表（历史记录，非规格）：

| 档位 | `default` | `critical` |
| --- | --- | --- |
| low | `low` | — |
| medium | `medium` | — |
| high | `medium` | `high` |
| max | `high` | `max` |

按 **`level_effective`**（实际运行的档位）取值，而不是 `level_cap` **[提议，OQ-6]**；预算才按 `level_cap` 记账（§4）。

### 3.4 降级（fallback）语义

| 项 | 规定 |
| --- | --- |
| 触发 | 连接失败、超时、5xx / overload、429 且重试耗尽、模型不可用（404） |
| 不触发 | 4xx 请求错误、内容策略拒绝、预算耗尽 |
| 顺序 | 按数组顺序依次尝试；每一项也有自己的 `request_timeout_s` 语义，总时长受 `total_timeout_s` 约束 |
| 约束 | 降级项必须满足本类别的全部校验（worker 的降级项也必须支持工具调用） |
| 记账 | 降级调用的 token 记入**同一 tier**；`attempt.finished.usage` 与 `budget.charged` 记录实际使用的 provider / model |
| worker | 能否在沙盒内切换 provider 取决于 `WorkerAdapter` 声明的能力；不支持则含 `fallback` 的配置被拒绝（V06，OQ-5） |

## 4. `levels`

`levels.default_level`：新任务的默认档位上限，取值为 `Level`（`"None"` / `"Low"` / `"Medium"` / `"High"` / `"Max"` / `"Ultimate"`，data-model §1），默认 `"Medium"` **[建议值，待调优]**。项目可覆盖。单个任务仍可在消息里指定上限（任务级参数，不是配置）。

档位条目的键一律小写：`none / low / medium / high / max / ultimate`（这是对象键，不是 `Level` 值；由 `Level` 值找条目时转小写，decisions P-71）。`levels.none` 只有一个键 `daily_tokens`（None 档不建交付单元，没有预算模板与图模板，V07）；其余五个条目各有图模板与预算。

`levels` 的其余顶层键：

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `default_level` | `Level` | `"Medium"` | 见上 |
| `token_weights.input` / `output` / `cache_read` / `cache_write` | 数值 > 0 | `1.0` / `5.0` / `0.1` / `1.25` **[建议值，待按价格调优]** | token 加权（P-44、P-70）：一次调用的加权 token = `Σ 权重 × 该类别 token 数`。所有预算里的 token 上限与台账用量都按加权值计；Egress 的 `tokens_total` 封顶也用同一组权重（签发令牌时随令牌下发）。默认值取一种常见的提供方价格比例（输入 : 输出 : 缓存读 : 缓存写 ≈ 1 : 5 : 0.1 : 1.25），不同提供方须自行调整。仅全局 |
| `none.daily_tokens` | 正整数 | 500,000 **[建议值，待调优]** | None 档会话台账的每日上限（加权 token，UTC 日，P-58）：每个项目的会话台账每天一个；`answer` 临时图与不属于交付单元的直接调用（如 `intent`）都计入。达到上限后当天不再派发新的 None 档读代码问答。项目可覆盖，仅收紧 |

### 4.1 字段

| 键 | 类型 | 对应 | 语义 |
| --- | --- | --- | --- |
| `graph_template` | string | `BudgetLedger.template`（族名；各阶段图的 `Workflow.template` 为 `<档位键>.<阶段>`，data-model） | 引用 `engine` 登记的模板族名，该族覆盖本档位各阶段的图（[architecture §8](../architecture.md#8-档位阶段与图) 表）。本文不定义模板内容；名字必须在 catalog 中存在且与档位匹配（V08） |
| `budget.tokens_by_tier.utility / planner / worker` | 正整数 | `BudgetLedger.limits.tokens_by_tier` | 该交付单元内该类模型的累计**加权** token 上限，以 Egress 计量为准，权重见 `levels.token_weights`（P-70） |
| `budget.nodes` | 正整数 | `limits.nodes` | 图中节点数上限（被图改写替换的节点也计；尝试另计，不占此项） |
| `budget.rounds` | 正整数 | `limits.rounds` | 评审迭代轮（`REVISE` 追加的"修正 + 复审"）与守护自愈轮之和的上限；超限或判定无进展即上报（§8） |
| `budget.wall_time_s` | 正整数（秒） | `limits.wall_time` | 该交付单元所有尝试运行时长之和的上限，不含等待人 / CI 的时间 |
| `budget.concurrency` | 正整数 | `limits.concurrency` | 同时运行的节点（沙盒）数上限；Ultimate 父交付单元指同时运行的子交付单元数 |
| `budget.forge_writes` | 正整数 | `limits.forge_writes` | 该交付单元 GitHub 写动作（Outbox 动作成功次数）的累计上限；耗尽 → `budget.exhausted` |
| `budget.forge_writes_per_hour` | 正整数 | （不进台账） | 按小时限速，由 Outbox 按交付单元的令牌桶执行，按 Outbox 动作数计（不按 API 调用数）。桶空时动作保持 `queued` 并设 `not_before`，**延迟**而不是失败（forge-actions §2.7）。与 `forge` 的安装级限流闸是两层，取更严者；与 `budget.forge_writes` 不混用（decisions P-22） |

### 4.2 语义规则

- **所有预算字段必填且有限**：不允许 `null`、`0`、"不限"（design.md：档位绑定硬预算与熔断）。
- **预算模板在交付单元创建时按 `level_cap` 固定**，不按 `level_effective`（P-57）；之后调整 `level_cap`、自动加码都不追加预算；预算耗尽 → `budget.exhausted` + InboxItem（§8）。
- **`level_effective`**：初值由引擎在规格阶段按风险评估给出（≤ `level_cap`），之后只在上限内升、不自动降；**已触发的闸门永不撤销**（P-57）。这是 `engine` 的行为，没有对应配置键。
- 运行中的交付单元不随配置变化（§2.6）；能否给运行中的交付单元追加预算见 OQ-10。
- Ultimate：父交付单元用 `levels.ultimate`，子交付单元各自按 `levels.max`（§8）；父交付单元的台账汇总所有子交付单元的用量（decisions P-22）。

### 4.3 建议默认值

**全部为 [建议值，待调优]**：没有真实任务数据支撑，只用于让系统能跑起来并保持档位单调递增。`wall_time_s` 的 5400 即 90 分钟。`nodes` 按"不先于 `rounds` 耗尽"取值（第二轮裁决 C29）：每轮 `REVISE` 追加"修正 + 复审"若干节点、每轮自愈追加一个 `heal` 节点，`nodes` 要容得下首轮图加上 `rounds` 轮的追加。

| 档位 | `graph_template`（建议名，以 engine 登记为准） | utility tokens | planner tokens | worker tokens | nodes | rounds | wall_time_s | concurrency | forge_writes | forge_writes_per_hour |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| low | `low.single` | 100,000 | 50,000 | 1,000,000 | 6 | 2 | 1,800 | 1 | 30 | 20 |
| medium | `medium.plan_build_verify` | 200,000 | 150,000 | 3,000,000 | 20 | 3 | 5,400 | 2 | 60 | 40 |
| high | `high.multi_review` | 400,000 | 300,000 | 8,000,000 | 40 | 4 | 10,800 | 3 | 120 | 60 |
| max | `max.adversarial` | 800,000 | 800,000 | 20,000,000 | 80 | 6 | 21,600 | 4 | 200 | 80 |
| ultimate | `ultimate.milestone` | 3,000,000 | 3,000,000 | 100,000,000 | 400 | 40 | 172,800 | 4 | 1,000 | 150 |

## 5. `policies`

判定归 `policy`（I3）；本节只定义可配置的输入。处置语义：

| 处置 | 含义 |
| --- | --- |
| `auto` | 无需人，直接放行 |
| `approve` | 需要 Gantry 审批：绑定 `(动作, 目标, subject_digest)` 的设备签名（[architecture §7.4](../architecture.md#74-危险动作与两类审批)） |
| `deny` | 拒绝，并按需写入收件箱 |

**fail-closed**：审批链路不可用时，`approve` 按 `deny` 执行并上报；某个动作被配置为 `approve` 但它在 data-model 里没有定义审批形状时，同样按 `deny` 处理并上报（P-54）。M1 的 `policy` v0 对 §5.2 中 `dependency_manifests`、`lockfiles`（及 `repo_meta`）的命中一律按 `deny` 处理并上报，M2 起才按 `approve`（decisions P-19）；`workflows` 在任何里程碑都是 `deny`，上报 `decision_needed`（`manual_action`）并附补丁（P-48）。`merge_pr` 等动作的签名审批 M1 起可用（decisions P-18），不受此影响。

### 5.1 `policies.actions`

键是动作 id，值是处置。**全部 14 个 `ForgeActionKind`（[forge-actions.md §3.1](forge-actions.md)）都列出，id 与 `kind` 同名**（decisions P-28）；另有三个**条件伪动作**（同一个 `kind` 在特定条件下的更严处置，由 `policy` 从载荷推出，`forge` 不感知）和三个**预留动作**（第一版不实现，forge-actions §3.4，decisions P-27）。`Approval.action` 也使用这些 id（forge-actions §2.5）。出厂默认值覆盖全部 20 个 id；`unlisted_action` 只是兜底。三个条件伪动作也是合法的 `Approval.action`；`ForgeActionKind` 被配置为 `approve` 时，只有在 data-model 中定义了审批形状的（目前是 `merge_pr`）才能真正发起审批，其余 fail-closed（P-54）。

| 动作 id | 对应 `ForgeActionKind` / 条件 | 默认 | 允许取值 | 依据 |
| --- | --- | --- | --- | --- |
| `merge_pr` | `merge_pr`（恒需审批，带 `expected_head_sha`） | `approve` | approve / deny | §7.4；M1 退出标准；GitHub 必需 review / check 仍须先满足 |
| `force_push` | `push_branch` 且 `force=true` | `approve` | approve / deny | §7.4；forge-actions §3.1 |
| `push_outside_prefix` | `push_branch` 到不以 `gantry/`（forge 的 `push.branch_prefix`）开头的分支 | `approve` | approve / deny | forge-actions §3.1 [提议：单列此伪动作] |
| `resolve_review_thread_human` | `resolve_review_thread` 且线程含人类评论 | `deny` | approve / deny | forge-actions §3.1 与其 OQ-9（"默认建议禁止"，待决）；取保守值 [提议] |
| `delete_branch` | **预留**，第一版不实现 | `deny` | approve / deny | forge-actions §3.4；decisions P-27 |
| `publish_release` | **预留** | `deny` | approve / deny | 同上 |
| `change_repo_settings` | **预留** | `deny` | approve / deny | 同上 |
| `push_branch` | `push_branch`（`gantry/` 前缀、非强推、未触及受保护路径） | `auto` | auto / approve / deny | 推送前的补丁校验（I8）是前置条件，与本键无关 |
| `create_pr` | `create_pr` | `auto` | auto / approve / deny | |
| `update_pr` | `update_pr` | `auto` | auto / approve / deny | |
| `upsert_status_comment` | `upsert_status_comment` | `auto` | auto / approve / deny | 设为 `deny` 会停掉状态清单评论 |
| `create_comment` | `create_comment` | `auto` | auto / approve / deny | |
| `reply_review_thread` | `reply_review_thread` | `auto` | auto / approve / deny | |
| `resolve_review_thread` | `resolve_review_thread`（线程无人类评论） | `auto` | auto / approve / deny | |
| `create_issue` | `create_issue` | `auto` | auto / approve / deny | |
| `update_issue` | `update_issue`（规格块 / 标题 / 标签集） | `auto` | auto / approve / deny | |
| `close_issue` | `close_issue` | `auto` | auto / approve / deny | |
| `add_labels` | `add_labels` | `auto` | auto / approve / deny | |
| `request_reviewers` | `request_reviewers` | `auto` | auto / approve / deny | 会通知对方，克制使用 |
| `rerun_failed_jobs` | `rerun_failed_jobs` | `auto` | auto / approve / deny | 次数上限是 forge 内部常量 |

- 架构 §7.4 把 merge、force push、推送到非 `gantry/` 分支与受保护路径的改动定义为"需要 Gantry 审批的危险动作"，删分支、发 release、改仓库设置第一版不实现，所以**上表前七行不允许 `auto`**。design.md 曾写"由配置决定自动放行或上报"，与此冲突，按优先级取 architecture（OQ-12）。"改 workflow 文件"由 §5.2 的路径规则 `workflows` 表达（固定 `deny`，P-48），不重复成动作 id。补丁命中处置为 `approve` 的路径规则时，审批动作是 `patch_apply_protected`（不是动作 id，没有对应键）。
- forge-actions §3.4：`approve_pr`（提交 `APPROVE` review）**永不提供**，`bypass_rules` 永不传——没有对应的动作 id，也不可配置（P-12）。
- `policies.unlisted_action`：只是兜底（出厂默认已覆盖全部动作，只有将来新增、而配置尚未列出的动作才会落到这里），`approve`（默认）/ `deny`；不能设为 `auto`（对应守护"不在规则表里一律上报"）。
- worker 的 proposal `kind`（worker-protocol §5.4）是 `ForgeActionKind` 的子集，由 `engine` 入队后才有动作 id；它们不直接出现在 `policies.actions`。

### 5.2 `policies.paths`

键为规则 id，值 `{globs[], disposition}`。作用于补丁与推送涉及的路径（I8 校验的一部分）。

| 项 | 规定 |
| --- | --- |
| 匹配 | gitignore 风格 glob，仓库相对 POSIX 路径，区分大小写；删除、重命名（新旧路径都算）、模式 / 符号链接变更都算触及 |
| 多条命中 | **取最严**：`deny` > `approve` > `auto` |
| 放行范围 | `policies.allow_globs`（字符串数组，默认 `["**"]`，仅全局，approver 键）：补丁只允许触及其中的路径，其余 `deny`（`policy.path_outside_allow`）。生效的路径集合 = JobSpec.allow ∩ `allow_globs`，减去 JobSpec.deny ∪ 命中 `deny` 的规则（worker-protocol §8.3 P8，取 `policy` **当前**规则） |
| `disposition` | `approve / deny`（规则的意义就是不放行，不允许 `auto`）。`approve` 的审批动作是 `patch_apply_protected`，由 `repo` 在提交前消费（P-54）。`workflows` 与 `sensitive_files` 只能是 `deny`（V15） |
| 项目覆盖 | 只能新增 id，或给已有 id **增加** `globs` / 提高处置 |

默认规则：

| id | globs | 处置 | 依据 |
| --- | --- | --- | --- |
| `workflows` | `.github/workflows/**` | `deny`，**不可放宽**（P-48）：Gantry 不修改 workflow 文件，App 不申请 `workflows` 权限；命中即 `policy.path_protected:workflows`，上报 `decision_needed`（`manual_action`）并附补丁 | P-48；§7.3；§7.4 |
| `dependency_manifests`（M1 按 `deny` 执行并上报，P-19） | `package.json`、`**/package.json`、`Cargo.toml`、`**/Cargo.toml`、`pyproject.toml`、`**/pyproject.toml`、`requirements*.txt`、`go.mod`、`**/go.mod`、`Gemfile`、`pom.xml`、`build.gradle*` | `approve` | P-11；"依赖清单"的权威清单未定（OQ-13），此列表是 **[建议]** |
| `lockfiles`（M1 按 `deny` 执行并上报，P-19） | `package-lock.json`、`pnpm-lock.yaml`、`yarn.lock`、`Cargo.lock`、`poetry.lock`、`uv.lock`、`go.sum`、`Gemfile.lock`（各加 `**/` 前缀变体） | `approve` | P-11；同上 |
| `sensitive_files` | `.env`、`.env.*`、`**/*.pem`、`**/*.key`、`**/id_rsa*`、`**/.npmrc` | `deny` | I8 的"敏感文件"校验；具体模式是 **[建议]** |
| `repo_meta` | `.github/actions/**`、`.github/CODEOWNERS`、`CODEOWNERS`、`.github/dependabot.yml`、`.devcontainer/**` | `approve` | **[建议]**，不是骨架要求：这些文件会改变 CI 执行面、评审归属或沙盒环境 |

### 5.3 `policies.escalation`（自动加码）

档位是上限，引擎可在上限内按风险加码，不能超过上限；需要超过时上报并暂停在当前阶段（[architecture §8](../architecture.md#8-档位阶段与图)）。

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `enabled` | bool，默认 `true` | 关闭 = 放宽 |
| `rules.<id>.when` | **恰好一种**：`{paths: [glob…]}` / `{diff: {files_gte?, lines_gte?}}` / `{labels: [string…]}` | `paths`：触及任一路径即命中。`diff`：改动文件数或增删行数任一达到阈值即命中。`labels`：关联 Issue / PR 带有任一标签（完全匹配） |
| `rules.<id>.raise_to` | `"Medium"` / `"High"` / `"Max"`（`Level`） | 把 `level_effective` 抬到该档位 |

求值规则：

- 多条命中取最高 `raise_to`；结果被 `level_cap` 截断；若规则要求超过上限 → 上报，不自行突破。
- 只能**抬高**，从不降低（降档规则骨架未定，OQ-14）。
- **外部文本触发的规则（标签、路径、diff 内容）只能提高审视，不得降低档位或放宽任何处置**（I5）。
- 加码不追加预算（§4.2）。
- 求值时点：进入阶段时、diff 变化时；这是 `engine` 的行为，不是配置。

建议默认规则 **[建议值，待调优]**（依据 §8 示例"触及 auth 路径插入对抗 review"与 design.md 的"diff 规模、敏感路径、风险标签"；`ci_failures` 条件只见于 design.md，未纳入，OQ-14）：

| id | `when` | `raise_to` |
| --- | --- | --- |
| `sensitive_paths` | `paths`：`**/auth/**`、`**/security/**`、`**/crypto/**`、`**/migrations/**` | `"Max"` |
| `large_diff` | `diff`：`files_gte` 30，`lines_gte` 1000 | `"High"` |
| `risk_labels` | `labels`：`security`、`breaking-change`、`risk:high` | `"Max"` |

### 5.4 `policies.approval` 与 `policies.patch`

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `approval.ttl_s` | 86400，范围 60–604800 **[建议值，待调优]** | 写入 `Approval.expires_at`；Forge 动作、补丁审批与内部动作的审批统一用它（P-75）；变大 = 放宽 |
| `patch.max_files` | 200 **[建议值，待调优]** | I8 的"大小"校验；超限 → `policy.denied` 并上报 |
| `patch.max_bytes` | 5,000,000 **[建议值，待调优]** | 补丁总字节 |
| `patch.max_changed_lines` | 10000 **[建议值，待调优]** | 总增删行数（`git apply --numstat` 得出，worker-protocol §8.3 P1） |
| `patch.max_file_bytes` | 1,000,000 **[建议值，待调优]** | 单文件字节 |

### 5.5 不变式规定、不可配置为放宽的项

下表每一行都**没有对应的放宽键**，或有键但校验器强制下限。

| 来源 | 固定的内容 | 配置层面的体现 |
| --- | --- | --- |
| I1 | 沙盒无 GitHub 网络、无长期凭据 | 任何 egress 档案不得含 GitHub 域名族（至少 `github.com`、`api.github.com` 及其子域；完整硬编码拒绝表由 `egress` 规格书维护，不是配置）→ V10；依赖指向 `github.com` 源默认拒绝（decisions Q-08）。沙盒没有 env / secret 挂载类配置键；凭据只由 `egress` 在反向代理出站时注入 |
| I2 | GitHub 写入唯一路径是 Outbox | 没有让其他模块写 GitHub 的键；`forge_writes`、`forge_writes_per_hour` 必须是有限正整数 |
| I3 | `policy` 是唯一放行点；审批绑定 `subject_digest`（PR 类为 head SHA），新推送使绑定 head 的审批失效 | 没有关闭绑定 / 失效的键；`approval.ttl_s` 有上限；审批消费时重新过 policy（§2.6）；§7.4 "守护在已审批 PR 上不得静默推送"→ 锁定类 `push_after_approval`（§6） |
| I5 | 外部文本是数据，进 prompt 要隔离 | 没有关闭隔离的键；外部文本触发的加码 / 分流规则只能提高审视（§5.3） |
| I6 | 客户端不执行、不持凭据 | API 永不返回 secret 值；没有向客户端下发凭据的键 |
| I7 | 配置与工作分离 | 写设置只有 api.md §4.9 的 settings 路由；`conversation`、规划、worker 都没有写路径，**不得把自然语言解析成设置变更**；工作页面无配置控件（档位上限是任务参数，见 §1.1） |
| I8 | 执行产物先验后推 | 路径规则、大小、敏感文件三项校验不可关闭；大小阈值只能在 `core` 定义的硬上限内调；`sensitive_files` 的 `deny` 不可降低；二进制与符号链接一律拒绝，没有白名单键（P-68） |
| P-48 | Gantry 不修改 `.github/workflows/` | `policies.paths.workflows` 固定 `deny`（V15 `invariant_floor`）；`github.*` 里没有申请 `workflows` 权限的键 |
| §7.4 / P-12 | Gantry 审批与 GitHub 审批是两件事 | 没有"跳过 GitHub 必需 review / check"的键；`merge_pr` 的 `approve` 不能替代 GitHub 规则 |
| §7.3 | 守护的上报类、兜底、忽略自身事件 | `guardian.triage` 的锁定类不可改（§6）；自事件过滤无键 |
| §8 | 档位是上限；预算有限且按上限记账 | 没有"允许超出上限"的键 |
| 审计 | 静默处理仍可审计（§7.3；design.md） | 事件日志审计无开关；收件箱写入不可关闭（`notify` 只控制推送） |
| I4 | 对账、事实源 | 无对应配置键 |

## 6. `guardian`

分流规则的**唯一来源是 [modules/guardian.md](../modules/guardian.md) §7.3 的有序决策表**（P-60），其中包括"无需动作"（`noop` / `ignore` / `bookkeeping`）的显式行；决策表之外的事件一律上报（`unknown` → `escalate`）。本节只定义决策表引用的**可配置覆盖层**（`cfg(<类 id>)`）与锁定类的清单；类 id 与决策表不一致时以决策表为准并回写本文。每个分流处置写审计事件 `guardian.triaged`（`event_ref`、`class`、`action`），自愈写 `guardian.heal_requested`，上报写 `guardian.escalated`（data-model §5.2）。

### 6.1 `guardian.triage.<class>`

取值含义：`silent_fix` 静默修复并推送；`silent_reply` 回复理由并关闭；`batch` 攒到下次推送；`escalate` 上报。

| 类 id | 情形 | 默认 | 允许取值 | 出处 |
| --- | --- | --- | --- | --- |
| `ci_failure_in_scope` | 本 PR 范围内的 CI 失败（非 workflow 文件、非依赖清单） | `silent_fix` | silent_fix / escalate | §7.3 |
| `bot_finding_valid` | 非安全类 bot finding，经验证属实 | `silent_fix` | silent_fix / escalate | design.md |
| `bot_finding_invalid` | 非安全类 bot finding，经验证不成立 | `silent_reply` | silent_reply / escalate | §7.3；回复用 `reply_review_thread`，M2 起可用（P-60） |
| `nit` | 普通 nit | `batch` | batch / escalate | §7.3 |
| `mechanical_conflict` | 可机械解决的冲突（生成文件等） | `escalate` | escalate / silent_fix | design.md 说静默，§7.3 未列 → 落到"兜底上报"；**默认取保守值**（OQ-14）。lockfile 冲突仍由 `policies.paths` 约束 |

**锁定类（不可配置，出现在 `triage` 里即 422 `locked_key`；设置页只读展示）**：

| 类 id | 处置 | 出处 |
| --- | --- | --- |
| `security_finding` | `escalate` | §7.3 |
| `touches_protected_path`（workflow、依赖清单、lockfile 等） | 由 `policies.paths` 决定 | §7.3；P-11 |
| `scope_expanding_fix` | `escalate` | §7.3 |
| `repeated_or_no_progress` | `escalate` | §7.3；M2 退出标准 |
| `budget_exhausted` | `escalate` | §7.3 |
| `approval_needed` | `escalate` | §7.3 |
| `push_after_approval` | `escalate`（已审批 PR 上不得静默推送） | §7.4 |
| `design_or_api_change` | `escalate` | guardian.md 决策表 F-02 |
| `unknown`（决策表里的上报行，以及决策表之外的一切事件） | `escalate` | §7.3 兜底；guardian.md U-01 |

**无需动作**不是一个类：决策表里显式列出的 `noop` / `ignore` / `bookkeeping` 行（CI 通过、GitHub approve、自身事件回流、陈旧事件、Issue 事件等）不可配置，只记账、不打扰（审计细则见 guardian.md §7.10）。守护的静默自愈不改变交付阶段；需要改代码且超出自愈范围时由引擎走 `review → implement`（P-60）。

### 6.2 其余键

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `no_progress.same_class_repeats` | 整数 ≥ 1 | 2 | 同类问题出现次数达到此值即上报。来自 M2 退出标准"同类问题重复两次" |
| `no_progress.stalled_rounds` | 整数 ≥ 1 | 2 | 连续无进展轮数达到此值即上报。来自 M2 退出标准"连续两轮无进展"。"同类 / 进展"的判定算法在 guardian.md |
| `status_checklist.enabled` | bool | `true` | 是否在 PR 上维护**单条可编辑**的状态清单评论（M2）。关闭不影响收件箱与事件日志里的审计记录 |
| `status_checklist.min_update_interval_s` | 整数 ≥ 0 | 30 **[建议值，待调优]** | 两次编辑评论的最小间隔，减少 Outbox 写入 |
| `patrol_interval_s` | 整数 ≥ 300 | 1800 **[建议值，待调优]** | 低频对账巡检周期，补齐丢失或乱序的 webhook 投递；绝不轮询（design.md） |

## 7. `sandbox`

| 键 | 类型 / 取值 | 默认 | 说明 |
| --- | --- | --- | --- |
| `driver` | `podman` | `podman` | `SandboxDriver` 实现（P-05）。`firecracker` 预留，第一版不接收不可信 fork 代码（P-09），schema 暂不接受 |
| `runtime` | `runsc / runc` | `runsc` | gVisor；M2 起默认开启（M2 `exec`），M1 允许 `runc`。`runtime` 不是不变式，但降级到 `runc` 是放宽 |
| `base_image` | 以 digest 固定的镜像引用 | 随本版本发布、固定摘要的官方 worker 镜像（P-66） | 基础镜像只读；必须含 `@sha256:`（V12）；对应 `Sandbox.image_digest`。"仅 approver 键"；安装引导期可由主机命令写入（§2.5） |
| `limits.cpu_cores` | 数值 > 0 | 2 **[建议值，待调优]** | → `Sandbox.limits.cpu` |
| `limits.mem_mb` | 整数 | 4096 **[建议值，待调优]** | → `limits.mem` |
| `limits.disk_mb` | 整数 | 10240 **[建议值，待调优]** | → `limits.disk` |
| `limits.pids` | 整数 | 512 **[建议值，待调优]** | → `limits.pids` |
| `limits.wall_time_s` | 整数 | 3600 **[建议值，待调优]** | → `limits.wall_time`；单个沙盒的墙钟上限，与 `models.worker.attempt_wall_s`、预算 `wall_time_s` 三者取最小 |
| `max_concurrent_sandboxes` | 整数 ≥ 1 | 6 **[建议值，待调优]** | 宿主机上同时存在的沙盒总数；调度器据此排队，不因此失败 |
| `prewarm.enabled` / `prewarm.pool_size` | bool / 整数 | `false` / `0` | 预热池（M2） |
| `devcontainer.mode` | `off / safe_subset` | `safe_subset` | `safe_subset` 忽略 `initializeCommand`、`mounts`、`runArgs`、特权字段（M2） |
| `idle_timeout_s` | 整数 | 300 **[建议值，待调优]** | 超过此时间没有收到合法进度事件即 kill（`timeout.idle`，worker-protocol §3.4） |
| `kill_grace_s` | 整数 | 10 **[建议值，待调优]** | SIGTERM 后到 SIGKILL 的宽限 |
| `io.progress.max_line_bytes` / `max_lines` / `max_rate_per_s` | 整数 | 8192 / 20000 / 20 **[建议值，待调优]** | 进度流（`progress.jsonl`）上限。worker-protocol §8.6 里的示意键 `worker.progress.*` 在此落位 |
| `io.artifacts.result_max_bytes` / `report_max_bytes` / `proposals_max_bytes` / `proposals_max_count` | 整数 | 1048576 / 262144 / 262144 / 20 **[建议值，待调优]** | 产物上限（`worker.artifacts.*` 落位） |
| `io.materials.max_item_bytes` / `max_total_bytes` | 整数 | 65536 / 524288 **[建议值，待调优]** | 写入沙盒 `inputs/` 的不可信材料上限（`worker.materials.*` 落位） |
| `io.logs_tail_bytes` | 整数 | 1048576 **[建议值，待调优]** | 采集的 stdout / stderr 尾部大小（`worker.logs.tail_bytes` 落位） |

沙盒的网络永远只经 Egress 代理，不可配置（I1）。资源限制是否随档位或节点角色变化（design.md 只说"与档位预算联动"）未定，第一版只提供全局默认（OQ-16）。

## 8. `egress`

`egress` 是独立进程，只读本节配置、`models.providers`、`levels.token_weights` 与它们引用的 secret，这份视图由 `gantryd` 经控制面下发（[egress-control.md](egress-control.md)，P-50）。两种模式（decisions P-16）：模型 API 与私有包仓库走**反向代理**（调用方把 base URL 指向 Egress，Egress 注入凭据并计量：沙盒调用方凭路径中的 `job_token` 识别为某个尝试，P-49；`models` 经回环反向代理凭 `svc_token` 识别，P-50）；公共包仓库走 **HTTPS CONNECT 白名单**（不注入凭据；沙盒内的 CONNECT 用 `Proxy-Authorization` 携带 `job_token`，Egress 据此按尝试计量与审计，P-72）。

### 8.1 键

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `groups.<name>` | `{hosts: [string…]}` | 白名单分组。`hosts` 是 DNS 主机名，允许一层前缀通配 `*.example.com`；默认只放行 443。未被 `injection` 或 provider 覆盖的主机走 CONNECT |
| `profiles.<name>` | `{groups: [name…]}` | 档案 = 分组集合；沙盒创建时固化为 `Sandbox.egress_profile` |
| `default_profile` | profile 名，默认 `default` | 所有沙盒用此档案（按节点角色区分档案见 OQ-16） |
| `injection.<id>` | `{hosts[], kind, header?, credential}` | **私有包仓库的反向代理路由**：沙盒内的包管理器把仓库地址指向 Egress，Egress 对列出的 `hosts` 在出站时加头，值取自 `credential`（`SecretRef`，§2.4）。`kind` 为 `bearer`（`Authorization: Bearer …`）或 `header`（自定义头名，如 `x-api-key`） |

- **模型 API 的反向代理路由由 `models.providers` 自动派生**：每个 provider 的 `base_url` 主机 + `credential.secret_ref` 形成只读的注入项，显示在设置页，不在 `egress.injection` 里重复声明（避免两处 secret_ref 漂移）。`egress.injection` 只放私有包仓库。
- 注入的凭据只发往列出的主机，**从不出现在沙盒内**（I1）；注入项的 `hosts` 必须属于某个分组（V13）。

### 8.2 建议的内置分组

**[建议值，待调优]**，出站主机清单需在实现时核对各生态官方文档：

| 分组 | 主机 |
| --- | --- |
| `model_api` | `api.anthropic.com` |
| `npm` | `registry.npmjs.org` |
| `pypi` | `pypi.org`、`files.pythonhosted.org` |
| `crates` | `crates.io`、`index.crates.io`、`static.crates.io` |
| `go` | `proxy.golang.org`、`sum.golang.org` |
| `maven` | `repo.maven.apache.org`、`repo1.maven.org` |

M1 只要求"白名单、只对模型 API 注入 key、出网日志"。M1 的 `default` 档案可只含 `model_api`；哪些语言包仓库默认放开见 OQ-11。

### 8.3 硬性拒绝（校验与运行时均执行，不是配置）

| 规则 | 依据 |
| --- | --- |
| GitHub 域名族 | I1；M1 退出标准 |
| IP 字面量；解析结果落在回环 / 私网 / 链路本地段的连接 | 防止沙盒触达宿主与内网服务（威胁模型："破坏宿主"）**[提议]** |
| 裸 `*`、多层通配 | 白名单失效 |
| 出网请求日志不可关闭 | design.md："审计：记录所有出网请求"；保留期见 OQ-21 |

## 9. `notify`

收件箱写入始终发生，**不可配置**；`notify` 只决定是否 / 往哪推送（D-04：没有问题不打扰）。

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `channels.<kind>.enabled` | bool | 渠道开关。`kind`：`desktop`（桌面端系统通知，第一版实现，[architecture §11](../architecture.md#11-扩展点)）；`mobile_push`（M4 APNs，保留）；`email`、`github_mention`（保留，Q-07 待决）。保留的 kind 在 schema 里暂不接受 |
| `routes.<id>` | `{kinds: [InboxItem.kind…], channels: [kind…]}` | 哪些上报类型推到哪些渠道。`InboxItem.kind` 取值见 [data-model](../data-model.md#inboxitem--approval)：`approval_needed / decision_needed / spec_needed / budget_exhausted / escalation / security_finding` |

- 通知文案可由 `utility` 模型生成（`notify` 对 `models` 的依赖是可选的，模型不可用时用固定模板，P-59）；正文不得原样透传不可信文本（I5）。
- **手机推送**（M4）：推送令牌（APNs device token）不是设置键，由手机经 `PUT /v1/devices/self/push-token` 注册（P-77）。
- **桌面系统通知**由客户端在收到 `inbox.raised` 且 `severity ≥ warn` 时弹出（P-63），不另设事件；`channels.desktop.enabled` 决定服务端是否把该事项推给桌面通道，本机外观页的开关决定本机是否弹窗（§13.2）。
- 其他按 `severity` 的路由第一版不支持（OQ-15）。
- 桌面端关闭时的离线通知见 Q-07。

## 10. `github`

仅全局。多数值由 `gantry github setup`（App Manifest 流程，P-65）写入初值；第一台设备配对之前可由主机命令写入（安装引导期，P-66），之后只能经 settings 路由 + 审批签名修改。设置页以展示 / 状态为主。

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `app.app_id` | 整数 | GitHub App ID（非密） |
| `app.private_key` | `SecretRef` | App 私钥（[architecture §6](../architecture.md#6-状态归属)：本地密钥文件） |
| `app.webhook_secret` | `SecretRef` | webhook HMAC secret |
| `commit_identity` | `{name, email}` | `repo` 创建提交时用的作者 / 提交者身份（P-69）；App 创建时由 `gantry github setup` 写入（建议值 `name = {slug}[bot]`、`email = {bot_user_id}+{slug}[bot]@users.noreply.github.com`，格式需核实，forge-actions §8）。改它会改变提交归属，是"仅 approver 键"（`github.*`） |
| `app.webhook_secret_previous` | `SecretRef`，可选 | 轮换窗口内仍接受的上一个 secret：`forge` 先试当前再试上一个（decisions P-29）。轮换步骤 **[提议]**：① 把旧值所在的 ref 填进本键；② 为 `app.webhook_secret` 写入新值；③ 在 GitHub 上更新 webhook secret；④ 窗口结束后移除本键（旧 ref 无引用即被回收，§2.4） |

- `installation_id`、`bot_user_id` 等是 `Project` 的字段（data-model），不在这里。
- **`forge.*` 的内部参数不是设置**（webhook 监听与载荷上限、对账批大小、Outbox 退避、推送传输、限流余量等）：forge-actions §11 把它们划为引导配置（`deploy`，重启生效）或 forge 内部常量，本文不收录。对账巡检周期是例外，按 forge-actions §7.1 放在 `guardian.patrol_interval_s`（§6.2）。`forge.merge.method` 是否做成设置键见 OQ-26。
- 私钥与 webhook secret 的轮换见 M3，走 `SecretRef.value`（§2.4）。

## 11. 校验规则

写入前由 `core` 的校验器 + `api` 提供的上下文执行（上下文来自 `models` 能力表、`engine` 模板登记、密钥存储、其他 resource 的当前值）。先 schema 校验再语义校验（api.md §4.9）。`error` → `422 validation_failed`，`errors[]` 为 `{pointer, code, message}`，`pointer` 相对 `value`；`warning` 不阻止写入，只在响应与设置页中提示（诉求 A4）。

| ID | 范围 | 规则 | 级别 | 错误码 |
| --- | --- | --- | --- | --- |
| V01 | 全部 | 未知键拒绝（每层 `additionalProperties: false`）；类型 / 枚举 / 范围错误 | error | `unknown_field` / `type` / `enum` / `range`（api.md 已有） |
| V02 | 全部 | 不得内联凭据：`SecretRef` 以外，键名含 `api_key / token / password / secret / private_key` 且值为字符串；或任何字符串形似密钥（§2.4） | error | `inline_secret_detected` |
| V03 | 全部 | 每个 `secret_ref` 匹配命名规则；已存在于密钥存储，或本次请求带了 `value`；`value` 不得为 `null` | error | `secret_ref_missing` / `secret_clear_in_use` |
| V04 | `models.worker`、`models.planner` 及其 `fallback` | **模型必须支持工具调用**（来自 `models` 的能力表，来源见 OQ-3；planner 也用于沙盒只读角色）；对 utility 不要求 | error | `worker_model_no_tools` |
| V05 | `models.*` | `reasoning_effort`、`effort_by_level` 的每个取值必须被该模型支持；`by_level` 只允许 planner / worker，且必须提供 `effort_by_level` 且 `low / medium / high / max` 齐全、每项有 `default`；`reasoning_effort` 为固定值时 `effort_by_level` 被忽略（可以存在，项目覆盖固定值时全局的该表仍在） | error | `effort_unsupported` / `effort_by_level_required` |
| V06 | `models.worker` | `adapter` 必须支持该 `provider.kind`；含 `fallback` 时 adapter 必须声明支持降级 | error | `adapter_provider_mismatch` / `fallback_unsupported` |
| V07 | `levels` | `default_level` 是合法 `Level`（首字母大写的线上值）；除 `default_level`、`token_weights` 外，键只能是小写档位键 `none / low / medium / high / max / ultimate`（P-71），大写键（如 `None`、`High`）拒绝；`none` 只允许 `daily_tokens`（正整数），不得有图模板或预算；`token_weights` 四项齐全且为正数；其余五个条目的预算字段（含 `forge_writes`、`forge_writes_per_hour`）必填、正整数 | error | `levels_keys` / `budget_not_finite` / `token_weights_invalid` |
| V08 | `levels.*.graph_template` | 模板名在 catalog 中存在，且与该档位匹配 | error | `template_unknown` / `template_level_mismatch` |
| V09 | `levels` | 各预算字段随档位非递减（`tokens_by_tier.*`、`nodes`、`rounds`、`wall_time_s`、`forge_writes`）；`ultimate` 预算 ≥ `max`；`concurrency` 不超过 `sandbox.max_concurrent_sandboxes`；`forge_writes_per_hour` 不超过 GitHub 内容创建类的官方每小时上限 × 余量（forge-actions §10.1 / §10.2：500 × 0.5 = 250，余量是 forge 的内部常量） | warning（最后一项 error） | `levels_not_monotonic` / `forge_rate_over_ceiling` |
| V10 | `egress` | 任何分组不得含 GitHub 域名族、IP 字面量、裸 `*`；`injection.hosts` 不得含 GitHub 域名族 | error | `egress_forbidden_host` |
| V11 | `models` ↔ `egress` | 每个被 worker（含降级）引用的 provider，其 `base_url` 主机属于 `default_profile` 所含分组；所有 provider 主机属于 `groups.model_api` | error | `provider_host_not_allowed` |
| V12 | `sandbox` | `base_image` 含 `@sha256:<64 位十六进制>`；`limits.*` 为正；`max_concurrent_sandboxes × limits.mem_mb` 超过宿主可用内存（来自 `status`）时提示 | error / warning | `image_not_pinned` / `sandbox_overcommit` |
| V13 | `egress.injection` | `hosts` 必须被某个分组覆盖；同一 `(host, header)` 只能有一个注入；`header` 不得是 `Host / Content-Length / Transfer-Encoding` 等逐跳或框架头 | error | `injection_host_not_in_group` / `injection_conflict` |
| V14 | `policies.actions` | 动作 id 属于 §5.1 的 20 个；取值在"允许取值"内（危险动作不得 `auto`） | error | `action_unknown` / `disposition_not_allowed` |
| V15 | `policies.paths` / `approval` / `patch` | glob 语法合法（`allow_globs` 非空）；`disposition` ∈ approve / deny；`sensitive_files` 与 `workflows` 的处置不得低于 `deny`，`workflows` 的 `globs` 不得缩小（P-48）；`approval.ttl_s`、`patch.*` 在硬上限内 | error | `glob_invalid` / `disposition_not_allowed` / `invariant_floor` |
| V16 | `policies.escalation` | `when` 恰好一种；`raise_to` ∈ `"Medium"` / `"High"` / `"Max"`；`diff` 阈值为正 | error | `escalation_when_invalid` |
| V17 | `guardian` | `triage` 的类 id 已知、取值在 §6.1 的"允许取值"内；锁定类出现即拒绝；`patrol_interval_s ≥ 300`；阈值 ≥ 1 | error | `locked_key` / `triage_value_invalid` |
| V18 | `notify` | 路由引用的渠道已启用且不是保留 kind；`kinds` 合法；`approval_needed / security_finding / budget_exhausted / escalation` 至少有一个已启用渠道可达 | error / warning | `channel_unavailable` / `urgent_unrouted` |
| V19 | 项目覆盖 | resource 可覆盖（否则 `422 not_overridable`）；键在 §2.2 允许范围内；"仅收紧"键不得比全局有效值更宽松；`egress.profiles` 的分组是全局对应档案的子集；不得新增 provider / secret | error | `override_not_allowed` / `override_loosens_global` |
| V20 | 全部 | `If-Match` 与当前 ETag 一致；角色满足 §2.5 | error | HTTP 层码：`428 precondition_required` / `412 precondition_failed` / `403 forbidden_role`（均为 api.md 已有） |
| V21 | `models` | 软提示：utility 的 effort 高于 `medium`、planner 低于 `high`、planner 无降级——偏离三类定位 | warning | `model_class_mismatch` |
| V22 | `models.worker` | `request_timeout_s ≤ attempt_wall_s`（error）；`attempt_wall_s > sandbox.limits.wall_time_s`（warning：有效值取最小，且项目收紧沙盒时间时会自然出现此情形） | error / warning | `timeout_inconsistent` / `attempt_exceeds_sandbox` |

## 12. 示例

示例中的 model id 是**占位符**，镜像 digest 是全零占位；数值沿用本文的 **[建议值，待调优]**。两份示例都是按 resource 拼起来的聚合视图；实际是每个顶层键一次 `PUT /v1/settings/{resource}`（项目为 `PUT /v1/projects/{id}/settings/{resource}`），请求体 `{"value": <该键的内容>}`。GET 响应里的 `SecretRef` 还会多一个只读的 `present`，写入时 `value` 才出现，示例里不展开。

### 12.1 全局配置

```json
{
  "models": {
    "providers": {
      "anthropic": {
        "kind": "anthropic",
        "base_url": "https://api.anthropic.com",
        "credential": { "type": "api_key", "secret_ref": "model.anthropic.main" }
      }
    },
    "retry": { "max_attempts": 3, "backoff_ms": 1000 },
    "utility": {
      "provider": "anthropic",
      "model": "example-utility-model",
      "reasoning_effort": "low",
      "fallback": [],
      "request_timeout_s": 60,
      "total_timeout_s": 120
    },
    "planner": {
      "provider": "anthropic",
      "model": "example-planner-model",
      "reasoning_effort": "high",
      "fallback": [
        { "provider": "anthropic", "model": "example-planner-fallback-model" }
      ],
      "request_timeout_s": 300,
      "total_timeout_s": 900
    },
    "worker": {
      "adapter": "claude-code",
      "provider": "anthropic",
      "model": "example-worker-model",
      "reasoning_effort": "by_level",
      "effort_by_level": {
        "low": { "default": "low" },
        "medium": { "default": "medium" },
        "high": { "default": "medium", "critical": "high" },
        "max": { "default": "high", "critical": "max" }
      },
      "fallback": [],
      "request_timeout_s": 600,
      "attempt_wall_s": 3600
    }
  },
  "levels": {
    "default_level": "Medium",
    "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 },
    "none": { "daily_tokens": 500000 },
    "low": {
      "graph_template": "low.single",
      "budget": {
        "tokens_by_tier": { "utility": 100000, "planner": 50000, "worker": 1000000 },
        "nodes": 6, "rounds": 2, "wall_time_s": 1800, "concurrency": 1, "forge_writes": 30, "forge_writes_per_hour": 20
      }
    },
    "medium": {
      "graph_template": "medium.plan_build_verify",
      "budget": {
        "tokens_by_tier": { "utility": 200000, "planner": 150000, "worker": 3000000 },
        "nodes": 20, "rounds": 3, "wall_time_s": 5400, "concurrency": 2, "forge_writes": 60, "forge_writes_per_hour": 40
      }
    },
    "high": {
      "graph_template": "high.multi_review",
      "budget": {
        "tokens_by_tier": { "utility": 400000, "planner": 300000, "worker": 8000000 },
        "nodes": 40, "rounds": 4, "wall_time_s": 10800, "concurrency": 3, "forge_writes": 120, "forge_writes_per_hour": 60
      }
    },
    "max": {
      "graph_template": "max.adversarial",
      "budget": {
        "tokens_by_tier": { "utility": 800000, "planner": 800000, "worker": 20000000 },
        "nodes": 80, "rounds": 6, "wall_time_s": 21600, "concurrency": 4, "forge_writes": 200, "forge_writes_per_hour": 80
      }
    },
    "ultimate": {
      "graph_template": "ultimate.milestone",
      "budget": {
        "tokens_by_tier": { "utility": 3000000, "planner": 3000000, "worker": 100000000 },
        "nodes": 400, "rounds": 40, "wall_time_s": 172800, "concurrency": 4, "forge_writes": 1000, "forge_writes_per_hour": 150
      }
    }
  },
  "policies": {
    "allow_globs": ["**"],
    "actions": {
      "merge_pr": "approve",
      "force_push": "approve",
      "push_outside_prefix": "approve",
      "resolve_review_thread_human": "deny",
      "delete_branch": "deny",
      "publish_release": "deny",
      "change_repo_settings": "deny",
      "push_branch": "auto",
      "create_pr": "auto",
      "update_pr": "auto",
      "upsert_status_comment": "auto",
      "create_comment": "auto",
      "reply_review_thread": "auto",
      "resolve_review_thread": "auto",
      "create_issue": "auto",
      "update_issue": "auto",
      "close_issue": "auto",
      "add_labels": "auto",
      "request_reviewers": "auto",
      "rerun_failed_jobs": "auto"
    },
    "unlisted_action": "approve",
    "paths": {
      "workflows": { "globs": [".github/workflows/**"], "disposition": "deny" },
      "dependency_manifests": {
        "globs": [
          "package.json", "**/package.json", "Cargo.toml", "**/Cargo.toml",
          "pyproject.toml", "**/pyproject.toml", "requirements*.txt",
          "go.mod", "**/go.mod", "Gemfile", "pom.xml", "build.gradle*"
        ],
        "disposition": "approve"
      },
      "lockfiles": {
        "globs": [
          "package-lock.json", "**/package-lock.json", "pnpm-lock.yaml", "**/pnpm-lock.yaml",
          "yarn.lock", "**/yarn.lock", "Cargo.lock", "**/Cargo.lock", "poetry.lock",
          "uv.lock", "go.sum", "**/go.sum", "Gemfile.lock"
        ],
        "disposition": "approve"
      },
      "sensitive_files": {
        "globs": [".env", ".env.*", "**/*.pem", "**/*.key", "**/id_rsa*", "**/.npmrc"],
        "disposition": "deny"
      },
      "repo_meta": {
        "globs": [".github/actions/**", ".github/CODEOWNERS", "CODEOWNERS", ".github/dependabot.yml", ".devcontainer/**"],
        "disposition": "approve"
      }
    },
    "escalation": {
      "enabled": true,
      "rules": {
        "sensitive_paths": {
          "when": { "paths": ["**/auth/**", "**/security/**", "**/crypto/**", "**/migrations/**"] },
          "raise_to": "Max"
        },
        "large_diff": {
          "when": { "diff": { "files_gte": 30, "lines_gte": 1000 } },
          "raise_to": "High"
        },
        "risk_labels": {
          "when": { "labels": ["security", "breaking-change", "risk:high"] },
          "raise_to": "Max"
        }
      }
    },
    "approval": { "ttl_s": 86400 },
    "patch": { "max_files": 200, "max_bytes": 5000000, "max_changed_lines": 10000, "max_file_bytes": 1000000 }
  },
  "guardian": {
    "triage": {
      "ci_failure_in_scope": "silent_fix",
      "bot_finding_valid": "silent_fix",
      "bot_finding_invalid": "silent_reply",
      "nit": "batch",
      "mechanical_conflict": "escalate"
    },
    "no_progress": { "same_class_repeats": 2, "stalled_rounds": 2 },
    "status_checklist": { "enabled": true, "min_update_interval_s": 30 },
    "patrol_interval_s": 1800
  },
  "sandbox": {
    "driver": "podman",
    "runtime": "runsc",
    "base_image": "registry.example/gantry/gantry-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    "limits": { "cpu_cores": 2, "mem_mb": 4096, "disk_mb": 10240, "pids": 512, "wall_time_s": 3600 },
    "max_concurrent_sandboxes": 6,
    "idle_timeout_s": 300,
    "kill_grace_s": 10,
    "io": {
      "progress": { "max_line_bytes": 8192, "max_lines": 20000, "max_rate_per_s": 20 },
      "artifacts": {
        "result_max_bytes": 1048576, "report_max_bytes": 262144,
        "proposals_max_bytes": 262144, "proposals_max_count": 20
      },
      "materials": { "max_item_bytes": 65536, "max_total_bytes": 524288 },
      "logs_tail_bytes": 1048576
    },
    "prewarm": { "enabled": false, "pool_size": 0 },
    "devcontainer": { "mode": "safe_subset" }
  },
  "egress": {
    "groups": {
      "model_api": { "hosts": ["api.anthropic.com"] },
      "npm": { "hosts": ["registry.npmjs.org"] },
      "pypi": { "hosts": ["pypi.org", "files.pythonhosted.org"] },
      "crates": { "hosts": ["crates.io", "index.crates.io", "static.crates.io"] },
      "go": { "hosts": ["proxy.golang.org", "sum.golang.org"] },
      "private_registry": { "hosts": ["npm.corp.example"] }
    },
    "profiles": {
      "default": { "groups": ["model_api", "npm", "pypi", "crates", "go", "private_registry"] }
    },
    "default_profile": "default",
    "injection": {
      "npm_private": {
        "hosts": ["npm.corp.example"],
        "kind": "bearer",
        "credential": { "secret_ref": "registry.npm.corp" }
      }
    }
  },
  "notify": {
    "channels": { "desktop": { "enabled": true } },
    "routes": {
      "urgent": {
        "kinds": ["security_finding", "approval_needed", "budget_exhausted", "escalation"],
        "channels": ["desktop"]
      },
      "normal": { "kinds": ["decision_needed", "spec_needed"], "channels": ["desktop"] }
    }
  },
  "github": {
    "app": {
      "app_id": 1234567,
      "private_key": { "secret_ref": "github.app.private_key" },
      "webhook_secret": { "secret_ref": "github.app.webhook_secret" }
    },
    "commit_identity": {
      "name": "gantry-acme[bot]",
      "email": "9988776+gantry-acme[bot]@users.noreply.github.com"
    }
  }
}
```

### 12.2 项目覆盖（`Project.settings_override`）

全部改动都在 §2.2 的允许范围内：模型自由覆盖；预算、策略、守护阈值、沙盒内存只收紧；egress 只收窄。覆盖涉及六个 resource（`models`、`levels`、`policies`、`guardian`、`sandbox`、`egress`），对应六次项目级 `PUT`；`notify`、`github` 不可覆盖。写入需要的角色：除 `guardian.status_checklist` 与 `models.worker` 外都是收紧，operator 即可；没有任何放宽。

```json
{
  "models": {
    "worker": { "model": "example-worker-model-alt", "reasoning_effort": "high" }
  },
  "levels": {
    "high": { "budget": { "nodes": 20, "wall_time_s": 7200 } }
  },
  "policies": {
    "actions": { "rerun_failed_jobs": "deny" },
    "paths": {
      "infra": { "globs": ["deploy/**", "terraform/**"], "disposition": "approve" }
    },
    "escalation": {
      "rules": {
        "payments": { "when": { "paths": ["src/payments/**"] }, "raise_to": "Max" }
      }
    }
  },
  "guardian": {
    "triage": { "nit": "escalate" },
    "no_progress": { "same_class_repeats": 1 },
    "status_checklist": { "enabled": false }
  },
  "sandbox": { "limits": { "mem_mb": 2048 } },
  "egress": {
    "profiles": { "default": { "groups": ["model_api", "npm", "private_registry"] } }
  }
}
```

有效值（该项目）：worker 固定 `high` 推理（不再 `by_level`，`effort_by_level` 不再生效）；`high` 档 `nodes` 20、`wall_time_s` 7200，其余预算继承全局；`rerun_failed_jobs` 为 `deny`；多出 `infra` 路径规则与 `payments` 加码规则；沙盒内存 2048 MiB；出网只剩模型 API、npm、私有仓库。

### 12.3 会被拒绝的写入（对照）

| 写入内容 | 错误码 | 原因 |
| --- | --- | --- |
| `policies.actions.merge_pr = "auto"` | `disposition_not_allowed` | 危险动作不允许 `auto`（V14） |
| `levels.high.budget.nodes = 50`（全局 40） | `override_loosens_global` | 预算只能降（V19） |
| `policies.paths.workflows.disposition = "auto"` | `disposition_not_allowed` | 路径规则不允许 `auto`（V15） |
| `policies.paths.workflows.disposition = "approve"` | `invariant_floor` | Gantry 不修改 workflow 文件，只能 `deny`（V15，P-48） |
| `levels.none.daily_tokens = 0` | `budget_not_finite` | 必须是正整数（V07） |
| `egress.profiles.default.groups` 加入 `maven` | `override_loosens_global` | 项目只能收窄（V19） |
| `guardian.triage.security_finding = "silent_fix"` | `locked_key` | 锁定类（V17） |
| `models.providers.alt = {…}` | `override_not_allowed` | 提供方与凭据仅全局（V19） |
| 项目级 `PUT /v1/projects/{id}/settings/notify` | `422 not_overridable` | `notify` resource 不可覆盖（api.md） |
| 全局 `PUT` 含 `"credential": {"secret_ref": "x", "value": null}` | `secret_clear_in_use` | `null` 不接受（§2.4） |
| 用 operator 设备把 `policies.actions.merge_pr` 从 `approve` 改成 `deny`，再改回 `approve` | 第二次 `403 forbidden_role`（`required_role: approver`） | 放宽需要 approver + 签名（§2.5）；注意 `deny → approve` 是放宽，`approve → deny` 是收紧 |
| approver 设备写入 `models.providers.anthropic.credential.value` 但不带 `Gantry-Approval` | `403 signature_required` | 带 secret `value` 的写入需要审批签名（§2.5，api.md §6.6） |
| `egress.groups.x.hosts = ["api.github.com"]` | `egress_forbidden_host` | I1（V10） |

## 13. 设置页映射

设置页是配置的**唯一入口**（D-09、I7）；viewer 看不到设置页（api.md §2.2），手机端没有设置（D-11）。页面按存储位置分三组（decisions P-31）：**服务端**页编辑全局设置；**项目**组每个项目一页，只列 §2.2 允许覆盖的键，显示继承值（置灰，来自 `view=effective`）与"覆盖"开关；**本机**页只存在于本机。表单由 `schemas/config/` 的 JSON Schema 与 `catalog`（诉求 A5）驱动，提交前先 `dry_run`（诉求 A2），据返回显示"需要 approver 签名"并直接发起审批（api.md §6.6），并用 `status` 显示"某模块尚未应用"。凭据输入框只写不读，已设置时显示 `present`。工作页面只显示运行状态（如某次 Attempt 实际用了哪个模型），不放任何配置控件。

| 组 | 设置页 | 对应键 / 内容 | 存储位置 | 里程碑 |
| --- | --- | --- | --- | --- |
| 服务端 | 模型 | `models.*`；provider 与凭据写入（approver + 签名） | 服务端（`settings.updated`），凭据在 Egress | M1 |
| 服务端 | 档位与预算 | `levels.*`（含 `default_level`）；`policies.escalation`（自动加码） | 服务端 | M2 |
| 服务端 | GitHub App | `github.*` 展示与 secret 轮换（写 `value`）；各项目的安装状态（`Project.installation_id`）；创建引导链接 | 服务端 | 未排期（OQ-19） |
| 服务端 | 沙盒与出网 | `sandbox.*`、`egress.*`（含自动派生的模型 API 反向代理路由，只读展示） | 服务端 | 未排期（OQ-19） |
| 服务端 | 危险动作策略 | `policies.actions / paths / allow_globs / approval / patch` | 服务端 | M2 |
| 服务端 | 后台守护 | `guardian.*` | 服务端 | M2 |
| 服务端 | 通知 | `notify.*` | 服务端 | 未排期（OQ-19） |
| 项目 | 每个项目一页 | 该项目对 `models`、`levels`、`policies`、`guardian`、`sandbox`、`egress` 中可覆盖键的覆盖（§2.2）；`notify`、`github` 不出现 | 服务端（`Project.settings_override`） | 未排期（OQ-19） |
| 本机 | 设备与证书 | 见下 | 设备实体在服务端；密钥与证书在本机 | M1（列表、改名、吊销；第二轮裁决 C28） |
| 本机 | 外观 | 见下 | **仅本机** | M3（第二轮裁决 C28） |

### 13.1 设备与证书

| 内容 | 位置 | 说明 |
| --- | --- | --- |
| 设备列表、名称、平台、角色、状态、`last_seen`；吊销；改角色；配对令牌发放 | 服务端 `pki` 表 | 这些是实体与动作，不进 settings 文档，也不产生 `settings.updated`（产生 `device.updated` / `device.revoked`）；改角色与吊销需要 approver 签名（decisions P-23，api.md §6.6） |
| 服务端证书与 CA 指纹（公开信息）、客户端证书序列与到期 | 服务端 | 页面展示 |
| 两把设备私钥（Keychain / Secure Enclave / TPM，不可导出）：mTLS 客户端密钥；审批签名密钥（每次使用需用户在场，P-47） | **仅本机** | I6；页面只显示两把公钥的指纹 |
| 本机客户端证书、已固定的离线根 CA 指纹（P-64）、服务端地址 | **仅本机**（`desktop-shell`） | 证书在剩余 1/3 有效期时自动续期（`POST /v1/devices/self/renew`）；页面也可手动触发续期 / 重新配对 |
| 证书有效期、配对令牌 TTL（design.md 示例 30 天 / 10 分钟） | 待定 | 是否做成运行期设置见 OQ-18 |

### 13.2 外观

全部存本机，由 `desktop-shell` 写入应用配置目录，`desktop-ui` 经 IPC 读写 **[提议]**（不用 WebView 的 localStorage，以免被清理）。不进事件日志、不随项目、不跨设备同步（OQ-22），丢失无业务后果，**不得存放任何凭据或配置键**。

| 项 | 说明 |
| --- | --- |
| 主题 | 浅色 / 深色 / 跟随系统 |
| 密度与字号 | D-11：紧凑、工程化 |
| 语言 | |
| 气泡边框颜色 | `agent_kind`（planner / worker / utility）→ 颜色（D-12） |
| 本机系统通知开关 | 只决定**本机是否弹窗**；服务端 `notify.channels.desktop` 决定是否发出，两层都开才会弹 |
| 布局记忆 | 面板宽度、折叠状态等 |

## 14. 开放问题

骨架没有定，本文给出了临时处理方式的一律已标出；**实现者不得自行拍板**，需要时开 Issue。

| # | 问题 | 本文当前处理 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | ~~谁能改设置、签名放在哪、M1 过渡~~ | 已决：见 decisions.md P-18、P-23（§2.5） | — |
| OQ-2 | ~~`models` 直接调用时谁持有 key~~ | 已决：见 decisions.md P-16（一律经 Egress） | — |
| OQ-3 | 模型能力（是否支持工具调用、支持的 effort 取值）的来源：内置表、配置声明还是 provider 探测？ | V04 / V05 假设 `models` 提供能力表；未知模型的处理未定 | `models`、`api` 校验 |
| OQ-4 | Worker 用 API key 还是订阅登录（Q-03） | 只建模 `api_key` | `models`、`worker`、`egress` |
| OQ-5 | worker 的降级能否在沙盒内跨 provider 切换，取决于 `WorkerAdapter` 能力 | V06：适配器不支持则拒绝 | `worker` |
| OQ-6 | "关键节点"的定义：data-model 的 Node 没有 `critical` 属性；推理深度随 `level_effective` 还是 `level_cap` | `critical` 由图模板标记；取 `level_effective` | `engine`、data-model |
| OQ-7 | ~~预算口径与 token 加权~~ | 已决：单位见 decisions.md P-22；加权见 decisions.md P-44、P-70（`levels.token_weights`，§4） | — |
| OQ-8 | ~~Ultimate 的预算作用域与 `concurrency` 含义~~ | 已决：见 decisions.md P-22 | — |
| OQ-9 | ~~`forge_writes_per_hour` 与 `limits.forge_writes` 口径~~ | 已决：见 decisions.md P-22（累计数进台账，按小时限速由 Outbox 令牌桶执行） | — |
| OQ-10 | 运行中能否给交付单元追加预算？预算耗尽上报后用户有哪些处置？是否需要"全局暂停 / 紧急止损"（属控制动作还是配置）。预算模板在创建时按 `level_cap` 固定、之后调上限也不变已决：见 decisions.md P-57；单个交付单元的暂停 / 继续已决：见 decisions.md P-56；预算耗尽的处置选项已决为 `raise_cap` / `abandon`（P-79），`raise_cap` 追加多少与记什么事件待决（decisions.md Q-11） | 本文不提供追加预算与全局暂停 | `engine`、`desktop-ui` |
| OQ-11 | 项目能否放宽：提高预算、换图模板、新增 egress 分组？语言包仓库组是默认全开（项目收窄）还是项目按需选（需要"项目可选分组"标记）？ | 一律只收紧 / 只收窄；全局默认档案需覆盖各项目所需 | `egress`、`sandbox` |
| OQ-12 | 危险动作是否允许 `auto`（architecture §7.4 与 design.md 冲突）？`force_push` 的目标范围（仅 Gantry 创建的分支？）。动作 id 词表已决：见 decisions.md P-28 | 取保守：前七个 id 不允许 `auto`；目标范围未建模 | `policy`、`forge`、`engine` |
| OQ-13 | M1 与 P-11 的过渡、M1 的 merge 签名已决：见 decisions.md P-18、P-19。仍待定："依赖清单"的权威清单；是否纳入 `.github/actions`、CODEOWNERS 等 | 清单为 [建议] | `policy` |
| OQ-14 | 守护分流以 guardian.md 决策表为唯一来源已决：见 decisions.md P-60；不自动降档已决：见 decisions.md P-57。仍待定：design.md 的"后台档位可独立配置"、CI 失败次数作为加码条件 | 见 §5.3、§6.1 | `guardian`、`engine` |
| OQ-15 | 是否按 `severity` 路由通知（枚举已决：见 decisions.md P-22；桌面系统通知由 `severity ≥ warn` 的 `inbox.raised` 触发已决：见 decisions.md P-63）；是否需要免打扰时段 | §9 暂不支持其他按 severity 的路由 | `notify` |
| OQ-16 | 沙盒出网档案与资源限制是否随节点角色 / 档位不同（`sandbox_ro` 角色不需要包仓库；design.md："资源与档位预算联动"）。依赖里的 GitHub 源见 decisions.md Q-08 | 单一 `default_profile`、单一全局 `limits` | `egress`、`sandbox` |
| OQ-17 | ~~`settings.updated` 进 data-model、`default_level` 的归属~~ | 已决：见 decisions.md P-21、P-22 | — |
| OQ-18 | 证书有效期、配对令牌 TTL 等 `pki` 参数是否是运行期设置 | 不收录 | `pki` |
| OQ-19 | 页面分组已决：见 decisions.md P-31；设备与证书（M1）、外观（M3）已决：见第二轮裁决 C28。仍待定：GitHub App / 沙盒与出网 / 通知 / 项目页的里程碑 | §13 标"未排期" | `desktop-ui`、milestones |
| OQ-20 | 稀疏存储下，升级改变内置默认值会静默改变未显式设置的键的行为；`schema_version` 只在事件上盖章、文档里没有，迁移策略未定 | §2.1 提议稀疏存储 | `core`、`deploy` |
| OQ-21 | 出网日志、审计日志的保留期 | 不收录 | `egress`、`policy` |
| OQ-22 | 本机偏好是否需要跨设备同步 | 否 | `desktop-shell` |
| OQ-23 | ~~"密钥模块"未登记在模块地图~~ | 已决：见 decisions.md P-38（`secrets` 模块）；Egress 凭据的转交见 decisions.md P-50 | — |
| OQ-24 | ~~Egress 注入机制~~ | 已决：见 decisions.md P-16（反向代理） | — |
| OQ-25 | 本文对 api.md 的诉求 A1–A9（§2.7）已被采纳，仍待定：(a) resource 从任务清单点名的三个扩到八个是否接受；(c) 没有独立的版本列表 / 回滚路由，历史取自事件流；(d) `catalog` / `status` 结构（§2.8）是否够用。(b) 已决：见 decisions.md P-22；`status` 字段名统一为 `applied_rev` 已决：见第二轮裁决 C26 | 以 api.md 与本文当前文本一致为准 | `api`、`desktop-ui` |
| OQ-26 | `forge.merge.method`（项目级合并方式）是否做成设置键：目前是 forge 内部常量（`webhook_secret_previous` 已决：见 decisions.md P-29） | 不收录 `forge.*` | `forge`、`deploy` |
| OQ-27 | ~~档位条目键的大小写~~ | 已决：见 decisions.md P-71（配置里的档位键一律小写，`levels.none.daily_tokens`；线上 `Level` 值仍首字母大写） | — |

## 补充范围（已批准（2026-10-27，用户统一批准））

`ConfigView.injection` 非空视图字段见 [config-view-injection.md](config-view-injection.md)；settings 读写契约与 V10/V11/V13 不变。
