# Gantry

> 基于 DAG 编排、与 GitHub 业务强关联、按步骤管理交付的 AI 开发平台。

Gantry（龙门架）是 [GraphAgent](https://github.com/LeXwDeX/OpenCode-GraphAgent) 与 [SpecGit](https://github.com/LeXwDeX/SpecGit) 的合并继任者：GraphAgent 提供 DAG 执行引擎的经验，SpecGit 提供 Issue → PR → 验证完成的交付生命周期，Gantry 把两者统一到一个独立 runtime 与桌面控制台之下。

**状态：设计阶段**，尚无可运行代码。

## 核心理念

- **GitHub 即持久状态**：是否完成一律以 GitHub 为准（Issue、PR、Check、Review、Merge），本地只保存编排过程状态。
- **一个旋钮，六个档位**：None / Low / Medium / High / Max / Ultimate，同一个 runtime，只是同一张图的不同规模。
- **交付阶段机**：Specify → Plan → Implement → Propose → Review → Merge → Close。
- **PR 后台守护**：审核与修复作为后台任务，没问题不打扰，只在需要人决定时上报。
- **CS 架构**：服务端部署于 Ubuntu 常驻执行，桌面端（Windows / macOS）只负责看与控制，私有 CA + mTLS 配对。
- **沙盒只计算，服务端才动作**：沙盒内无写权限、无长期凭据。

详见 [设计讨论记录](docs/design.md)。

## License

[MIT](LICENSE)
