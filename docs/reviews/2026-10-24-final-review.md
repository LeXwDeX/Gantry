# 终审记录（2026-10-24）

> 范围：AGENTS.md、README.md、docs/ 下全部设计文档（docs/reviews/ 旧文件只读，prototype/ 与 docs/design.md 未改）。分两步：先落实最后一批裁决（[decisions.md](../decisions.md) P-71–P-82、Q-09），再做独立的批判性复核并直接修复能确定的问题；需要产品负责人决定的问题记入 decisions.md"待决"（Q-10–Q-12），本文只给推荐，不拍板。

## 1. 方法

| 检查 | 做法 | 结果 |
| --- | --- | --- |
| A 一致性 | 脚本复查：相对链接与锚点（按 GitHub 规则生成锚点）、表格每行列数、所有 `json` 代码块能否解析、全文出现的 `/v1/…` 路由与 [api.md §4.1](../interfaces/api.md) 路由总表对照、事件名与 data-model 事件目录对照；另按清单检索已废弃的标识符（`levels.None`、`WriteCtx`、`ActorRef`、`DeviceActor`、`RequestCtx`、`keep_scope`、`rework`、`continue_without`、`check_approval`、`FakeSandboxDriver`、`FakeSecrets`、`x-gantry-delivery`、`gantry:<job_token>`、`job_token_invalid`、"47 条 / 12 条"等） | 修复后：0 断链、0 列数不符、0 非法 JSON；路由 48 条前后一致；废弃标识符只在已决问题的"原问题"与裁决行里作为历史出现 |
| B 模块许愿 | 对 20 个模块逐一按 AGENTS.md"领取模块"流程走一遍：读模块规格 → 读它在"依赖 / 输入 / 输出"里引用的契约与 data-model 章节 → 列出"开工前还缺什么"（签名对不上、依赖不在 `core::ports`、替身缺失、词表不一致、待决问题挡路） | 见 §4 |
| C 泳道并行 | 对每个里程碑的每条泳道：① 只依赖 M0 的契约与替身吗；② 它调用的每个跨泳道接口都在 `core::ports` 且有 M0 替身吗；③ 里程碑退出标准是否都能落到某个模块的验收项 | 见 §5 |
| D 不变式 | I1–I8 各找负责模块与能证明它的验收用例 | 见 §6 |
| E 残留 | 过时措辞、已决却仍标"未决"的开放问题、互相冲突的默认值、示例与规格不符 | 并入 §3.1 |

## 2. 最后一批裁决的落实

