# M0 复核修复清单

本文件记录实现期复核发现，不修改权威契约，也不代表验收已经通过。

## 首轮独立验证

- 格式与静态检查通过；Rust 测试首次 223 项通过，但带锁文件的复跑发现 `FakeGitHub` HTTP 测试间歇性 `ConnectionReset`。必须找出分帧或关闭连接的根因，不能用重试掩盖。
- Mock 测试 19 项通过，真实 HTTP 分页、事件回放、游标错误与响应头检查通过。
- schema 检查通过了生成物漂移与元校验，但仍有 45 个未验证文档块和 14 个拒绝型事件 schema。这不等于完整契约一致性验收通过。
- 已定义的 `OutboxAction.payload` 未接入强类型；`Proposal.params` schema 未约束与 `kind` 的对应关系；`ProposalsMode` 序列化与 Worker 契约不符。
- 替身与 `Store::WriteTx` 的事务互操作尚未实现，不能靠各替身自己的事务测试代替。

上述实现缺陷正在修复，最终以修复后的验证和复审结果为准。

## 设置模块独立审查

审查结果：`BLOCKING`，两个 P1。该审查只读，未重跑测试。

1. `settings/rules.rs` 对 `sandbox.prewarm`、`sandbox.io` 使用不可比较规则，但两者不是仅 approver 可写的键。扩大数值后 `relaxes[]` 为空，错误地不要求签名。修复应覆盖全局数值放宽与收紧的差分、权限回归测试。
2. `settings/clamp.rs` 在对象规则中整体回退全局值。示例：全局 `nodes=10, rounds=4`，项目旧覆盖 `nodes=20, rounds=2`，预期生效值为 `nodes=10, rounds=2`，实际却是 `nodes=10, rounds=4`。修复应保留未冲突字段的收紧，并覆盖预算、沙盒限额、动作、路径规则和出网档案。

其他需要核对的边界：含 `null` 的项目输入不能使已收紧的全局值回退到更宽的内置值；`has_direction` 应识别受规则约束的子路径；有明确定义方向的敏感键也应准确记录放宽差分。

`sandbox.devcontainer.mode=off` 的安全语义尚不清晰。本轮不因名称猜测其方向，不把审查建议当成用户裁定。

## 待终审核对的假设

- `ConfigView.injection` 的控制面示例只有空对象，但 `config.md` §8.1 已定义设置侧的注入结构。控制面是否同形不能直接假定；应核对提供方模块的转换定义。即使维持未定，Rust 端不能接受任意 `Value` 并通过 `Debug` 或序列化泄露其中的凭据，而仅靠生成 schema 拒绝。
- OpenAPI 示例的形状验证与 Rust 类型往返验证是两种证据。统计必须分别列出完整验证、部分验证和跳过，不能把只校验 OpenAPI 的示例计作完整 Rust 契约验证。

## 设置模块修复（第二轮）

本轮只修 `core::settings` 的规则比较、逐键 clamp、`null` 回退与 `has_direction`，未改 `docs/interfaces/`、`schemas/` 或其他模块。

- **P1-1 放宽方向**：`sandbox.prewarm`、`sandbox.io` 的数值上限改为递归数值比较；全局调大 `pool_size` 或任一 `io.*` 上限会记录 `relaxes[]` 并要求 approver 签名，调小不要求。`sandbox.runtime` 改为 `runsc → runc` 判定放宽。`egress.groups` / `egress.injection` 补上"新增分组 / 注入项或新增主机 = 放宽"。
- **P1-2 逐键 clamp**：`levels.*.budget`、`sandbox.limits`、`policies.actions`、`policies.paths`、`policies.escalation`、`policies.approval`、`policies.patch`、`guardian.triage`、`guardian.no_progress`、`egress.profiles` 不再整对象回退。示例：全局 `nodes=10, rounds=4`、项目 `nodes=20, rounds=2` 现在得到 `nodes=10, rounds=2`。路径规则的 `globs`、处置分别比较；`egress.profiles.<name>.groups` 在项目新增分组时整组回退到全局（与既有 clamp 语义一致），未新增分组时保留项目收窄。
- **`null` 回退**：`effective` 把存储文档中的 `null` 当作"未设置"再合并；项目 `null` 不能把已收紧的全局值带回内置默认，项目整体 `null` 等于不覆盖。存储层含 `null` 仍应由调用方按 V01 拒绝。
- **`has_direction`**：改为查最近的规则并排除 `Incomparable`，因此 `levels.high.budget.nodes`、`sandbox.prewarm.pool_size`、`sandbox.io.progress.max_lines`、`policies.paths.x.globs`、`egress.profiles.default.groups` 等子路径可被识别。
- **未翻转**：`sandbox.devcontainer.mode=off` 的安全含义仍待决，现有 `safe_subset → off = 放宽` 方向保持不变。`sandbox.prewarm.enabled` 的布尔方向、`egress.default_profile` 的跨档案比较、`sandbox.base_image` 的内容比较没有契约定义的机械方向，仍按 `Incomparable` / 仅 approver 处理。

验证：新增 `crates/gantry-core/tests/settings_regressions.rs`（16 项，先失败后通过）；`cargo test -p gantry-core` 通过；`cargo clippy -p gantry-core --all-targets -- -D warnings` 通过；`cargo check --workspace` 通过（`gantry-store` 有一条既有未使用函数警告，与本轮无关）。
