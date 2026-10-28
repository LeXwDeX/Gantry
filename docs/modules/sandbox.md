# 沙盒管理（`sandbox`）

> 状态：设计 v0.1 · 里程碑：M1 起（M2 / M3 增量） · 泳道：`exec`（见 [milestones.md](../milestones.md)） · crate / 目录：`crates/gantry-sandbox/`

> **协作扩展边界（提议）** —— 多工作树协作的完整语义见 [collaboration.md](../collaboration.md)；**设计提案，待单独契约批准，不替换本文现行规格**。
>
> - 跨树材料只作为服务端中介提供的固定快照进入 `SandboxSpec`；本模块不挂载其他工作树，也不接受指向其他树的活动路径。
> - 输入在 `stage` 时一次性固定；运行中不改 `job.json` / `inputs`。`inputs/READY` 仍只是就绪标记，不携带数据。
> - 先停后取：停止并确认无残留进程后才 `collect`；stdout 只用于展示，材料与产物一律按不可信数据处理。
> - 本提案不新增挂载、容器面方法或引擎面方法。

`sandbox` 把一次 Attempt 关进一个一次性、只读根、无凭据、只能经 Egress 出网的容器里运行，监督它直到停止，再按白名单取回产物并销毁。它是 [Worker 协议](../interfaces/worker-protocol.md) 在服务端一侧的执行者：沙盒与服务端之间只有文件、worker 的 stdout 和运行时信号（decisions P-15）。

> 标注 **[提议]** 的是本文为了能写出接口与测试而给的默认，骨架没定；标注 **需核实** 的是作者没有确认的外部行为（Podman / gVisor / 内核）；标注 OQ-S*n* 的点见 §13，实现者不得自行拍板。

## 1. 职责

