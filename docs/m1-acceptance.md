# M1 验收证据分级（提议）

> 状态：**提议，待用户确认**。本文不修改 [milestones.md 的 M1 退出标准](milestones.md#m1-单-issue-走通low-档端到端)，只把每条标准拆成"现在能在本机拿到的证据"和"必须有外部资源才能拿到的证据"，并列出外部资源的最小清单。是否把 M1 分成两步宣布，由用户决定。

## 1. 先说清楚：眼下的阻塞不是外部环境

截至 2026-10-26，仓库里只有 `gantry-core`、`gantry-store`、`gantry-engine`（部分）、`gantry-fakes`、`gantry-xtask` 与浏览器模拟场景前端。M1 端到端路径上的 `forge`、`repo`、`sandbox`、`worker`、`egress`、`models`、`policy`、`notify`、`conversation`、`api`、`pki`、`secrets`、`desktop-shell` 都还没有 crate。

所以即使明天就拿到 GitHub App、模型密钥、Podman 主机和签名证书，M1 端到端也跑不起来。外部资源只阻塞**最后一步**的证据；在那之前，各泳道都可以按 [milestones.md 的替身表](milestones.md#替身让泳道互不阻塞) 在本机推进。本文的目的就是把"本机能做到哪一步"写死，避免两种误判：

- 把"缺外部环境"当成停工理由；
- 把替身测试通过写成"M1 退出标准已满足"。

## 2. 三级证据

| 级别 | 含义 | 需要什么 | 能证明什么 | 不能证明什么 |
| --- | --- | --- | --- | --- |
| **A 本机替身** | 进程内，真实模块 + `core::ports` 替身，`Store::open_in_memory` / `open_temp` | 当前 macOS 开发机即可 | 状态机、幂等、崩溃恢复、事件形状、权限判定、上报逻辑 | 任何与真实 GitHub、真实容器、真实模型、真实系统密钥库的交互 |
| **B 本机真实组件** | 把替身换成本机可得的真实组件；第三方服务用 HTTP 级替身 | 本机一台 Ubuntu 24.04 虚拟机（Lima / UTM，arm64 即可）跑 rootless Podman；`FakeGitHub`（已有，回环 HTTP）；模型用回环 HTTP 桩（按提供方 API 形状回放） | 沙盒隔离与出网控制、Egress 注入与计量、补丁在真实 git 上的应用与提交、webhook HMAC 与重复投递、mTLS 与配对流程（用 `TestCA`） | GitHub 的真实语义（权限、限流、CI、合并规则、webhook 到达时序）、真实模型行为、x86_64 目标主机、签名后的桌面安装包 |
| **C 外部资源** | 真实第三方与目标平台 | 见 §4 清单 | M1 退出标准原文要求的全部 | — |

B 级与 C 级之间最大的差别是 GitHub：`FakeGitHub` 只能证明 Gantry 按自己的理解调用 GitHub，不能证明 GitHub 按这个理解响应。凡涉及 GitHub 行为的退出标准，B 级只算"预演"。

## 3. 逐条映射 M1 退出标准

| # | M1 退出标准 | A 级证据（现在可做） | B 级证据（本机虚拟机） | 必须 C 级的部分 | 前置实现 |
| --- | --- | --- | --- | --- | --- |
| 1 | 对话发起 Low 任务 → PR 创建 → CI 通过 → 桌面端审批 merge → Issue 关闭 | engine.md §12.2"端到端（替身）"：`FakeSandboxService` 回放 → `FakeRepo` allow → `FakeForge` 注入 CI 成功 → 阶段序列与审批流程 | 真实 `repo` + `sandbox`（Podman）+ `worker`（Claude Code 镜像，模型走回环桩）+ `forge` 对 `FakeGitHub`；桌面端用未签名开发构建 | 真实仓库上的 PR、Actions CI、合并与 Issue 自动关闭；真实模型改代码 | 全部服务端泳道；`desktop-shell` |
| 2 | 关闭桌面端任务继续；重开后从游标续上、界面一致 | §12.2"关闭客户端任务继续"；`tools/mock-api` 的游标续传已有 29 项测试 | 真实 `api` 事件流 + 开发构建客户端，杀客户端进程再起 | 无（与 GitHub 无关；目标平台差异见 #9、#10） | `api`、`store` SSE、`desktop-shell` |
| 3 | 沙盒内访问 `github.com` / `api.github.com` 失败；读不到模型 key | 无（替身没有网络） | **可完整证明**：虚拟机内 rootless Podman + Egress，在沙盒里 `curl` 两个域名应失败；扫描沙盒文件系统与环境变量无 key | 在 x86_64 目标主机上复跑一次（网络栈与 AppArmor 可能不同，deploy.md 已标"需核实"） | `sandbox`、`egress`、`worker` |
| 4 | 同一 webhook 投递两次不产生重复评论 / PR | `FakeForge` 注入重复事件；Outbox 幂等键唯一 | `FakeGitHub` 以 HTTP 重放同一 `X-GitHub-Delivery` | 用真实 App 的"Redeliver"复跑一次 | `forge` |
| 5 | "创建 PR 成功、写库之前"崩溃，重启不重复创建 PR | §12.2"不重复创建 PR"（`FakeForge` 联测） | `FakeGitHub` 在返回 201 后让 `gantryd` 崩溃（故障注入），重启后对账 | 真实 GitHub 上复跑一次（对账依赖真实 API 的查询语义） | `forge`、`store` 持久化 |
| 6 | workflow 补丁被拒并以 `manual_action` 附补丁上报；依赖清单 / lockfile 被拒并上报 | §12.2"受保护路径" | 真实 `repo` + `policy` 在真实 git 检出上判定 | 无 | `repo`、`policy`、`notify` |
| 7 | 写入模型凭据需要 approver 设备签名（用户在场）；无签名被拒 | `TestCA` + 软件密钥模拟签名原语；无签名请求被拒 | 开发构建在真机上用 Secure Enclave / Touch ID 签名（**是否需要正式签名才能访问 Secure Enclave 需核实**，见 §4 注） | Windows Hello + TPM 路径；签名后的安装包 | `api`、`pki`、`secrets`、`desktop-shell` |
| 8 | `job_token` 尝试结束后失效、只能给本尝试记账；网段限制在目标主机核实 | `EgressStub` 吊销与计量 | 真实 Egress：尝试结束后用旧令牌请求被拒；跨尝试记账被拒；网段限制在虚拟机里先测 | 目标主机 rootless 网络下的网段限制（P-73：不能强制则标明，不阻塞退出） | `egress`、`sandbox` |
| 9 | 设备证书剩余 1/3 有效期自动续期，续期后事件流与审批不受影响 | `TestCA` 短有效期证书 + 假时钟 | 开发构建对真实 `api` 续期 | Windows 客户端复跑；Ubuntu 24.04 的 OpenSSL 3.0 证书日期问题见 #12 | `pki`、`api`、`desktop-shell` |
| 10 | 吊销设备后其事件流在心跳间隔内断开（≤ 60 秒） | 假时钟下的心跳吊销检查 | 真实 `api` + 开发构建 | 无 | `api`、`pki` |

结论：10 条里，第 3、6、10 条在 B 级就能拿到与 C 级等价的证据（第 3 条仍需目标主机复跑一次）；第 2、7、8、9 条 B 级能证明核心、C 级补平台差异；第 1、4、5 条的 GitHub 部分只有 C 级算数。

## 4. 外部资源最小清单（C 级）

用户提供。每项都按"最小权限、可随时销毁"准备，不使用任何生产仓库或生产账号。

| 资源 | 最小规格 | 用途（对应 §3 行号） | 安全要求 |
| --- | --- | --- | --- |
| GitHub 测试组织 + 1 个测试仓库 | 独立的免费组织；仓库含一个能在 1 分钟内跑完的 Actions 工作流、一个分支保护规则（要求该 check）、一个示例 Issue | 1、4、5 | 不与任何生产组织共用；结束后可删除 |
| GitHub App | 由 `gantry github setup` 用 Manifest 创建（deploy.md），权限**只取** forge-actions §9.6 的 M1 子集：`metadata:read`、`contents:write`、`pull_requests:write`、`issues:write`、`checks:read`；**不得**含 `workflows`、`administration`（P-48）；只安装到上面的测试仓库 | 1、4、5 | 私钥只进服务端 `secrets`，不进仓库、不进聊天 |
| webhook 入口 | 隧道或带 TLS 的反向代理到测试主机回环端口（P-65） | 1、4 | 只暴露 webhook 路径 |
| 模型 API 密钥 | 一把单独的密钥，在提供方控制台设**月度花费上限**（建议 ≤ 20 美元）；只经设置页写入（需 approver 签名，第 7 行同时被验证） | 1 | 不进环境变量文件、不进仓库；验收结束后吊销 |
| 目标主机 | Ubuntu 24.04 LTS x86_64，≥ 4 vCPU、≥ 8 GiB 内存、`/var/lib` ≥ 20 GiB 可用（deploy.md 建议值）；可 root；cgroup v2 | 1、3、5、8、9 | 一次性主机，验收后重装 |
| macOS 签名 | Apple Developer ID 证书（签名 + 公证） | 7 的安装包形态 | 证书不进仓库与 CI 日志 |
| Windows 签名与设备 | 代码签名证书；一台带 TPM 2.0 与 Windows Hello 的 Windows 设备 | 7、9 的 Windows 路径 | 同上 |

注：Secure Enclave 中带"用户在场"访问控制的密钥，在未签名或临时签名（ad-hoc）的开发构建里能否创建与使用，取决于钥匙串访问组授权（entitlement）要求，**需核实**。若不能，第 7 行的 B 级证据也要等 macOS 签名证书，这时它应提前到清单首位。

## 5. 建议的推进方式（待用户确认）

1. **现在**：各泳道按替身推进到 A 级全绿；`be-engine` 先补 [engine.md §7.14](modules/engine.md) 定义的决策器（#58），再补 §12.2 全部 A 级用例。
2. **服务端泳道有可运行实现后**：在本机 Ubuntu 虚拟机上建立 B 级环境（rootless Podman、`FakeGitHub`、模型回环桩、开发构建客户端），把 §3 的 B 列做成可重复脚本。B 级环境不需要任何第三方账号或密钥。
3. **B 级全绿后**再申请 §4 的资源，一次性跑完 C 列；C 列只复跑 B 级已经通过的场景，不在真实环境里首次调试。
4. 宣布方式二选一，由用户决定：
   - **(a) 单步**：维持 milestones.md 原样，C 列全绿才算 M1 退出；
   - **(b) 两步**：A + B 全绿记为"M1 本机验收通过"，C 列全绿才记"M1 退出"。两步都不改变退出标准原文，只是多一个对外可说的中间状态。

本文推荐 (b)：它让 M2 的引擎 / 守护工作可以在 B 级基础上开工，而不会把未在真实 GitHub 上验证过的结果说成完成。
