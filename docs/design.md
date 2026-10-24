# Gantry 设计讨论记录

> 状态：历史记录（2026-10-24）。本文保留最初的设计讨论，已被 [architecture.md](architecture.md)、[decisions.md](decisions.md) 等正式文档取代；两者冲突时以正式文档为准。对本文的评审见 [reviews/2026-10-24-design-review.md](reviews/2026-10-24-design-review.md)。

## 背景与定位

目标是一个基于 DAG 编排、与 GitHub 业务强关联、按步骤管理交付的 AI 开发平台，本质上是 GraphAgent v1 与 SpecGit 的合并继任者。

- **GraphAgent v1**：opencode fork 上的 DAG 工作流引擎（节点、wave、gate、事件溯源、崩溃恢复），已进入维护期，README 声明继任项目将独立重设计 runtime。
- **SpecGit**：Issue 规格 → 代码变更 → 原生 PR/MR → 经验证的完成，以 forge 真实状态为准。
- **Gantry**：GraphAgent 提供执行引擎，SpecGit 提供交付生命周期，新增控制台作为管理面。

编排规模从单对话提交一直延伸到 PR 多角色 review、对抗性 review 的大规模编排。

## 核心判断：GitHub 对象即持久状态

是否完成一律以 GitHub 为准；本地只保存编排过程状态（图结构、节点尝试、预算）。这样"信现实，不信模型自述"自然成立，并卸掉 v1 的部分崩溃恢复复杂度。

| DAG 概念 | GitHub 对象 |
| --- | --- |
| 需求 / 规格节点 | Issue / Milestone |
| 产物 | Branch / Commit / PR |
| Gate 判定 | Check run、Review（APPROVE / REQUEST_CHANGES） |
| 人工闸门 | Required reviewers、branch protection |
| 唤醒信号 | Webhook 事件 |
| 完成判定 | Merge + 关联 Issue 关闭 |

## 编排档位：None → Ultimate

用户只调一个档位旋钮，系统内部拆成两个维度：**推理深度**（单节点 reasoning effort）和**编排深度**（图规模、角色数、对抗轮次）。所有档位共用同一个 runtime，只是同一张图的不同规模。

| 档位 | 编排形态 | 节点推理 | 典型场景 |
| --- | --- | --- | --- |
| None | 单轮对话，不建图 | none/low | 问答、改一行 |
| Low | 单节点，直接提交 | low | 小修复 |
| Medium | plan → build → verify | medium | 常规功能 |
| High | 开 PR，多角色并行 review | 关键节点 high | 跨模块改动 |
| Max | 对抗 review + reasoner 预演 + arbiter 裁决，有限轮迭代 | 关键节点 max | auth、迁移、核心路径 |
| Ultimate | Milestone 级 fan-out：多 Issue × 多 PR 带依赖并行，每个 PR 内按 Max | 决策用顶级模型，量产用性价比模型 | 一个版本或子系统 |

- **档位是上限，不是固定值**：策略在上限内按 diff 规模、敏感路径、风险标签、CI 失败次数自动升降档，超过上限才问用户。
- **每档绑定预算模板**：token、节点数、轮次、并发。Ultimate 下预算就是安全阀。
- **决策与量产分离**：延续 v1 的 advanced / standard tier 思路。

## 交付阶段机

"步骤式管理"就是一条阶段机：每个阶段的出口绑定 GitHub 证据，DAG 只负责阶段内部展开。用户看阶段进度，需要时再展开内部的图。

1. Specify：创建或选择 Issue / Milestone
2. Plan：拆解与设计，高档位含需求准入问答
3. Implement：沙盒内改代码、跑检查
4. Propose：创建或采纳 PR，保留 Issue 引用
5. Review：多角色 / 对抗 review，后台修复
6. Merge：经服务端策略与人工闸门
7. Close：forge 确认合并与 Issue 关闭

## PR 后台守护

PR 的审核与修复作为后台任务，事件驱动唤醒；没问题不打扰前台，只在需要人决定时上报。后台档位可独立配置，实现"写得快、审得狠"。

| 分流 | 情形 |
| --- | --- |
| 静默自愈 | 本 PR 范围内的 CI 失败（lint、类型、测试） |
| 静默自愈 | 经验证属实、修复代价相称的 bot finding；不属实则回复理由后关闭 |
| 静默自愈 | nit 与小的局部修改，攒到下次推送 |
| 静默自愈 | 可机械解决的冲突（lockfile、生成文件） |
| 上报 | 设计分歧、API / schema 变更 |
| 上报 | security finding |
| 上报 | 修复会扩大 PR 范围 |
| 上报 | 同类问题反复出现（根因未找到） |
| 上报 | 预算耗尽或升档超上限 |
| 上报 | 需要人的 approval |

