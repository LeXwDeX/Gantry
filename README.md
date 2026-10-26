# Gantry

> 基于 DAG 编排、与 GitHub 业务强关联、按阶段管理交付的 AI 开发平台。

Gantry（龙门架）是 [GraphAgent](https://github.com/LeXwDeX/OpenCode-GraphAgent) 与 [SpecGit](https://github.com/LeXwDeX/SpecGit) 的合并继任者：借鉴 GraphAgent 的 DAG 编排经验与 SpecGit 的"Issue → PR → 验证完成"交付语义，在独立重新设计的运行时上实现，不复用 GraphAgent 的 AGPL 代码。

**状态：M0 统一开发基线已合入，M1 进行中（只完成引擎的局部）。** 已有 Rust 领域核心、跨模块端口、内存存储与测试替身、可运行的内存模拟接口（Mock API）和浏览器模拟场景（scenario）前端，以及引擎的事件写入与控制面。前后端尚未接线，真实前后端联调未验证。

M0 契约文本仍为"送审、尚未批准"。真实业务、持久化服务和生产鉴权尚未实现；前端不是生产客户端或桌面安装包。当前状态与目录入口见 [AGENTS.md](AGENTS.md)，迭代计划见 [NEXT-STEPS.md](NEXT-STEPS.md)。

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

场景构建与端到端检查指引见[前端说明](apps/desktop/README.md#检查与构建)。上述命令检查当前实现，不代表真实联调通过或未定契约已经冻结。完整边界见 [M0 实施记录](docs/m0-implementation.md)和[契约缺口](docs/m0-contract-gaps.md)。[M0 契约冻结提案](docs/m0-contract-proposal.md)仍是未批准草案，不是权威契约；须先经独立契约 PR 批准，再另获授权实施。

## 一句话架构

用户在桌面端的主对话里描述任务并选择档位（None → Ultimate）；服务端（Ubuntu）把任务展开成由多个 agent 节点组成的图，在沙盒中执行，以 GitHub 的 Issue / PR / Check / Review 为交付证据推进到合并；PR 打开后由后台守护处理 CI 与评审，没问题不打扰。桌面端（Windows / macOS）只查看、控制和签名，与服务端经私有 CA 配对后走双向 TLS。

## 文档

| 文档 | 内容 |
| --- | --- |
| [AGENTS.md](AGENTS.md) | agent 入口：项目现状、架构与目录入口、开发规约、里程碑收尾 |
| [docs/architecture.md](docs/architecture.md) | 架构总图与不变式 |
| [docs/decisions.md](docs/decisions.md) | 已确定 / 提议 / 待决 |
| [docs/data-model.md](docs/data-model.md) | 数据模型 |
| [docs/interfaces/](docs/interfaces/) | 模块间接口契约 |
| [docs/modules/](docs/modules/) | 模块规格书 |
| [docs/milestones.md](docs/milestones.md) | 可并行开发的里程碑 |
| [docs/prototype.md](docs/prototype.md) | 界面原型说明（源码在 [prototype/](prototype/)） |
| [docs/design.md](docs/design.md) | 设计讨论记录（历史） |
| [docs/reviews/](docs/reviews/) | 设计评审记录 |

## 许可证

[MIT](LICENSE)
