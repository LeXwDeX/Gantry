# Worker 协议（服务端 ↔ 沙盒内 worker）

> 状态：设计 v0.1 · 契约 · 协议版本 `gantry-worker/1` · 机器可读版本落在 `schemas/worker/`（M0 产出，清单见 §11.1）
> 使用者：`sandbox`、`worker`、`engine`、`repo`（直接）；`policy`、`forge`、`egress`、`models`（间接）。`sandbox` 与 Egress 之间的控制面（签发 / 吊销 `job_token`、拉取用量）见 [egress-control.md](egress-control.md)
> 标注 **[提议]** 的取值是本文给出的默认值，骨架文档没有定；标注 OQ-n 的点见 §12 开放问题，不要自行拍板。

本文规定一次 Attempt 在沙盒里执行时，服务端与沙盒内 worker 交换什么、怎么交换、哪些可信。它是 [architecture.md §5](../architecture.md#5-信任边界与不变式) 中 I1（沙盒只计算）、I2（GitHub 写入唯一路径）、I3（放行唯一决策点）、I5（外部文本不可信）、I8（执行产物先验后推）的落地契约。核心立场：**沙盒与服务端之间只有文件和 stdout，没有任何网络端点或 socket（decisions P-15），沙盒交回的一切都是不可信输入。**

## 0. 一页纸

| 方向 | 内容 | 载体 | 可信度 |
| --- | --- | --- | --- |
| 服务端 → 沙盒 | JobSpec、输入材料、上游产物 | 只读挂载 `/gantry/job.json`、`/gantry/inputs/` | 服务端生成；其中的 GitHub / 上游文本是**不可信数据**，带隔离标记 |
| 服务端 → 沙盒 | 工作区 | `repo` 从项目镜像仓准备的一次性副本，挂载到 `/gantry/work/` | 仓库内容是不可信数据 |
| 服务端 → 沙盒 | 取消 / 超时 | 容器运行时发信号（SIGTERM → SIGKILL） | 服务端 |
| 沙盒 → 服务端 | 进度 | worker 的 stdout（JSON Lines），沙盒管理器经运行时日志接口采集 | **不可信自报**，只用于展示 |
| 沙盒 → 服务端 | 产物 | `/gantry/out/` 下固定四个文件，进程停止后由管理器提取 | **不可信**，逐项校验后才被采纳 |
| 沙盒 → 外部 | 模型 API、包仓库 | 只经 Egress：模型 API 走反向代理（模型 base URL 指向 `http://<egress>/j/<job_token>/<provider>`，由 Egress 校验 `job_token`、注入 key、按本尝试计量，P-49），包仓库走 `HTTPS_PROXY`（CONNECT 白名单，代理凭据里带 `job_token`，P-72）；沙盒内没有任何 key | `job_token` 视为沙盒可读：它只能让 Egress 给本尝试记账 |

不存在：网络回连、Unix socket、沙盒内能调用服务端的 token、服务端对沙盒 `.git` 或工作区目录的任何读取。`job_token` 不是服务端 token：它只被 Egress 的沙盒侧监听器接受，只对本尝试有效、尝试结束即作废（P-49）；"只能从沙盒网段使用"的强制方式需核实（P-73），安全底线是令牌本身只能给本尝试记账。

## 1. 原则

| # | 原则 | 依据 |
| --- | --- | --- |
| W1 | 通道只有三种：只读文件（进）、stdout 与 `out/` 文件（出）、运行时信号（控制）。沙盒内没有可连的服务端端点 | I1 |
| W2 | 沙盒产出一律不可信：**先停后取**、文件名白名单、逐项校验。通过校验只表示"在规则下可接受"，不表示可信 | I1, I8 |
| W3 | 以服务端自己观察到的事实为准：运行时的退出 / 信号 / OOM、代理计量、服务端 git 的结果。worker 自报只做展示 | I1 |
| W4 | 补丁只在服务端自建的干净检出上用服务端 git 校验与应用；服务端**不**在沙盒产出的 `.git` 上运行 git，也不遍历 `/gantry/work/` | I8 |
| W5 | `job.json` 里只有服务端生成的值或经 pattern 校验的标识符；GitHub / 上游文本只出现在 `materials` / `upstream` 里，且带隔离标记 | I5 |
| W6 | worker 只能"提议"动作；是否放行由 `policy` 判定，执行只走 `forge` Outbox，幂等键由服务端派生 | I2, I3 |
| W7 | 协议与 agent 无关。Claude Code 是第一个适配，JobSpec / result / 事件 / 补丁格式里没有 agent 专有字段 | P-07 |
| W8 | 失败闭合：不认识的约束、不合规的产物一律拒绝，不"尽力而为" | — |

责任划分：

| 模块 | 在本协议中负责 |
| --- | --- |
| `engine` | 解析 tier → 模型名；预留预算；生成 JobSpec 的可信部分（role 指令、constraints、upstream 引用）；经 `SandboxService` 提交 / 取消（P-52）；消费分类后的结果；重试、上报、入队 Outbox |
| `sandbox` | 对引擎提供 `SandboxService`，对下经 `SandboxDriver` 操作容器（P-52）：创建 / 注入 / 启动 / 监督 / 提取 / 销毁；计时器与信号；隔离包装与写入 `inputs/`；经 Egress 控制面为本尝试签发 / 吊销 `job_token` 并拉取计量（[egress-control.md](egress-control.md)）；采集 stdout |
| `repo` | 从镜像仓准备工作区副本；在干净检出上应用补丁并做机械检查，产出 `PatchFacts`（补丁闸门，机械拒绝发 `patch.rejected`）；交 `policy` 做规则判定；以 `github.commit_identity` 生成提交（P-69） |
| `worker`（沙盒内） | 读 JobSpec、驱动 agent、写 `out/`、打进度、按退出码约定退出 |
| `worker`（服务端侧 `WorkerAdapter`） | 声明镜像、命令行、所需环境变量、支持的协议版本；不碰产物内容 |
| `policy` | 基于 `PatchFacts` 的规则判定（路径规则、大小上限、敏感文件、权限位变化）、proposals 判定、审批（P-69） |
| `egress` | 模型 API 反向代理（校验 `job_token`、注入 key、按尝试计量与封顶，P-49）、包仓库 CONNECT 白名单、出网审计 |

## 2. 通道与沙盒布局

### 2.1 目录

| 沙盒内路径 | 读写 | 内容 | 提供者 | 生命周期 |
| --- | --- | --- | --- | --- |
| `/gantry/job.json` | 只读 | JobSpec（§4） | `sandbox` stage | 一个 Attempt |
| `/gantry/inputs/` | 只读 | `MANIFEST.json`、`materials/`、`upstream/`、`diff.patch`（review / verify 用）、`output.schema.json` | `sandbox` stage | 同上 |
| `/gantry/work/` | 读写 | 工作区一次性副本，worker 的 cwd；**永不被服务端读取** | `sandbox` stage | 随沙盒销毁 |
| `/gantry/out/` | 读写，有配额 | 产物目录，启动时为空 | `sandbox` stage 建空目录 | 进程停止后提取 |
| `/gantry/home/`、`/gantry/tmp/` | 读写，tmpfs | agent CLI 的 `HOME`、`TMPDIR` | 运行时 | 随沙盒 |
| 根文件系统 | 只读 | 镜像（按 digest 固定）：worker、agent CLI、工具链 | 镜像 | — |

`MANIFEST.json`：`{ "job_sha256": "...", "files": [{ "path": "...", "sha256": "...", "bytes": 0 }] }`。worker 启动时校验，用于发现挂载错误或串用，**不是**安全机制（沙盒能读到它）。

### 2.2 环境变量

| 变量 | 值 | 说明 |
| --- | --- | --- |
| `GANTRY_JOB` / `GANTRY_OUT` / `GANTRY_WORK` | `/gantry/job.json` / `/gantry/out` / `/gantry/work` | 固定路径，便于测试替身 |
| `HTTPS_PROXY`、`https_proxy` | `http://job:<job_token>@<egress>:3128`（即签发响应的 `connect_proxy_url`，P-72） | 包仓库的出口（白名单，不注入上游凭据）；包管理器以 `Proxy-Authorization`（Basic，用户名 `job`）把 `job_token` 交给 Egress，Egress 据此按尝试放行、计量与审计 |
| 模型 base URL（如 `ANTHROPIC_BASE_URL`，由适配层设置） | `http://<egress>/j/<job_token>/<provider_id>`（由控制面签发令牌时返回，P-49） | 模型调用的唯一出口；Egress 校验令牌、注入 key、按本尝试计量。令牌只出现在这个变量与 `HTTPS_PROXY` 里，不写进 `job.json`、`inputs/`、事件或日志 |
| `NO_PROXY` | `localhost,127.0.0.1` | — |
| `HOME`、`TMPDIR` | `/gantry/home`、`/gantry/tmp` | — |
| agent 专用变量 | 由适配层设置（§10） | — |

不注入 `GITHUB_TOKEN`、模型 key、SSH 私钥；镜像不含 `gh`，git credential helper 为空。`job_token` 是唯一出现在沙盒里的令牌，不是凭据：泄露给沙盒内任何进程的后果只是给本尝试多记账（仍受 `tokens_total` 封顶）。

### 2.3 对 Egress 的要求

协议依赖 `egress` 提供以下能力（控制面契约见 [egress-control.md](egress-control.md)，模式见 decisions P-16、P-49、P-50）：

| # | 要求 | 说明 |
| --- | --- | --- |
| E1 | 沙盒网络里除 Egress 的沙盒侧监听器外无路由，DNS 由 Egress 解析；控制面 `egress.sock` 与回环监听器都不可达 | 无旁路 |
| E2 | 只放行本 job 的 `egress_profile`：模型 API（反向代理）+ 包仓库主机（CONNECT，凭 `Proxy-Authorization` 中的 `job_token` 识别尝试，P-72）。`github.com`、`api.github.com` 不在其内；依赖指向 `github.com` 源默认拒绝（decisions Q-08） | M1 退出标准；包仓库白名单本身是外泄通道，见 OQ-15 |
| E3 | 模型 API 用反向代理模式：worker 把模型 base URL 指向 `http://<egress>/j/<job_token>/<provider_id>`（明文到 Egress，Egress 再以 TLS 连上游）；Egress 去掉路径前缀，丢弃客户端自带的认证头，注入真实 key | 沙盒内无 key，也无需信任代理 CA |
| E4 | 按尝试计量 token：Egress 只凭路径中的 `job_token` 识别尝试（P-49）——令牌由服务端在创建沙盒时经控制面签发，只对该尝试有效（"只接受来自沙盒网段的请求"的强制方式需核实，P-73），不信任沙盒自报的任何其他标识（请求头、`x-gantry-*`）；达到 `limits.tokens_total` 时切断后续模型请求 | 计费口径见 §7.4；即使沙盒内的进程读到令牌，也只能给本尝试记账 |
| E5 | 包仓库走 HTTPS CONNECT 白名单，不注入凭据；私有包仓库走反向代理模式注入凭据 | — |
| E6 | 出网审计日志带 `job_id` | M3 回放 |
| E7 | 尝试结束（`collect` 之后）由 `sandbox` 经控制面吊销 `job_token` 并取最终计量；吊销后该令牌的任何请求都被拒 | — |

### 2.4 工作区副本

| 要求 | 说明 |
| --- | --- |
| 来源 | `repo` 从项目镜像仓（服务端裸仓，由 `forge` 拉取更新）检出 `workspace.base_sha`；沙盒内无 GitHub 网络，`git fetch / push` 必然失败，这是设计 |
| 自包含 | `.git` 完整可用，不依赖镜像仓路径或 alternates；HEAD 游离在 `base_sha`，index 干净 |
| 不含 | remote 配置、凭据、hooks、`.git/config` 中的 url / include |
| 子模块 / LFS | 子模块只有 gitlink 无内容，LFS 只有 pointer 文件；worker 不得假定能取到 |
| review / verify | `base_sha` 是候选 head（原 base 加已校验补丁的本地提交）；`pr_base_sha` 是变更起点；`inputs/diff.patch` 是 `pr_base_sha..base_sha`，由服务端 git 生成 |
| 历史深度、子模块 / LFS 策略 | **OQ-13** |

## 3. 生命周期

### 3.1 总览

```mermaid
sequenceDiagram
  participant E as engine
  participant S as sandbox 管理器
  participant X as egress
  participant W as worker（沙盒内）
  participant V as repo + policy
  E->>E: 预留预算，生成 JobSpec，解析 tier 与模型
  E->>S: create（SandboxSpec）
  S->>X: 签发 job_token（job_id、白名单、token 上限）
  S->>S: stage：准备工作区，写 job.json 与 inputs，建空 out
  S->>W: start（只读挂载，环境变量）
  loop 运行中
    W-->>S: stdout JSON Lines（不可信自报）
    S-->>E: 进度转发（仅展示）
  end
  W->>W: 写 out 产物，result.json 最后原子写入，退出
  S->>S: 确认全部进程已停，再 collect out（白名单，不跟随链接）
  S->>X: 吊销 job_token，取本 job 最终计量
  S-->>E: RawArtifacts + 运行时事实 + 计量
  S->>S: destroy（容器，工作区，staging）
  E->>V: 校验结果，补丁，proposals
  V-->>E: 通过 / 需审批 / 拒绝
  E->>E: 分类，写 attempt.finished
```

要点：`destroy` 在 `collect` 之后立刻执行，**校验发生在沙盒之外**；校验失败不需要保留沙盒。

### 3.2 阶段

| 阶段 | 操作 | 执行者 | 做什么 | 事件 | 失败 → 分类（§9） |
| --- | --- | --- | --- | --- | --- |
| 0 预留 | — | `engine` | 从 BudgetLedger 预留；解析 tier → 模型；生成 JobSpec；创建 Attempt（n+1） | `node.started`、`attempt.started` | 预算不足：不创建，`budget.exhausted` |
| 1 创建 | `create` | `sandbox` | 经 Egress 控制面为本尝试签发 `job_token`（绑定 `job_id`、档案、`tokens_total`）；按 SandboxSpec 创建**未启动**的容器，模型 base URL 带该令牌 | `sandbox.created` | `infra.sandbox_create` / `infra.egress` |
| 2 注入 | `stage` | `sandbox` | 挂载 `repo` 准备的工作区副本；写 `job.json`、`inputs/`（含隔离包装，§4.4）；建空 `out/`；算 MANIFEST | — | `infra.workspace_prep` |
| 3 启动 | `start` | `sandbox` | 以非 root 用户启动 `gantry-worker run`；开始计 `wall_time` | — | `infra.sandbox_create` |
| 4 运行 | — | worker | 校验 JobSpec → 驱动 agent → 流式输出进度 | `attempt.progress`（采样，§7） | §3.3、§3.4 |
| 5 结束 | `stop` | `sandbox` | 进程退出，或超时 / 取消 / 被杀：确保容器内**全部**进程已终止 | — | — |
| 6 提取 | `collect` | `sandbox` | 进程停止后从 `out/` 提取白名单文件；取退出状态、OOM / 信号事实、stdout / stderr 尾部；吊销 `job_token` 并取最终计量 | — | `infra.collect` |
| 7 销毁 | `destroy` | `sandbox` | 删除容器、工作区副本、staging；确认 `job_token` 已吊销（幂等）；失败重试，残留由巡检清理 | `sandbox.destroyed` | 不影响分类 |
| 8 终结 | — | `engine` + `repo` | 按 §8 校验 → 分类 → 写 `attempt.finished`（含 `result_ref`、`failure_class`、`usage`）；`repo` 机械检查补丁、产出 `PatchFacts` 交 `policy` 判定，为通过的补丁生成本地提交；proposals 交 `policy` | `patch.validated` / `patch.rejected`（`repo`）、`attempt.finished`、`node.finished`、`policy.denied`（仅策略判定） | §9 |

`SandboxSpec`（engine → sandbox）与 data-model 的 Sandbox 实体对应：

| 字段 | 来源 | 说明 |
| --- | --- | --- |
| `job` | engine | JobSpec 的可信部分 + 待包装的原始材料 |
| `image_digest`、`driver`、`trust` | 配置 / 档位 | `trust`：trusted / untrusted |
| `limits` | 预算模板 + [config.md](config.md) 的 `sandbox.limits.*` | cpu、mem、disk、pids、`wall_time`；与 JobSpec `constraints.limits` 同源 |
| `egress_profile`、`egress.tokens_total` | 配置 / 预算 | 白名单与 token 封顶（签发 `job_token` 时交给 Egress，强制方是 `egress`） |
| `workspace` | engine | `repo`、`base_sha`、（review / verify）`pr_base_sha`；对应 `Sandbox.worktree_snapshot` = `{mirror, base_sha, pr_base_sha?}`（镜像仓引用 + 基线提交 SHA） |

预热池（M2）**[提议]**：容器可先启动并空等，`stage` 完成后由管理器**原子创建** `/gantry/inputs/READY`，worker 见到该文件后才读 `job.json`。M1 不需要，但 worker 的启动逻辑按"等待 READY"写，两种方式都兼容（OQ-16）。

### 3.3 退出码

退出码是 worker 的自报，**只作线索**；终态由 §9 的优先级用服务端观察到的事实决定。

| 退出码 | 名称 | worker 的含义 | `result.json` |
| --- | --- | --- | --- |
| 0 | OK | 协议走完，`result.status` 为 `succeeded` 或 `failed` | 必须存在且完整 |
| 10 | BAD_JOB | `job.json` 缺失 / 不合法 / `protocol` 或 `agent` 不支持 / `constraints`、`output` 里有不认识的字段 / MANIFEST 不符 | 可无；若有则 `error.kind = bad_job` |
| 11 | AGENT_ERROR | agent CLI 起不来或崩溃，或适配层自身异常 | 尽力写 |
| 12 | ABORTED | 收到 SIGTERM / SIGINT，有序收尾 | 尽力写，`status = cancelled` |
| 其他 / 信号 | — | 未定义；按运行时事实判定（`128+n`、OOM 标志） | 视为无效 |

硬规则：`succeeded` 必须同时满足 **退出码 0** 且 **`result.json` 完整合法**。退出码非 0 但存在合法 `result.json` 时，只读取其中的 `error` 供诊断，不采纳其产物。

### 3.4 超时与取消

| 情形 | 触发 | 动作 | 产物 | 结果 |
| --- | --- | --- | --- | --- |
| 墙钟超时 | `sandbox` 计时器（从 `start` 起算 `wall_time_s`） | SIGTERM → 宽限 `kill_grace_s`（默认 10，**[提议]**）→ 容器级 SIGKILL | 仍提取，标记 `partial`，仅供诊断，不应用补丁、不执行 proposals | `timeout.wall` |
| 空闲超时 | 超过 `idle_timeout_s`（默认 300，**[提议]**）没有收到合法进度事件 | 同上 | 同上 | `timeout.idle` |
| 取消 | `engine` 调用 `sandbox.cancel(job_id)`（用户取消、节点被图改写替换等） | 同上；worker 可在宽限期内写 `status = cancelled` | 同上 | Attempt `cancelled`，`failure_class = cancelled`（不计入重试与上报） |
| token 封顶 | `egress` 切断模型请求（预留 / 回收见 OQ-14） | agent 因 API 错误退出，worker 写 `failed` | 按失败处理 | `budget.tokens` |
| OOM / pids / 磁盘配额 | 运行时 cgroup | 运行时终止 | 同上 | `exec.oom` / `exec.killed` |
| 取消与超时同时发生 | 看谁先被记录 | — | — | 先记录者胜 |
| `create` / `stage` 阶段被取消 | — | 直接 `destroy` | — | 同取消 |

worker 不需要区分 SIGTERM 的原因：原因只由管理器记录。取消与超时之间没有控制通道，信号 + 计时器就是全部。

### 3.5 崩溃恢复

| 场景 | 处理 |
| --- | --- |
| `gantryd` 重启 | 沙盒带标签 `gantry.job_id`、`gantry.sandbox_id`。对每个没有 `attempt.finished` 的 Attempt 查运行时：仍在运行 → 重新接管日志（用 `seq` 去重）；已退出 → 直接 `collect`；受理后已不存在 → 终结为 `infra_error`（`infra.lost`），Sandbox `lost`；从未被受理（`submit` 返回前崩溃）→ 重新 `submit`，不计为失败（sandbox.md §6.1 `RecoveredJob`） |
| 提取幂等 | `collect` 的结果只取决于已停止沙盒的 `out/`，重复执行得到相同的内容寻址对象 |
| 补丁提交幂等 | 校验得到的 `result_tree`（git tree OID）写入 `attempt.finished`；重放时先查该事件，已存在则不重复提交 |
| 孤儿沙盒 | 巡检按标签找出没有对应未终结 Attempt 的沙盒并销毁 |

## 4. JobSpec

`/gantry/job.json`，UTF-8 JSON，一个 Attempt 一份，创建后不可变。

### 4.1 示例（Medium 档，`build` 节点）

```json
{
  "protocol": "gantry-worker/1",
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "created_at": "2026-10-24T12:40:58.004Z",
  "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
  "delivery_id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD",
  "workflow": { "id": "wfl_01JBX3F2K8M4N6P1Q3R5S7T9VX", "graph_rev": 1 },
  "node": { "id": "nod_01JBX3G5H7J9K2M4N6P8Q1R3S5", "role": "build", "exec": "sandbox", "required": true },
  "attempt": { "n": 1 },
  "context": { "level_effective": "Medium", "revision_round": 0, "previous_attempts": [] },
  "execution": {
    "agent": "claude-code",
    "tier": "worker",
    "model": "claude-sonnet-5-5",
    "reasoning_effort": "medium",
    "agent_options": {}
  },
  "task": {
    "title": "Fix off-by-one in pagination of GET /api/items",
    "instructions": "Implement the plan found under inputs/upstream. Keep the change minimal and add regression tests. Do not modify CI configuration or dependency manifests.",
    "output_language": "zh-CN"
  },
  "workspace": {
    "path": "/gantry/work",
    "repo": "acme/widgets",
    "base_sha": "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
    "pr_base_sha": null
  },
  "isolation": {
    "nonce": "3f9a1c7e5b2d4a60c8e1f7a2b9d04c35",
    "begin": "<<<GANTRY-UNTRUSTED-BEGIN nonce={nonce} id={id} kind={kind} source={source} truncated={truncated}>>>",
    "end": "<<<GANTRY-UNTRUSTED-END nonce={nonce} id={id}>>>"
  },
  "materials": [
    {
      "id": "mat-001",
      "kind": "issue_body",
      "source": { "repo": "acme/widgets", "number": 5, "item_id": null },
      "path": "/gantry/inputs/materials/mat-001.txt",
      "sha256": "sha256:a1c17679fd08a9521fe95ad479614fdd6a3ed048c337e7005f8827f3bec2318b",
      "bytes": 2051,
      "truncated": false,
      "original_bytes": 1840
    },
    {
      "id": "mat-002",
      "kind": "issue_comment",
      "source": { "repo": "acme/widgets", "number": 5, "item_id": 2481733095 },
      "path": "/gantry/inputs/materials/mat-002.txt",
      "sha256": "sha256:dcca958d73c4800eb83b1f3612cd9f7860f385c59cfe8f4b2dc605064b84fdf6",
      "bytes": 835,
      "truncated": false,
      "original_bytes": 610
    }
  ],
  "upstream": [
    {
      "node_id": "nod_01JBX3G0A1B2C3D4E5F6G7H8J9",
      "role": "plan",
      "attempt_id": "att_01JBX3Z1P2Q3R4S5T6V7W8X9Y0",
      "dir": "/gantry/inputs/upstream/nod_01JBX3G0A1B2C3D4E5F6G7H8J9/",
      "files": ["result.json", "report.md"],
      "ref": "sha256:b5c7112ed154fa7b01b0870ca4c67957cdc1c252a75ff320c4d4286e7adccc11"
    }
  ],
  "targets": [],
  "constraints": {
    "tools": { "edit": true, "shell": true },
    "paths": {
      "allow": ["src/**", "tests/**", "docs/**"],
      "deny": [".git/**", ".github/workflows/**"],
      "approval_required": ["Cargo.toml", "Cargo.lock", "package.json", "package-lock.json"]
    },
    "limits": { "wall_time_s": 1800, "idle_timeout_s": 300, "max_turns": 60, "tokens_total": 800000 },
    "patch": { "max_bytes": 5000000, "max_files": 200 }
  },
  "output": {
    "structured_schema": {
      "id": "gantry.worker.output.build/1",
      "path": "/gantry/inputs/output.schema.json",
      "sha256": "sha256:3f9fab9a1fa6705bf39ab80590d0274f5ee16077712206883e799c5022a6a45d"
    },
    "patch": "required",
    "report": "required",
    "proposals": { "allowed_kinds": ["open_pr", "escalate"], "max": 3 }
  }
}
```

`bytes` 大于 `original_bytes` 是正常的：`bytes` 是带隔离标记的文件大小（`original_bytes` 是规范化后、截断前的正文大小）。

### 4.2 字段表

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `protocol` | string | 是 | `gantry-worker/<major>`。worker 不支持该 major → 退出码 10。同 major 内只增字段不删字段 |
| `job_id` | `att_` ULID | 是 | 等于 `Attempt.id`（一个 Attempt = 一个沙盒 = 一个 job，不新增 ID 类型）。贯穿 stdout 事件、`egress` 计量、`result.json` |
| `created_at` | RFC 3339 | 是 | — |
| `project_id`、`delivery_id` | ID | 是 | 仅标注与日志 |
| `workflow.id`、`workflow.graph_rev` | ID、int | 是 | — |
| `node.id`、`node.role`、`node.exec`、`node.required` | — | 是 | `role` 见 §4.3；`review:<视角>` 的视角部分匹配 `[a-z][a-z0-9_-]*`；`exec` 只会是 `sandbox` 或 `sandbox_ro`（`direct` 的角色不进沙盒） |
| `attempt.n` | int | 是 | 同节点第几次尝试 |
| `context.level_effective` | Level | 是 | 信息性 |
| `context.revision_round` | int | 是 | `REVISE` 追加的轮次，从 0 起 |
| `context.previous_attempts[]` | object[] | 否 | `{attempt_id, failure_class, failure_detail}`，**只含码值**。上次产物的任何文本只能以 `upstream_output` 材料进入，不得进 `instructions` |
| `execution.agent` | enum | 是 | `claude-code` 等；与镜像内适配层不符 → 退出码 10 |
| `execution.tier` | enum | 是 | `planner` / `worker`（沙盒内的角色只用这两类，[architecture §9](../architecture.md#9-三类模型)） |
| `execution.model` | string | 是 | `models` 按设置解析出的模型名；worker 不得自行更换。示例值仅示意 |
| `execution.reasoning_effort` | enum | 是 | `none` / `low` / `medium` / `high` / `max`；由适配层映射到 agent 的推理开关 |
| `execution.agent_options` | object | 否 | agent 专有、不透明，由适配层按白名单校验；**只能收紧**，不得放宽 `constraints` |
| `task.title` | string | 是 | 可信（服务端生成或用户亲手输入） |
| `task.instructions` | string | 是 | **可信指令**：role 模板 + 用户在对话里亲自输入的文本。任何来自 GitHub 或沙盒的文本一律不得进入此字段 |
| `task.output_language` | BCP 47 | 否 | `summary` / `report.md` 的语言 |
| `workspace.path` | string | 是 | 固定 `/gantry/work` |
| `workspace.repo` | `owner/repo` | 是 | — |
| `workspace.base_sha` | 40 hex | 是 | 工作区 HEAD。**产出的补丁相对它** |
| `workspace.pr_base_sha` | 40 hex / null | 否 | review / verify 的变更起点 |
| `isolation.nonce` | 32 hex | 是 | 每个 job 随机；材料内容里不得出现（§4.4） |
| `isolation.begin` / `end` | string 模板 | 是 | 隔离标记格式，占位符 `{nonce}` `{id}` `{kind}` `{source}` `{truncated}` |
| `materials[]` | object[] | 否 | 不可信文本材料（§4.4）。元数据只含标识符、哈希、大小，**不含任何自由文本** |
| `upstream[]` | object[] | 否 | 上游节点产物引用。目录挂载在 `inputs/upstream/<node_id>/`；整个目录按不可信处理 |
| `targets[]` | object[] | 否 | proposals 可指向的**句柄白名单**：`{handle, kind, ref}`，如 `{"handle": "thread-1", "kind": "review_thread", "ref": "acme/widgets#12"}`；`kind` ∈ `review_thread` / `issue` / `pr`，`ref` 只是展示线索。worker 看不到 GitHub 的 node_id 和线程 ID，只能引用句柄。`open_pr`、`escalate` 不需要目标 |
| `constraints.tools` | object | 是 | `edit`、`shell` 布尔 |
| `constraints.paths.allow` / `deny` / `approval_required` | glob[] | 是 | 相对仓库根；语法归 `policy` 路径规则。`approval_required` 的规则表由 `policy` 提供，这里只是快照 |
| `constraints.limits` | object | 是 | `wall_time_s`、`idle_timeout_s`、`max_turns`、`tokens_total` |
| `constraints.patch` | object | `output.patch ≠ forbidden` 时 | `max_bytes`、`max_files` |
| `output.structured_schema` | object | 是 | `id`（`gantry.worker.output.<schema>/<major>`）、`path`、`sha256` |
| `output.patch` | enum | 是 | `required` / `optional` / `forbidden` |
| `output.report` | enum | 是 | `required` / `optional` |
| `output.proposals` | object / `"forbidden"` | 是 | `{allowed_kinds[], max}` |

兼容规则：顶层与大多数对象里的**未知字段被忽略**；`constraints.*` 与 `output.*` 里的未知字段 → 退出码 10（worker 无法执行它不认识的约束，W8）。

### 4.3 role

`role`、`tier`、`exec` 的固定搭配以 [architecture §9](../architecture.md#9-三类模型) 为准（decisions P-17）。`exec = sandbox` 时工作区可写；`sandbox_ro` 时工作区只读挂载、`constraints.tools.edit = false`、`output.patch = forbidden`；`direct` 的角色由 `models` 直接调用，不经本协议，但其 `structured_output` 用本文 §6 的同一套 schema，由 `models` 校验后交 `engine` 一视同仁地消费。

| role | 作用 | tier | exec | `output.patch` | 典型 proposals | `structured_output.kind` |
| --- | --- | --- | --- | --- | --- | --- |
| `plan` | 把规格拆成步骤（计划阶段） | planner | `sandbox_ro` | forbidden | — | `plan` |
| `build` | 实现 / 修复 | worker | `sandbox` | required 或 optional | `open_pr`、`escalate` | `build` |
| `verify` | 在候选 head 上跑检查并判定 | worker | `sandbox` | forbidden | — | `verify` |
| `single` | Low 档单节点：实现 + 自检 | worker | `sandbox` | required 或 optional | `open_pr`、`escalate` | `build` |
| `review:<视角>` | 某视角的评审 | worker | `sandbox_ro` | forbidden | — | `review` |
| `reasoner` | 预演执行路径，找失败点（计划阶段，Max 起） | planner | `sandbox_ro` | forbidden | — | `reasoner` |
| `arbiter` | 对一组 review 做唯一裁决 | planner | `sandbox_ro` | forbidden | — | `arbiter` |
| `heal` | 后台守护的自愈（CI 失败、属实的 bot finding、review 意见） | worker | `sandbox` | required 或 optional | `reply_review_thread`、`resolve_review_thread`、`escalate` | `build` |
| `answer` | None 档涉及代码的问答 | worker | `sandbox_ro` | forbidden | — | `answer` |
| `summarize` | High 档汇总 review | planner | `direct` | — | — | `summarize` |
| `admission` | 需求准入（仅 High 及以上，P-55） | planner | `direct` | — | — | `admission` |
| `intent` | 意图解析 | planner | `direct` | — | — | `intent` |
| `classify` | CI 日志分类、事件摘要、状态问答等打杂文本 | utility | `direct` | — | — | `classify` |

推理深度随档位，由配置决定（[config.md §3](config.md)），解析进 `execution.reasoning_effort`。

### 4.4 输入材料：打包与隔离标记

不可信文本的来源（I5）：Issue / PR 正文、评论、review thread、CI 日志、依赖文档、文件摘录、上游节点产物。

**打包流水线（`sandbox.stage`，使用 `core` 的 `UntrustedText` 类型）：**

| 步 | 做什么 |
| --- | --- |
| 1 选取 | `engine` 按 `@` 引用与阶段需要，经 `ForgeReader` 按需拉取 Issue / PR 正文与评论的原始文本（不可信文本，不进快照，P-59），附来源（`repo`、`number`、`item_id`） |
| 2 规范化 | 按 UTF-8 解码（非法序列替换）；去掉 NUL 与除 `\n` `\t` 外的 C0 控制符、ANSI 转义；去掉 bidi 控制符（U+202A–202E、U+2066–2069）与 Unicode Tag 字符（U+E0000–E007F）；记录去掉的个数 |
| 3 截断 | 单条上限 64 KiB、总量上限 512 KiB（**[提议]**）；超出取头 + 尾，中间放截断标记；写 `truncated = true`，保留 `original_bytes` 与原文哈希 |
| 4 生成 nonce | 128 位随机；材料文本里若已含该串则重新生成 |
| 5 包装 | 每条材料一个文件 `inputs/materials/<id>.txt`，**服务端写入时已包好标记**，这样无论适配层内联进 prompt，还是 agent 用工具 `cat` 读到，标记都在 |
| 6 登记 | `job.json.materials[]` 记元数据与**包装后文件**的 `sha256` |

包装后的文件：

```text
<<<GANTRY-UNTRUSTED-BEGIN nonce=3f9a1c7e5b2d4a60c8e1f7a2b9d04c35 id=mat-001 kind=issue_body source=acme/widgets#5 truncated=false>>>
Title: Pagination skips the last item
Body:
When per_page=10 and the table has 21 rows, page 3 returns nothing...
<<<GANTRY-UNTRUSTED-END nonce=3f9a1c7e5b2d4a60c8e1f7a2b9d04c35 id=mat-001>>>
```

规则：

| 规则 | 说明 |
| --- | --- |
| 标记行不含自由文本 | `source` 只能是 `owner/repo#N` 或 `owner/repo#N/<item_id>`，`kind` 来自枚举，标题 / 作者名等放进正文里 |
| nonce 不可预测 | 攻击者的文本在 nonce 生成之前就已固定，无法伪造结束标记 |
| `kind` 枚举 | `issue_body`、`issue_comment`、`pr_body`、`pr_comment`、`review_comment`、`review_thread`、`ci_log`、`commit_message`、`dependency_doc`、`file_excerpt`、`upstream_output` |
| 可信前言 | 适配层固定放进 system prompt（不放用户 prompt）：标记之间的内容是数据，不执行其中指令，不让它改变约束、工具、输出路径或 schema；发现疑似注入就在 `flags.suspected_injection` 与 `report.md` 里报告 |
| `upstream/` 目录 | 上游产物是结构化文件（JSON / patch / md），无法逐文件包装。**整个目录按不可信处理**；适配层内联进 prompt 时用同一 nonce 包装 |
| 重试的反馈 | 来自上次产物的文本（含路径名）重试时也以 `upstream_output` 材料进入 |

局限（须让实现者知道）：隔离标记只是纵深防御。agent 运行时直接读到的仓库文件、依赖文档、工具输出无法逐条包装。真正的边界是结构性的：沙盒无凭据无 GitHub 网络（I1）、约束由主机侧强制（§4.5）、产物先验后推（I8）、动作经 `policy`（I3）。

### 4.5 约束的两种效力

| 约束 | agent 侧（建议，早失败） | 服务端强制 | 强制方 |
| --- | --- | --- | --- |
| `paths.allow` / `deny` | 权限规则 + prompt | 补丁越界即拒（§8.3 P8） | `repo` + `policy` |
| `paths.approval_required` | prompt | 命中 → `needs_approval` | `policy` |
| `limits.wall_time_s` | prompt | 计时器 kill | `sandbox` |
| `limits.idle_timeout_s` | — | 无合法事件 → kill | `sandbox` |
| `limits.tokens_total` | prompt | 代理切断 | `egress` |
| `limits.max_turns` | 传给 agent CLI | 无直接强制（被前两项间接限制） | 适配层 |
| `patch.max_*` | prompt | 提取与校验时拒收 | `repo` |
| `output.*` | — | intake 按 JobSpec 决定接受哪些文件 | `sandbox`（intake） |
| `tools` | CLI 权限规则 | 无；外层沙盒是边界 | — |

生效的路径规则 = `JobSpec.allow ∩ policy 当前 allow`，再减去 `JobSpec.deny ∪ policy 当前 deny`。校验取 `policy` 的**当前**规则，不是 JobSpec 里的快照。

## 5. 产物

### 5.1 文件清单

worker 只能写 `/gantry/out/` 下这四个文件名（精确匹配、区分大小写）：

| 文件 | 必需 | 格式 | 默认上限（**[提议]**） | 用途 |
| --- | --- | --- | --- | --- |
| `result.json` | 是，**最后写**，作为完成标记 | JSON | 1 MiB | 状态、摘要、`structured_output`、产物声明、自报用量 |
| `changes.patch` | 按 `output.patch` | git 扩展 unified diff（§5.3） | 5,000,000 字节（`policies.patch.max_bytes`） | 代码改动 |
| `report.md` | 按 `output.report` | Markdown，不可信 | 256 KiB | 给人读的报告 |
| `proposals.json` | 否 | JSON | 256 KiB，≤ 20 条 | 动作提议（§5.4） |

其他文件一律丢弃并记 `artifact.unexpected` 审计。`result.json` 先写 `result.json.tmp` 再 `rename`，所以最终文件名下只会出现完整内容；被杀时残留的 `.tmp` 被忽略。

### 5.2 result.json

```json
{
  "protocol": "gantry-worker/1",
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "job_sha256": "sha256:5e8c9902207afaeb7120430c585a445f21e92932081d64bc99f80e4925bcb002",
  "status": "succeeded",
  "summary": "Fixed the off-by-one in paginate(); added 2 regression tests; cargo test --workspace passes.",
  "error": null,
  "structured_output": {
    "kind": "build",
    "changes_summary": "paginate() computed the last page offset with page * per_page instead of (page - 1) * per_page.",
    "steps_status": [
      { "step_id": "s1", "status": "done", "note": "" },
      { "step_id": "s2", "status": "done", "note": "two tests added" }
    ],
    "checks_run": [
      { "command": "cargo test --workspace", "exit_code": 0, "outcome": "pass", "summary": "142 passed" }
    ],
    "known_issues": [],
    "no_change_reason": null
  },
  "artifacts": {
    "patch": {
      "path": "changes.patch",
      "sha256": "sha256:a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570",
      "bytes": 2210,
      "base_sha": "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
      "files_changed": 2
    },
    "report": {
      "path": "report.md",
      "sha256": "sha256:845e91831319e89c4d656bdb80c278ac09a7230d61e5dfd2e1b1fbb436ac8917",
      "bytes": 1530
    },
    "proposals": {
      "path": "proposals.json",
      "sha256": "sha256:c0cbf0fbcaf64c0b201506063e2b6b12b9999482a61de6d6078321000fafdf76",
      "bytes": 640
    }
  },
  "flags": { "suspected_injection": [] },
  "usage_reported": {
    "input_tokens": 412000,
    "output_tokens": 18300,
    "cache_read_tokens": 350000,
    "cache_write_tokens": 21000,
    "turns": 24,
    "tool_calls": 61
  },
  "agent": { "kind": "claude-code", "version": "x.y.z", "model": "claude-sonnet-5-5" },
  "started_at": "2026-10-24T12:41:02.311Z",
  "ended_at": "2026-10-24T12:58:47.902Z"
}
```

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `protocol`、`job_id` | — | 是 | 必须与 JobSpec 一致，否则整份结果无效 |
| `job_sha256` | sha256 | 是 | `job.json` 字节的哈希。防串用，不防伪造 |
| `status` | enum | 是 | `succeeded` / `failed` / `cancelled` |
| `summary` | string | 是 | ≤ 1000 字符，纯文本，不可信，用于事件行 |
| `error` | object / null | `failed` / `cancelled` 时必填 | `{kind, message, retryable_hint}`；`kind` ∈ `task_failed`、`agent_error`、`max_turns`、`budget`、`output_invalid`、`bad_job`、`aborted`、`internal`；`message` ≤ 1000 字符 |
| `structured_output` | object | `succeeded` 时必填 | 按 role 的 schema（§6），含 `kind` |
| `artifacts.<name>` | object / null | 是 | 声明每个产物的 `path`、`sha256`、`bytes`；服务端重算，**不符即整份无效**。`patch` 另含 `base_sha`（必须等于 JobSpec）与自报 `files_changed` |
| `flags.suspected_injection[]` | object[] | 否 | `{material_id, note}`，自报，仅展示，不降低任何校验 |
| `usage_reported` | object | 是 | 自报用量，**只用于展示**（§7.4） |
| `agent` | object | 是 | 实际用的 CLI 与版本、模型名 |
| `started_at`、`ended_at` | time | 是 | 自报，仅展示 |

服务端把 `result.json`、各产物、采集的 stdout / stderr 尾部、运行时事实、`egress` 计量、校验结论组成 `ResultBundle`，存为内容寻址对象；`Attempt.result_ref` 指向其清单哈希。该对象库的位置、备份、保留期见 OQ-12。清单**由服务端生成**（不是 worker 写的）：`sandbox` 交原料（`JobOutcome`），`engine` 组装清单，经 `core::jcs` 规范化后入 CAS。形状已在 #84 拍板，Rust 类型是 `gantry_core::worker::ResultBundle`，schema 是 `schemas/worker/result-bundle.schema.json`：

```json
{
  "bundle_version": 1,
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "job_sha256": "sha256:5e8c9902207afaeb7120430c585a445f21e92932081d64bc99f80e4925bcb002",
  "collected_at": "2026-10-24T12:58:48.000Z",
  "terminal": { "cause": "exited", "exit_code": 0, "signal": null, "oom": false, "timer": null, "cancel_requested": false },
  "files": {
    "result.json": { "sha256": "sha256:b4d78c6828c77821e5673bb398468fb13dd99b7a44bc3a228f4429bcaea3dbc8", "bytes": 3120 },
    "changes.patch": { "sha256": "sha256:a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570", "bytes": 2210 },
    "report.md": { "sha256": "sha256:845e91831319e89c4d656bdb80c278ac09a7230d61e5dfd2e1b1fbb436ac8917", "bytes": 1530 },
    "proposals.json": { "sha256": "sha256:c0cbf0fbcaf64c0b201506063e2b6b12b9999482a61de6d6078321000fafdf76", "bytes": 640 },
    "progress.jsonl": { "sha256": "sha256:7c9e0d75ba6d887d43620bee283b0a39704a4a7ef82b39364a46c97cdc207d73", "bytes": 48211 }
  },
  "logs": {
    "stdout_tail": { "sha256": "sha256:3f1a2b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a", "bytes": 4096 },
    "stderr_tail": { "sha256": "sha256:9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d", "bytes": 512 }
  },
  "usage_metered": { "input_tokens": 415200, "output_tokens": 18100, "cache_read_tokens": 351000, "cache_write_tokens": 21000, "requests": 26, "weighted_tokens": null },
  "usage_reported": { "input_tokens": 412000, "output_tokens": 18300, "cache_read_tokens": 350000, "cache_write_tokens": 21000, "turns": 24, "tool_calls": 61 },
  "egress": { "capped": false, "incomplete": false },
  "classification": { "failure_class": null, "failure_detail": null },
  "validation": {
    "patch": { "verdict": "allow", "sha256": "sha256:a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570", "result_tree": "f2c03d84a858156b0121130fd1ec2f391aa90865", "files": 2 },
    "proposals": [
      { "id": "p1", "kind": "open_pr", "disposition": "accepted", "reasons": null, "text_ref": { "sha256": "sha256:d2a84f4b8b650937ec8f73cd8be2c74add5a911ba64df27458ed8229da804a26", "bytes": 812 } },
      { "id": "p2", "kind": "reply_review_thread", "disposition": "accepted", "reasons": null, "text_ref": { "sha256": "sha256:6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b", "bytes": 214 } }
    ],
    "unexpected_artifacts": []
  }
}
```

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `bundle_version` | 是 | 固定 `1` |
| `job_id`、`job_sha256` | 是 | 同 JobSpec / `result.json` |
| `collected_at` | 是 | **确定性时间，截到秒**：取沙盒停止时间（`TerminalFacts` 一侧的服务端观测值）；`terminal.cause = lost` 时没有停止时间，取引擎判定该尝试丢失的那条事件的时间戳。重复 `collect` 得到同一清单、同一 `result_ref`（§3.5）；`repo` 的提交时间戳也取它（repo.md §6.3），所以重放得到同一 `result_commit` |
| `terminal` | 是 | 运行时终止事实；`cause` ∈ `exited` / `killed_timer` / `killed_cancel` / `killed_runtime` / `lost`，`exited` 时 `exit_code` 必填；`timer` ∈ `wall` / `idle` |
| `files` | 是，可为空 | 键只能是 `result.json` / `changes.patch` / `report.md` / `proposals.json` / `progress.jsonl`，值是 CAS 引用 `{sha256, bytes}` |
| `logs` | 是，可为空 | `stdout_tail` / `stderr_tail` 的 CAS 引用 |
| `classification` | 是 | `failure_class` / `failure_detail`，必须与 `attempt.finished` 一致（引擎写入时核对） |
| `validation` | 可为 `null` | `result.json` 缺失或无效（§5.2"不符即整份无效"）时为 `null`，清单仍然生成以便取证 |
| `validation.patch` | 可为 `null` | `sha256` 必须等于 `files["changes.patch"].sha256`（= 补丁审批的 subject_digest，P-20）；`verdict = deny` 时 `result_tree` 为 `null`；`result_tree` 以 `attempt.finished` 为准，引擎写入时核对 |
| `validation.proposals[]` | 是，可为空 | `{id, kind, disposition, reasons, text_ref}`；`id` 唯一；`disposition` ∈ `accepted` / `needs_approval` / `denied` / `invalid`（后两者带 `reasons[]`）；`text_ref` 只出现在 `accepted` 上，是引擎另存的**净化文本**的 CAS 引用，`accepted` 的 `open_pr` 必须有。`text_ref` 对象的内容是该提议 `params`（§5.4，如 `open_pr` 的 `{title, body, draft}`）经引擎净化后的 JCS 规范化 JSON。执行体预取提议原文（engine.md §7.14.2 "提议原文预取"，`ProposalRef { result_ref, proposal_id }`）时，先读 `result_ref` 指向的本清单，按 `proposal_id` 找到条目，再读它的 `text_ref`；不碰 worker 原始的 `proposals.json` |
| `usage_metered`、`usage_reported`、`egress`、`validation.unexpected_artifacts` | 否 | M1 只定义形状，语义留到 M2。`usage_metered` 是 Egress 计量加 `weighted_tokens`（P-70）；`egress` 只有 `capped` / `incomplete`，`upstream_error_summary` 是不可信文本，不入清单 |

不入清单的内容：
- `result_commit`：提交时间戳取自本清单的 `collected_at`，提交在清单定稿之后才产生；`result_commit` 只在 `attempt.finished` 里（engine.md §7.14.6 E1）。`attempt.finished` 是 `result_tree` / `result_commit` 的权威来源。
- `project_id`：归属经 `Attempt.result_ref` 反查。结果读取（decisions.md Q-12，路由形态归 M2）以清单为索引授权：只允许读本项目某个 Attempt 的 `result_ref`，以及该清单 `files` / `logs` / `text_ref` 列出的摘要；摘要本身不是权限。
- 被拒产物的隔离区引用：留到 M2，与 OQ-12 一起定。

### 5.3 补丁格式：unified diff（git 扩展头）

**选定：git 扩展 unified diff，单个文件 `changes.patch`；不用 `git format-patch`。**

| 维度 | unified diff（选定） | `git format-patch` |
| --- | --- | --- |
| 提交身份与说明 | 没有。作者、说明、trailer 全由服务端生成 | 携带沙盒编造的 From / Date / Subject / 正文，可伪造 `Co-authored-by`、`Signed-off-by`；还要额外净化 |
| 解析面 | `git apply`，只有 diff 语法 | `git am`：mbox + mailinfo + hooks + 多补丁序列，面更宽 |
| 与不可变 Attempt 的对应 | 一个 Attempt 一个补丁一棵结果树 | 多提交序列，需逐个校验、中途失败要回滚 |
| 校验的事实源 | 应用后用服务端 git 枚举变更（§8.3） | 同样可以，另需校验每个提交的元数据 |
| 丢失的信息 | 沙盒内的提交粒度与说明 | 保留 |
| 结论 | 采用 | 不采用 |

提交说明由服务端生成，与 architecture §9 把"提交说明"归 Utility 一致；是否需要保留多提交粒度见 OQ-10。

**生成**（由适配层执行，不是让 agent 自己写 diff；适配层对自己沙盒里的 `.git` 也不信任，最坏结果只是产出一份无效补丁）：

```bash
git -c core.fsmonitor=false -c core.hooksPath=/dev/null add -A -- .
git -c core.fsmonitor=false diff --cached --no-color --no-ext-diff --no-textconv \
    --no-renames --full-index -U3 --src-prefix=a/ --dst-prefix=b/ \
    <base_sha> > /gantry/out/changes.patch
```

| 约定 | 说明 |
| --- | --- |
| 相对 `base_sha` | 比较的是 index 与 `base_sha` 的树，所以 agent 在沙盒里自行 `git commit` 不影响结果 |
| `--no-renames` | 重命名表现为删除 + 新增，两个路径都被检查 |
| `--full-index` | `index` 行带完整前像 blob ID，应用端可验证前像 |
| 无 `--binary` | 二进制变更没有可应用的内容，在校验时被拒（§8.3 P7） |
| 空补丁 | 不产生文件，`artifacts.patch = null`；`build` 须在 `no_change_reason` 说明 |
| 编码 | UTF-8、LF；文件内容里的 CRLF 原样保留 |

**服务端应用**（在服务端自建的一次性检出上，`GIT_CONFIG_NOSYSTEM=1`、`core.hooksPath=/dev/null`、`core.fsmonitor=false`；**严格模式**，不加 `--3way`、`--reject`、`--recount`、`--unidiff-zero`、`--ignore-whitespace`）：

```bash
git apply --index --whitespace=nowarn -p1 changes.patch
```

`--index` 保证全有或全无。

### 5.4 proposals.json

只是**提议**。worker 无法也不应直接动 GitHub（I1）；服务端对每条提议先校验，再交 `policy`，放行后由 `engine` 入队 `forge` Outbox（I2）。幂等键由服务端按 [forge-actions §2.2](forge-actions.md#22-幂等键) 派生，worker 不提供。

```json
{
  "protocol": "gantry-worker/1",
  "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
  "proposals": [
    {
      "id": "p1",
      "kind": "open_pr",
      "target": null,
      "params": {
        "title": "fix: off-by-one in GET /api/items pagination",
        "body": "Fixes the last-page truncation reported in #5. Adds two regression tests.",
        "draft": false
      },
      "rationale": "Change complete; workspace tests pass."
    },
    {
      "id": "p2",
      "kind": "escalate",
      "target": null,
      "params": {
        "inbox_kind": "decision_needed",
        "message": "The API docs describe page numbers as zero-based; confirm that 1-based is intended before merging."
      },
      "rationale": "The spec and the docs disagree."
    }
  ]
}
```

| `kind` | `target` | `params` | 对应 Outbox 动作（[forge-actions §3.1](forge-actions.md#31-总表)） | 说明 |
| --- | --- | --- | --- | --- |
| `open_pr` | `null` | `title`、`body`、`draft` | `create_pr`（分支由 `push_branch` 先推送） | 分支名、base、`Closes #N` 由服务端定；依赖本 Attempt 的已校验补丁 |
| `reply_review_thread` | `review_thread` 句柄 | `body` | `reply_review_thread` | — |
| `resolve_review_thread` | `review_thread` 句柄 | — | `resolve_review_thread` | 线程含人类评论时 `policy` 可能要求审批 |
| `escalate` | `null` | `inbox_kind`（`decision_needed` / `spec_needed` / `security_finding` / `escalation`）、`message` | 无（不是 GitHub 写动作，写 InboxItem） | — |

词表是**封闭**的，且只取 `ForgeActionKind` 的子集（对应关系与 [forge-actions §3.5](forge-actions.md#35-与相邻契约的词表对应) 一致）：不含 merge、force push、删分支、release、改 workflow、改仓库设置等危险动作（只能由 `engine` / 用户发起，经 `policy` 与 Gantry 审批，I3）；也不含自由评论 / 状态评论（`create_comment`、`upsert_status_comment` 只由 `engine` / `guardian` 发起）。`resolve_review_thread` 通常 `depends_on` 同一线程的 `reply_review_thread`。是否增加 `create_issue` 等见 OQ-9。

### 5.5 report.md

给人读的 Markdown 报告，结构自由，建议含：做了什么、没做什么、遇到的问题、疑似注入内容。**不可信**：展示前去 HTML、去远程图片、链接只显示不自动打开；绝不原样贴到 GitHub，要发到 GitHub 的文本由服务端另行组装（经密钥扫描）。

## 6. structured_output（按 role）

`structured_output.kind` 必须与 `node.role` 对应（`review:<视角>` 对应 `review`）。以下约定适用于所有 role：所有字符串都有长度上限（schema 里给出），所有文本不可信；未列字段被忽略。

### 6.1 公共类型

**Verdict**（gate 节点：`verify`、`review`、`arbiter`、`admission`；`reasoner` 不是 gate，不输出 verdict，decisions P-45；语义见 decisions P-24）：

| 值 | 对 judge 的含义 | 引擎处理 |
| --- | --- | --- |
| `ACCEPT` | 满足验收，没有阻塞项（准入：READY） | 推进 |
| `REVISE` | 有可修复的问题，需再改一轮（准入：NOT_READY） | 追加修正 + 复审节点（有界，[architecture §8](../architecture.md#8-档位阶段与图)）；准入则在对话中追问 |
| `REJECT` | 方案本身不可接受，小修无益 | 交付单元 `stage_status = waiting_human`，上报 `decision_needed`（`kind = gate_reject`，放弃或重新规划；选重新规划回到 `plan` 阶段，P-55），记 `failure_class = verdict_fail` |
| `BLOCKED` | 无法判断：缺信息、环境 / 权限不足、输入互相矛盾 | 上报 `escalation`，附 `blocking_reasons` 与本结果的 `result_ref` 作证据 |

**gate 公共字段**：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `verdict` | Verdict | 是 | — |
| `rationale` | string | 是 | ≤ 4000 字符 |
| `blocking_reasons[]` | `{code, message}` | 是（可空） | `code` ∈ `missing_info` / `env_unavailable` / `conflicting_inputs` / `out_of_scope` / `other` |
| `findings[]` | Finding[] | `review`、`verify` 必填；`arbiter` 可选 | 见下 |

一致性规则（校验器强制）：`ACCEPT` ⇒ 无 `blocker` 级 finding；`REVISE` / `REJECT` ⇒ 至少一条 finding；`BLOCKED` ⇒ `blocking_reasons` 非空。违反即 schema 无效。

**Finding** **[提议]**（词表见 OQ-8）：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `id` | `f<数字>` | 是 | 结果内唯一 |
| `severity` | enum | 是 | `blocker` / `major` / `minor` / `nit` |
| `category` | enum | 是 | `security` / `correctness` / `reliability` / `performance` / `maintainability` / `test` / `docs` / `style` / `other`。guardian 的"security 上报""nit 攒批"分流依赖它 |
| `title` | string | 是 | ≤ 120 |
| `description` | string | 是 | ≤ 2000 |
| `location` | object / null | 否 | `{path, line_start, line_end, side}`，`side` ∈ `head` / `base`。**仅为线索**，发到 GitHub 前由 `forge` 对 diff 重新校验 |
| `evidence` | string | 否 | ≤ 1000，代码或命令输出摘录 |
| `suggested_fix` | string | 否 | ≤ 2000，文字建议，不是补丁 |
| `confidence` | enum | 是 | `low` / `medium` / `high` |
| `in_scope` | bool | 是 | 是否在本 PR 改动范围内（"修复会扩大范围"需上报） |

### 6.2 各 role

**`plan`**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `steps[]` | `{id, title, detail, files[], depends_on[], verification}` | 是 | `id` 形如 `s1`；`depends_on` 引用同表 id，必须无环；`verification` 是建议的验证命令 |
| `risks[]` | `{severity, description, mitigation}` | 是（可空） | `severity` ∈ `low` / `medium` / `high` |
| `assumptions[]` | string[] | 是（可空） | — |
| `blocking_questions[]` | `{question}` | 是（可空） | 非空表示计划无法在现有规格下成立，engine 据此追问 / 上报 |
| `touched_paths_estimate[]` | glob[] | 是（可空） | 自报的触及范围，只作 engine 风险加码的线索，**不是约束** |

**`build`**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `changes_summary` | string | 是 | ≤ 2000 |
| `steps_status[]` | `{step_id, status, note}` | 是（可空） | `status` ∈ `done` / `partial` / `skipped` |
| `checks_run[]` | `{command, exit_code, outcome, summary}` | 是（可空） | `outcome` ∈ `pass` / `fail` / `error` / `skipped`。**自报**，不是 CI 证据（check 结果的事实源是 GitHub，I4） |
| `known_issues[]` | string[] | 是（可空） | — |
| `no_change_reason` | string / null | 无补丁时必填 | — |

**`verify`**：gate 公共字段 +

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `checks[]` | `{name, command, exit_code, outcome, duration_s, evidence}` | 是 | 同样是自报 |
| `plan_coverage[]` | `{step_id, satisfied, evidence}` | 否 | 对照上游 `plan` |

**`review`**：gate 公共字段 +

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `perspective` | string | 是 | 与 `role` 的视角部分一致 |

**`reasoner`**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `scenarios[]` | `{id, description, path[], predicted_outcome, likelihood, failure_points[]}` | 是 | `path[]` 是预演的步骤序列；`predicted_outcome` ∈ `ok` / `fail` / `unknown`；`failure_points[]`：`{location, reason}` |
| `plan_adjustments[]` | `{step_id, change, reason}` | 是（可空） | — |

`reasoner` 只产出预演报告，**没有** `verdict` 字段（decisions P-45）；出现即按未知字段忽略，引擎不据此判定。

**`arbiter`**：gate 公共字段 +

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `round` | int | 是 | 评审轮次 |
| `rulings[]` | `{finding_ref, ruling, severity_final, duplicate_of, reason}` | 是 | `finding_ref = {node_id, finding_id}`，必须指向本 job `upstream` 里的 review 结果（引用完整性由校验器检查）；`ruling` ∈ `upheld` / `dismissed` / `downgraded` / `duplicate` |
| `required_changes[]` | `{id, description, finding_refs[]}` | 是（可空） | `REVISE` 时的返工清单 |
| `pr_summary` | string | 是 | ≤ 4000，Markdown。供 `forge` 组装"由 arbiter 统一发出"的评审摘要；仍按不可信文本处理 |
| `escalate` | `{needed, reason_code}` | 是 | 需要上报人时 `needed = true` |

**`admission`**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `verdict` | Verdict | 是 | READY = `ACCEPT`，NOT_READY = `REVISE`（decisions P-24）；`REJECT` / `BLOCKED` 同 gate 公共语义 |
| `blocking_reasons[]` | `{code, message}` | `BLOCKED` 时非空 | 同 gate 公共字段 |
| `missing[]` | `{aspect, question}` | `REVISE` 时非空 | `aspect` ∈ `acceptance_criteria` / `scope` / `constraints` / `repro_steps` / `other`；`question` 用于对话里追问，也是 InboxItem `spec_needed` 的 payload |
| `refined_spec` | `{title, body_markdown, acceptance_criteria[]}` / null | 否 | "规格写回 Issue"的候选；写回由 `engine` 经 Outbox 做，不经 proposals |
| `risk_flags[]` | string[] | 是（可空） | 如 `touches_auth`、`schema_change`，供 engine 自动加码参考 |

**`summarize`**（High 档汇总 review）

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `text` | string | 是 | ≤ 4000，给人读的汇总 |
| `findings[]` | Finding[] + `source_refs[]` | 是（可空） | 去重汇总后的 finding，每条带来源 `{node_id, finding_id}` |

**`classify`**（打杂文本）

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `target` | enum | 是 | `ci_classification` / `event_line` / `status_answer` / `commit_message` / `status_checklist` / `notification` |
| `text` | string | 是 | ≤ 4000 |
| `classification` | `{category, in_pr_scope, confidence}` / null | `ci_classification` 时必填 | `category` ∈ `lint` / `type` / `test` / `build` / `infra` / `flaky` / `unknown` |

**`intent`** **[提议]**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `intent` | enum | 是 | `task`（建或定位交付单元）/ `query`（只回答）/ `control`（调档、放弃等，仍需经对应路由的权限检查） |
| `delivery_id` | `dlv_` / null | 否 | 定位到的已有交付单元 |
| `needs_code` | bool | 是 | `query` 是否需要读仓库（是则派 `answer` 节点，否则 `classify` 的 `status_answer`） |

**`answer`** **[提议]**：`answer_markdown`（string，≤ 8000，必填）、`refs[]`（`{path, line_start?, line_end?}`，可空）。

**`single`、`heal`**：`structured_output.kind = build`，字段同 `build`（`single` 的自检记入 `checks_run`）。

### 6.3 示例

`plan`：

```json
{
  "kind": "plan",
  "steps": [
    {
      "id": "s1",
      "title": "Fix the offset computation in paginate()",
      "detail": "Use (page - 1) * per_page when computing the offset.",
      "files": ["src/api/items.rs"],
      "depends_on": [],
      "verification": "cargo test -p api items::pagination"
    },
    {
      "id": "s2",
      "title": "Add regression tests for the last page",
      "detail": "Cover 21 rows with per_page=10, pages 1 to 3.",
      "files": ["tests/items_pagination.rs"],
      "depends_on": ["s1"],
      "verification": "cargo test --workspace"
    }
  ],
  "risks": [
    { "severity": "low", "description": "Clients may rely on the old zero-based behaviour.", "mitigation": "Check API docs and existing callers." }
  ],
  "assumptions": ["page numbers are 1-based"],
  "blocking_questions": [],
  "touched_paths_estimate": ["src/api/**", "tests/**"]
}
```

`review:security`：

```json
{
  "kind": "review",
  "perspective": "security",
  "verdict": "REVISE",
  "rationale": "The pagination helper trusts a client-supplied per_page without an upper bound.",
  "blocking_reasons": [],
  "findings": [
    {
      "id": "f1",
      "severity": "major",
      "category": "security",
      "title": "Unbounded per_page allows resource exhaustion",
      "description": "paginate() uses per_page as given; a request with per_page=10000000 loads the whole table into memory.",
      "location": { "path": "src/api/items.rs", "line_start": 41, "line_end": 48, "side": "head" },
      "evidence": "let limit = params.per_page;",
      "suggested_fix": "Clamp per_page to a configured maximum such as 100.",
      "confidence": "high",
      "in_scope": true
    }
  ]
}
```

`arbiter`：

```json
{
  "kind": "arbiter",
  "verdict": "REVISE",
  "round": 1,
  "rationale": "One security finding is upheld; the style finding is a duplicate of an existing lint rule.",
  "blocking_reasons": [],
  "rulings": [
    {
      "finding_ref": { "node_id": "nod_01JBX3G7K9M1N3P5Q7R9S1T3VW", "finding_id": "f1" },
      "ruling": "upheld",
      "severity_final": "major",
      "duplicate_of": null,
      "reason": "Reproducible; the bound is missing on the only public entry point."
    }
  ],
  "required_changes": [
    {
      "id": "c1",
      "description": "Clamp per_page to a configured maximum and add a test.",
      "finding_refs": [{ "node_id": "nod_01JBX3G7K9M1N3P5Q7R9S1T3VW", "finding_id": "f1" }]
    }
  ],
  "pr_summary": "One blocking issue remains: per_page is unbounded (src/api/items.rs). Everything else looks good.",
  "escalate": { "needed": false, "reason_code": null }
}
```

## 7. 进度事件

worker 的 stdout **只**输出协议事件，每行一个 JSON 对象（JSON Lines，UTF-8，LF 结尾）。agent CLI 与它启动的命令的 stdout 由适配层管道接走，不继承 worker 的 fd 1。stderr 不解析，只作诊断尾部留存。

### 7.1 格式

```jsonl
{"v":1,"seq":1,"type":"hello","job_id":"att_01JBX4Q9ZK3M8T5V2N7RHC6WYD","agent":"claude-code","agent_version":"x.y.z"}
{"v":1,"seq":2,"type":"stage","stage":"explore","detail":"reading src/api/items.rs"}
{"v":1,"seq":3,"type":"tool","phase":"start","call_id":"c1","tool":"Bash","summary":"cargo test -p api items::pagination"}
{"v":1,"seq":4,"type":"tool","phase":"end","call_id":"c1","tool":"Bash","ok":true,"duration_ms":4210}
{"v":1,"seq":5,"type":"usage","input_tokens":18200,"output_tokens":2400,"cache_read_tokens":9000,"cache_write_tokens":1200,"turns":3}
{"v":1,"seq":6,"type":"heartbeat","in_flight":1}
{"v":1,"seq":7,"type":"warn","code":"material_truncated","detail":"mat-002 was truncated by the server"}
{"v":1,"seq":8,"type":"finish","status":"succeeded"}
```

| `type` | 字段 | 说明 |
| --- | --- | --- |
| `hello` | `job_id`、`agent`、`agent_version` | 第一行 |
| `stage` | `stage`、`detail` | `stage` ∈ `init` / `explore` / `plan` / `edit` / `test` / `review` / `package` / `finalize` **[提议]**；`detail` ≤ 200 字符 |
| `tool` | `phase`（`start` / `end`）、`call_id`、`tool`、`summary`（start）、`ok`、`duration_ms`（end） | 摘要规则：Bash 取命令行，编辑 / 读取取路径与操作，搜索取 pattern；**不含文件内容或命令输出**；`summary` ≤ 200 字符 |
| `usage` | `input_tokens`、`output_tokens`、`cache_read_tokens`、`cache_write_tokens`、`turns` | **累计值**不是增量，丢行不影响总数 |
| `heartbeat` | `in_flight`（未结束的工具调用数） | 重置空闲计时；适配层只在 agent 有活动或工具调用进行中时发，不独立定时发 |
| `warn` | `code`、`detail` | 非致命问题 |
| `finish` | `status` | 提示性；终态不取决于它 |

`seq` 从 1 开始严格递增。worker 可附 `ts`，服务端忽略。

### 7.2 采集

| 项 | 做法 |
| --- | --- |
| 通道 | 沙盒管理器经**运行时日志接口**（主机侧）读取容器 stdout / stderr；不挂 socket，不开端口 |
| 缓冲 | 行长上限 8 KiB（**[提议]**），超长行丢弃并计数，不无限缓冲 |
| 存储 | 合法事件写入该 Attempt 的 `progress.jsonl`（内容寻址对象）；转发给客户端的是采样节流后的 `attempt.progress` 事件（data-model §5.2），仅展示 |
| 尾部 | stdout 非协议行与 stderr 各留末尾 1 MiB（**[提议]**），供诊断，展示时当不可信纯文本 |

### 7.3 防伪

这些事件全部是不可信自报。沙盒里的任何进程都可以写 `/proc/<pid>/fd/1`，所以**不存在"来自真 worker"的保证**。设计目标不是辨真伪，而是让伪造没有后果。

| 威胁 | 处理 |
| --- | --- |
| 伪造 / 篡改事件 | 事件**只用于展示和计费估算**：不驱动状态机、不决定成败、不影响权限与预算 |
| 畸形 / 超大 / 高频 | 严格 JSON schema，未知 `type` 丢弃，非 JSON 行丢弃；速率上限 20 行/秒、总量上限 20 000 行（**[提议]**）；超限继续排空管道（避免阻塞 worker）但丢弃并计数 |
| 乱序 / 重放 | `seq` 必须严格递增，否则丢弃 |
| 时间伪造 | 忽略 worker 的时间戳，服务端打 `received_at` |
| 展示层注入 | 入库前去 ANSI、控制符、bidi 控制符；UI 一律纯文本渲染，标注"agent 自报" |
| 用量虚报 | 自报 `usage` 不进账本；扣费、预算、熔断只认 `egress` 计量 |
| 用伪造心跳躲空闲超时 | 只能延迟到 `wall_time`；`wall_time` 是 `sandbox` 计时器的硬上限，不依赖 worker |
| 宣称"已完成" | `finish` 不被信任；终态由 §9 的优先级决定 |

### 7.4 计费口径

| 数据 | 来源 | 用途 |
| --- | --- | --- |
| **计量用量** | `egress` 反向代理按 `job_token` 归属到本尝试统计的模型 API 请求 / 响应 token（P-49；`sandbox` 吊销令牌时经控制面取最终值，egress-control.md） | 写入 `Attempt.usage`，按 `levels.token_weights` 加权后驱动 BudgetLedger 与 `budget.charged` / `budget.exhausted`，封顶切断 |
| 自报用量 | stdout `usage` 事件、`result.usage_reported` | 进度条与展示；与计量值显著偏离时记审计日志（异常信号） |

## 8. 服务端校验

### 8.1 流水线

| 步 | 名称 | 判定 | 失败 → |
| --- | --- | --- | --- |
| V1 | 运行时事实归并 | 退出状态、信号、OOM、计时器、`egress` 切断标记 → 终止原因 | §9 优先级 |
| V2 | 产物 intake | 白名单与文件类型（§8.2） | `exec.protocol` |
| V3 | `result.json` | schema；`job_id`、`job_sha256` 匹配；产物声明与实际一致 | `exec.protocol` |
| V4 | `structured_output` | role schema；一致性规则（§6.1）；引用完整性 | `exec.output_invalid` |
| V5 | 补丁（`repo` 机械检查并产出 `PatchFacts`，`policy` 规则判定，P-69） | §8.3 | `policy.*` 或 `exec.output_invalid` |
| V6 | proposals | §8.4 | 逐条处置，不使 Attempt 失败 |
| V7 | 终结 | 分类、写事件（`repo` 写 `patch.validated` / `patch.rejected`；`attempt.finished`）、`repo` 为通过的补丁生成提交 | — |

V5 / V6 仅当 V1–V4 通过且 `status = succeeded` 时执行；否则补丁与 proposals 一律丢弃（原文留存审计）。

### 8.2 Intake（V2）

| 检查 | 方法 |
| --- | --- |
| 只认四个文件名 | 其余丢弃并记 `artifact.unexpected`；不提取 `work/`、`home/`、`tmp/` |
| 必须是普通文件 | `lstat` 判定；符号链接、硬链接（`nlink > 1`）、FIFO、设备、socket 一律丢弃；用 `O_NOFOLLOW` 与 `O_NONBLOCK` 打开 |
| 大小 | 读到"上限 + 1 字节"即判超限，不信 `stat` 的大小 |
| 先停后取 | 只在容器已停止、全部进程已终止之后读取，消除 TOCTOU |
| 文本 | `result.json` / `proposals.json` / `report.md` 必须是合法 UTF-8；JSON 深度 ≤ 32、禁止重复键 |
| 落地 | 复制进服务端内容寻址存储，此后只读副本 |
| 不该有补丁 | `output.patch = forbidden` 却存在 `changes.patch` → 丢弃，记 `artifact.unexpected`，**不**使 Attempt 失败 |

### 8.3 补丁校验清单（V5）

**原则：不自己写 patch 解析器，以"应用之后服务端 git 看到的事实"为准**，避免校验器与 `git apply` 的解析差异被利用。顺序：先看原始字节数 → 在一次性检出上应用 → 用服务端 git 枚举结果 → 逐项规则。

**分工**（decisions P-69）：`repo` 做**机械检查**——能否干净应用、文件列表、大小、二进制、符号链接、子模块、权限位、路径规范——并产出 `PatchFacts`；`policy` 只基于 `PatchFacts` 与配置做**规则判定**（路径规则、大小上限、敏感文件、权限位变化），不解析补丁、不读文件系统。机械拒绝由 `repo` 发 `patch.rejected`（带 `reason`），**不发** `policy.denied`；`policy.denied` 只用于策略判定。下表"归属"列写明是哪一种。一次性检出里**不执行任何仓库内容**（hooks 关闭，不构建不测试），`deny` 或 `needs_approval` 时整体丢弃或冻结。路径逃逸、穿过符号链接写入由 `git apply` 自身拒绝，P4 / P5 是第二道防线。

| # | 检查 | 方法 | 默认判定 | 归属 |
| --- | --- | --- | --- | --- |
| P1 | 大小 | `repo` 量出补丁字节数、`git apply --numstat -z` 的文件数与总变更行、应用后单文件大小，写入 `PatchFacts`（过大时可在应用前提前拒绝）；`policy` 对照 `policies.patch.max_bytes` / `max_files` / `max_changed_lines` / `max_file_bytes` 判定 | deny `policy.size` | 事实 `repo`，判定 `policy` |
| P2 | 严格可应用 | 在 `base_sha` 的一次性检出上 `git apply --index -p1`，原子 | `exec.output_invalid`（`patch.rejected`，`reason = patch_not_applicable`） | `repo`（机械） |
| P3 | 枚举事实 | `git diff --cached --raw -z --no-renames --full-index HEAD` 与 `--numstat -z`；其后各项都基于这份清单，组成 `PatchFacts` | — | `repo` |
| P4 | 路径规范 | UTF-8；无绝对路径、`..`、空组件、`\`、NUL；任何组件不得是 `.git`（含大小写变体、NTFS 8.3 `GIT~1`、HFS 可忽略字符）；不得有两条路径仅大小写或 Unicode 规范化不同 | deny `policy.path_invalid` / `policy.dotgit` | `repo`（机械） |
| P5 | 文件类型 | 仅允许模式 `100644`、`100755`。`120000`（符号链接）→ **一律拒绝**（P-68）；`160000`（子模块 gitlink）→ 拒；`.gitmodules` 的任何改动同处理 | deny `policy.symlink` / `policy.submodule` | `repo`（机械） |
| P6 | 权限位 | 新文件 `100755`：允许并记录；已有文件 `100644 ↔ 100755`：作为事实交 `policy` | `needs_approval`（审批动作 `patch_apply_protected`；M1 等同 deny，decisions P-19） | 事实 `repo`，判定 `policy` |
| P7 | 二进制 | `numstat` 为 `- -`，或补丁出现 `Binary files` / `GIT binary patch` | deny `policy.binary`，**一律拒绝**，没有白名单（P-68） | `repo`（机械） |
| P8 | 路径规则 | 每个变更路径（新旧路径都算）：不在 `allow` 或命中 `deny` → deny；命中 `workflows`（`.github/workflows/**`）→ **一律 deny**，M2 起也不变（P-48），上报 `decision_needed`（`kind = manual_action`）并附补丁；命中其他处置为 `approve` 的规则（依赖清单、lockfile、`repo_meta`…）→ M1 deny 并上报（fail-closed），M2 起 `needs_approval`（审批动作 `patch_apply_protected`，decisions P-19、P-54）。取 `policy` **当前**规则 | deny `policy.path_outside_allow` / `policy.path_denied` / `policy.path_protected:<规则 id>`（如 `policy.path_protected:workflows`）；`needs_approval`（M2） | `policy` |
| P9 | 敏感文件与内容 | 文件名（`.env*`、私钥、证书、含凭据的 `.npmrc` 等）与新增行的密钥模式扫描（新增行由 `repo` 惰性提供） | deny `policy.secret` | `policy` |
| P10 | base 一致 | `artifacts.patch.base_sha` 等于 JobSpec；提交的 parent 必须是 `base_sha`。Worktree 分支头已前移时不自动 rebase 补丁，按冲突处理，交 `engine` 重新派发 | 冲突 → `engine` 决定 | `repo`（机械） |
| P11 | 空补丁 | 声明了补丁但无变更，视为无补丁 | — | `repo` |

`failure_detail` 的子码沿用 `policy.*` 前缀（含机械拒绝的 `policy.path_invalid`、`policy.dotgit`、`policy.symlink`、`policy.submodule`、`policy.binary`），以保持 data-model 词表稳定；区分"机械拒绝"与"策略拒绝"看事件：前者只有 `patch.rejected`，后者另有 `policy.denied`。

`repo` 的输出：`PatchFacts`（形状以 [modules/policy.md](../modules/policy.md) 为准：能否干净应用、文件列表及每个文件的模式 / 状态 / 增删行 / 二进制 / 符号链接 / 子模块标记、补丁字节数、总变更行）交 `policy`；最终结论为 `ValidatedPatch`：`{sha256, result_tree, files[{path, mode, status, added, deleted}], verdict: allow | needs_approval | deny, reasons[]}`。`repo` 写 `patch.validated`（allow / needs_approval）或 `patch.rejected`（机械拒绝、不可应用、策略 deny）。`result_tree` 是应用后的 git tree OID，提交只能用它；`sha256` 是补丁内容的摘要，补丁审批（`patch_apply_protected`）的 `subject_digest` 就是它（decisions P-20、P-54）。

提交（verdict = allow，或 `patch_apply_protected` 审批已被 `repo` 消费之后）：`repo` 在项目镜像仓里创建提交，parent = `base_sha`，提交身份取配置 `github.commit_identity`（App 创建时写入，P-69），说明由服务端生成（可由 Utility 模型根据已校验的 diff 生成），带 trailer `Gantry-Attempt: <job_id>`。之后推送由 `forge` Outbox 执行。

### 8.4 proposals 校验（V6）

逐条执行，任一条不通过只影响该条：

| 检查 | 说明 |
| --- | --- |
| `kind` | 在 `output.proposals.allowed_kinds` 内；总数 ≤ `max` |
| `target` | 必须是 JobSpec `targets[].handle` 之一，且 `kind` 兼容；不接受 worker 给出的 GitHub 编号或 node_id |
| `params` | 符合该 `kind` 的 schema；长度上限 |
| 文本净化 | 中和 `@用户` / `@团队` 提及、跨仓库引用、`/gantry` 之类命令串；去 HTML；过密钥扫描（与 P9 同一扫描器）。细则归 [forge-actions.md](forge-actions.md) |
| 依赖 | `open_pr` 需要本 Attempt 的补丁已校验为 `allow`；补丁 `needs_approval` 时一并挂起 |
| 判定 | 交 `policy`：`accepted` / `needs_approval` / `denied`；`accepted` 的由 `engine` 入队 Outbox（I2），幂等键服务端派生 |

### 8.5 校验失败的处理

| 结果 | Attempt | 补丁 | proposals | 事件 / 上报 | 重试 |
| --- | --- | --- | --- | --- | --- |
| `allow` | succeeded | 生成本地提交，交 `engine` 推进阶段 | 逐条判定 | — | — |
| `needs_approval`（M2 起） | succeeded（补丁待批） | 保留为 `held`，**不提交不推送**；`engine` 发起 Gantry 审批（`patch_apply_protected`，`subject_digest` = `ValidatedPatch.sha256`，P-54） | 依赖补丁的挂起 | `approval.requested`；InboxItem `approval_needed` | 批准后由 `repo` 消费审批并提交（审批在提交前消费）；拒绝 → 视同 `deny` |
| `deny`（策略判定） | failed，`policy_denied` | 丢弃（不应用到任何持久检出）；原文隔离留存审计 | 全部丢弃 | `patch.rejected` + `policy.denied`（含 reason 码与路径）。命中 `workflows`（`policy.path_protected:workflows`）：上报 `decision_needed`（`kind = manual_action`，`payload.patch` 指向隔离留存的补丁，供用户手动应用，P-48），M1、M2 相同；M1 命中其他受保护路径（`policy.path_protected:<规则 id>`）上报 InboxItem `escalation`；其他 deny 是否升级见 OQ-5 | 默认不自动重试；是否带反馈重试由 `engine` 决定 |
| `deny`（机械拒绝：P4、P5、P7） | failed，`policy_denied` | 同上 | 全部丢弃 | 只有 `patch.rejected`（`reason` 即子码），不发 `policy.denied`（P-69） | 默认不自动重试 |
| 结构无效（V2–V4、P2） | failed，`exec_failed` | 丢弃 | 丢弃 | `attempt.finished`；P2 另有 `patch.rejected`（`reason = patch_not_applicable`） | 可重试 |
| 部分 proposals 无效 | 不受影响 | — | 逐条 `rejected` 并记原因 | 每条一个 `policy.denied` | — |

M1 的 `policy` v0 把 `needs_approval` 一律按 `deny` 处理并上报（fail-closed，decisions P-19，[milestones M1](../milestones.md#m1-单-issue-走通low-档端到端)），M2 引入路径规则审批后才有 `held` 路径。workflow 改动不在此列：它在任何里程碑都是 `deny`（P-48）。

### 8.6 默认限额

配置项归 [config.md](config.md)，键名与默认值以它为准；下表只是本文用到的限额与 config 键的对照（此前本节的示意键 `worker.*` 已在 [config §7](config.md#7-sandbox) 落位）。

| 用途 | config 键 | 默认 **[建议值，待调优]** |
| --- | --- | --- |
| 补丁文件数 / 总字节 / 单文件字节 / 增删行数（§8.3） | `policies.patch.max_files` / `max_bytes` / `max_file_bytes` / `max_changed_lines` | 200 / 5,000,000 / 1,000,000 / 10000 |
| 墙钟 | `sandbox.limits.wall_time_s`、`models.worker.attempt_wall_s` | 3600、3600；有效 `wall_time` 取二者与预算剩余的最小值 |
| 空闲超时 / SIGTERM 宽限 | `sandbox.idle_timeout_s` / `sandbox.kill_grace_s` | 300 / 10 |
| 进度流（§7） | `sandbox.io.progress.max_line_bytes` / `max_lines` / `max_rate_per_s` | 8192 / 20000 / 20 |
| 产物（§5） | `sandbox.io.artifacts.result_max_bytes` / `report_max_bytes` / `proposals_max_bytes` / `proposals_max_count` | 1 MiB / 256 KiB / 256 KiB / 20 |
| 不可信材料（§4） | `sandbox.io.materials.max_item_bytes` / `max_total_bytes` | 64 KiB / 512 KiB |
| 日志尾部 | `sandbox.io.logs_tail_bytes` | 1 MiB |

## 9. 失败分类

映射到 [data-model](../data-model.md#workflow--node--attempt) 的 `Attempt.failure_class`（`timeout` / `exec_failed` / `verdict_fail` / `policy_denied` / `cancelled` / `budget_exhausted` / `infra_error`）；子码写入 `Attempt.failure_detail`。

| `failure_class` | `failure_detail` | 服务端判定依据 | 重试提示 |
| --- | --- | --- | --- |
| `timeout` | `timeout.wall` | 墙钟计时器触发 | 可重试 1 次；仍超时则上报 |
| `timeout` | `timeout.idle` | 空闲计时器触发 | 可重试 |
| `exec_failed` | `exec.oom` | 运行时报告 OOM | 需调大 `limits` 才有意义 |
| `exec_failed` | `exec.killed` | 非管理器发出的信号、pids 超限、退出码 12 却无取消 / 超时记录 | 可重试 |
| `exec_failed` | `exec.worker_crash` | 退出码非 0 且不是 10 / 12（11 或未定义）；即使有 `result.json`，其 `error` 也只作诊断 | 可重试 |
| `exec_failed` | `exec.bad_job` | 退出码 10 | 不重试，是 `engine` / 镜像不匹配的缺陷 |
| `exec_failed` | `exec.agent_failed` | `result.status = failed`（`task_failed`、`agent_error`、`max_turns`） | 视原因 |
| `exec_failed` | `exec.model_api` | `egress` 记录持续上游错误（5xx / 429）超阈值 | 退避后可重试 |
| `exec_failed` | `exec.protocol` | 退出 0 但 `result.json` 缺失 / 非法；`job_id` 或 `job_sha256` 不符；声明与实际不符 | 可重试 |
| `exec_failed` | `exec.output_invalid` | `structured_output` 不合 schema / 一致性规则；补丁不可应用；引用完整性失败 | 可带反馈重试 |
| `budget_exhausted` | `budget.tokens` | `egress` 报告封顶切断；同时 `budget.exhausted` | 不重试，上报 `budget_exhausted` |
| `infra_error` | `infra.sandbox_create`、`infra.workspace_prep`、`infra.egress`、`infra.collect`、`infra.lost` | 沙盒基础设施步骤失败 | 新沙盒重试 |
| `verdict_fail` | `verdict.revise` / `verdict.reject` / `verdict.blocked` | 结果合法、`status = succeeded`，gate 的 verdict 不是 `ACCEPT` | 不是重试，按 §6.1 进入图的迭代 / 上报 |
| `policy_denied` | `policy.size` / `path_invalid` / `dotgit` / `symlink` / `submodule` / `binary` / `path_outside_allow` / `path_denied` / `path_protected:<规则 id>` / `secret` | §8.3 的 deny（`path_invalid` / `dotgit` / `symlink` / `submodule` / `binary` 是 `repo` 的机械拒绝，其余是 `policy` 的判定） | 默认不重试；`path_protected:workflows` 上报 `manual_action` |
| `cancelled` | `cancel.requested` | 取消先于其他终止原因 | 不是失败，不重试不上报；Node `cancelled` |

**判定优先级**（自上而下，命中即止；用服务端观察到的事实，不用 worker 自报）：

1. 已记录取消请求 → `cancelled`
2. 墙钟或空闲计时器触发 → `timeout`
3. 运行时报告 OOM / 异常信号 → `exec.oom` / `exec.killed`
4. `egress` 报告封顶切断 → `budget.tokens`
5. 退出码 ≠ 0 → 10：`exec.bad_job`；12 而无取消 / 超时记录：`exec.killed`；其他：`exec.worker_crash`
6. 退出码 0 但 V2 / V3 失败 → `exec.protocol`
7. `result.status = failed` → `exec.agent_failed`
8. V4 失败 → `exec.output_invalid`；V5 失败 → `policy.*` / `exec.output_invalid`
9. gate 的 verdict ≠ `ACCEPT` → `verdict.*`
10. 否则成功

`result.error.kind` 只在 5–7 步内作为细分线索。

## 10. 适配层：Claude Code 与预留

### 10.1 适配层契约（任何 agent 都必须满足）

| # | 要求 |
| --- | --- |
| A1 | 启动先校验 `job.json`（`protocol`、`agent`、`constraints`、`output` 全部可识别）和 MANIFEST；否则退出码 10 |
| A2 | JobSpec 是唯一配置来源；仓库内的 agent 配置不得成为权限来源 |
| A3 | 内联到 prompt 的不可信内容按 `isolation` 包装；可信指令只来自 `task.instructions` 与适配层固定前言，且放在 system prompt |
| A4 | 把 agent 的事件翻译成 §7 事件，只用白名单字段，不转发工具输出正文 |
| A5 | 产物由**适配层**生成：`changes.patch`（适配层跑 git，§5.3）、`report.md`、`proposals.json`；`result.json` 最后原子写 |
| A6 | `structured_output` 用 `output.schema.json` 校验；失败至多修复 1 轮（计入预算），仍失败则 `status = failed`、`error.kind = output_invalid` |
| A7 | 处理 SIGTERM / SIGINT：宽限期内终止 agent，尽力写 `status = cancelled` 的结果，退出码 12 |
| A8 | 不开网络、不读 JobSpec 以外的凭据；所有出站只经 Egress（模型调用走带 `job_token` 的 base URL，其余走 `HTTPS_PROXY`）；不把 `job_token` 写进 `out/`、进度事件或日志 |

一致性测试用 `FakeAgent`（按脚本产出事件与文件）验证 A1–A8，不依赖真实模型。

### 10.2 Claude Code（第一实现，P-07）

镜像：`gantry-worker`（单一二进制）+ 按版本固定的 `claude` CLI + 工具链；非 root 运行。**以下命令行标志与环境变量以锁定的 CLI 版本为准，适配层必须钉死版本并有冒烟测试。**

调用示意：

```bash
claude -p \
  --output-format stream-json --verbose \
  --model "$MODEL" \
  --max-turns "$MAX_TURNS" \
  --allowedTools "$ALLOWED" --disallowedTools "$DENIED" \
  --permission-mode "$MODE" \
  --append-system-prompt "$PREAMBLE" \
  --strict-mcp-config --mcp-config '{"mcpServers":{}}' \
  --setting-sources user \
  < "$PROMPT_FILE"
```

**JobSpec → CLI**

| JobSpec | Claude Code |
| --- | --- |
| `execution.model` | `--model` |
| `execution.reasoning_effort` | 思考预算开关（如 `MAX_THINKING_TOKENS` 或所锁版本的等效参数）；`none` 关闭；各档对应的预算在 worker 配置表里 |
| `constraints.limits.max_turns` | `--max-turns` |
| `constraints.tools`、`paths.deny` | `--allowedTools` / `--disallowedTools` 与权限规则（对 `.git/**`、workflow 路径的编辑 deny）。这是纵深防御，**不承担安全保证**，外层沙盒才是边界 |
| `task.instructions` + 固定前言 | `--append-system-prompt` |
| `materials`、`upstream` | 用户 prompt 内联；大文件给路径让 agent 用 Read 读（文件自带隔离标记） |
| `output.structured_schema` | `--json-schema`（若所锁版本支持），否则 prompt 约束 + 适配层校验 |
| `execution.agent_options` | 白名单字段，只能收紧 |

**按 role 的权限档**（示意）

| role | 编辑 | Bash | 备注 |
| --- | --- | --- | --- |
| `build` | 是 | 是 | — |
| `verify` | 否（构建产物写入由 Bash 完成） | 是 | 不产生补丁 |
| `single`、`heal` | 是 | 是 | 同 `build` |
| `review:*`、`plan`、`reasoner`、`arbiter`、`answer`（`sandbox_ro`） | 否 | 只读命令（`git diff` / `log` / `show`、`grep`） | 不产生补丁 |

**stream-json → 进度事件**

| CLI 消息 | 事件 |
| --- | --- |
| `system` init | `hello` |
| `assistant` 的 `tool_use` | `tool(start)`，并按工具类别推断 `stage` |
| `user` 的 `tool_result` | `tool(end)`，只取 `ok` 与耗时 |
| `assistant` 的 usage | `usage`（累计） |
| `result` | `finish`；`subtype` 映射 `error.kind`（如 max-turns → `max_turns`，执行错误 → `agent_error`）。CLI 给出的费用字段**忽略**（计费以代理计量为准） |

**需要特别处理的点**

| 项 | 处理 |
| --- | --- |
| 仓库内 `.claude/` 配置、hooks、`.mcp.json`、插件 | 属仓库内容（I5），不得成为权限来源：只加载适配层写入 `HOME` 的配置，禁用项目级 hooks / MCP。仓库的 `CLAUDE.md` / `AGENTS.md` 是否自动载入见 OQ-17 |
| API key 与 base URL | 沙盒内无 key，但 CLI 需要有值才启动：设占位值（如 `ANTHROPIC_API_KEY=gantry-egress-managed`，**不是密钥**）；`ANTHROPIC_BASE_URL` 设为 `http://<egress>/j/<job_token>/<provider_id>`，Egress 校验令牌、丢弃占位 key 并注入真实 key（§2.3 E3、E4） |
| 遥测 / 自动更新 / 反馈 | 用环境变量关闭（如 `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`、`DISABLE_AUTOUPDATER`、`DISABLE_TELEMETRY`）；这些流量本来也会被白名单挡掉，关闭是为了避免启动重试拖慢 |
| 会话持久化 | 写入 `/gantry/home`（tmpfs）；一个 job 一个会话；A6 的修复轮用同会话续接 |
| 上游错误重试风暴 | 代理按 job 限速并封顶；适配层把连续上游错误计入 `exec.model_api` |
| 子 agent / 并行工具 | 共用同一代理与 token 封顶 |
| 无 TTY | `-p` 非交互 |

### 10.3 预留：Codex 等

| 协议元素 | Claude Code | 其他 agent |
| --- | --- | --- |
| 启动 | `claude -p … stream-json` | 各自的非交互模式（如 `codex exec --json`，若 CLI 提供） |
| 事件翻译 | §10.2 | 适配层各自翻译，输出同一套 §7 事件 |
| 结构化输出 | `--json-schema` / 适配层校验 | CLI 的 schema / 最后消息文件（若提供）+ 适配层校验 |
| 权限 | CLI 权限规则 | 关闭 CLI 自带沙盒与审批，以外层沙盒为边界；仍下发最小权限 |
| 认证 | 占位 key + base URL 指向 Egress 反向代理（路径带 `job_token`） | 同；CLI 必须允许把 base URL 设为带路径前缀的地址；若只支持订阅登录则不能无人值守（Q-03） |
| 网络 | 模型 API 主机在 `egress_profile` 内 | 把该提供方的 API 主机加入对应 `egress_profile` |

**不得变化**：JobSpec / `result.json` / 事件 / 补丁格式 / 退出码 / 服务端校验。新增 agent 只新增一个适配层和一个 `execution.agent` 取值。

## 11. 衔接

### 11.1 `schemas/worker/`（M0 交付，`contract` 泳道）

| 文件 | 内容 |
| --- | --- |
| `job.schema.json` | JobSpec |
| `result.schema.json` | `result.json` 外壳 |
| `output.<schema>.schema.json` | `plan`、`build`、`verify`、`review`、`reasoner`、`arbiter`、`answer`（沙盒角色）与 `admission`、`summarize`、`intent`、`classify`（`direct` 角色，由 `models` 校验）各一份 |
| `proposals.schema.json` | `proposals.json` |
| `progress-event.schema.json` | 进度事件 |
| `fixtures/` | 黄金 `out/` 目录与 stdout 录像，供 `FakeSandboxService` 回放和一致性测试 |

本文所有示例 JSON 应作为这些 schema 的正例测试数据。

<a id="output-schema-registry"></a>
### 11.1.1 结构化输出 schema 的可信来源

服务端以 `gantry_core::worker::output_schema(id: &str) -> Option<&'static [u8]>` 为共同的可信只读来源（D-20）。它精确匹配 `gantry.worker.output.<kind>/1`，其中 `kind` 仅为 `plan`、`build`、`verify`、`review`、`reasoner`、`arbiter`、`answer`、`summarize`、`admission`、`intent`、`classify`。未知 ID、大小写变体和未知 major 返回 `None`；`single` 与 `heal` 沿用 §4.3 的 `build`，不新增别名。可查询全部 11 种格式不代表 M1 引擎启用其他角色。

每项返回编译时内嵌的 `schemas/worker/output.<kind>.schema.json` 原始字节，包含文件结尾换行；不在运行时读文件、重新序列化或做 JCS 规范化。`xtask` 仍是这些文件的唯一生成源，`cargo --locked xtask schemas --check` 检查生成文件漂移。引擎与沙盒使用同一版本的这些字节；`output.structured_schema.sha256` 是对原始字节计算的 `sha256:` 摘要，`path` 仍为 `/gantry/inputs/output.schema.json`，线上 ID、路径和摘要格式不变。

沙盒生产 `OutputSchemas` 适配器委托该入口；`StaticSchemas` 只用于测试。M1 常规冷启动路径在 worker 启动前校验 ID、路径与摘要：未知 ID 或摘要不符返回 `InvalidSpec`，不运行 worker；可以已经创建容器，须按既有生命周期清理。M2 预热池仍按 §3.2 的提议处理，不由本规则启用。本规则不启用插件、动态注册或仓库配置来源。注册表与接线仍需后续实现，真实 `gantryd` 装配仍为 `NotReady`，由 #105 跟踪。

实现验收须逐项比较 11 个 ID 的查询结果与对应生成文件原始字节及其摘要，并验证未知 ID、大小写变体、未知 major 和 `single` / `heal` 别名均返回 `None`。

### 11.2 事件

| 事件 | 状态 | 说明 |
| --- | --- | --- |
| `node.started`、`attempt.started`、`sandbox.created`、`sandbox.destroyed`、`attempt.finished`、`node.finished`、`policy.denied`、`approval.requested`、`budget.charged`、`budget.exhausted` | 已在 [data-model §5.2](../data-model.md#52-事件类型目录) | 本文直接使用 |
| `attempt.progress` | 已在 data-model §5.2 | 采样节流后转发给客户端，仅展示 |
| `patch.validated`、`patch.rejected` | 已在 data-model §5.2 | 由 `repo` 写的补丁校验结论（机械拒绝只有 `patch.rejected`，P-69）；提交由 `attempt.finished` 的 `result_tree` 幂等重建（§3.5） |

### 11.3 一致性与对抗测试

| 测试 | 期望 |
| --- | --- |
| 黄金路径（FakeAgent 产出合法四件套） | `succeeded`，补丁生成提交 |
| 沙盒内访问 `github.com`、`api.github.com` | 失败（M1 退出标准） |
| 沙盒内 `env`、文件系统搜索模型 key | 无（M1 退出标准） |
| 补丁改 `.github/workflows/x.yml` | M1、M2 都 `policy_denied`（`policy.path_protected:workflows`），上报 `decision_needed`（`manual_action`）并附补丁（P-48，M1 / M2 退出标准） |
| 补丁改 `Cargo.lock` | M1 `policy_denied`（`policy.path_protected:lockfiles`）并上报；M2 `needs_approval`（`patch_apply_protected`） |
| 补丁含符号链接 / 子模块 / `.git/hooks/*` / `a/../../x` | `policy_denied`，对应 detail；只有 `patch.rejected`，没有 `policy.denied`（机械拒绝） |
| 补丁含大小写不同的 `.GIT/config`、`GIT~1` | `policy.dotgit` |
| 补丁含二进制 | `policy.binary`（一律拒绝，P-68） |
| 补丁超大 / 文件数过多 | `policy.size` |
| 补丁基于错误的 `base_sha` / 上下文不匹配 | `exec.output_invalid` |
| `out/` 里有符号链接 `result.json -> /etc/passwd`、FIFO、额外文件 | 丢弃，不跟随，不阻塞 |
| `result.json` 缺失 / 非法 / `job_id` 不符 / 声明哈希不符 | `exec.protocol` |
| 退出码 0 但 `status = failed` | `exec.agent_failed` |
| 伪造 stdout 事件（乱序 `seq`、超长行、未知类型、虚报 usage） | 丢弃或仅展示，账本不变 |
| 材料正文含伪造的结束标记 / 指令 | 标记不生效；nonce 冲突时重新生成 |
| 墙钟超时、空闲超时、取消 | 对应分类；SIGTERM → SIGKILL；产物不被采纳 |
| 服务端在 `collect` 之后 / 提交之前崩溃 | 重启后恢复，不重复提交 |
| `egress` 封顶 | `budget_exhausted`（`budget.tokens`），`budget.exhausted` |
| 尝试结束后用同一 `job_token` 再请求模型；从沙盒网段以外使用 `job_token`；用伪造的令牌 | 全部被 Egress 拒绝，不计入任何尝试（P-49） |
| proposals 指向 `targets` 以外的句柄 / 含 `@` 提及 | 该条被拒，其余不受影响 |

## 12. 开放问题

骨架文档没有定的点。"正文暂用"列说明本文为了能写出 schema 所做的临时处理，**不是决定**。

| # | 问题 | 影响 | 候选 | 正文暂用 |
| --- | --- | --- | --- | --- |
| OQ-1 | ~~planner / utility 档的 role 是否走沙盒~~ | — | — | 已决：见 decisions.md P-17（§4.3） |
| OQ-2 | ~~Egress 如何注入 key 与按 job 计量~~ | — | — | 已决：见 decisions.md P-16；沙盒身份改为 `job_token`，见 decisions.md P-49；控制面见 decisions.md P-50（§2.3） |
| OQ-3 | ~~补丁闸门归哪个模块~~ | — | — | 已决：见 decisions.md P-25（`repo`） |
| OQ-4 | 符号链接、二进制已决：见 decisions.md P-68（一律拒绝，无白名单）。仍待定：子模块、`.gitmodules` 是否可经审批放行；真实仓库的图片等二进制资源如何供给已转为 decisions.md Q-09（推荐：第一版一律拒绝，M3 起经审批放行） | P5 的子模块判定 | 一律拒 / 可审批 | 子模块一律 deny |
| OQ-5 | 补丁审批的绑定（`subject_digest` = 补丁 sha256）与 M1→M2 切换已决：见 decisions.md P-19、P-20；审批动作 `patch_apply_protected` 已决：见 decisions.md P-54；workflow 补丁一律拒绝并以 `manual_action` 上报已决：见 decisions.md P-48。仍待定：受保护路径以外的 `deny` 是否一律升级 InboxItem；`.github/` 下 `actions/`、`CODEOWNERS`、`dependabot.yml`、`.gitattributes` 是否进规则表（config.md 的 `repo_meta` 是建议） | 上报噪音；规则表范围 | — | 仅受保护路径的 M1 拒绝上报 |
| OQ-6 | ~~`reasoner` 是否为 gate~~ | — | — | 已决：见 decisions.md P-45（`reasoner` 不输出 verdict，§6.2 已删去该字段）；REJECT / BLOCKED 见 P-24，重新规划回到 `plan` 见 P-55 |
| OQ-7 | ~~data-model 缺口（`failure_class`、`Attempt.status`、`usage`、`Sandbox.status`、`worktree_snapshot`）~~ | — | — | 已决：见 decisions.md P-22（§9） |
| OQ-8 | 词表缺失：Finding 的 `severity` / `category`、review 视角清单（Low 单节点与 High 汇总节点的 role 已决：见 decisions.md P-17） | §6 schema；guardian 分流依赖 `category` 和 `nit` | — | 见 §6.1，标 **[提议]** |
| OQ-9 | proposals 词表（已与 forge-actions §3.5 对齐，`heal` 的 role 已决：见 decisions.md P-17）。仍待定：是否需要 worker 提议 `create_issue`（后续跟进）；review 评论由谁发（本文假设只由 `arbiter` / `summarize` 的结果经 `engine` 发） | §5.4 | — | 封闭小词表，不含危险动作与自由评论 |
| OQ-10 | 是否需要保留多提交粒度；提交说明来源（`classify` 生成 vs `build` 建议）。提交由 `repo` 创建、身份取 `github.commit_identity` 已决：见 decisions.md P-69 | §5.3、§8.3 | 服务端按 plan step 切分提交 | 单补丁、单提交，说明服务端生成 |
| OQ-11 | ~~事件目录缺 `attempt.progress`、补丁校验事件~~ | — | — | 已决：见 decisions.md P-21（§11.2） |
| OQ-12 | 内容寻址结果库已列入 architecture §6（decisions.md P-25）。仍待定：保留期；被拒产物（quarantine）保留多久 | 存储增长 | — | — |
| OQ-13 | 镜像仓由 `repo` 维护、`forge` 拉取更新（decisions.md P-25）。仍待定：历史深度、submodule / LFS 策略 | `repo` 的工作区准备；review 需要的历史 | 浅克隆 + 服务端生成 `diff.patch` | 见 §2.4 |
| OQ-14 | 单次 Attempt 的 `tokens_total` 如何从 BudgetLedger 预留 / 回收；封顶是硬切断还是软限；并发 Attempt 争用预算（token 加权已决：见 decisions.md P-70，`levels.token_weights`） | `engine`、`egress` | — | 硬切断，按加权 token 计 |
| OQ-15 | 包仓库白名单本身是低带宽外泄通道（URL 路径 / 查询串 / 发布）；是否改为 `egress` 内置只读镜像 / 缓存代理。依赖指向 `github.com` 源默认拒绝，见 decisions.md Q-08；`CONNECT` 如何识别尝试已决（P-72，`Proxy-Authorization` 携带 `job_token`，审计按尝试归集） | I1 的实际强度 | 只读缓存代理 | 未处理 |
| OQ-16 | 预热池（M2）下"先启动、后注入"用 `READY` 文件标记是否接受 | worker 启动逻辑；M2 | — | §3.2，**[提议]** |
| OQ-17 | 仓库内的 `CLAUDE.md` / `AGENTS.md` 是否自动载入 agent 上下文：它们对 agent 有用，但按 I5 属于"仓库内容" | 适配层的 prompt 组装 | 作为 `file_excerpt` 材料包装后注入；或完全不载入 | 未处理，适配层按 I5 不赋予其权限 |

## 13. 事件 data 契约

13 个事件 data 的字段见 [event-data.md](event-data.md)（已批准（2026-10-27，用户统一批准））。运行时按 `core` 事件目录的实际落地情况校验：已落地 data 校验的事件按本契约校验，尚未落地的仍按 fail-closed 拒绝；本文其余语义不变。