工程要点：

- **分支所有权 / 锁**：同一时刻只有一方有写权，避免后台修复与前台开发打架。
- **防死循环**：不设硬轮次上限，而是检测同类问题重复，转根因分析或上报。
- **静默但可审计**：PR 上维护实时状态清单，控制台收件箱记录处理与跳过的理由。
- **唤醒机制**：webhook 为主，低频安全网巡检兜底，绝不轮询。

## 产品形态与 CS 架构

产品倾向做**编排与交付控制台**而非自研编辑器；服务端部署在 Ubuntu 常驻执行，桌面端（Win / Mac）只看与控制，关掉也不影响后台任务。

```
┌──────────── 桌面端 (Windows / macOS, Tauri) ─────────────┐
│ 控制台：档位旋钮 · 阶段机 · 图视图 · PR 时间线 · 收件箱 · 预算面板 │
└──────────────────────────┬───────────────────────────────┘
                           │ mTLS · HTTPS API + 事件流（可断点续传）
┌──────────────────────────▼──── 服务端 (Ubuntu) ───────────┐
│ API 网关        Engine           Forge Adapter   Worker Adapter │
│ mTLS 鉴权       DAG·阶段机·档位   GitHub App      Claude Code /   │
│ 设备管理        策略              Webhook         Codex          │
│                                                                 │
│ Sandbox Manager Egress Proxy     策略与审计      状态库          │
│ 生命周期·镜像   出网白名单        危险动作审批    SQLite →        │
│ 预热池          凭据注入          回放            Postgres        │
└──────────┬───────────────────────────────────┬──────────────┘
           │ 计算结果 / 动作请求                │ App token 写入 · Webhook
┌──────────▼──────────┐             ┌──────────▼──────────┐
│ 沙盒 × N            │             │ GitHub              │
│ 只计算：无写权限、   │             │ Issue·PR·Check·Review│
│ 无长期凭据          │             │ （持久状态的真相）    │
└─────────────────────┘             └─────────────────────┘
```

Engine 是无界面 runtime，不依赖任何 agent 宿主（v1 绑在 opencode fork 的教训）；Claude Code、Codex 等作为 worker 被调用。

- **桌面端**：Tauri（Rust，与 SpecGit 同栈），前端用 Web 技术，日后可直接出 Web 版。
- **代码同步就用 Git**：代码只在服务端，用户需要时 `git fetch` agent 推送的分支。
- **GitHub App 而非 PAT**：权限更细、token 短期、自带 webhook。
- **先定契约**：OpenAPI + 带游标的事件 schema，断线重连不丢事件。
- **分层**：Engine / Forge Adapter（SpecGit Rust core 可下沉至此）/ Worker Adapter / UI。

## 沙盒机制

核心原则：**沙盒只负责计算，服务端负责动作**。沙盒里永远没有 GitHub 写权限和长期凭据；push、评论、review、merge 都是调用服务端 Forge Adapter 的请求，经策略审核后执行。即使 agent 被注入，也只能"提出"动作。

威胁模型：agent 执行任意代码（构建、测试、postinstall）；PR 评论、Issue 正文、依赖文档带来的 prompt 注入；凭据外泄、破坏宿主、滥用网络。

| 层 | 做法 |
| --- | --- |
| 隔离 | 可插拔驱动：默认 rootless Podman/Docker + gVisor；不可信输入用 Firecracker / Kata microVM |
| 文件系统 | 一次性 clone / worktree，基础镜像只读，不挂载宿主目录，产物只从约定路径取出 |
| 网络 | 默认禁止出网，经 Egress Proxy 走白名单；凭据由代理在出站时注入 |
| 资源 | cgroups 限制 CPU、内存、磁盘、进程数、时长，与档位预算联动 |
| 命令拦截 | 沙盒内类 dcg 的危险命令守卫，提前拦截并留审计，外层隔离兜底 |
| 审计 | 记录所有命令、出网请求与服务端动作，可回放 |

档位 × 信任级别：

| 档位 | 可信输入（自己的 Issue/PR） | 不可信输入（fork、外部评论） |
| --- | --- | --- |
| None–Medium | 容器，出网白名单 | 容器 + gVisor，只读，禁止出网 |
| High–Max | 容器 + gVisor | microVM，不给 secrets |
| Ultimate | 每个 PR 独立沙盒，并发受预算约束 | microVM，所有动作需人工批准 |

