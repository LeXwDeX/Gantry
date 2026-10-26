# 事件夹具来源

`cases.json` 是 M0 事件 `data` 的正例与反例集合，按 `EventType::as_str()` 的名称索引。
测试 `crates/gantry-core/tests/events.rs` 读取它，不修改契约文档。

## 来源

| 范围 | 权威 |
| --- | --- |
| 事件名称与信封 | `docs/data-model.md` §5.1–§5.2 |
| `settings.updated` | `docs/interfaces/config.md` §2.3；`docs/interfaces/api.md` §5.1 示例 |
| `delivery.*` | `docs/modules/engine.md` §11（最小字段）；`docs/interfaces/api.md` §4.4、§5.1 |
| `workflow.*` / `node.*` / `attempt.*` | `docs/modules/engine.md` §11 |
| `patch.*` | `docs/modules/repo.md` §11；`docs/interfaces/worker-protocol.md` §8.3 |
| `sandbox.*` | `docs/modules/sandbox.md` §11 |
| `worktree.*` | `docs/modules/repo.md` §11 |
| `message.posted` | `docs/interfaces/api.md` §4.5（完整 `Message`） |
| `inbox.*` | `docs/modules/notify.md` §7.3；扁平 `inbox.raised` 取自 `fixtures/api/events.json` |
| `approval.*` | `docs/modules/policy.md` §6.1、§7.3、§7.5 |
| `outbox.succeeded` / `failed` | `docs/data-model.md` §5.2；`docs/interfaces/forge-actions.md` §2.6、§4.2 |
| `budget.exhausted` | `docs/modules/core.md` §7.5 |
| `device.*` | `docs/modules/pki.md` §7.4.3、§7.5 |
| `forge.*` | `docs/interfaces/forge-actions.md` §6.2–§6.4；结构以 `crates/gantry-core/src/forge.rs` 为准 |

## 已知简化

- `forge.issue_comment.on` 使用 `"issue"`：文档允许 `issue` 与 `pull_request`，但当前
  `ForgeEventKind` 的 `IssueCommentOn` 把 `pull_request` 序列化为 `pullrequest`，
  与文档不一致（见 `docs/m0-contract-gaps.md` §4）。夹具不掩盖该缺陷，只在事件层用
  文档和实现都接受的 `issue`。
- `forge.issue_closed.closed_via_pr` 使用 PR 号 `12`：forge.rs 将该字段定义为
  `Option<u64>`；文档只写"可空"。
- `inbox.raised` 同时提供 `{item, supersedes?}` 与扁平 `InboxItem` 两种形状；二者都已在
  权威或既有夹具中出现，见 `docs/m0-contract-gaps.md` §3。
- `invalid[]` 中 `settings.updated` 的两条反例分别覆盖：SecretRef 位置带 `value`、
  `secret_changes[]` 元素带 `value` / `hash`。
