# M0 实施与验收边界

本次工作只交付后端开发基线、共享契约与测试替身，不修改 `apps/**`，不实现 M1 的真实业务，也不提交或推送 Git。

## 权威与未决项

遵循 `decisions.md` > 架构不变式 > `interfaces/` > `modules/` 的优先级。类型以 `data-model.md` 为准；客户端路由、请求和响应以 `interfaces/api.md` 为准；设置以 `interfaces/config.md` 为准。

- `core::ports` 包含 core 规格 §6.10 的 16 组端口。写操作传递 `ActorCtx`，不重新引入 `WriteCtx`、`ActorRef` 等别名。
- 文档已列出的提议默认值可用于 M0，但保留其提议性质。不得新增路由、事件类型、配置键或裁定 `decisions.md` 的待决事项。
- `core` §7.5 仍有事件数据形状缺口。未被权威契约或提供方明确规定的字段不得猜测。对未定事件保留事件名，校验明确拒绝未定数据，生成拒绝型 schema 并注明依据；不能据此声称完整事件契约已经冻结。
- `core` §13 的未定状态边只实现明确列出的边，其余拒绝。恢复暂停和 PR 重开需要显式上下文，不能通过纯状态值猜测原因。
- `egress-control.md` 与模块规格冲突时，控制面契约优先。Rust 名称可使用模块命名空间消除重名，但线上字段不变。
- M0 设置逻辑包含合并、收紧、放宽差分、权限要求与 V01–V03。V04–V22 的真实模块校验不在本次范围。

## 实施顺序

1. 并行交付 OpenAPI、夹具和 Mock API，以及 Rust 基础领域类型。
2. 完成协议、设置、事件和端口类型，交付类型生成的机器可读契约。
3. 完成全部指定替身、内存事件存储和示例测试。
4. 执行格式、静态检查、测试、schema 漂移检查和真实 HTTP 回放验证。
5. 独立终审和对抗性复核后，记录完成项、阻塞项与未验证项。

## 验收清单

- Rust workspace 可执行 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-features`。
- `core` 不含 IO、运行时依赖或不安全代码；ID、时间、严格 JSON、规范化、状态机有边界测试。
- `cargo xtask schemas --check` 检查生成物漂移；schema 通过元校验、夹具验证及 OpenAPI 路由和实体一致性检查。
- Mock 的路由与响应形状取自同一份 OpenAPI，不在实现中复制另一套路由表；事件流支持游标、过滤、心跳和纪元号。
- 每种 M0 替身至少一个示例测试；端口实现可由编译器检查。测试替身不伪装为真实业务实现。
- `Store::open_in_memory` 支持事件日志、事务、投影、Outbox 与游标的最小一致性行为。
- 前端接入说明给出启动命令、夹具标识、请求示例、事件续传和明确的模拟边界。

## 当前阻塞

完整事件契约冻结被 `core` §7.5 的未定数据形状阻塞。已尝试向用户确认处理方式，但自主目标执行期间交互提问不可用。继续实施不受影响部分，保留缺口，不把默认建议当作用户裁定。

## 集成验收结果（2026-10-25）

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`：通过。
- `cargo test --workspace --all-features --locked`：266 个测试通过，0 失败
  （core 215、fakes 30、store 20、xtask 1）。
- `cargo xtask schemas --check`：0 failure、0 warning；151 个生成 schema 无漂移。
  76 个文档块 = 49 完整验证（schema + Rust）、20 部分验证（仅 schema；示例省略字段）、
  7 显式 skip；skip 全部在 `unverified.json` 登记。实现漏项没有 warning 类别，
  未登记的新缺漏直接失败。
- `npm test --prefix tools/mock-api`：19 个测试通过。
- `http_framing` 与 `runtime_fakes`：此前集成记录为各连续 50 次通过；本次 workspace 运行各 10/11 项通过。含 150ms 延迟发送、分片 body、非法/冲突 Content-Length 与不完整 body。