| 裁决 | 内容 | 落实位置 |
| --- | --- | --- |
| P-71 | 配置档位键一律小写（`levels.none.daily_tokens` 等），线上 `Level` 仍首字母大写 | config §1.3 / §2.2 / §4 / V07 / §12 示例 / §13、data-model、core（`LevelKey` 含 `None`，`key()` 不再返回 `Option`）、engine、models、conversation、architecture；config OQ-27、core OQ-15 关闭 |
| P-72 | 包仓库 `CONNECT` 用 `Proxy-Authorization` 携带 `job_token`（`HTTPS_PROXY=http://job:<job_token>@…`） | egress-control §0 / §3.1 / §4.1（`connect_proxy_url` 含令牌）/ §5（407 / 403）/ §8、egress、sandbox、worker、worker-protocol §2.2 / §2.3、config §8、deploy 端口表；egress-control OQ-1 的 `CONNECT` 部分关闭 |
| P-73 | "令牌只能从沙盒网段使用"标"需核实"，安全底线是能力令牌 | egress-control §3.1 / §4.7（`source_check: unverified`）/ §8、egress、sandbox N4 与验收、deploy、worker-protocol、architecture §9、milestones M1 退出标准改写为"核实并记录"；egress-control OQ-2 转为本裁决 |
| P-74 | `svc_token` 每个 `gantryd` 进程一枚；每次调用带 `X-Gantry-Ledger`、`x-gantry-tier`、`x-gantry-call-id`；令牌吊销时对账 | egress-control §3.2 / §4.3 / §4.5（按 `svc_token_id` 查询）/ §5 / §8 重写、egress（`rotate_service_token`、`svc_token_usage`）、models（请求头、轮换、对账步骤）、core 端口方法、data-model 台账 ID、deploy 端口表；egress-control OQ-3 关闭 |
| P-75 | `abandoned` 只在 PR 关闭后重开时可恢复；手机可续期证书；内部动作审批有效期用 `policies.approval.ttl_s` | data-model、engine T15（原已一致）、api §2.3、config §5 |
| P-76 | 手机档案配对可省略 `approval_pubkey`，永不能成为 approver（`409 mobile_cannot_approve`） | api §1.3 / §2.2 / §2.3 / §4.10 / §7、pki（配对、`set_role`、`recover --promote`、验收）、api 模块、mobile、desktop-ui 设备页、data-model、milestones M4；mobile OQ-M12 关闭 |
| P-77 | 新路由 `PUT /v1/devices/self/push-token`（M4，手机可用） | api §2.3 / §3.3 / §4.1（#48，合计 48 条、手机 13 条）/ §4.10 / §7、api 模块、notify（`InboxPort::register_push_token`、`push_tokens` 表）、mobile、desktop-shell 路由分类（新增"桌面不用"类）、config §9、deploy、milestones M4；api OQ-2、notify N-11(a)、mobile OQ-M2 的路由部分关闭 |
| P-78 | 统一写上下文 `core::ports::ActorCtx` | core §6.10 定义；api 模块 §6.3、engine、conversation、notify、policy、repo 的签名全部替换；api OQ-A7、conversation OQ-12(b) 关闭 |
| P-79 | 上报项 `reason_code` 与选项词表固定在 data-model | data-model 新增词表一节；engine §7.11、guardian §7.9（评审争议改为 `decision_needed` + `fix` / `keep`）、notify（校验与手机限制）、conversation 卡片、desktop-ui、mobile、api §4.1 / §4.8；engine OQ-13、guardian G-OQ-14、conversation OQ-6、desktop-ui OQ-U8 ③、mobile OQ-M9 关闭 |
| P-80 | `revocations.journal` 只追加、同步复制到备份目标、按时间戳重放、回滚不删 | pki §7.6 / 配置 / 失败处理、deploy（`backup.journal_target` 必填，替代异步 `journal_hook`）、api §4.10、architecture §6、data-model；pki OQ-K13 关闭 |
| P-81 | `MemorySecrets` 进 M0 替身 | milestones 替身表、core §6.9、secrets M0 行、forge / egress / pki 替身引用；secrets OQ-10 关闭 |
| P-82 | 桌面端消息带 `level_cap` 且 `@` 已有交付 = 调整该交付上限 | api §4.4 / §4.5 / §7、conversation §7.3、engine `set_level_cap`、data-model；conversation OQ-3 的上限部分、desktop-ui OQ-U11 关闭 |
| Q-09 | 二进制资源供给（待决） | decisions 待决表；worker-protocol OQ-4 引用 |

## 3. 问题清单

### 3.1 已修复

