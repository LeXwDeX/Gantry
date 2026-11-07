# Gantry

> 以 agent 运行时为重心、编排为其二次封装的 AI 开发平台。

Gantry（龙门架）是 [GraphAgent](https://github.com/LeXwDeX/OpenCode-GraphAgent) 与 [SpecGit](https://github.com/LeXwDeX/SpecGit) 的合并继任者：借鉴 GraphAgent 的编排经验与 SpecGit 的"Issue → PR → 验证完成"交付语义，在独立重新设计的运行时上实现，不复用 GraphAgent 的 AGPL 代码。

**状态：路线已调整为 3.0，当前里程碑是 3.0 K · 内核收口。** 2.x 的 M0 统一开发基线已合入，已有 Rust 领域核心、跨模块端口、内存存储与测试替身、可运行的内存模拟接口（Mock API）和浏览器模拟场景（scenario）前端，以及引擎的事件写入与控制面。前后端尚未接线，真实前后端联调未验证。3.0 的方向、里程碑顺序与验收见 [docs/milestones.md](docs/milestones.md)；3.0 的内核与能力授权模型（#109）已写入 [docs/architecture.md](docs/architecture.md) 与 [docs/decisions.md](docs/decisions.md)（D-24 起）；`interfaces/`、`modules/`、`data-model.md` 仍按 2.x 编写，随各 3.0 里程碑更新。

M0 契约文本已于 2026-10-27 由用户统一批准。真实业务、持久化服务和生产鉴权尚未实现；前端不是生产客户端或桌面安装包。当前状态与目录入口见 [AGENTS.md](AGENTS.md)，迭代计划见本地未追踪的 `NEXT-STEPS.md`（已冻结）。

## 本地接入

需要 Rust 工具链与 Node.js 22 或以上版本。以下命令均在仓库根目录运行。

### 内存模拟接口

```bash
npm ci --prefix tools/mock-api
npm start --prefix tools/mock-api
```

Mock 仅监听 `http://127.0.0.1:8787`，用 `GET /v1/me` 检查服务。它不提供 TLS、鉴权或 CORS，也不执行真实任务。桌面界面仍应通过外壳进程调用，不直接从 WebView 连接。

路由与响应形状来自 [`schemas/openapi.yaml`](schemas/openapi.yaml)。夹具 ID、请求示例、事件游标和模拟边界见[前端接入说明](docs/frontend-integration.md)。

### 浏览器模拟前端

```bash
npm ci --prefix apps/desktop
npm run dev --prefix apps/desktop
```

`dev` 显式使用 `scenario` 模式，数据来自浏览器内存，不连接上述 Mock API。场景、操作与限制见[前端说明](apps/desktop/README.md)。

### 检查

后端与契约检查：

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo xtask schemas --check
npm test --prefix tools/mock-api
```

前端检查与资源构建：

```bash
npm run lint --prefix apps/desktop
npm run typecheck --prefix apps/desktop
npm test --prefix apps/desktop
npm run build --prefix apps/desktop
```

场景构建与端到端检查指引见[前端说明](apps/desktop/README.md#检查与构建)。上述命令检查当前实现，不代表真实联调通过或未定契约已经冻结。完整边界见 [M0 实施记录](docs/m0-implementation.md)和[契约缺口](docs/m0-contract-gaps.md)。`docs/m0-contract-proposal.md`（本地未追踪）仍是未批准草案，不是权威契约；须先经独立契约 PR 批准，再另获授权实施。

## 一句话架构

3.0 分五层：L0 内核 → L1 能力（工具与执行器）→ L2 agent 运行时 → L3 计划驱动编排 → L4 接入与信任；下层不知道上层，上层只能组合下层原语。内核事件驱动、函数核心、无状态，状态是事件日志与外部事实的投影。主 agent 是编排的初始化对话对象，也是唯一的动态授权方；能力按「角色 × 能力」由配置暴露，授权只能衰减，执行时校验。编排由计划工具驱动：模型写计划，内核按事实改条目状态。项目是 git 仓库时，内置交付包自动开启，以 Issue / PR / Check / Review 为事实推进交付，守护由 PR 事件唤醒。

第一个可用成果是本机主 agent（0 档）加 CLI。服务端模式、PKI、桌面端（Windows / macOS）和手机端放在最后，作为装配选项，功能保留。3.0 上线只用自建执行器，不接入外部 agent。

## 文档

| 文档 | 内容 |
| --- | --- |
| [AGENTS.md](AGENTS.md) | agent 入口：项目现状、架构与目录入口、开发规约、里程碑收尾 |
| [docs/architecture.md](docs/architecture.md) | 架构总图、分层与不变式（3.0） |
| [docs/decisions.md](docs/decisions.md) | 已确定 / 提议 / 待决 |
| [docs/data-model.md](docs/data-model.md) | 数据模型 |
| [docs/interfaces/](docs/interfaces/) | 模块间接口契约 |
| [docs/modules/](docs/modules/) | 模块规格书 |
| [docs/milestones.md](docs/milestones.md) | 3.0 里程碑与 2.x 历史规划 |
| [docs/prototype.md](docs/prototype.md) | 界面原型说明（源码在 [prototype/](prototype/)） |
| `docs/design.md`（本地未追踪） | 设计讨论记录（历史） |
| `docs/reviews/`（本地未追踪） | 设计评审记录 |

## 许可证

[MIT](LICENSE)
