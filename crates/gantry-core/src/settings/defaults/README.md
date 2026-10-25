# settings 内置默认

这些 JSON 随二进制编译（`include_str!`），来源是 [docs/interfaces/config.md](../../../../../docs/interfaces/config.md)。
数值保留文档里的 **建议值，待调优** 性质，不是经过真实任务验证的定论。

- `models.*.provider` / `model` 没有默认值：未配置时由 `engine` 拒绝创建交付单元。
- 默认值不含任何真实密钥；`SecretRef` 只出现在用户配置里。
- `sandbox.base_image` 使用全零占位 digest；部署时由安装引导写入真实镜像摘要。
- `egress.profiles.default` 只含 `model_api`，是 M1 的最小权限起点（config.md §8.2 允许）。
- V04–V22 的语义校验不在 M0；这里只保证 JSON 能解析成对应的强类型，并通过 V01–V03。