| # | 类别 | 问题 | 修复 | 文件 |
| --- | --- | --- | --- | --- |
| R-01 | 契约冲突 | 补丁审批 `patch_apply_protected` 的消费点两套说法：P-54 / data-model / api / forge-actions 写"`repo` 在提交前消费"，policy / repo / engine 写"`repo` 只读不消费、由承载提交的 `push_branch` 在 `forge` 执行前消费"；`target` 形状也不同（`{branch, path_rules}` 与 `{attempt_id, rules, paths}`） | 以 P-54 为准：`PolicyPort::check_approval` 改为 `consume_patch_approval`（`consumed_by = att_…`，崩溃重放 `AlreadyConsumedBySameAttempt`）；`repo` 提交算法加第 4a 步；`decide_action` 第 3 步改为只核对"已被该尝试消费"；`target` 形状对齐 data-model | policy、repo、engine、core |
| R-02 | 模块与契约不符 | egress 模块与 egress-control 契约有 8 处分歧：`svc_model` 路径与鉴权（`/s/<provider_id>` + `Bearer` 对 `/s/<svc_token>/<provider_id>`）、`CONNECT` 用户名（`gantry` 对 `job`）、私有仓库路由（`/j/<job_token>/registry/…` 对 `/r/<job_token>/…`）、错误码（`job_token_invalid` 对 `token_invalid`）、审计规则名、`issue_job_token` 重放语义（返回同一令牌对重签）、Egress 重启后 `svc_token` 是否轮换、回环端口（契约 8090 对 deploy / 模块 8082） | 模块全部对齐契约；回环端口按 deploy 端口表统一为 `127.0.0.1:8082`（`egress.listen.svc_model`），改契约的 [提议] 值 | egress、egress-control、models、deploy |
| R-03 | 端口放错位置 | 跨泳道接口不在 `core::ports`，泳道并行时无法编译：`DeliveryControl` / `ConversationService` / `WorktreeLocks` / `PrRefresher` 写在 `api` 模块，`HealPort` / `ClassifyPort` 写在 `engine` crate，`WorkerAdapter` 在 `worker` crate，`SecretSource` / `SecretSink` 在 `secrets` crate | 按 P-51 的原则（调用方与提供方分属不同泳道即进 `core::ports`）补入 core §6.10，并补 M0 替身 `FakeEngine`、`FakeConversation`、`StubWorkerAdapter`、`MemorySecrets`，`FakeRepo` 加 `WorktreeLocks`、`FakeForge` 加 `PrRefresher`；同泳道接口（`EngineApi`、`ModelRouter`、`ApprovalService`、`DeviceDirectory`）不进 | core、milestones、engine、guardian、worker、secrets、forge、api、architecture、AGENTS.md |
| R-04 | 时序矛盾 | `WorkerAdapter::env(job, &JobTokenGrant)` 由 `engine` 构造 `SandboxSpec.exec` 时调用，但 `job_token` 要到 `sandbox` 建容器前才签发；而 sandbox 又写"经 `WorkerAdapter::env` 写入"，与"`sandbox` 不依赖 `worker`"矛盾 | 拆成 `env(job)`（无令牌）与 `token_env()`（只给变量名）；`ExecSpec` 增加 `token_env`，`sandbox` 签发后按名字填值 | worker、sandbox |
| R-05 | 形状不一致 | `notify` 的 `InboxSummary` 是 `{total, by_project[{…, by_severity}], no_project}`，api 契约是 `{total_open, by_project[{project_id?, open, max_severity}]}` | `notify` 对齐契约，验收改为"与 api §4.8 示例逐字段同形" | notify |
| R-06 | 词表分裂 | 选项 id 各写一套：engine `keep_scope` / `continue_without` / `retry`、guardian `rework`、`spec_needed` 有时带选项、`budget_exhausted` 无选项 | 按 P-79 统一；重试失败节点只走控制路由；`github_state_changed` / `approval_denied` 的 `continue` / `abandon` 保留并标"[提议，P-79 未覆盖]" | engine、guardian、notify、conversation、desktop-ui、mobile、api |
| R-07 | 形状不一致 | conversation 的 `escalation` 卡片带 `options[]`，data-model 的 `Card.escalation` 只有 `inbox_id` | 卡片只带 `inbox_id`，按钮取 InboxItem 的 `payload.options`；`spec_needed` 用 `choice` + 空选项（定稿，不再"暂用"） | conversation |
| R-08 | 排期不一致 | api 模块把 #8（放弃交付）排在 M2，milestones M1 已要求放弃 | 改为 M1 | api 模块 |
| R-09 | 计数过时 | 路由"47 条 / 手机 12 条"散落在 api 模块、desktop-shell、mobile、policy | 统一为 48 / 13；desktop-shell 路由分类增加"桌面不用"类 | 同左 |
| R-10 | 过时表格 | worker-protocol §8.6 把 config §7 早已定义的键仍列为"新增，须契约 PR 回写"，且键名是示意名 `worker.*` | 改为"用途 → config 键 → 默认值"对照表 | worker-protocol |
| R-11 | 映射错误 | config 说 `levels.*.graph_template` 对应 `Workflow.template`；data-model 定为族名记在 `BudgetLedger.template`，`Workflow.template` 是 `<档位键>.<阶段>`；api §4.7 示例用了族名 | config 映射与 api 示例（`medium.implement`）改正 | config、api |
| R-12 | 编号 | engine §1 职责编号 12、14、13 乱序 | 调整为顺序 | engine |
| R-13 | 过时示例 | core 的 `known_failure` 示例引用 api 早已改正的 `Workflow.status: "finished"` | 改为"终审时没有已知不符项" | core |
| R-14 | 命名残留 | `FakeSandboxDriver`（已统一为 `FakeSandboxService`）、`FakeSecrets`（应为 `MemorySecrets`） | 统一 | milestones、worker-protocol、sandbox、forge、egress |
| R-15 | 与裁决冲突 | deploy 用异步的 `.path` 单元 + `journal_hook` 做吊销日志异地同步，P-80 要求同步复制、失败即吊销失败 | 改为必填的 `backup.journal_target`，由 `pki` 同步追加；验收与恢复演练同步修改 | deploy、pki |
| R-16 | 未核实的保证 | M1 退出标准与多处验收把"令牌不能从沙盒网段以外使用"写成硬保证 | 按 P-73 改为"在目标主机核实；不可强制时在 Egress 状态标明，不作为退出阻塞项" | milestones、sandbox、deploy、egress、egress-control、worker-protocol |
| R-17 | 依赖方向 | engine / guardian 写"`HealPort` / `ClassifyPort` 定义在 `engine` crate，`guardian` 依赖它" | 改为 `core::ports`，替身 `FakeEngine` | engine、guardian |
| R-18 | 已决仍开放 | 下列问题已被裁决或其他契约回答，却仍标未决：store OQ-12（`occurred_at` 精度，forge-actions §6.4 已定三位毫秒）、deploy OPS-14 / OPS-22 的相应部分、config OQ-10 的处置选项部分，以及 §2 表中列出的各项 | 标"已决"并指向依据 | 各模块 |
| R-19 | 潜在冲突，先登记 | sandbox 预热池（M2）提议把 `job_token` 经 `inputs/` 文件交给已启动的 worker，违反 worker-protocol §2.2"令牌只出现在两个环境变量里、不写进 `inputs/`" | 新增 sandbox OQ-S15：预热池默认关闭，启用前须契约 PR 修订 §2.2；其余路径不受影响 | sandbox |
| R-20 | 缺口 | `PUT /v1/devices/self/push-token` 的服务端落点没有接口 | `InboxPort` 增加 `register_push_token`（M4，替身只记录调用） | notify、core、api 模块、milestones |