| # | 职责 | 依据 |
| --- | --- | --- |
| S1 | **两层接口**（P-52）：引擎面 `SandboxService`（定义在 `core::ports`：提交 JobSpec、等待结果、取消、恢复、巡检；`engine` 只依赖它）和容器面 `SandboxDriver`（本模块内部：rootless Podman + gVisor 的创建 / 启动 / 停止 / 日志 / 销毁）；驱动可替换 | architecture §10–§11；decisions P-05、P-51、P-52 |
| S2 | **生命周期**：`create → stage → start → 监督 → stop → collect → destroy`，每一步幂等，可从崩溃中恢复 | [worker-protocol §3.2、§3.5](../interfaces/worker-protocol.md#32-阶段) |
| S3 | **注入（stage）**：向 `repo` 要工作区副本；写 `job.json`、`inputs/`（不可信材料的规范化、截断、nonce、隔离标记、`MANIFEST.json`）；建空 `out/` | worker-protocol §4.4、§2.1 |
| S4 | **隔离与资源限制**：只读根、非 root、丢弃全部能力、私有命名空间、无设备、cgroup 限额（`cpu` `mem` `disk` `pids` `wall_time`）；沙盒网络只通 Egress | I1；[config §7](../interfaces/config.md#7-sandbox)；data-model [Sandbox](../data-model.md#worktree--sandbox) |
| S5 | **计时与终止**：墙钟超时、空闲超时、取消、SIGTERM → 宽限 → SIGKILL；"先记录者胜"；确认容器内全部进程已终止 | worker-protocol §3.4 |
| S6 | **进度采集**：经运行时日志接口读 worker 的 stdout / stderr，严格校验、限速、去注入，写 `progress.jsonl`，转发采样节流后的 `attempt.progress` | worker-protocol §7 |
| S7 | **产物提取（intake）**：进程停止后从 `out/` 取白名单四个文件，不跟随链接、不被 FIFO 阻塞、读到"上限 + 1"即判超限，落 CAS | worker-protocol §8.2（V2） |
| S8 | **与 Egress 协作**：建容器前经 `EgressControl::issue_job_token` 为本尝试申请 `job_token`（带白名单档案、token 上限、权重），把它写进沙盒环境（模型 base URL 路径与 `HTTPS_PROXY`）；`collect` 后 `revoke_job_token` 取最终计量并作废令牌 | worker-protocol §2.3（E1–E7）、§3.2；decisions P-16、P-49 |
| S9 | **并发与排队**：宿主机上同时存在的沙盒总数不超过 `max_concurrent_sandboxes`，超出排队不失败 | config §7 |
| S10 | **巡检与恢复**：`gantryd` 重启后接管仍在运行的沙盒；销毁没有对应未终结 Attempt 的孤儿沙盒与残留 staging | worker-protocol §3.5 |
| S11 | **预热池（M2）**、**`devcontainer.json` 安全子集（M2）**、**逃逸检测告警（M3）** | milestones M2 / M3 `exec` |

### 里程碑交付

| 里程碑 | 本模块交付 |
| --- | --- |
| M0 | 无实现。评审并冻结 §6 的类型与 trait（`core::ports::SandboxService`、`SandboxSpec`、`JobOutcome`；本模块内部的 `SandboxDriver`、`ContainerSpec`）与 `sandbox.created` / `sandbox.destroyed` 的 `data`；`contract` 泳道交付的 `FakeSandboxService`（实现引擎面 `SandboxService`，按脚本返回 Worker 协议结果，P-52）须通过本文 §12.A 的一致性用例 |
| M1 | S1–S10：Podman 驱动（`runc` 为必选，`runsc` 同一份代码路径、环境具备即可跑）、stage 与材料打包、挂载与资源限制、计时器与取消、进度采集、intake / collect、销毁、崩溃恢复与孤儿巡检、与 `egress` 的 `job_token` 签发 / 计量 / 吊销、驱动一致性套件、对抗用例。验收退出标准的两条："沙盒内访问 `github.com` / `api.github.com` 失败"和"沙盒内读不到任何模型 key" |
| M2 | gVisor（`runsc`）默认开启并通过全部隔离用例；预热池（`READY` 文件，OQ-S8）；`devcontainer.json` 安全子集（忽略 `initializeCommand`、`mounts`、`runArgs`、特权字段，OQ-S7）；`sandbox_ro` 的只读工作区挂载 |
| M3 | 出网审计回放所需的 `job_id` / `sandbox_id` 关联查询入口（审计本体在 `egress`）；沙盒逃逸检测告警（信号清单 OQ-S13）；密钥轮换对本模块无影响（沙盒内无密钥） |
| M4 | 无新增接口。Ultimate 多交付单元并行下的排队公平性、`max_concurrent_sandboxes` 与 Egress 容量压测 |

## 2. 不负责

| 事项 | 归属 |
| --- | --- |
| 准备工作区副本的内容（git 操作）、补丁校验与提交 | `repo`（`sandbox` 只给 `dest` 目录、挂载结果、之后销毁） |
| 在沙盒里运行 agent、生成 `changes.patch` / `result.json` / `proposals.json` | `worker`（沙盒内） |
| `result.json` / `structured_output` / proposals 的校验（V3、V4、V6）与失败分类、重试、写 `attempt.finished` | `engine`（`sandbox` 只给事实与原料，见 §5） |
| 预算预留与回收、`BudgetLedger`、`budget.charged` | `engine`（`sandbox` 只取 Egress 计量并交出） |
| 模型 key、白名单实现、token 封顶的强制 | `egress` |
| 路径规则、审批、放行判定 | `policy` |
| 事件日志与 CAS 的存储实现 | `store`（CAS 归属见 OQ-S10） |
| GitHub 任何读写 | `forge`（沙盒内没有 GitHub 网络，I1） |
| 镜像（基础镜像、worker 镜像）的构建与发布 | `worker` / `deploy`（镜像组成见 OQ-S5）；`sandbox` 只校验 digest 并 `--pull=never` |

## 3. 依赖

规格书与本文同批编写，对方文件若尚不存在，以 `interfaces/` 契约为准。

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `core` | 端口 `core::ports::SandboxService`（本模块实现）、`RepoService`、`EgressControl`（本模块调用），P-51；`SandboxId` `AttemptId` `ProjectId`、`UntrustedText`、`TokenUsage`、`Level`、`JobSpec` / `ProgressEvent` 类型与 JSON Schema 校验 | data-model；`schemas/worker/`；[core.md §6.10](core.md) |
| `repo` | `RepoService::prepare_workspace`（并取 `diff.patch`） | [repo.md §6.2](repo.md#62-沙盒工作区) |
| `egress` | 控制面 `EgressControl`（经 `/run/gantry/egress.sock`，P-50）：`endpoints`、`issue_job_token`、`job_usage`、`revoke_job_token`、`forget_job`、`status` | [egress.md §6](egress.md#6-对外接口)；[interfaces/egress-control.md](../interfaces/egress-control.md) |
| `store` | 经 `WriteTx::append` 追加 `sandbox.created` / `sandbox.destroyed` / `attempt.progress`；`Cas::put` / `put_file`（`CasKind::Artifact` / `Progress`）写原始产物、日志尾部、`progress.jsonl` | [store.md §6.1、§6.4](store.md)；data-model §5；architecture §6 |
| 宿主机（`deploy` 提供） | 系统用户 `gantry-sbx`（subuid / subgid 各 65536、`enable-linger`）、`podman system service` 的套接字 `/run/gantry-sbx/podman.sock`（属组 `gantry-sbxctl`）、rootless Podman（版本 **需核实**）、`runsc`（gVisor，**需核实**版本与 rootless 兼容性）、cgroup v2、主机防火墙（沙盒用户只能到 Egress）、Linux ≥ 5.6（`openat2`） | [deploy.md §6.2、§6.3、§7.8](deploy.md)；architecture §10 |

依赖：`core`、`repo`、`egress`（`job_token` 与计量，worker-protocol §3.2）、`store`（事件、CAS），与 architecture §4 一致（P-59）。`worker` 依赖 `sandbox`（它在沙盒里跑），所以 `sandbox` **不**依赖 `worker` crate：启动命令、环境变量、镜像由 `engine` 从 `WorkerAdapter` 取出后放进 `SandboxSpec.exec`。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| `SandboxSpec` | `engine` | §6.1；`job` 是 JobSpec 的可信部分，`raw_materials` 是待包装的原始文本 | 可信结构；其中 `raw_materials[].text` 与 `upstream` 是**不可信数据**（I5） |
| 工作区副本 | `repo.prepare_workspace` | 目录 | 仓库内容为不可信数据 |
| `job_token` 与计量 | `egress` 控制面 | §6 / egress.md | 可信；`job_token` 是沙盒可见的能力凭证（只对本尝试记账有效） |
| 容器 stdout / stderr | 运行时日志接口 | JSON Lines | **不可信自报**，只用于展示（W3） |
| `out/` 目录内容 | 容器停止后的宿主机目录 | 四个白名单文件 | **不可信**，逐项校验后才被采纳（W2） |
| 运行时事实 | Podman `inspect` / `wait`、cgroup 事件 | 退出码、信号、OOM 标志、状态 | 服务端自己观察到的事实（W3） |
| 取消请求 | `engine` | `cancel(job_id)` | 可信 |
| 配置 | `sandbox.*`（建沙盒时固化）、引导配置 | [config §7](../interfaces/config.md#7-sandbox) | 可信 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| `JobOutcome`（终止事实、白名单产物的 CAS 引用、`unexpected` 列表、日志尾部引用、`progress_ref`、`usage_metered`、Egress 标志） | `engine` | §6.1。`engine` 在其上做 V1、V3、V4，把 `repo` 的 V5 结论与分类合成最终 `ResultBundle` 并写 `Attempt.result_ref`（OQ-S10） |
| `sandbox.created` / `sandbox.destroyed` | `store` | [data-model §5.2](../data-model.md#52-事件类型目录) |
| `attempt.progress`（采样节流，仅展示） | `store` → 客户端 | 同上 |
| `ProgressSample` 流 | `engine`（`SandboxService::progress` 订阅，仅展示） | §6.1 |
| 原始产物、`progress.jsonl`、stdout / stderr 尾部 | CAS | 内容寻址对象 |
| 指标与日志 | 运维 | §11 |

## 6. 对外接口

### 6.1 引擎面（`core::ports::SandboxService`，P-51、P-52）

`SandboxService` 定义在 `core::ports`，签名以本节为准（core.md §6.10 引用）；`engine` 只依赖它，不接触 `SandboxDriver`。M0 的 `FakeSandboxService` 实现同一个 trait。

```rust
#[async_trait]
pub trait SandboxService: Send + Sync {
    /// 受理一个 job（JobSpec 的可信部分 + 待包装材料）。按 job_id 幂等：重复提交返回同一个 JobHandle。
    /// 返回即"已受理（可能仍在排队）"，不表示容器已创建。返回 Ok 之前受理记录（相位 Accepted 的 meta.json）已原子写盘，
    /// 重启后仅新格式且证明从未启动时可按准备重入例外返回 Unknown（E4）。
    async fn submit(&self, ctx: &Ctx, spec: SandboxSpec) -> Result<JobHandle, SandboxError>;
    /// 等待终态。可重复调用、可在 recover 之后调用；同一 job 恒返回同一结果。
    /// Err 包括启动前失败及已受理但无法可信恢复的 JobIdentityUnavailable。
    async fn wait(&self, job_id: &AttemptId) -> Result<JobOutcome, SandboxError>;
    /// 采样节流后的展示数据；丢包无影响；订阅晚了只拿到之后的样本。
    fn progress(&self, job_id: &AttemptId) -> BoxStream<'static, ProgressSample>;
    /// 取消。信号 + 计时器是唯一控制手段。create / stage 阶段直接 destroy。幂等。
    async fn cancel(&self, ctx: &Ctx, job_id: &AttemptId) -> Result<(), SandboxError>;
    /// engine 结算与 attempt.finished 提交后调用：删除 meta.json，并让 egress 忘记该 job 的最终用量（forget_job）。
    async fn release(&self, ctx: &Ctx, job_id: &AttemptId) -> Result<(), SandboxError>;
    /// gantryd 重启：对每个没有 attempt.finished 的 Attempt 查运行时并接管（§7.10）。
    /// 请求先按任务身份契约整批预验证；有效请求按输入顺序每项返回一项 RecoveredJob（E4）。
    async fn recover(&self, ctx: &Ctx, unfinished: &[UnfinishedAttempt]) -> Result<Vec<RecoveredJob>, SandboxError>;
    /// 销毁没有对应未终结 Attempt 的沙盒与残留 staging。
    async fn sweep_orphans(&self, live: &BTreeSet<AttemptId>) -> Result<SweepReport, SandboxError>;
    async fn capacity(&self) -> CapacitySnapshot;  // 运行中 / 排队中 / 上限 / 预热池，供调度与展示
}

/// 不含任何运行时类型（core 不依赖 tokio）；结果经 wait / progress 取得。
pub struct JobHandle { pub job_id: AttemptId, pub sandbox_id: SandboxId }

/// recover 对单个未终结尝试的判定（engine.md §7.14.6 E4），分类规则见 §7.10。
pub enum RecoveredJob {
    Running(JobHandle),            // 已受理且仍在推进，包括拒绝收尾 → engine 继续 wait
    Exited(JobHandle),             // 已停止 → wait 稳定返回 JobOutcome 或 JobIdentityUnavailable
    Lost(JobHandle),               // 受理后丢失：容器不在且产物不可提取 → wait 返回 Infra{Lost}，engine 终结为 infra.lost
    Unknown { job_id: AttemptId }, // 从未受理，或新格式证明从未启动并持久重置 Accepted → 以相同 ID 重新 submit 完整 SandboxSpec
}

pub struct SandboxSpec {
    pub job_id: AttemptId,                 // == Attempt.id == JobSpec.job_id
    pub sandbox_id: SandboxId,             // 预分配，使 submit 幂等
    pub project_id: ProjectId,
    pub delivery_id: Option<DeliveryId>, pub conversation_id: Option<ConversationId>,  // 二者恰有一个；None 档会话上的临时图只有 conversation_id（P-58）
    pub job: JobSpecTrusted,               // job.json 中除 materials[] 元数据与 isolation.nonce 外的全部可信字段
    pub raw_materials: Vec<RawMaterial>,   // {id, kind, source(仅标识符), text: UntrustedText}
    pub upstream: Vec<UpstreamInput>,      // {node_id, role, attempt_id, ref: CasRef, files[]}：从 CAS 复制到 inputs/upstream/<node_id>/
    pub exec: ExecSpec,                    // {image_digest, argv, env, token_env}：engine 取自 WorkerAdapter；env 里没有任何密钥与令牌
    pub driver: DriverKind, pub trust: Trust,   // Trust: Trusted | Untrusted（OQ-S6）
    pub limits: SandboxLimits,             // == Sandbox.limits：cpu, mem, disk, pids, wall_time（与 JobSpec.constraints.limits 同源）
    pub timers: Timers,                    // {idle_timeout_s, kill_grace_s}，来自 sandbox.* 配置
    pub io: IoLimits,                      // sandbox.io.*：进度、产物、材料、日志尾部上限
    pub egress: EgressRequest,             // {profile: ProfileName, groups: Vec<GroupName>（解析后的集合）, tokens_total: u64, weights: TokenWeights（levels.token_weights，P-70）}
    pub workspace: WorkspaceRequest,       // {repo, base_sha, pr_base_sha?, mode: Rw | Ro}；Ro 对应 exec = sandbox_ro
    pub config_rev: Seq,                   // 引擎组装本 spec 时 ConfigView 的 rev（egress.weights 所属快照）；原样填入 IssueJobToken.config_rev，不取签发时刻的生效修订（#220）
}

pub struct JobOutcome {
    pub job_id: AttemptId, pub sandbox_id: SandboxId,
    pub job_sha256: Option<Sha256Digest>,  // stage 最终 job.json 原始字节的可信摘要；None 表示不可用
    pub terminal: TerminalFacts,           // == ResultBundle.terminal
    pub partial: bool,                     // 超时 / 取消 / 被杀后提取：仅供诊断
    pub files: BTreeMap<ArtifactName, CasObject>,   // 只可能有 result.json / changes.patch / report.md / proposals.json
    pub unexpected: Vec<UnexpectedArtifact>,        // {name, reason}：丢弃的文件，进 ResultBundle.validation.unexpected_artifacts（M1 可选，语义 M2）
    pub progress: Option<CasObject>, pub progress_stats: ProgressStats,
    pub stdout_tail: CasObject, pub stderr_tail: CasObject,
    pub usage_metered: TokenUsage,         // egress 计量：input / output / cache_read / cache_write / requests
    pub weighted_tokens: u64,              // 按 levels.token_weights 加权（P-70）
    pub egress: EgressFlags,               // {capped: bool, upstream_error_summary, incomplete: bool}
    pub timings: Timings,                  // created / staged / started / stopped / collected（服务端观测）
}
pub struct TerminalFacts {
    pub cause: TerminalCause,              // Exited | KilledTimer | KilledCancel | KilledRuntime | Lost
    pub exit_code: Option<i32>, pub signal: Option<i32>, pub oom: bool,
    pub timer: Option<TimerKind>,          // Wall | Idle
    pub cancel_requested: bool,
}

pub enum SandboxError {
    Infra { detail: InfraDetail, message: String },   // InfraDetail: SandboxCreate | WorkspacePrep | Egress | Collect | Lost（对应 infra.*，新沙盒重试）
    JobIdentityUnavailable { message: String, usage: Option<Box<FinalUsage>> }, // 已受理但不可可信恢复；稳定终态
    InvalidSpec(String),     // 引擎侧缺陷：image 未固定 digest、limits 为 0、source 不符模式、job.json 不通过 schema……不重试
    NotFound, ShuttingDown,
}
```

`submit` 之后 `wait(job_id)` 返回 `Result<JobOutcome, SandboxError>`；普通启动前失败返回 `Err`，超时、被杀、产物缺失通常返回 `Ok(JobOutcome)`。已受理任务无法可信恢复且无可靠 outcome 时，返回 `JobIdentityUnavailable`；其计量、终结和禁止重试规则见 [任务身份契约](../interfaces/worker-protocol.md#job-identity)。

### 6.2 容器面（本模块内部，不对 `engine` 暴露，P-52）

```rust
#[async_trait]
pub trait SandboxDriver: Send + Sync {
    fn capabilities(&self) -> DriverCaps;   // { runtime, oom_reporting, disk_quota, supports_pool, ... }
    async fn image_present(&self, digest: &ImageDigest) -> Result<bool, DriverError>;
    async fn create(&self, spec: &ContainerSpec) -> Result<ContainerId, DriverError>;   // 未启动
    async fn start(&self, id: &ContainerId) -> Result<(), DriverError>;
    /// SIGTERM → 等待 grace → SIGKILL（容器级）。返回时容器必须已处于 exited，且其 PID 命名空间内没有残留进程。
    async fn stop(&self, id: &ContainerId, grace: Duration) -> Result<(), DriverError>;
    async fn kill(&self, id: &ContainerId) -> Result<(), DriverError>;                  // 立即 SIGKILL
    async fn wait(&self, id: &ContainerId) -> Result<ExitFacts, DriverError>;           // {exit_code, signal, oom_killed, finished_at}
    async fn inspect(&self, id: &ContainerId) -> Result<ContainerState, DriverError>;
    /// 主机侧日志接口。从 cursor 起读，可重复读（崩溃恢复靠 seq 去重）。不挂 socket、不开端口。
    fn logs(&self, id: &ContainerId, since: Option<LogCursor>) -> BoxStream<'static, Result<LogChunk, DriverError>>;
    async fn remove(&self, id: &ContainerId) -> Result<(), DriverError>;                // 幂等
    async fn list(&self, selector: &LabelSelector) -> Result<Vec<ContainerSummary>, DriverError>;
    // 与容器无关的宿主机文件操作（属主适配、目录清理、out/ 读取）经同一个驱动，因为它们需要沙盒用户身份（§7.7）
    async fn prepare_dirs(&self, layout: &StagingLayout) -> Result<(), DriverError>;
    async fn read_out(&self, layout: &StagingLayout, plan: &IntakePlan) -> Result<IntakeResult, DriverError>;
    async fn purge_staging(&self, layout: &StagingLayout) -> Result<(), DriverError>;
}

pub struct ContainerSpec {
    pub name: String, pub labels: BTreeMap<String, String>,   // 必含 gantry.job_id、gantry.sandbox_id
    pub image: ImageDigest, pub argv: Vec<String>, pub env: BTreeMap<String, String>,
    pub token_env: TokenEnvSpec,           // WorkerAdapter::token_env()：需由 sandbox 在签发 job_token 后填值的变量名（模型 base URL、HTTPS_PROXY）
    pub mounts: Vec<Mount>,                // Bind{src, dst, ro} | Tmpfs{dst, size_mb}
    pub limits: SandboxLimits,
    pub network: NetworkAttach,            // 沙盒网段与到 Egress 的路由（§7.4）
    pub runtime: Runtime,                  // Runsc | Runc
    pub stop_grace: Duration, pub log: LogPolicy,
    // 注意：没有 privileged / cap_add / devices / host_network / user_ns_host 等字段。
    // 加固项是驱动里的常量（§7.3），ContainerSpec 无法放宽；驱动在 create 前跑 validate_invariants()。
}
```

`FakeDriver`（本模块自带，只用于本模块测试）实现 `SandboxDriver`，用内存模拟容器：可脚本化其写入 `out/` 的文件、stdout 行、退出码 / 信号 / OOM，并配合 `tokio::time::pause()` 控制计时器。

## 7. 内部设计

### 7.1 状态机与并发模型

每个 job 一个 tokio 任务（job actor）持有状态机并串行推进；外部事件（取消、计时器、容器退出、日志块）经 `mpsc` 命令送入。`Semaphore(max_concurrent_sandboxes)` 控制同时存在的沙盒数，FIFO 排队。

```
Accepted → Queued → Creating → Created → Staging → Staged → Starting → Running
                                                                  │
         Destroyed ← Destroying ← Collected ← Collecting ← Stopped ← Stopping ←┘
任一前置阶段失败 → Failed(InfraDetail) → Destroying（若容器已建）
```

与 data-model `Sandbox.status` 的对应：`Created` / `Staging` / `Staged` / `Starting`（已建、已注入、未启动）→ `created`；`Running` / `Stopping` → `running`；`Stopped` / `Collecting` / `Collected` → `stopped`；`Destroyed` → `destroyed`；恢复时查无此沙盒 → `lost`。`running` / `stopped` / `lost` 没有对应事件（数据模型只有 `sandbox.created` 与 `sandbox.destroyed`），见 OQ-S9。

每次相位迁移先把 `meta.json` 原子写盘（临时文件 + `fsync` + `rename`）再执行动作，使进程重启后能知道"做到了哪一步"。`Staged` 与可信摘要必须一起保存后才能启动，详见 [任务身份契约](../interfaces/worker-protocol.md#job-identity)；不承诺父目录同步或掉电保障。

### 7.2 staging：目录、inputs 打包与 job.json

```
<workspaces_dir>/<sbx_id>/              （默认 /var/lib/gantry/workspaces，deploy.md §6.3：gantry:gantry-sbx 0710；属主细节见 §7.7）
    meta.json                            相位、container_id、deadline（UTC）、terminal_reason、`job_token` 是否已签发（不存令牌本身）；不挂载
    progress.spool.jsonl                 已接受的合法进度事件，崩溃后重读去重；不挂载
    job.json                             → /gantry/job.json  只读
    inputs/                              → /gantry/inputs    只读
        MANIFEST.json  materials/  upstream/<node_id>/  diff.patch(review/verify)  output.schema.json  READY
    work/                                → /gantry/work      读写（exec=sandbox）或只读（sandbox_ro）
    out/                                 → /gantry/out       读写，有配额；启动时为空
```

`stage` 步骤（worker-protocol §3.2 阶段 2、§4.4）：

1. `RepoService::prepare_workspace(dest = work/, ...)`；`sandbox_ro` 角色同样准备，只是挂载只读。`diff.patch`（review / verify）由 `repo` 返回 `CasRef`，复制进 `inputs/`。
2. 材料打包流水线（对每个 `RawMaterial`，使用 `core` 的 `UntrustedText`）：
   - **规范化**：UTF-8 解码（非法序列替换）；去 ANSI 转义序列；去 NUL 与除 `\n` `\t` 外的 C0 控制符；去 bidi 控制符（U+202A–202E、U+2066–2069）与 Unicode Tag 字符（U+E0000–E007F）；记录去掉的个数。
   - **截断**：单条 ≤ `io.materials.max_item_bytes`（64 KiB）、总量 ≤ `max_total_bytes`（512 KiB）；超出取头 + 尾，中间放固定的截断标记；`truncated = true`，保留 `original_bytes` 与原文哈希。总量分配顺序 **[提议]**：按给定顺序，后续条目取剩余额度（至少 1 KiB 头部），仍放不下则只留标记行（OQ-S12）。
   - **nonce**：128 位 OS 随机数（32 hex）；材料文本含该串则重新生成。
   - **包装**：文件 `inputs/materials/<id>.txt`，首尾是 `isolation.begin` / `end` 模板展开的标记行。`kind` 必须在枚举内；`source` 必须匹配 `owner/repo#N` 或 `owner/repo#N/<item_id>`，否则 `InvalidSpec`（标记行不含自由文本，W5）。
   - **登记**：`job.json.materials[]` 记 `id kind source path sha256 bytes truncated original_bytes`，其中 `sha256` / `bytes` 针对**包装后的文件**。
3. `upstream/<node_id>/`：从 CAS 复制 `files[]`；整个目录按不可信处理，不逐文件包装（适配层内联时用同一 nonce 包装）。
4. `output.schema.json`：生产 `OutputSchemas` 委托 `core` 内置查询，取原始字节并校验 ID、路径和 `sha256`；未知 ID 或摘要不符返回 `InvalidSpec`，不启动 worker，已创建资源按生命周期清理。`StaticSchemas` 只用于测试。共同来源和字节规则见 [worker-protocol §11.1.1](../interfaces/worker-protocol.md#output-schema-registry)。
5. 写 `job.json`（规范化序列化，字节固定；`job_sha256` 即这些字节的哈希），写 `MANIFEST.json`；用 `schemas/worker/job.schema.json` 校验 `job.json`，不通过 → `InvalidSpec`（引擎缺陷，不启动）。
6. 全部输入文件 `0444`、目录 `0555`；`out/` 与 `work/` 的属主适配见 §7.7。
7. 原子创建 `inputs/READY`（临时文件 + `rename`）。worker 总是等待该文件再读 `job.json`（[worker.md §7.1](worker.md#71-运行流程)）：常规路径下它在 `start` 之前就已存在，预热池下在认领并 `stage` 完成后才创建（§7.11）。这样两种路径的 worker 逻辑相同（OQ-S8）。

### 7.3 Podman 驱动与隔离配置

驱动经 `gantry-sbx` 运行的 `podman system service` 套接字（libpod REST API，或等价的 `podman --remote`）调用，见 §7.7——deploy.md 已排除 `sudo`（`gantryd` 单元带 `NoNewPrivileges`）。能连上该套接字就能以 `gantry-sbx` 创建任意容器，所以下面这份清单是 `gantryd` 一侧**唯一**的参数闸：它们是**驱动里的常量**，`ContainerSpec` 无权放宽，`validate_invariants()` 在每次 `create` 前逐项断言（违反 → `DriverError::InvariantViolated`，不创建）：

| 项 | 取值 | 目的 | 状态 |
| --- | --- | --- | --- |
| 运行时 | `--runtime=runsc`（gVisor，M2 起默认）；M1 允许 `runc`（`sandbox.runtime`，降级是放宽，需 approver） | gVisor 在用户态内核拦截系统调用，缩小宿主内核攻击面 | `runsc` 在 rootless Podman 下的网络模式、cgroup 限额、OOM 报告、`--file-access` 对 bind mount 的行为 **需核实**（OQ-S4） |
| 根文件系统 | `--read-only`、`--read-only-tmpfs=false`；镜像按 `@sha256:` 固定，`--pull=never` | 根只读；运行期绝不联网拉镜像 | 确定 |
| 可写区域 | 仅 `/gantry/work`（bind，读写）、`/gantry/out`（bind，读写，配额）、`/gantry/home`、`/gantry/tmp`、`/dev/shm`（tmpfs，`rw,nosuid,nodev`，带 `size`） | 其余皆只读 | `/gantry/tmp` 是否可加 `noexec` 取决于构建工具是否需要在 tmp 执行，**需核实**；`/gantry/work` 必须允许执行（构建 / 测试） |
| 只读挂载 | `/gantry/job.json`、`/gantry/inputs`：`ro,nosuid,nodev,noexec` | worker 与 agent 不能改输入 | 确定 |
| 挂载源白名单 | 只允许 `<workspaces_dir>/<sbx_id>/` 下的四个路径；**不挂** Podman / docker socket、`/run/secrets`、上述四个路径以外的任何宿主路径、镜像仓、`repo` 的 scratch | I1：没有任何回连服务端的 socket | 确定（`validate_invariants` 断言） |
| 用户 | `--user 1000:1000`（容器内非 root） | 减小提权面 | 确定 |
| 用户命名空间 | rootless 默认私有 userns：容器内 uid 1000 映射到沙盒用户的 subuid；`/etc/subuid` `/etc/subgid` 给 `gantry-sbx` 独占 ≥ 65536 的区间且不与其他系统用户重叠；不用 `--userns=host`，不用 `--privileged` | 容器内即便取得 root，在宿主上也只是无权限的映射 uid | 确定；映射区间与目录属主见 §7.7 |
| 能力 | `--cap-drop=ALL`，`--security-opt=no-new-privileges` | 无 `CAP_NET_RAW` / `NET_ADMIN` / `SYS_ADMIN` 等 | 确定 |
| seccomp / AppArmor | Podman 默认 seccomp profile；Ubuntu 默认 AppArmor profile（对 `runc`）；`runsc` 下由 gVisor 的应用内核接管 | 纵深防御 | 两者叠加后的实际行为 **需核实** |
| 命名空间 | PID、IPC、UTS、cgroup、网络、挂载均私有：`--ipc=private --uts=private --cgroupns=private`，不用 `--pid=host`；`--init`（回收僵尸、转发信号） | 隔离；SIGTERM 能到达 worker | 确定 |
| 网络 | 私有网络命名空间；**只有到 Egress 的路径**（沙盒网段）；不发布端口，不用 `--network=host`；`/etc/hosts` 用 `--add-host` 固定 Egress 名称；沙盒内无 DNS 服务；关闭 IPv6 | I1、E1 | 见 §7.4；归属靠 `job_token`（P-49），网络只负责隔离 |
| 设备 | 不加 `--device`；无 `/dev/fuse`、`/dev/kvm` | — | 确定 |
| 资源 | `--cpus=<cpu>`、`--memory=<mem>m` 且 `--memory-swap` 同值（无 swap）、`--pids-limit=<pids>`、`--ulimit nofile=4096:4096`、`--ulimit core=0` | 限额；fork 炸弹、内存耗尽被 cgroup 终止 | rootless 需要 cgroup v2 委派（`cpu` `memory` `pids` 等控制器）与 `loginctl enable-linger`，Ubuntu 默认只委派 `memory` `pids`，`cpu` 需要 systemd drop-in，**需核实** |
| 磁盘 | `limits.disk_mb` 约束 `work/` + `out/` + 工作区增长 | 防写满宿主 | bind mount 没有天然配额；候选：专用文件系统的 project quota、loop 镜像、tmpfs，**需核实**（OQ-S3） |
| 环境变量 | `--unsetenv-all` 后只设 `ExecSpec.env` 的白名单（`GANTRY_*`、`HTTPS_PROXY`（含 `job_token`）、`NO_PROXY`、模型 base URL（路径含 `job_token`，P-49）、占位 key、`HOME`、`TMPDIR`、适配层变量）；**不**使用 `--env-host`，不设 `GITHUB_TOKEN`、不设任何真实密钥 | I1：沙盒内无任何长期凭据；`job_token` 只对本尝试记账有效、尝试结束即作废 | `--unsetenv-all` 可用性 **需核实**；`validate_invariants` 另断言 env 名不匹配 `*TOKEN*` `*SECRET*` `*PASSWORD*` `*PRIVATE_KEY*`（占位 key `ANTHROPIC_API_KEY=gantry-egress-managed` 在显式豁免名单内）；`job_token` 只允许出现在模型 base URL 与 `HTTPS_PROXY` 两个值里 |
| 日志 | `--log-driver=k8s-file --log-opt max-size=<log.max_bytes>`；服务端用 `logs --follow` 读 | 容器 stdout 洪水不写满磁盘；崩溃恢复可重读 | 超限轮转 / 截断的具体语义 **需核实** |
| 停止 | `--stop-signal=SIGTERM --stop-timeout=<kill_grace_s>`，`--restart=no`，不用 `--rm`（销毁在 `collect` 之后显式执行） | 与 worker-protocol §3.4 一致 | 确定 |
| 标签 | `gantry.managed=1`、`gantry.job_id`、`gantry.sandbox_id` | 崩溃恢复与孤儿巡检 | 确定（标签名取自 worker-protocol §3.5） |
| 镜像来源 | 只接受 `ExecSpec.image_digest`（含 `@sha256:`），create 前 `image_present` | 防止 per-job 任意镜像 | 确定 |

`create` 示意（仅表达意图，非最终命令；字段取值见上表）：

```bash
podman create --name gantry-sbx-<sbx_id> --runtime=runsc --pull=never --init \
  --read-only --read-only-tmpfs=false --user 1000:1000 --cap-drop=ALL --security-opt=no-new-privileges \
  --cpus=<cpu> --memory=<mem>m --memory-swap=<mem>m --pids-limit=<pids> --ulimit nofile=4096:4096 --ulimit core=0 \
  --ipc=private --uts=private --cgroupns=private --shm-size=64m \
  --network=<NetworkAttach> --add-host=egress.gantry.internal:<egress-ip> \
  -v <sbxdir>/job.json:/gantry/job.json:ro,nosuid,nodev,noexec \
  -v <sbxdir>/inputs:/gantry/inputs:ro,nosuid,nodev,noexec \
  -v <sbxdir>/work:/gantry/work:rw,nosuid,nodev \
  -v <sbxdir>/out:/gantry/out:rw,nosuid,nodev,noexec \
  --tmpfs /gantry/home:rw,nosuid,nodev,size=256m,uid=1000,gid=1000,mode=0700 \
  --tmpfs /gantry/tmp:rw,nosuid,nodev,size=512m,uid=1000,gid=1000,mode=0700 \
  --unsetenv-all --env PATH=… --env GANTRY_JOB=/gantry/job.json --env GANTRY_OUT=/gantry/out --env GANTRY_WORK=/gantry/work \
  --env HTTPS_PROXY=http://job:<job_token>@egress.gantry.internal:3128 --env HOME=/gantry/home --env TMPDIR=/gantry/tmp \
  --env ANTHROPIC_BASE_URL=http://egress.gantry.internal:8081/j/<job_token>/anthropic --env <占位 key>=… \
  --log-driver=k8s-file --log-opt max-size=16m --stop-signal=SIGTERM --stop-timeout=10 --restart=no \
  --label gantry.managed=1 --label gantry.job_id=<att_id> --label gantry.sandbox_id=<sbx_id> \
  --workdir /gantry/work <image@sha256:…> gantry-worker run
```

gVisor 的 `--runtime-flag`（如 `platform`、`network`、`file-access`、`overlay2`）取值 **需核实**；`file-access=shared`（默认）更适合"容器停止后宿主读 `out/`"，`exclusive` 可能让宿主看不到未回写的数据。

### 7.4 网络与归属（P-49）

沙盒的**归属**不再依赖网络身份：服务端为每个 Attempt 签发一枚 `job_token`（P-49），放在模型 API base URL 路径中（`http://<egress>/j/<job_token>/<provider>/…`）与 `HTTPS_PROXY` 的代理凭据里；令牌只对该尝试有效、尝试结束即作废（"只能从沙盒网段使用"的强制方式需核实，P-73）。沙盒即使读到令牌也只能给自己的尝试记账。**网络只负责隔离**，契约要求（worker-protocol §2.3）：

| # | 要求 | 说明 |
| --- | --- | --- |
| N1 | 沙盒网络里除 Egress 外无路由（E1） | 出路只有：模型 API 反向代理端点、CONNECT 端点、（M2）私有包仓库路由 |
| N2 | 到宿主机其他端口（含 `gantryd` 的任何监听、Egress 的 `svc_model` 回环监听与控制面）、网关上的其他服务、局域网、云元数据地址 `169.254.169.254`、**其他沙盒**，一律不通 | I1 |
| N3 | 沙盒内无 DNS；域名由 Egress 在 CONNECT / 反向代理时解析 | E1 |
| N4 | 来自沙盒的连接源地址属于沙盒网段（`egress.sandbox_cidrs`），使 Egress 能拒绝在沙盒外使用的 `job_token`——rootless Podman（slirp4netns / pasta）下 Egress 能否看到真实源地址**需核实**（P-73），M1 用 `[集成:podman]` 用例核实；不能强制时安全底线是能力令牌本身 | P-49、P-73 |
| N5 | 尝试结束后令牌立即作废（`revoke_job_token`，E7） | — |

[deploy.md §7.8](deploy.md) 的主机防火墙按发起**用户**过滤（`meta skuid "gantry-sbx" … tcp dport { 8081, 3128 }`），保证 N1、N2；区分沙盒由令牌完成，不再需要每沙盒唯一的源地址。

```rust
pub struct NetworkAttach { /* 渲染成 --network / --add-host 等参数：沙盒网段与到 Egress 的路由 */ }
```

建容器之前先向 Egress `issue_job_token(...)`（环境变量只能在创建时设定），把返回的 `model_base_urls` 与 `connect_proxy_url`（`http://job:<job_token>@…`，P-72）按 `ExecSpec.token_env` 声明的变量名写入容器环境（本模块不依赖 `worker` crate，只用名字）（见 [egress.md §6](egress.md#6-对外接口)）；后续任一步失败都 `revoke_job_token`。**拿不到令牌就不启动**（失败闭合：`infra.egress`）。

### 7.5 计时器、取消与终态判定

- `wall_time` 计时从 `start` 起（预热池：从 `READY` 起）；`idle_timeout` 从同一点起，每收到一条**合法**（schema 通过、`seq` 递增）进度事件重置，包括因限速被丢弃的合法事件（丢弃是本地策略，不代表没活动）。用伪造心跳可以把空闲超时推迟到 `wall_time`，后者是硬上限（worker-protocol §7.3）。
- 终止原因是一个**只写一次**的单元：`{Cancel, WallTimer, IdleTimer, RuntimeExit}`，先写入者胜；写入 `meta.json`（`fsync`）之后才发信号，所以崩溃重启不会丢"为什么杀它"。
- 动作：`stop(grace)` = SIGTERM → `kill_grace_s`（默认 10）→ SIGKILL；然后确认容器 `exited` 且其 PID 命名空间无残留（`inspect` 状态 + 运行时确认；`runsc` 另查沙盒进程已消失，**需核实**）。
- `create` / `stage` 阶段被取消：不 `start`，直接 `destroy`，`terminal.cause = KilledCancel`，不取产物。
- 超时 / 取消后仍执行 `collect`，但 `partial = true`，`engine` 不应用补丁、不执行 proposals（worker-protocol §3.4）。
- 终态事实 → `TerminalFacts`：`cause`（Exited 或三种 Killed*）、`exit_code`、`signal`、`oom`（来自运行时报告）、`timer`、`cancel_requested`。`sandbox` 不判 `failure_class`，那是 `engine` 按 §9 优先级做的事。
- 墙钟用 `start` 时刻的 UTC 加 `limits.wall_time` 持久化为 `deadline`；重启后据此重建计时器（时钟被大幅回拨的风险接受并记录）。

### 7.6 进度采集

- 来源：`SandboxDriver::logs` 的 stdout 块；按行切分，行长 > `max_line_bytes`（8 KiB）丢弃并计数，**继续排空**，不让容器因管道写满而阻塞。
- 每行：必须是 JSON 对象且通过 `progress-event.schema.json`；未知 `type` 丢弃；`seq` 必须严格大于上一条（从 1 起）；字符串字段去 ANSI、控制符、bidi 控制符；忽略 worker 的 `ts`，打服务端 `received_at`。
- 速率 ≤ `max_rate_per_s`（20）、总数 ≤ `max_lines`（20000）；超限继续排空但丢弃并计数。
- 合法事件写 `progress.spool.jsonl`（批量 `fsync`）；结束时转入 CAS 成为 `progress.jsonl`。转发给客户端的是节流后的 `attempt.progress` 事件，采样间隔 **[建议值，待调优]** 1 s，同一类事件取最新。
- stdout 的非协议行与 stderr 各保留末尾 `logs_tail_bytes`（1 MiB），作为不可信纯文本入 CAS。
- 自报 `usage` / `finish` 只展示，不进账本、不决定终态（W3）；与 Egress 计量显著偏离时记审计日志。

### 7.7 用户映射与文件属主 **[提议]**

三个互相独立的系统用户（architecture §3；名字与家目录取自 [deploy.md §6.2](deploy.md)）：

| 用户 | 角色 | 说明 |
| --- | --- | --- |
| `gantry` | `gantryd` | 持有状态库、`secrets/`、CAS、镜像仓与工作树；**不是**沙盒用户；读不到 `/var/lib/gantry-sbx`、`/var/lib/gantry-egress` |
| `gantry-sbx` | rootless Podman 的属主 | 无登录 shell，家目录 `/var/lib/gantry-sbx`，`loginctl enable-linger`，subuid / subgid 各 65536 且不与其他用户重叠；读不到 `gantryd` 与 Egress 的任何数据（deploy.md §6.3 读取矩阵） |
| `gantry-egress` | `egress` | 见 [egress.md](egress.md)；与上两者无文件共享 |

**控制通道（方向已由 deploy.md 定下）**：`gantryd` 单元带 `NoNewPrivileges`，不能靠 `sudo` 切到 `gantry-sbx`（deploy.md §7.7、OPS-11）。由 `gantry-sbx` 运行 `podman system service`，套接字 `/run/gantry-sbx/podman.sock`，属组 `gantry-sbxctl`（成员：`gantry`）；`PodmanDriver` 经它调用。该套接字永不挂进沙盒（P-15）。引导配置项 `sandbox.runtime_user` / `sandbox.runtime_socket` 已在 deploy.md §10 列出。开发 / CI 把 `sandbox.runtime_socket` 指向调用者自己的用户级 Podman 套接字（`$XDG_RUNTIME_DIR/podman/podman.sock`），其余路径相同。

**目录与属主**（deploy.md §6.3：`workspaces/` 为 `gantry:gantry-sbx 0710`，作业目录的所有权与 ACL "待定"，OPS-11）：

- 作业目录 `<workspaces_dir>/<sbx_id>/` 由 `gantryd` 创建；`job.json` 与 `inputs/` 由 `gantry` 写入后设为只读，`gantry-sbx` 经组读取。
- `repo` 与 `sandbox` 都不 `chown`，`gantryd` 也不持有 `CAP_CHOWN`。
- 两个未决的难点（OQ-S2）：
  - **`work/`**：`repo` 以 `gantry` 身份写入的文件，容器内 uid 1000（映射到 `gantry-sbx` 的某个 subuid）无权修改，而 `gantry` 无法把属主改成那个 subuid。候选：① `repo` 产出归档，由 `gantry-sbx` 名下的一次性**辅助容器**（固定镜像、只挂作业目录）解包，文件天然属于容器用户；② 把 `work/` 作为 `:O`（overlay）挂载，容器写入落在上层，下层保持原样；③ 作业目录交给 `gantry-sbx` 所有并用 `:U` 适配属主，同时让 `repo` 能以组权限写入。三者的行为在 rootless + gVisor 下都 **需核实**。
  - **`out/`**：容器内创建的文件属主是映射后的 subuid，`gantry` 通常读不了；读取方式见 §7.8。
- **销毁**：删除含 subuid 属主文件的作业目录同样要在 `gantry-sbx` 的用户命名空间内进行（辅助容器执行删除）；经同一控制通道。

### 7.8 `collect`：先停后取（intake，V2）

前置：容器已 `exited` 且确认无残留进程（§7.5）；**绝不**在进程仍可能写入时读取（消除检查时与使用时间窗竞争，TOCTOU）。读取只发生在 `out/`，不读 `work/` `home/` `tmp/`。

`SandboxDriver::read_out`（§6.2）把 `out/` 的目录项作为"不可信条目流"交给上层，`sandbox` 对每个条目施加下表的规则。**规则与读取方式无关**；读取方式取决于 §7.7 的属主方案（OQ-S2）：

- **方式 A，主机直读**（作业目录对 `gantry` 可读时）：以 `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)` 打开的目录 fd 为根枚举；`fstatat(AT_SYMLINK_NOFOLLOW)` 判型；`O_NOFOLLOW | O_NONBLOCK | O_RDONLY` 打开后对 fd 再 `fstat` 复核。
- **方式 B，经运行时导出**（`out/` 属主为映射 subuid、`gantry` 读不了时）：容器已停止，经 Podman 的归档导出接口（libpod `containers/{id}/archive`，等价 `podman cp`）取 `/gantry/out` 的 tar 流（对 bind mount、FIFO、符号链接的行为 **需核实**）。tar 由 Podman 生成，但**条目的名字、类型、大小仍由沙盒决定**，所以同样按规则处理：只接受普通文件条目；符号链接、硬链接、目录、设备、FIFO、PAX / GNU 扩展头、稀疏文件一律记入 `unexpected`；tar 头里声明的大小不可信，以实际读到的字节数为准并在"上限 + 1"处中止；总字节数封顶。

| 检查 | 方法 |
| --- | --- |
| 名字白名单 | 精确匹配 `result.json` `changes.patch` `report.md` `proposals.json`（区分大小写，名字里不得含路径分隔）；其余（含子目录、`result.json.tmp` 残留）→ 记入 `unexpected`，丢弃 |
| 必须是普通文件 | 符号链接、`nlink > 1` 的硬链接、FIFO、设备、socket 一律丢弃，不跟随、不阻塞（方式 A 靠 `lstat` 与 `O_NONBLOCK`，方式 B 靠条目类型） |
| 大小 | 读到"上限 + 1 字节"即判超限，不信 `stat` / 头里声明的大小；上限取 `io.artifacts.*` 与 `JobSpec.constraints.patch.max_bytes` 中适用者 |
| 文本 | `result.json` / `proposals.json` / `report.md` 必须是合法 UTF-8；JSON 深度 ≤ 32、禁止重复键（需要自带检测重复键的 JSON 解析，`serde_json` 默认不拒绝） |
| 按 JobSpec 取舍 | `output.patch = forbidden` 却存在 `changes.patch` → 丢弃并记 `unexpected`，**不**使 Attempt 失败 |
| 落地 | 流式写入 CAS（`Cas::put_file`，`CasKind::Artifact`），之后只用 CAS 副本；`sha256` 由服务端重算 |

同时取：运行时事实（退出码、信号、`oom_killed`、完成时间）、stdout / stderr 尾部、`progress.jsonl`、Egress 计量与标志。`collect` 的结果只取决于已停止沙盒的 `out/`，重复执行得到相同的内容寻址对象。`result.json` 的 schema、`job_id` / `job_sha256` 匹配、产物声明与实际一致等语义校验（V3、V4）归 `engine`，`sandbox` 不解释内容。

### 7.9 与 Egress 的协作

```
submit → (排队) → egress.issue_job_token(job_id, profile, groups, tokens_total, weights) → create 容器（未启动，env 含令牌）→ stage → start
... → stop → collect → egress.revoke_job_token(job_id) → usage_metered → destroy
```

- 签发失败 / Egress `health != ok` → 不启动，`Infra{Egress}`（失败闭合：配置未生效时沙盒等同无网络，config §2.6）。
- `revoke_job_token` 在**所有**路径上执行（正常、超时、取消、`stage` 失败、恢复后）；它原子地"取最终计量并作废令牌"，且幂等（重复调用返回同一份最终用量），所以 `collect` 与写 `attempt.finished` 之间崩溃，重放仍得到相同的 `usage_metered`。身份拒绝路径在确认吊销、最终计量已随终态交给 engine 且结算终结已提交、engine 请求 `release()` 后才可遗忘，详见 [任务身份契约](../interfaces/worker-protocol.md#job-identity)。
- `EgressFlags.incomplete = true` 表示 Egress 崩溃时有请求在途，用量是下界（egress.md §7.2）。
- 沙盒的 `egress_profile` 在创建时固化进 `sandbox.created`（档案名 + 解析后的分组集合）；"有效白名单 = 创建时快照 ∩ 当前配置"由 Egress 执行（config §2.6）。

### 7.10 崩溃恢复与孤儿巡检

`recover(unfinished)`（worker-protocol §3.5）：对每个没有 `attempt.finished` 的 Attempt，按标签 `gantry.job_id` 查运行时：

查运行时前在 `submit_lock` 内完成请求绑定预验证；请求冲突整批 `InvalidSpec`，与通过预验证后的逐 job 证据拒绝不同，详见 [任务身份契约](../interfaces/worker-protocol.md#job-identity)。按 `job_id` 的内部操作只在绑定成功后使用。

| 运行时状态 | 处理 |
| --- | --- |
| 仍在运行 | 读 `meta.json` 重建 deadline 与终止原因；从头重读日志，按 `seq` 去重并并入 `progress.spool.jsonl`；继续监督 |
| 已退出 | 直接 `collect`（若 `meta.json` 记录了取消 / 计时器原因则沿用，否则按 `Exited`） |
| 容器已不存在但 staging 在 | 若 `out/` 完整可读则 `collect`（`cause = Lost`），否则 `Infra{Lost}`；Sandbox `lost` |
| 容器与 staging 都不在 | `Infra{Lost}` |
| 相位在 `Accepted` … `Staged`，或 `Starting` 且能证明从未启动 | 新格式的显式 `null` 或有效摘要，凭相位及运行时事实确认从未启动，可清理残留、持久 `Accepted` 并返回 `Unknown`，由 engine 以相同 ID 重提完整 `SandboxSpec`；重新 stage 并保存新摘要后才启动一次。旧记录缺摘要、损坏、错配或 `Starting` 执行情况不明不得重提。启动前已请求取消的，直接重出取消结果（`Exited`）；完整规则见 [任务身份契约](../interfaces/worker-protocol.md#job-identity) |
| 没有任何受理痕迹（无 `meta.json`、无带该 `gantry.job_id` 标签的容器、无 staging），且 `store` 中没有该沙盒的 `sandbox.created` | 未知 job（从未受理，例如 `attempt.started` 落库后、`submit` 返回前崩溃）；不建任何东西 |

每行对应的 `RecoveredJob`（§6.1，E4）：仍在运行或拒绝收尾推进 → `Running`；已退出或拒绝结果已稳定 → `Exited`；可按普通丢失处理的受理后丢失 → `Lost`；没有任何受理痕迹，或符合上述新格式准备重入例外 → `Unknown`。身份不可验证且有受理迹象时不能用 `Unknown` 或 `Lost`，各 job 独立返回，见 [任务身份契约](../interfaces/worker-protocol.md#job-identity)。

`sweep_orphans(live)`：列出带 `gantry.managed=1` 的容器与 `<workspaces_dir>/*`，对不在 `live` 且超过宽限（默认 5 min [建议值，待调优]）的，先 `revoke_job_token` 再 `remove` 与 `purge`。巡检周期默认 10 min [建议值，待调优]。`gantryd` 优雅退出时**不**杀运行中的容器，留给重启后的 `recover` 接管（deadline 已持久化）。

### 7.11 预热池（M2）**[提议]**

容器可先启动并空等；`stage` 完成后由管理器**原子创建** `/gantry/inputs/READY`（临时文件 + `rename`），worker 见到该文件才读 `job.json`（worker-protocol §3.2，OQ-16）。

- 池成员按"镜像 digest + 运行时 + limits 档"分桶；成员的 bind mount 源是预分配的空 staging 目录，认领后再填充（宿主目录内容对已运行的容器可见）。
- 认领是原子的；认领时才向 Egress `issue_job_token`，令牌随 `stage` 写入（池成员在认领前没有任何令牌，其模型 / CONNECT 请求一律被 Egress 拒绝）。令牌经 `inputs/` 下的只读文件交给已启动的 worker [提议]，因为环境变量在启动后无法注入——这与 worker-protocol §2.2"令牌只出现在模型 base URL 与 `HTTPS_PROXY` 两个变量里、不写进 `inputs/`"冲突，预热池启用前须先经契约 PR 修订 §2.2（OQ-S15）。
- `wall_time` 与 `idle_timeout` 从 `READY` 起算。
- 成员**一次性**：用过即销毁，绝不复用；配置变更（镜像、限额、运行时）使旧桶整体作废；空闲超过 TTL（默认 10 min [建议值，待调优]）销毁。
- 池成员在认领前不含任何 job 数据、不持有令牌；池大小 `prewarm.pool_size`，`enabled = false` 时 M1 的路径完全不变。

### 7.12 `devcontainer.json` 安全子集（M2）

`sandbox.devcontainer.mode = safe_subset` 时，读取工作区里的 `.devcontainer/devcontainer.json`（来自 GitHub，不可信数据），忽略 `initializeCommand`、`mounts`、`runArgs` 与一切特权字段（`privileged`、`capAdd`、`securityOpt`、`init`、`forwardPorts`……）。子集内哪些字段生效（镜像？`containerEnv`？生命周期命令？）骨架没定（OQ-S7）；本模块只保证：无论内容如何，`validate_invariants` 里的加固项不可被它改变，且其中的命令只会在沙盒内、在网络白名单下执行。

### 7.13 持久化

`sandbox` 没有自己的数据库：运行状态是容器标签 + `meta.json`；事实事件经 `store`；产物经 CAS。`meta.json` 与 staging 在 `release()` 或孤儿巡检后删除。

当前本地 CAS 接缝可使用 `Store::cas()`；core 结构化输出 schema 查询与沙盒生产适配器已具备，见 §7.2。真实 `gantryd` 装配仍为 `NotReady`，接线由 #105 负责。替身驱动测试不代表真实 Podman 验收。

## 8. 不变式与安全要求

| 不变式 | 本模块如何保证 |
| --- | --- |
| **I1** 沙盒只计算 | 沙盒与服务端之间只有：只读输入、可写 `out/`、stdout（日志接口）、运行时信号；不挂 socket、不发布端口、无 DNS；网络只通 Egress；沙盒内无 GitHub 网络、无任何长期凭据（环境变量白名单、无 `--secret`、无 `/run/secrets`；`job_token` 只是本尝试的记账凭证，尝试结束即作废）；`validate_invariants()` 把这些写成断言 |
| **I5** 外部文本不可信 | 材料规范化 + 截断 + 随机 nonce 的隔离标记，标记行不含自由文本（W5）；产物、日志、`unexpected` 名字、进度字符串全部按不可信文本处理，入库前去控制符；`job.json` 的可信字段里不含任何 GitHub / 沙盒文本 |
| **I8** 执行产物先验后推 | **先停后取**；`out/` 白名单 + `lstat` 判型 + `O_NOFOLLOW` + 读到"上限 + 1"；永不读 `work/`、不在沙盒产出的 `.git` 上运行 git；校验发生在沙盒销毁之后（销毁不等校验） |
| **W3** 以服务端观察到的事实为准 | 终止原因、退出状态、OOM、计时器、Egress 计量都由本模块观察；worker 的退出码与 stdout 只作线索 / 展示 |
| **W8** 失败闭合 | 未知输入形态、Egress 不可用、`job.json` 不通过 schema、`validate_invariants` 失败——一律不启动 |

附加要求：

- 不得有任何代码路径让沙盒用户以外的主体（含 `gantryd`）在容器仍可写入时读取 `out/`。
- `meta.json`、`progress.spool.jsonl`、对 Podman 套接字的请求与响应日志里不得出现密钥；日志中的路径与名字按不可信文本处理。
- `validate_invariants()` 是 `gantryd` 一侧唯一的参数闸（没有包装器二次把关），按安全边界对待：它的输入只能来自 `ContainerSpec`（没有"透传任意 Podman 参数"的入口），并有属性测试覆盖（§12.A）。`gantry-sbx` 无特权、读不到 `gantryd` 与 Egress 的数据（deploy.md 读取矩阵）是其后的第二道防线。
- 资源限额由 cgroup 强制，不依赖 worker 自觉；若宿主未委派所需控制器，驱动在 `capabilities()` 里如实报告，`gantryd` 启动自检对 `mem` / `pids` 缺失直接拒绝启动 **[提议]**。

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 镜像不存在 / digest 未固定 | `InvalidSpec` 或 `Infra{SandboxCreate}`；不拉取 |
| `create` 失败 | `Infra{SandboxCreate}`；清理已建资源；`engine` 在新沙盒上重试 |
| 申请 `job_token` 失败 / Egress 不健康 | `Infra{Egress}`；销毁容器 |
| `prepare_workspace` / 材料打包失败 | `Infra{WorkspacePrep}`；`InvalidSpec` 用于 `source` 格式、`job.json` schema 等引擎缺陷 |
| `start` 失败 | `Infra{SandboxCreate}` |
| 运行时 OOM / pids / 磁盘配额 | 运行时终止，`TerminalFacts{KilledRuntime, oom}`；`engine` 归 `exec.oom` / `exec.killed` |
| 墙钟 / 空闲超时、取消 | SIGTERM → 宽限 → SIGKILL；`partial` 提取；`engine` 归 `timeout.*` / `cancel.requested` |
| 日志读取中断 | 重连并从上次 cursor 之后继续，`seq` 去重；多次失败只影响展示，不影响终态 |
| `collect` 读取失败 | `Infra{Collect}`；staging 保留一次重试窗口再销毁 |
| 容器消失（运行中被外部杀掉） | `TerminalFacts{Lost}` / `Infra{Lost}` |
| `destroy` 失败 | 重试；仍失败则留给巡检；不影响分类 |
| `gantryd` 崩溃 | `recover` 接管（§7.10） |
| 宿主磁盘 / 内存不足 | 排队或拒绝受理（`capacity()` 如实报告）；不降低任何隔离项 |

## 10. 配置

设置项（resource `sandbox`，[config §7](../interfaces/config.md#7-sandbox)；沙盒创建时固化，只影响新沙盒，config §2.6）：

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `sandbox.driver` | `podman` | 仅 `podman` |
| `sandbox.runtime` | `runsc` | M1 允许 `runc`（放宽，需 approver 签名） |
| `sandbox.base_image` | 随版本发布、固定摘要的官方 worker 镜像（P-66） | 必须含 `@sha256:`；`Sandbox.image_digest`；"仅 approver"键：第一台设备配对前可由主机命令写入（引导期，P-66），之后需审批签名。与项目工具链的关系见 OQ-S5 |
| `sandbox.limits.cpu_cores` / `mem_mb` / `disk_mb` / `pids` / `wall_time_s` | 2 / 4096 / 10240 / 512 / 3600 | [建议值，待调优]；`wall_time` 取与 `models.worker.attempt_wall_s`、预算剩余的最小值，由 `engine` 折算进 `SandboxSpec.limits` |
| `sandbox.max_concurrent_sandboxes` | 6 | 宿主同时存在的沙盒总数；超出排队 |
| `sandbox.idle_timeout_s` / `kill_grace_s` | 300 / 10 | — |
| `sandbox.io.progress.max_line_bytes` / `max_lines` / `max_rate_per_s` | 8192 / 20000 / 20 | — |
| `sandbox.io.artifacts.result_max_bytes` / `report_max_bytes` / `proposals_max_bytes` / `proposals_max_count` | 1048576 / 262144 / 262144 / 20 | `changes.patch` 的上限取 `policies.patch.max_bytes` |
| `sandbox.io.materials.max_item_bytes` / `max_total_bytes` | 65536 / 524288 | — |
| `sandbox.io.logs_tail_bytes` | 1048576 | — |
| `sandbox.prewarm.enabled` / `pool_size` | `false` / `0` | M2 |
| `sandbox.devcontainer.mode` | `safe_subset` | M2 |
| `egress.default_profile` / `egress.profiles` | `default` | 由 `engine` 解析成 `SandboxSpec.egress` |

引导配置（`deploy` 拥有，文件、重启生效；config §1.1）**[提议]**：

| 项 | 默认 | 说明 |
| --- | --- | --- |
| `sandbox.workspaces_dir` | `/var/lib/gantry/workspaces` | 作业目录根（deploy.md §6.3 的 `…/workspaces/`） |
| `sandbox.runtime_user` | `gantry-sbx` | rootless Podman 属主（deploy.md §10 已列） |
| `sandbox.runtime_socket` | `/run/gantry-sbx/podman.sock` | `podman system service` 套接字（deploy.md §10 已列）；开发 / CI 指向用户级套接字 |
| `sandbox.helper_image` | 无 | §7.7 的辅助容器镜像（按 digest 固定），OQ-S2 方案选定后才需要 |
| `sandbox.out_quota_bytes` | 12 MiB | `out/` 总配额（四个产物上限之和加余量）；`changes.patch` 上限随 `policies.patch.max_bytes` |
| `sandbox.tmpfs_home_mb` / `tmpfs_tmp_mb` | 256 / 512 | tmpfs 大小 [建议值，待调优] |
| `sandbox.log_max_bytes` | 16 MiB | 容器日志上限 |
| `sandbox.progress_sample_ms` | 1000 | `attempt.progress` 采样间隔 |
| `sandbox.orphan_grace_s` / `sweep_interval_s` | 300 / 600 | 孤儿巡检 |

## 11. 可观测性

事件（[data-model §5.2](../data-model.md#52-事件类型目录)）：

| 事件 | `data` 至少含 |
| --- | --- |
| `sandbox.created` | `sandbox_id`、`attempt_id`、`driver`、`runtime`、`image_digest`、`limits`、`egress_profile`（名称 + 解析后的分组集合）、`worktree_snapshot`（`{mirror, base_sha, pr_base_sha?}`）、`trust` |
| `sandbox.destroyed` | `sandbox_id`、`final_status`（`destroyed` / `lost`）、`reason` |
| `attempt.progress` | `attempt_id`、采样后的 `stage` / `detail` / 工具摘要 / 自报用量（明确标注自报，仅展示） |

`Sandbox.status` 的 `running` / `stopped` / `lost` 没有对应事件（OQ-S9）；`artifact.unexpected` 在 worker-protocol 里被称为"审计"，但数据模型事件目录里没有它，本文把它放进 `JobOutcome.unexpected` → `ResultBundle.validation.unexpected_artifacts`。

日志（结构化，字段 `job_id`、`sandbox_id`、`phase`）：每个相位的开始 / 结束 / 耗时、对 Podman 套接字的调用名（不含内容）、终止原因、intake 丢弃项、进度丢弃计数；路径与名字按不可信文本。不记录材料正文与产物内容。

指标：`sandbox_active{status}`、`sandbox_queue_depth`、`sandbox_phase_seconds{phase}`、`sandbox_terminal_total{cause,timer}`、`sandbox_oom_total`、`sandbox_intake_rejected_total{reason}`、`sandbox_progress_dropped_total{reason}`、`sandbox_egress_token_failures_total`、`sandbox_orphans_swept_total`、`sandbox_pool_{size,hits,misses,discarded}`（M2）。

## 12. 测试与验收

测试替身：`FakeDriver`（内存容器，可脚本化 `out/` 文件、stdout 行、退出码 / 信号 / OOM；配合 `tokio::time::pause`）、`EgressStub`（M0，`contract` 泳道交付，实现 `EgressControl`：记录 `issue_job_token` / `revoke_job_token` 调用、脚本化用量与健康，P-51）、`FakeRepo`（M0，返回预制工作区目录）、`Store::open_in_memory`（M0，含内存 CAS，P-51）。集成测试标记 `[集成:podman]`（需要 rootless Podman；`runsc` 用例另标 `[集成:gvisor]`，CI 的夜间任务跑）和 `[集成:egress]`（需要真实 `egress` 进程 + `MockUpstream`）。容器内的行为由一个最小测试镜像里的 `fake-worker` 静态二进制脚本化产生（可按脚本写 `out/`、打 stdout、探测环境、尝试联网），使本模块的测试不依赖 `worker` 泳道。

### A. 驱动一致性套件（同一份用例分别跑在 `FakeDriver` 与 Podman 驱动上；M0 / M1）

- [ ] `create` 后容器未启动；`start` 后运行；`stop(grace)` 先 SIGTERM，宽限后 SIGKILL，返回时已 `exited` 且无残留进程。
- [ ] `remove` 幂等；`list` 按标签选择；`logs(since)` 可重复读且顺序稳定。
- [ ] `validate_invariants`：`ContainerSpec` 含 host 网络、特权、设备、cap_add、可写 `inputs`、宿主任意路径挂载、socket 挂载、含 `*TOKEN*` 的环境变量名（非豁免）、未固定 digest 的镜像——每一种都被拒绝，且不产生容器。
- [ ] 属性测试：对随机生成的 `ContainerSpec`（含恶意字段取值），`validate_invariants` 通过的规格渲染出的 Podman 请求**永远**不含 `privileged`、`host` 网络 / PID / IPC / userns、`cap_add`、设备、`seccomp=unconfined`、`apparmor=unconfined`、宿主任意路径挂载、套接字挂载，且根只读、`no-new-privileges` 与 `cap_drop=ALL` 存在（它是 `gantryd` 一侧唯一的参数闸）。
- [ ] `FakeSandboxService`（M0）与真实 `SandboxService`（`FakeDriver` 之上）通过同一套引擎面一致性用例：`submit` 幂等、`wait` 对同一 job 恒返回同一结果、`cancel` 与 `release` 幂等（P-52）。

### B. staging 与材料打包（M1）

- [ ] `job.json` 通过 `job.schema.json`；`MANIFEST.json` 与实际文件一致；`inputs/` 文件 `0444`、目录 `0555`；`out/` 启动时为空。
- [ ] 生产 schema 适配器返回与 core 查询相同的原始字节；未知 ID、大小写变体、未知 major 或摘要不符 → `InvalidSpec`，worker 未启动；已创建资源得到清理。
- [ ] 规范化：NUL、除 `\n` `\t` 外的 C0、ANSI 序列、bidi 控制符、Unicode Tag 字符被去除，计数正确；非法 UTF-8 被替换。
- [ ] 截断：单条 64 KiB、总量 512 KiB 的边界（`limit` 不截、`limit + 1` 截）；头 + 尾 + 标记；`truncated`、`original_bytes`、`bytes`（包装后大于 `original_bytes`）正确；`sha256` 是包装后文件的哈希。
- [ ] 材料正文含伪造的 `<<<GANTRY-UNTRUSTED-END …>>>` 或与 nonce 相同的串：标记不生效；nonce 冲突时重新生成（用固定随机源制造冲突）。
- [ ] `source` 含空格 / 控制符 / 自由文本、`kind` 不在枚举 → `InvalidSpec`，不启动。
- [ ] `sandbox_ro`：工作区以只读挂载，写入得到 `EROFS`（集成）。
- [ ] `job.json` 与 `ExecSpec.env` 中没有任何形似密钥的值（正则 + 熵扫描；占位 key 豁免）。

### C. 隔离（`[集成:podman]`，M1 用 `runc`，M2 起 `runsc` 全部重跑）

- [ ] 容器内 `id -u` ≠ 0；`/proc/self/status` 的 `CapEff` / `CapPrm` 为 0；`NoNewPrivs` 为 1。
- [ ] 写 `/usr`、`/etc`、`/gantry/inputs/*`、`/gantry/job.json` 全部失败（`EROFS` / `EACCES`）；可写 `/gantry/work`、`/gantry/out`、`/gantry/home`、`/gantry/tmp`。
- [ ] 无 `/dev` 额外设备、无 `/dev/fuse`、无 `/dev/kvm`；没有 Podman / docker socket，没有 `/run/secrets`。
- [ ] 环境变量集合恰等于白名单（逐项比对）；无 `GITHUB_TOKEN`；`env` 与 `/proc/*/environ` 里搜不到测试用的模型 key 哨兵串；`job_token` 只出现在模型 base URL 与 `HTTPS_PROXY` 中。
- [ ] **M1 退出标准：沙盒内读不到任何模型 key**：真实 `egress` 的密钥库里放哨兵 `sk-gantry-sentinel-…`，沙盒内 `fake-worker --probe` 遍历 `env`、所有 `/proc/<pid>/environ`、整个可读文件系统（含 `/gantry/*`、镜像层、tmpfs）搜索哨兵与 `sk-` 模式，并经反向代理完成一次真实模型调用（`MockUpstream` 在带外记录它收到的 `x-api-key`，**不**回显）；断言：沙盒侧从未出现哨兵，上游收到的是哨兵。
- [ ] **M1 退出标准：沙盒内访问 `github.com` 与 `api.github.com` 失败**：直连（含按 IP 直连）失败；经 `HTTPS_PROXY` 的 CONNECT 被 `egress` 拒绝（403）并留下带 `job_id` 的审计记录；`git clone https://github.com/...` 失败。
- [ ] 与主机防火墙联跑（deploy.md §7.8 的 `nft` 规则，`doctor` 的金丝雀沙盒）：沙盒只能到 Egress 的两个端口；防火墙规则被临时移除时，本模块的网络配置本身（无 DNS、无多余路由、私有网络命名空间）仍使 `github.com` 不可达（两道独立防线各自可验）。
- [ ] 网络面：沙盒内尝试连接 `127.0.0.1:<gantryd 端口>`、宿主网关其他端口、局域网地址、`169.254.169.254`、另一个并发沙盒的地址、UDP/53 DNS，全部失败或快速超时；IPv6 不可用。
- [ ] 两个并发沙盒的归属互不混淆：A 的请求在 `egress` 里归到 A 的 `job_id`；A 伪造头部无效；A 的尝试结束（`revoke_job_token`）后用 A 的令牌请求被拒；在宿主（非沙盒网段）上使用 A 的令牌被拒——**需核实**（P-73）：核实不可强制时改为断言 Egress `status.detail` 含 `source_check: unverified`，并把结论回写 egress-control.md OQ-2。
- [ ] 资源：fork 炸弹被 `pids` 限制；内存超限被 OOM 终止且 `TerminalFacts.oom = true`（`runc`；`runsc` 的 OOM 报告 **需核实**，不能报告时用例改为断言退出信号 + cgroup 事件）；`work/` 写满时按 OQ-S3 的方案失败而不是写满宿主。
- [ ] 容器内全部进程在 `stop` 返回后不存在（后台进程、`setsid` 脱离的进程都被清掉）。
- [ ] 日志洪水：容器向 stdout 持续写 GB 级数据，宿主磁盘占用有界（`log.max_bytes`），`sandbox` 不阻塞、不 OOM。

### D. 生命周期、计时器、取消（M1）

- [ ] 黄金路径（`fake-worker` 产出合法四件套）：`wait` 返回 `Ok(JobOutcome)`，`terminal.cause = Exited`，`files` 含四个文件，`usage_metered` 取自 `EgressStub`，事件顺序 `sandbox.created` → … → `sandbox.destroyed`。
- [ ] 墙钟超时：SIGTERM 在 `wall_time` 到点发出，`kill_grace_s` 后 SIGKILL（假时钟精确断言），`partial = true`，`timer = Wall`；空闲超时同理，`timer = Idle`。
- [ ] 只被伪造心跳续命的 worker 在 `wall_time` 到点仍被杀。
- [ ] 取消：运行中取消 → `KilledCancel`；`create` / `stage` 阶段取消 → 不 `start`、直接销毁；取消与超时同刻到达，先写入者胜（用确定性调度的测试覆盖两种顺序）。
- [ ] `submit` 幂等：同 `job_id` 重复提交返回同一 `JobHandle`，不创建第二个容器。
- [ ] 排队：超过 `max_concurrent_sandboxes` 的 `submit` 排队不失败，释放一个名额后 FIFO 放行。
- [ ] 各阶段注入失败（`create`、`prepare_workspace`、申请 `job_token`、`start`、`collect`）→ `Err(Infra{对应 detail})`，且容器 / staging / 令牌全部被清理（令牌已吊销）。

### E. 进度采集（M1）

- [ ] 伪造事件：乱序 `seq`、重复 `seq`、超长行（> 8 KiB）、未知 `type`、非 JSON、含 ANSI / bidi 的字符串、虚报 `usage`——全部丢弃或仅展示；`usage_metered` 不变；`sandbox` 持续排空管道不阻塞 worker。
- [ ] 速率上限与总行数上限生效，超限继续排空并计数；`progress_stats` 的分项计数准确。
- [ ] 非协议 stdout 行与 stderr 只进入尾部，大小 ≤ `logs_tail_bytes`，取末尾。
- [ ] `attempt.progress` 事件按采样间隔节流，且不会驱动任何终态（用 `finish` 事件说"成功"但退出码 ≠ 0 的 worker 验证）。

### F. intake 与 collect（M1；对应 worker-protocol §11.3）

- [ ] `out/` 里有符号链接 `result.json -> /etc/passwd`、FIFO、设备文件、硬链接、子目录、额外文件、`result.json.tmp` 残留：均被丢弃并出现在 `unexpected`，**不跟随、不阻塞**（FIFO 用例带超时断言）。
- [ ] 大小边界：`limit` 通过、`limit + 1` 被拒；`stat` 声称很小但实际很大的文件按实际读到的字节判超限。
- [ ] （方式 B，归档导出）构造的 tar 流：头里声明的 `size` 很小而实际数据很大、符号链接 / 硬链接 / 目录 / FIFO / 设备条目、PAX 与 GNU 扩展头、稀疏文件、重复条目、含 `/` 或 `..` 的条目名、总字节数超限——全部记入 `unexpected` 或被拒，读取在上限 + 1 处中止，不无限缓冲。
- [ ] 非 UTF-8 的 `result.json`、含重复键的 JSON、深度 33 的 JSON → 被拒。
- [ ] `output.patch = forbidden` 却有 `changes.patch` → 丢弃并记 `unexpected`，其余产物照常交出。
- [ ] 先停后取：在容器仍运行时调用 `read_out` 被拒；`worker` 在被杀的瞬间仍在写，取到的内容在 `stop` 返回后稳定且重复 `collect` 的 CAS 哈希相同。
- [ ] `work/`、`home/`、`tmp/` 从不被读取（`FakeDriver` 记录所有读取路径并断言）。

### G. 与 Egress 协作（M1）

- [ ] 顺序：`issue_job_token` 在 `create` 之前；`revoke_job_token` 在 `collect` 之后；两者在所有失败 / 取消 / 超时 / 恢复路径上都成对出现（用 `EgressStub` 的调用记录断言）。
- [ ] `revoke_job_token` 重复调用返回同一份用量（模拟 `collect` 与 `attempt.finished` 之间崩溃后重放）。
- [ ] Egress 不健康 → 不启动，`Infra{Egress}`，容器已清理。
- [ ] `[集成:egress]` Egress 重启后运行中的沙盒仍能被正确归属（依赖 egress 的令牌表持久化，egress.md §7.2）。

### H. 崩溃恢复（M1）

- [ ] 在 `Creating` / `Created` / `Staging` / `Starting` / `Running` / `Stopped` / `Collected` / `Destroying` 每个相位后杀掉并重启 `gantryd`：`recover` 之后——运行中的继续监督且 deadline 不变（`Running`）；已退出的直接 `collect`（`Exited`）；已不存在的得到 `Infra{Lost}`（`Lost`）；从未受理的得到 `Unknown` 且不建任何东西；没有重复沙盒；`collect` 的 CAS 对象哈希不变。
- [ ] 终止原因已写入 `meta.json` 但信号尚未发出时崩溃，重启后补发信号，不丢原因。
- [ ] 孤儿：带标签但无对应未终结 Attempt 的容器、无容器的 staging 目录被 `sweep_orphans` 清除（先 `revoke_job_token`）；有对应未终结 Attempt 的不被误杀。
- [ ] 日志重读不重复计数（`seq` 去重）。
- [ ] 请求绑定冲突的批次原子拒绝、相同项幂等、原合法槽位及非零计量不受影响；逐项边界与副作用断言见 [任务身份契约 §14.4](../interfaces/worker-protocol.md#job-identity)。

### I. 预热池（M2）

- [ ] 认领前池成员不含 job 数据、没有 `job_token`（其请求被拒）；`READY` 之前 worker 不读 `job.json`。
- [ ] 认领原子（并发 `submit` 不会拿到同一成员）；用过即销毁，不复用；配置（镜像 / 限额 / 运行时）变更使旧桶作废；空闲超时销毁。
- [ ] `wall_time` / `idle_timeout` 从 `READY` 起算；池关闭时 M1 路径行为不变。
- [ ] 池成员与冷启动沙盒通过同一份隔离用例（§12.C）。

### J. 与退出标准的对应

- [ ] M1 "沙盒内访问 `github.com` 和 `api.github.com` 失败；沙盒内读不到任何模型 key"：§12.C 两条集成用例，在 M1 末的跨泳道集成中以真实 `egress` + Podman 运行。
- [ ] M2 "gVisor 默认开启"：`sandbox.runtime` 缺省为 `runsc`，且 §12.C 全部用例在 `runsc` 下通过（`[集成:gvisor]`）。

## 13. 开放问题

| # | 问题 | 影响 | 本文暂用 |
| --- | --- | --- | --- |
| OQ-S1 | **已决：见裁决 C3 / P-49**（每个尝试一枚 `job_token`，放在模型 API base URL 路径中，取代"沙盒网络身份"；网络只负责隔离）。原问题：**沙盒网络身份**：契约要求 Egress 按"登记时绑定的源地址"区分沙盒（worker-protocol E4），但 rootless Podman 下宿主上的 Egress 未必能观察到每个沙盒的唯一源地址；deploy.md §7.8 的主机防火墙按发起**用户**过滤，只能保证"沙盒只能到 Egress 的两个端口"，区分不了沙盒（它自己也标了"需核实"，OPS-11）。候选方案见 §7.4（特权辅助建网 / Egress 入驻 rootless netns / 每 job 端口 / 每 job Unix socket / 每 job 能力令牌 / 按发起进程识别）。这是 M1 退出标准的前置依赖 | `sandbox`、`egress`、`deploy` | 已落实到 §7.4、§7.9；拿不到令牌就不启动 |
| OQ-S2 | **`gantryd` 与 `gantry-sbx` 之间的文件所有权**：控制通道已由 deploy.md 定为 Podman 套接字（`sudo` 与 `NoNewPrivileges` 冲突），但 `work/` 如何对容器内 uid 可写（辅助容器解包 / `:O` overlay / `:U`）、`out/` 如何读（主机直读 / 归档导出）、作业目录的所有权与 ACL（deploy.md OPS-11 "待定"）、销毁含 subuid 文件的目录，都未决；备选的"套接字"权限面比包装器更大，`validate_invariants` 因此成为唯一的参数闸 | 部署复杂度、攻击面、`repo` 与 `sandbox` 的边界 | 控制通道用套接字；`work/` 与 `out/` 的方案留待 spike（§7.7、§7.8 给出候选与统一的 intake 规则） |
| OQ-S3 | `work/` 与 `out/` 的磁盘配额如何强制（bind mount 无配额；候选：专用文件系统 + project quota、loop 镜像、tmpfs、容器存储配额） | 写满宿主风险、`limits.disk` 语义 | 未强制，仅由工作区预检 + `out/` 配额 + 巡检告警兜底 |
| OQ-S4 | gVisor 在 rootless Podman 下的兼容性：网络模式（`sandbox` / `host` / `none`）与 OQ-S1 方案的关系、cgroup 限额、OOM 报告、`--file-access`、`--platform`（`systrap` / `kvm`）、与 AppArmor / seccomp 的叠加 | M2 默认开启 gVisor 的可行性 | M1 用 `runc` 先行；M2 前做兼容性 spike |
| OQ-S5 | （`sandbox.base_image` 的默认值**已决：见裁决 C20 / P-66**——随版本发布、固定摘要的官方 worker 镜像，引导期可由主机命令写入；项目工具链怎么进沙盒仍未决）镜像组成与归属：`sandbox.base_image` 只有一个，而 worker 适配层要"声明镜像"（含固定版本的 `claude` CLI 与工具链）；项目自己的语言工具链（Rust / Node / Python）怎么进沙盒（预装、devcontainer、运行期经 Egress 安装）；谁构建、谁发布、谁预拉取（deploy.md OPS-11 也在问：`sandbox.base_image` 无默认值而设置页未排期，M1 谁来填） | `sandbox`、`worker`、`deploy` | `SandboxSpec.exec.image_digest` 由 `engine` 从适配层取；`sandbox` 只校验 digest 并 `--pull=never` |
| OQ-S6 | `Sandbox.trust`（trusted / untrusted）在驱动层的语义：骨架只有字段，没有区别对待的规则（design.md 的"档位 × 信任级别"表是历史记录，且已被 P-09 缩小范围） | 隔离强度是否随 trust 变化 | 记录该字段，两种取值都按最严配置处理，不放宽 |
| OQ-S7 | `devcontainer.json` 安全子集的字段清单，以及"环境复现"在无网络沙盒里如何完成（依赖安装走哪个白名单） | M2 | 只保证加固项不可被覆盖 |
| OQ-S8 | 预热池用 `READY` 文件标记是否接受（worker-protocol OQ-16）；本文让**所有**路径都创建 `READY`（常规路径在 `start` 前，预热池在认领后），以统一 worker 逻辑，这比契约字面（"M1 不需要"）多做了一步；池分桶策略与 TTL | M2、worker 启动逻辑 | §7.2 第 7 步、§7.11 |
| OQ-S9 | 事件缺口：`Sandbox.status` 的 `running` / `stopped` / `lost` 没有对应事件；`artifact.unexpected` 审计不在事件目录里 | 投影与审计 | 状态保存在 `meta.json` 与标签；`unexpected` 进 `ResultBundle` |
| OQ-S10 | （替身部分**已决：见裁决 C5 / P-51**——`Store::open_in_memory`、`EgressStub` 为 M0 替身）CAS 归属已由 store.md 提议归 `store`（其 OQ-3）：`Cas::put` / `put_file` / `open`，`CasKind::{ResultBundle, Artifact, Progress, Quarantine}`，`sandbox` 写原始产物、`engine` 写结果包清单（store.md §4），与本文一致。`ResultBundle` 清单形状**已决**（#84，worker-protocol §5.2，`core::worker::ResultBundle`）：`collected_at` 取 `Timings.stopped` 截到秒（`Lost` 取引擎判定丢失那条事件的时间），不取 `collected`；`stdout_tail` / `stderr_tail` 进 `logs`；`progress` 进 `files["progress.jsonl"]`；`usage_metered` 与 `weighted_tokens` 进 `usage_metered`；`EgressFlags` 只有 `capped` / `incomplete` 入清单，`upstream_error_summary` 留在诊断；`partial` 与 `timings` 不入清单。仍待定：被拒产物保留期与隔离区引用（OQ-12，M2）；`store.cas.max_object_bytes`（默认 64 MiB）与产物上限的关系；原 M0 替身缺口已补 | `sandbox`、`engine`、`store` | `sandbox` 交原料，`engine` 组装清单 |
| OQ-S11 | **已决：见裁决 C6 / P-52**（引擎面 `SandboxService`、容器面 `SandboxDriver`，`engine` 只依赖前者；M0 引擎面替身为 `FakeSandboxService`）。原问题：命名层级：milestones 里的 `FakeSandboxDriver` 被说成替代 `sandbox` + `worker`、供 `engine` 使用，所以它应实现引擎面接口；architecture §11 的 `SandboxDriver` trait 则是容器面。engine.md 把引擎面端口也叫 `SandboxDriver`，并把它写成逐步的 `create / stage / start / cancel / collect / destroy / 重新接管`、回传 `RawArtifacts`；本文把引擎面接口收敛成 `SandboxService::submit / cancel / release / recover`（生命周期由本模块内部驱动），回传 `JobOutcome`。两边需要对齐一个名字与粒度 | M0 替身与 trait 命名、`engine` 的终结管线 | 已落实到 §6；`RawArtifacts` 已统一为 `JobOutcome` |
| OQ-S12 | 材料总量分配算法（总量 512 KiB 超限时各条如何分配）与 `attempt.progress` 的采样参数骨架未定 | stage、展示 | §7.2 / §7.6 的 **[提议]** |
| OQ-S13 | M3 逃逸检测的信号清单（意外的系统调用拒绝 / gVisor panic 日志、Egress 收到无效或跨网段的 `job_token`、挂载点之外的宿主文件变化……）及告警去向（InboxItem？日志？） | M3 | 暂不实现，仅预留 `capacity()` 与日志字段 |
| OQ-S14 | `engine` / `guardian` 的失败重试策略不归本模块；但 `Infra{*}` 是否一律"新沙盒重试"、重试上限，需 `engine` 规格确认 | 错误恢复 | `sandbox` 只返回 `Infra` 与 `retryable` 提示 |
| OQ-S15 | 预热池（M2）的令牌交付：容器已启动，环境变量无法再注入；本文提议经 `inputs/` 下的只读文件交给 worker，但 worker-protocol §2.2 规定令牌不写进 `inputs/` | 预热池能否与 `job_token` 共存；适配层（`worker`）需在启动 agent CLI 前读文件设变量 | 预热池默认关闭（`prewarm.enabled = false`），启用前须契约 PR 修订 worker-protocol §2.2 |
