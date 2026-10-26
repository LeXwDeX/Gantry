# 协议夹具来源

所有夹具取自契约文档的示例，或在本文件注明的等价替换。夹具只用于类型与 schema 测试，不代表真实 Worker / Egress 行为。

| 文件 | 来源 | 说明 |
| --- | --- | --- |
| `forge/actions.json` | forge-actions §3.3（14 个 `payload` 示例） | 逐块按 `kind` 合并；内容原样。 |
| `forge/create_pr_outbox.json` | forge-actions §2.1 | Outbox 信封示例；`payload` 原样。 |
| `forge/events.json` | forge-actions §6.4（`pr_synchronized`、`check_suite_completed`） | 两个完整信封原样。 |
| `worker/job_spec.json` | worker-protocol §4.1 | `job.json` 示例原样。 |
| `worker/result.json` | worker-protocol §5.2 | `result.json` 示例原样。 |
| `worker/manifest.json` | worker-protocol §5.2（`ResultBundle` 清单示例） | 原样。 |
| `worker/proposals.json` | worker-protocol §5.4 | 原样。 |
| `worker/progress.jsonl` | worker-protocol §7.1 | 8 行 JSON Lines 原样。 |
| `worker/outputs.json` | worker-protocol §6.3（`plan`、`review:security`、`arbiter`） | 三个 `structured_output` 示例，按 role 键合并；内容原样。 |
| `egress/job_token.json` | egress-control §4.1、§4.2 | 签发请求、签发响应、`FinalUsage` 三个示例原样。 |
| `egress/svc_token.json` | egress-control §4.3 | 签发请求与响应原样。 |
| `egress/secrets.json` | egress-control §4.4 | 暂存请求/响应、激活响应、`present` 请求/响应原样。 |
| `egress/usage.json` | egress-control §4.5 | 用量分页示例原样。 |
| `egress/config.json` | egress-control §4.6 | 配置视图示例原样。 |
| `egress/status.json` | egress-control §4.7 | `status` 与 `endpoints` 示例原样。 |

## 已知偏差

- `SvcTokenId` 的后缀：文档示例 `svt_01JBX5A2C4E6G8J1L3N5Q7S9UW` 含 Crockford 之外的字母 `L`、`U`。类型按 `^svt_[0-9A-Z]{26}$` 宽松校验，理由见 `src/egress.rs` 注释。`StagedId` 的现有示例后缀合法；同一宽松 pattern 只用于兼容历史值，不表示当前示例非法。

## 未定形状（不生成夹具、不猜测）

- `forge-actions` §6.3：`pr_updated` / `issue_updated` 的 `changed[]` 逐字段“新值”形状未定义；`Rust` 类型只保留字段名。
- `forge-actions` §2.6 / `worker-protocol` §9 的失败子码词表：以字符串承载已知与未知子码，不建封闭枚举。

注：`egress-control` §4.6 的 `injection` 已由 config-view-injection 契约定义，drift 已由 forge-drift 契约定义；本目录仍不为其生成额外夹具，夹具覆盖由事件与 schema 测试承担。