### 3.2 转为待决（产品负责人决定）

| 编号 | 问题 | 推荐 |
| --- | --- | --- |
| Q-09 | 二进制资源如何供给（裁决本批新增） | 第一版一律拒绝并上报；M3 起经审批放行 |
| Q-10 | 第一台配对的设备是手机时系统没有 approver，而引导期在"第一台设备配对"时就结束 | 引导期持续到第一台桌面设备配对，该设备成为 approver |
| Q-11 | `budget_exhausted` 的 `raise_cap` 追加多少、记什么事件 | 每次追加一份该档模板的原值，新增事件 `budget.limit_raised`，operator、仅桌面、不需签名 |
| Q-12 | 结果对象（补丁、`result_ref`）没有读取路由：补丁审批只能"盲签"，`manual_action` 的 workflow 补丁无法下载 | M2 新增 `GET /v1/results/{sha256}`（viewer、仅桌面、大小上限同 `policies.patch.max_bytes`，客户端校验摘要后按不可信文本渲染）；取不到内容时补丁审批单禁用签名 |

另有一项需要契约 PR 而非产品决定：R-19（预热池令牌交付）。

## 4. 模块可实现性结论

"可开工"指只凭本仓库文档与 M0 替身即可写到本模块当前里程碑的验收全部通过；"有条件可开工"指主体可开工，但某个里程碑的部分范围被列出的待决项或核实项挡住。没有"不可开工"的模块。

