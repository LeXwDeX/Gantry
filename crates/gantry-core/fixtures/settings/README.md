# settings 夹具

`global_example.json` 与 `project_example.json` 是 config.md §12 示例的**精简原样**引用：

- 全局示例覆盖八个 resource、一个模型凭据、一个私有仓库凭据与 GitHub App 凭据；
- 项目示例覆盖 models / levels / policies / guardian / sandbox / egress 的"只收紧"键；
- 两份都不含任何密钥值，只有 `SecretRef` + `present`。

这些文件用于 `tests/settings.rs` 的合并、clamp、`relaxes[]` 与 V01–V03 回放。
它们不是运行时默认值；默认值在 `crates/gantry-core/src/settings/defaults/`。