- **危险动作**（merge、force push、删分支、release）一律经服务端策略，由配置决定自动放行或上报。
- **环境复现**：复用 `devcontainer.json` + setup 脚本；按仓库缓存镜像层，维护预热沙盒池。
- **暂不做桌面端本地执行**：第一版执行全在服务端，本地 runner 以后作为可选 Worker 接入。

## 通信安全：私有 CA + mTLS 配对

服务端自建私有 CA，首次连接即配对：CA 指纹与一次性口令带外传递，之后全程 mTLS 双向校验，即使暴露公网也安全。

部署与配对流程：

1. Ubuntu 执行安装脚本：生成私有 CA（ECDSA P-256，私钥 0600，可选 TPM），签发服务端证书，启动 systemd 服务，只接受 TLS 1.3。
2. 服务端执行 pair，显示二维码 / 配对串：服务端地址 + CA 指纹 + 一次性 token（10 分钟、单次有效）。
3. 客户端本地生成密钥对，私钥存系统钥匙串 / TPM，不出设备。
4. 客户端用 CA 指纹固定信任连接服务端（防中间人），提交 CSR + token。
5. 服务端校验 token，签发短期客户端证书（如 30 天）。
6. 可选 SAS 确认：两端同时显示 6 位码，人工核对。

| 点 | 做法 |
| --- | --- |
| 私钥存储 | Mac Keychain / Secure Enclave，Windows TPM（CNG），不可导出 |
| 证书轮换 | 客户端证书短期，在已有 mTLS 通道上自动续期；服务端证书同理 |
| 吊销 | 服务端是唯一校验方，握手时查数据库吊销表即可，不需 CRL / OCSP |
| 设备管理 | 控制台列出设备，可重命名、吊销、分配角色（只读 / 控制 / 可审批危险动作） |
| token 防爆破 | 短 TTL、单次有效、限速，失败若干次即作废 |
| CA 备份 | 私钥丢失则所有设备重新配对，部署时引导离线备份 |

- **危险动作审批用设备私钥签名**：Touch ID / Windows Hello 解锁后对"动作 + PR + commit SHA"签名，审批不可抵赖，会话被劫持也无法批准。
- **通道分离**：客户端 ↔ 服务端走 mTLS；GitHub Webhook 单独监听，HMAC 签名校验；沙盒 ↔ 服务端走 Unix socket / 内网，每沙盒短期 token，只能调用"提出动作"接口。
- **实现**：两端都用 Rust，共用 crate（`rustls` + `rcgen`）；只用标准 X.509 + TLS 1.3，不发明加密协议；两端开 NTP，签发时生效时间往前留余量。
- Tailscale 降级为可选的网络层加固，只缩小暴露面。

## 风险

| 风险 | 应对 |
| --- | --- |
| 成本失控（Max / Ultimate） | 档位绑定硬预算与熔断 |
| PR 噪音：多角色评论刷屏 | 汇总成一条摘要 + 少量行内评论，由 arbiter 统一发出 |
| 对抗 review 误报 | 反驳轮次 + 裁决，一条 finding 不直接触发返工 |
| 后台与前台写同一分支 | 分支所有权 / 锁 |
| prompt 注入 | 沙盒无凭据，动作经服务端策略 |
| Webhook 丢失或内网不可达 | cloudflared / smee 隧道 + 低频巡检兜底 |
| 沙盒冷启动慢 | 镜像层缓存 + 预热池 |
| CA 私钥丢失 | 离线备份 |

## 待决问题与下一步

以下问题定下来后，整理为正式架构设计文档，并用 Opus 做一轮对抗性评审。

- [ ] 目标用户：个人开发者还是团队？（决定权限、协作、闸门轻重）
- [ ] 服务端单人自托管还是多人共用？（状态库选型、租户隔离）
- [ ] Ubuntu 机器在公网还是内网？（webhook 接入与客户端连接方式）
- [ ] 是否支持 fork PR / 外部贡献者？（不支持则第一版可不做 microVM）
- [ ] 桌面端是否内置编辑器，还是纯控制台 + 外部编辑器？
- [ ] `graph-agent` 仓库是否已是 successor？SpecGit 并入还是作为独立 CLI 被调用？
- [ ] 桌面端框架：Tauri（倾向）还是 Electron？
- [ ] License：仓库当前为 MIT；若复用 GraphAgent v1 的 DAG 引擎代码（AGPL-3.0-or-later），需重写或调整许可证。