| 模块 | 结论 | 理由 / 条件 |
| --- | --- | --- |
| `core` | 可开工 | M0 交付物已齐：领域类型、状态机、事件、16 组 `core::ports` 端口与 `ActorCtx`、替身清单；剩余开放问题都有暂用处理 |
| `store` | 可开工 | 事件日志、投影、游标、`log_epoch` 均有形状与验收；`dedup_key` 适用范围已与 `ActorCtx` 对齐 |
| `engine` | 有条件可开工 | M1 可开工；M2 的 `raise_cap` 执行待 Q-11（不影响 M2 退出标准）；M4 父子台账汇总仍是 OQ-18 的暂用处理 |
| `conversation` | 可开工 | 卡片、上限解析、`ActorCtx`、P-82 已定；GitHub 原生入口属 M4 且待 Q-04 |
| `models` | 可开工 | `svc_token`、请求头、对账已定；能力表来源（OQ-1）有可注入实现作暂用 |
| `notify` | 有条件可开工 | M1 / M2 可开工；M4 APNs 的设置键与"直连还是经中继"未定（N-11 (b)(c)、deploy OPS-22） |
| `forge` | 可开工 | 端口与替身齐备；GitHub API 细节标"需核实"属实现期核对 |
| `guardian` | 可开工 | M2 依赖的 `HealPort` / `ClassifyPort` 已进 `core::ports` 并有 `FakeEngine`；上报词表已定 |
| `repo` | 可开工 | 补丁审批消费点已与 P-54 对齐 |
| `sandbox` | 有条件可开工 | M1 可开工，但 rootless Podman 下的若干性质（源地址可见性 P-73、`--unsetenv-all`、gVisor 组合）须在目标主机核实；M2 预热池被 OQ-S15 挡住（默认关闭，不影响退出标准） |
| `worker` | 可开工 | 协议、产物、`env` / `token_env` 拆分已定；Claude Code 无头模式参数为实现期核对 |
| `egress` | 可开工 | 已与控制面契约逐项对齐；网段检查按 P-73 降级为核实项；私有仓库路由（M2）为 [提议] |
| `pki` | 有条件可开工 | M1 可开工；M4 中"第一台设备是手机"的情形待 Q-10 |
| `policy` | 可开工 | 判定、审批状态机、两种消费路径（Forge 动作、补丁）已定 |
| `secrets` | 可开工 | M0 交付 `MemorySecrets` 与 trait（P-81） |
| `api` | 有条件可开工 | M1 可开工（所有被委托接口都有替身）；M2 若采纳 Q-12 需新增结果读取路由 |
| `desktop-shell` | 有条件可开工 | M1 可开工；M2 补丁审批的内容展示待 Q-12（否则只能禁用签名） |
| `desktop-ui` | 有条件可开工 | M1 可开工；M2 补丁差异展示待 Q-12，`raise_cap` 按钮文案取服务端 `label`，不受 Q-11 阻塞 |
| `mobile` | 可开工 | P-76 / P-77 解除了此前两个阻塞（审批公钥、推送令牌路由）；推送载荷有暂用最小集 |
| `deploy` | 可开工 | 端口表、单元、备份恢复（含 `backup.journal_target`）已定；网络隔离性质由金丝雀沙盒核实 |

## 5. 里程碑泳道并行性结论

| 里程碑 | 泳道 | 结论 | 说明 |
| --- | --- | --- | --- |
| M0 | `contract`、`ops` | 可并行 | 交付物见 milestones；端口清单扩充为 16 组，替身 15 种（含 Mock API 与 `TestCA`） |
| M1 | `be-engine` | 可并行 | 跨泳道依赖：`FakeForge`、`FakeSandboxService`、`FakeRepo`、`FakePolicy`、`EgressStub`、`StubWorkerAdapter` |
| M1 | `be-forge` | 可并行 | `FakeGitHub`、`FakeRepo`、`FakePolicy`、`FakeInbox`、`MemorySecrets` |
| M1 | `exec` | 可并行 | `FakePolicy`、`MemorySecrets`、`Store::open_in_memory`；P-73 的核实在泳道内完成，不阻塞 |
| M1 | `sec` | 可并行 | 此前不可并行（`api` 依赖的引擎、会话、人工锁、PR 对账接口没有替身），R-03 后可并行 |
| M1 | `fe-desktop` | 可并行 | Mock API 与 `TestCA` |
| M1 | `ops` | 可并行 | 脚本与单元可独立开发；联调在里程碑末尾 |
| M2 | `be-engine`、`be-forge`、`sec`、`ops` | 可并行 | `guardian` 用 `FakeEngine`；补丁审批消费路径已统一（R-01） |
| M2 | `exec` | 有条件 | 预热池须先解决 OQ-S15；gVisor 默认开启的主机核实 |
| M2 | `fe-desktop` | 有条件 | 补丁审批内容展示待 Q-12 |
| M3 | 全部泳道 | 可并行（`exec` 有条件） | 逃逸检测告警的信号清单未定（sandbox OQ-S13、egress OQ-E12） |
| M4 | `be-engine`、`fe-mobile` | 可并行 | — |
| M4 | `be-forge` | 有条件 | GitHub 原生任务入口待 Q-04 |
| M4 | `sec` | 有条件 | 手机为第一台设备的处理待 Q-10 |
| M4 | `ops` | 有条件 | APNs 设置键与接入方式未定（notify N-11、deploy OPS-22） |

