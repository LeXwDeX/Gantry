# Claude Code stream-json 录制

由本机 `claude 2.1.303` 实际输出，不调用真实模型：模型 base URL 指向本地脚本化的 Messages API 桩，
第一轮返回一次 `Bash` 工具调用（`echo CANARY-tool-output-77`），第二轮返回最终信封。
参数与适配层一致：`-p --output-format stream-json --verbose --setting-sources user --strict-mcp-config
--mcp-config '{"mcpServers":{}}' --permission-mode dontAsk`，临时 `HOME`。

| 文件 | 情形 |
| --- | --- |
| `success.jsonl` | `system/init` → assistant 文本 → assistant `tool_use` → user `tool_result` → assistant 最终信封 → `result/success` |
| `max_turns.jsonl` | 桩始终返回 `tool_use`，`--max-turns 5` 耗尽 → `result/error_max_turns` |

唯一的改动：本机临时目录路径替换为 `/gantry/work`、`/gantry/home`。
固定的 CLI 版本升级时必须重新录制并更新黄金断言（worker.md §12.G）。