集成阶段修复了 `InboxKind`、`StageStatus`、`DeliveryOrigin`、`Exec`、`IssueCommentOn` 的线上名称，补齐 `ports` 的 `validate_raise_kind` 与 `Disposition` 再导出，修复 `gantry-fakes` 的借用、`Clone`、`Ord` 和 clippy 问题，并让 OpenAPI 的 `Approval.action`、`BudgetLedger.config_rev`、`delivery.stage_changed` 与 core 对齐。夹具映射改为独立的 `workflows-list`、`node`、`device-renew` 和 `settings-status`，避免一个夹具被不同响应形状复用。

本轮继续修复了以下实现缺陷：

- `OutboxAction.payload` 由 `serde_json::Value` 改为按 `kind` 严格解码的 `ActionPayload`，并生成 `schemas/forge/outbox-action.json`；store 入队与所有调用方改用强类型。
- `OutboxAction` 的 schema 从“`kind` 枚举 + payload `anyOf`”改为 14 个顶层 `oneOf` 分支；每支绑定 `kind` 常量与对应 payload 子 schema，kind/payload 错配由 schema 拒绝。顶层仍容忍未知字段，保持实体输出兼容。
- 新增 `core::ports::OutboxEnqueueEnvelope` 与 `schemas/forge/outbox-enqueue-envelope.json`，准确验证 forge-actions §2.1 的入队信封（含 `enqueued_by`）；`OutboxAction` 仍不含该字段，`ActorCtx` / `EnqueueRequest` 签名未变。
- `config.md §12` 的全局与项目聚合文档按 8/6 个 resource 逐资源验证：`xtask` 调用生产 `resource_schema`，再用对应 `*Patch` 类型反序列化；不生成新 wrapper，也不为凑完整类型强加字段。catalog 示例仍标 `partial`。`*Patch` 刻意不实现 `Serialize`（凭据写入类型不应回写 secret），因此 G-07 的 Rust 证据只覆盖反序列化；schema 由同一批生产类型派生。
- `Proposal.params` 按 `kind` 收窄为 4 个强类型变体；`ProposalsMode` 序列化为 worker-protocol §4.2 的 `"forbidden"` / `{allowed_kinds,max}`，移除 G-01/G-02/G-14 的 skip。
- `delivery.stage_changed.stage_status` 改为必填，依据 data-model §5.2；OpenAPI、store 投影、事件夹具和负例同步。
- `inbox.raised` 按提供方 notify.md §7.3 统一为唯一 `{item, supersedes?}` 形状；删除扁平 `InboxItem` 的 `untagged` 兼容，同步事件夹具、OpenAPI、schema 与负例，不新增协议。
- 两个 HTTP 回环替身的监听连接先解除 `O_NONBLOCK` 再读写；抽出共享 HTTP 请求读取器；重复/非法 Content-Length、非法 Transfer-Encoding、缺 method/path、body 未读满一律返回 400；真实 IO 错误仍向上返回。
- store 拒绝重复 `delivery.created`，比对 `settings.updated.base_rev` 并在失败时回滚，补充项目级 settings 读取；新增对应测试。
- 未验证夹具改为显式 `unverified.json`：只允许 `example` 与 `contract-undefined`；没有 `implementation-gap` 类别，实现漏项必须修复。统计按完整验证 / 部分验证 / skip 分开报告。

仍未完成（不声称完整 M0 冻结）：未定事件数据形状（`core` §7.5，14 个，含 `outbox.enqueued` 的 `data`）、`ConfigView.injection`、forge drift 的其余 8 种形状，以及两处原文示意错误（egress 25 位 request_id、Message 示例 id）。入队信封（G-06）与 config 聚合示例（G-07）已按真实字段/资源验证，但 `outbox.enqueued` 事件 `data` 仍未冻结。OpenAPI 与 core 实体仍存在视图级差异：OpenAPI 可省略部分 core 可选字段，`required` 与 `additionalProperties` 不完全相同；`xtask` 将这些列为人工核对信息，未把它们宣称为完全一致。