**退出标准的覆盖**：M1 的 10 条与 M2 的 8 条都能落到至少一个模块的验收项（端到端类由 engine"端到端（替身）"、policy 端到端用例与里程碑末尾的跨泳道联调共同覆盖）。两点提醒：① 跨泳道联调（milestones"依赖关系"一节）没有指定负责泳道，建议由 `ops` 在 CI 里组装；② M2"签名批准后才提交推送"在 Q-12 定案前只能以"盲签"通过，形式上满足、实质上不满足 §6.5"界面展示必须与签名载荷一致"的要求。

## 6. 不变式落点（I1–I8）

| 不变式 | 负责模块 | 证明它的验收用例 |
| --- | --- | --- |
| I1 沙盒只计算 | `sandbox`（`egress`、`worker`、`deploy` 协同） | sandbox §12"M1 退出标准：沙盒内读不到任何模型 key""访问 `github.com` 失败"、环境变量白名单逐项比对；egress §12.D 硬性拒绝；worker 环境变量测试；deploy 金丝雀沙盒 |
| I2 GitHub 写入唯一路径 | `forge` | forge §12 webhook 重复投递不重复动作、"创建 PR 成功写库前崩溃"不重复；engine 的 crate 依赖检查（无 GitHub 客户端）；core `IdempotencyKey::derive` 唯一入口 |
| I3 放行唯一决策点 | `policy` | policy §12 `consume_for_execution` 三方 head 一致、同动作重放、`consume_patch_approval`；engine"新推送使审批失效"；M2 退出标准"审批后出现新推送，审批自动失效" |
| I4 状态各有事实源 | `forge`（对账）、`store`（事件日志） | forge §12 外部推送 → `forge.drift_detected(head_diverged)`；store 只追加触发器与去重用例；engine PR 关闭 / 重开用例 |
| I5 外部文本不可信 | `core`（`UntrustedText`）、`models` / `sandbox`（材料包装） | conversation"注入抵抗"、guardian"分类只能收紧"、models 材料包装用例、各客户端纯文本渲染用例 |
| I6 客户端不执行、不持凭据 | `api`（路由面）、`desktop-shell`、`mobile` | api §12 路由白名单"没有接收代码 / 命令 / 补丁的路由"、带 secret 写入后读不到值；desktop-shell IPC 白名单；mobile M-S1 网络层只含 13 条路由 |
| I7 配置与工作分离 | `api`（settings 是唯一配置写入口）、`core`（校验） | api §12"请求体带 `model` 或预算字段 → `422 unknown_field`"；desktop-ui `buildSend` 只发三个键 |
| I8 执行产物先验后推 | `repo`（`policy`、`forge` 协同） | repo §12 补丁闸门 P1–P11 与 `provenance`；forge §12 `push_branch` 的 I8 机械复核；policy `decide_action` 第 3 步 |

## 7. 仍需核实（不是设计缺口，实现时在目标环境确认）

- rootless Podman（slirp4netns / pasta）下 Egress 能否看到沙盒真实源地址（P-73），以及 `--unsetenv-all`、gVisor 与 rootless 的组合（sandbox、deploy）。
- 部分包管理器只在收到 `407` 质询后才发送代理凭据（egress-control §5 的 `407` 行为 [提议]）。
- Anthropic API 的端点白名单、用量字段与头名（egress §7.3 / §7.4），Claude Code 无头模式参数（worker）。
- GitHub App Manifest 流程与 webhook 语义细节（forge、deploy，P-65 已标需核实）。
