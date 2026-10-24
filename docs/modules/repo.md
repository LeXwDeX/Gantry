# 仓库与工作树（`repo`）

> 状态：设计 v0.1 · 里程碑：M1 起（M2 增量） · 泳道：`exec`（见 [milestones.md](../milestones.md)） · crate / 目录：`crates/gantry-repo/`

`repo` 是服务端上唯一接触 Git 对象的模块：维护每个项目的裸镜像仓（bare mirror），为沙盒准备一次性工作区，并在服务端自建的干净检出上应用沙盒交回的补丁、调 `policy` 校验、落成提交。服务端不在沙盒产出的 `.git` 上运行 git，也不遍历沙盒的工作区目录（[architecture §5](../architecture.md#5-信任边界与不变式) I8）。

> 标注 **[提议]** 的是本文为了能写出接口与测试而给的默认，骨架没定；标注 **需核实** 的是作者没有确认的外部行为；标注 OQ-R*n* 的点见 §13，实现者不得自行拍板。

## 1. 职责

| # | 职责 | 依据 |
| --- | --- | --- |
| R1 | **裸镜像仓**：每项目一个服务端裸仓（目录布局、创建、完整性检查、`gc` 维护、损坏后重建）；向 `forge` 提供受控的拉取 / 推送访问（只给路径与 refspec，不持有任何凭据） | architecture §4、§6；decisions P-25 |
| R2 | **工作树**（Worktree）生命周期：创建、查询、移除；跟踪分支头；产生 `worktree.created` / `worktree.removed` | data-model [Worktree](../data-model.md#worktree--sandbox)；api.md §4.6 |
| R3 | **写权锁**（write lock）：持有者 `engine` / `guardian` / `human`，带租约，到期自动释放，产生 `worktree.locked` / `worktree.unlocked`；提交必须出示有效的锁令牌 | data-model Worktree.lock；api.md §4.6、§3.7 |
| R4 | **准备沙盒工作区**：从镜像仓为一次 Attempt 生成自包含的一次性副本（HEAD 游离在 `base_sha`、index 干净、无 remote / 凭据 / hooks）；review / verify 另生成 `diff.patch` | [worker-protocol §2.4](../interfaces/worker-protocol.md#24-工作区副本) |
| R5 | **补丁闸门**：在服务端自建的一次性检出上用服务端 git 严格应用补丁，以"应用后 git 看到的事实"逐项执行 P1–P11（自己做结构性检查，路径规则 / 权限位 / 敏感内容交 `policy`），输出 `ValidatedPatch`，写 `patch.validated` / `patch.rejected` | worker-protocol §8.3；decisions P-25 |
| R6 | **生成提交**：`verdict = allow` 后在镜像仓里创建提交（parent = `base_sha`，tree = `result_tree`，作者 / 说明由服务端生成，带 trailer `Gantry-Attempt`），推进工作树分支头；重放幂等 | worker-protocol §8.3 末段、§3.5 |
| R7 | **持有补丁（`held`，M2）**：`needs_approval` 的补丁固化保存，不提交不推送；审批 `granted` 且 `subject_digest` 等于补丁 sha256 后才提交 | worker-protocol §8.5；decisions P-20 |
| R8 | **提交溯源**：回答"某个提交 OID 是否是某次已校验补丁的产物"，供 `forge` 在推送前核对（I8 的执行点之一） | forge-actions §3.3 #1、§8 |

### 里程碑交付

| 里程碑 | 本模块交付 |
| --- | --- |
| M0 | 无实现。评审并冻结 §6 的类型与 trait、`worktree.*` / `patch.validated` / `patch.rejected` 的 `data` 形状（落入 `schemas/events/`）。`FakeRepo`（由 `contract` 泳道交付）必须通过本文 §12.A 的"替身一致性用例" |
| M1 | R1–R6 全部：镜像仓、工作树与写权锁（含 `api` 的人工锁）、`prepare_workspace`（Low 档只需 `build` / `single` 形态，不需要 `diff.patch`）、补丁闸门 P1–P11（P6 / P8 / P9 经 `policy` v0：workflow / 依赖清单 / lockfile 一律 deny，`needs_approval` 一律按 deny，decisions P-19）、`allow` 路径的提交生成与崩溃幂等、R8 |
| M2 | R7 `held` 补丁（保存、批准后提交、拒绝丢弃）；`diff.patch` 与候选 head 工作区（Medium / High 的 `review` / `verify`）；`advance_head`（工作树头与人工 / 外部推送对齐，OQ-R10） |
| M3 | 无新增接口。Max 档多轮迭代下的压力验证：同一工作树反复"候选提交 → 评审 → 修正"、工作区准备耗时 |
| M4 | 无新增接口。Ultimate 多交付单元并行：多工作树并发、镜像仓并发读写与 `gc` 互斥的压力验证 |

## 2. 不负责

| 事项 | 归属 |
| --- | --- |
| 从 GitHub 拉取更新镜像仓、推送分支、持有 App token、订阅默认分支 `push` 并产生 `forge.base_advanced` | `forge`（`repo` 只提供路径与 refspec，I2） |
| 路径规则、敏感文件与内容扫描、权限位处置、审批验签 | `policy`（`repo` 提供事实并调用） |
| 沙盒容器、挂载、产物提取（intake）、`/gantry/inputs` 的写入 | `sandbox`（它把 `dest` 目录交给 `repo`，再自己挂载） |
| 提交说明文本的生成 | `models`（`classify.commit_message`），经 `engine` 传入；`repo` 只净化并加 trailer |
| 分支名生成、何时开 PR、冲突后的重新派发、阶段推进 | `engine` |
| Outbox 入队与 `push_branch` 执行 | `engine` / `forge`（I2） |
| 读取沙盒的 `.git` 或 `/gantry/work`、在其中运行 git | **任何模块都不做**（I8）；`repo` 的接口里没有接受此类路径的函数 |
| 事件日志与内容寻址结果库（CAS）的存储实现 | `store`（CAS 归属见 OQ-R9） |
| GitHub 上的分支保护、要求签名提交的规则与传输选择 | `forge`（[forge-actions §8](../interfaces/forge-actions.md#8-提交与签名)） |

## 3. 依赖

规格书与本文同批编写，对方文件若尚不存在，以 `interfaces/` 契约为准。

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `core` | ID 类型（`prj_` `dlv_` `wtr_` `att_`）、`Oid`、`Sha256`、`CasRef`、`Seq`、`UntrustedText`、失败码常量（`policy.*`）、事件 `data` 类型 | [data-model §1–§3](../data-model.md) |
| `store` | `Store::begin_write() -> WriteTx`（`append` / `append_with` 追加 `worktree.*` / `patch.*`，与投影同事务）、`ReadSnapshot::worktree(id)`（`Worktree` 与锁的投影）、`Cas`（`put` / `put_file` / `open` / `stat`，`CasKind::Artifact` / `Quarantine`）；`CasRef` 即 `Sha256Digest` | [store.md §6](store.md)；[data-model §5](../data-model.md#5-事件)；architecture §6 |
| `policy` | `PatchPolicy`（§6.4）：`evaluate_patch`（P1 复核、P5–P9）、`patch_limits`、`scan_text`、`check_approval`（M2） | [policy.md §6.2、§7.4](policy.md)；worker-protocol §8.3；[config §5.2、§5.4](../interfaces/config.md#52-policiespaths) |
| 系统 `git` | 子进程，只经 `GitCmd` 封装调用（§7.2）；最低版本 **需核实**（`git apply`、`commit-tree`、`core.protectNTFS` 行为） | worker-protocol §5.3 |

`repo` **不**依赖 `forge`、`sandbox`、`engine`（architecture §4 依赖图：`repo → sandbox / forge / engine`）。反向地，`forge` 通过 [forge.md §6](forge.md) 草拟的 `MirrorPort`（`mirror_path` / `begin_fetch` / `commit_info`）使用本模块，见 §6.1；`engine` 使用 `RepoService`（[engine.md](engine.md) 的依赖表已按本文的方法名引用）。提交身份（机器人名 / 邮箱）由调用方传入，见 OQ-R3。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 镜像 / 工作树 / 锁 / 提交请求 | `engine`、`guardian`、`api`（人工锁）、`forge`（`ensure_mirror`、`mirror_access`） | §6 签名 | 进程内可信调用；参数仍校验（分支名过 `git check-ref-format` 且前缀合规 [提议]） |
| 镜像仓内容（对象与上游 refs） | `forge` 经 `mirror_access(Fetch)` 拉取 | git 对象；`refs/mirror/heads/*` | **不可信数据**（I5）：仓库内容本身可能含恶意文件名 / 属性 / 子模块，`repo` 不执行其中任何东西 |
| `changes.patch` | `engine` 传 `CasRef`（`sandbox` intake 后已落 CAS） | git 扩展 unified diff（[worker-protocol §5.3](../interfaces/worker-protocol.md#53-补丁格式unified-diffgit-扩展头)） | **不可信** |
| 补丁声明（`result.json` 的 `artifacts.patch`） | 同上 | `sha256` `bytes` `base_sha` `files_changed` | 不可信自报；`repo` 重算后比对 |
| `PrepareWorkspace` | `sandbox.stage` | §6.2 | 可信 |
| `policy` 判定与限额 | `policy` | `PatchPolicyVerdict`、`PatchLimits`（[policy.md §6.2](policy.md)） | 可信 |
| 提交说明 | `engine`（来自 `models` 的 `classify.commit_message` 或模板） | `CommitMessage` | 含模型输出，按**不可信文本**净化 |
| 提交身份、时间戳 | `engine` | `CommitIdentity`、`UtcTime` | 可信（时间戳取 ResultBundle 的 `collected_at`，保证重放得到同一 OID） |
| 已 `granted` 的审批 ID（`held` 补丁，M2） | `engine` | `ApprovalId`；`repo` 经 `policy.check_approval` 复核状态与 `subject_digest` | 可信，但复核 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| `PreparedWorkspace`（含可选 `diff_patch: CasRef`） | `sandbox` | §6.2；`dest` 目录内容满足 worker-protocol §2.4 |
| `PatchOutcome`（`ValidatedPatch` / 不可应用 / 基线冲突 / 无改动） | `engine` | worker-protocol §8.3 末段的 `ValidatedPatch`：`{sha256, result_tree, files[], verdict, reasons[]}` |
| `CommitRecord` | `engine` → `push_branch` 的 `local_head_oid`、`expected_tree_oid`、`validation_ref` | [forge-actions §3.3 #1](../interfaces/forge-actions.md#33-payload-示例) |
| `MirrorFetchGuard` / `MirrorGuard`（仓库路径 + 允许的 refspec，持共享锁）、`CommitInfo{parent, tree, trailers}` | `forge`（`MirrorPort`） | §6.1 |
| `Worktree`、`LockToken` | `engine`、`guardian`、`api` | data-model Worktree；api.md §4.6 |
| 事件 `worktree.created` / `locked` / `unlocked` / `removed`、`patch.validated`、`patch.rejected` | `store` | [data-model §5.2](../data-model.md#52-事件类型目录)；`data` 形状以 `schemas/events/` 为准 |
| 被拒补丁原文、`held` 补丁、`ValidatedPatchRecord` | CAS | 隔离留存审计；保留期 OQ-R9（worker-protocol OQ-12） |

## 6. 对外接口

以下是 Rust 签名草案；`Ctx` 携带 `actor`、`causation_id`、`correlation_id`，用于写事件信封。`FakeRepo`（M0）实现整个 `RepoService`。

### 6.1 镜像仓与工作树

```rust
#[async_trait]
pub trait RepoService: Send + Sync {
    // —— 镜像仓 ——
    /// 幂等。不存在则创建（先建临时目录，成功后原子 rename）。仓库初次 clone 由 forge 执行（它持有 token），
    /// 本函数只负责布局与配置：返回后 forge 经 mirror_access(Fetch) 拉取。
    async fn ensure_mirror(&self, ctx: &Ctx, project: &ProjectId, remote: &RepoSlug) -> Result<MirrorInfo, RepoError>;
    /// 受控访问。Fetch / Push / Read 都持共享锁；`repo` 自己的 gc 持独占锁。守卫 drop 即释放。
    /// Push 模式让 forge 在 `git push` 期间镜像仓里的对象不会被 gc 回收。
    async fn mirror_access(&self, project: &ProjectId, mode: MirrorMode) -> Result<MirrorGuard, RepoError>;
    /// 提交对象的只读信息（git cat-file，不执行任何东西）：parent、tree、trailers。forge 推送前用它做 I8 机械复核。
    async fn commit_info(&self, project: &ProjectId, oid: &Oid) -> Result<Option<CommitInfo>, RepoError>;
    /// 仅解析镜像仓内已有的 ref / OID，不联网。
    async fn resolve(&self, project: &ProjectId, rev: &RevSpec) -> Result<Oid, RepoError>;
    async fn has_commit(&self, project: &ProjectId, oid: &Oid) -> Result<bool, RepoError>;
    /// 完整性检查（git fsck 的受限形式）；失败时置 NeedsRebuild，由 forge 重新拉取。
    async fn check_mirror(&self, project: &ProjectId) -> Result<MirrorHealth, RepoError>;

    // —— 工作树 ——
    /// 幂等：同一 delivery 已有工作树则返回它。分支名由 engine 生成（`gantry/` 前缀）。
    async fn create_worktree(&self, ctx: &Ctx, req: CreateWorktree) -> Result<Worktree, RepoError>;
    async fn get_worktree(&self, id: &WorktreeId) -> Result<Option<Worktree>, RepoError>;
    async fn branch_head(&self, id: &WorktreeId) -> Result<Oid, RepoError>;
    async fn remove_worktree(&self, ctx: &Ctx, id: &WorktreeId, lock: &LockToken) -> Result<(), RepoError>;
    /// M2。仅快进：new_head 必须在镜像仓内且是当前分支头的后代。
    async fn advance_head(&self, ctx: &Ctx, id: &WorktreeId, new_head: &Oid, lock: &LockToken) -> Result<Oid, RepoError>;

    // —— 写权锁 ——
    async fn acquire_lock(&self, ctx: &Ctx, id: &WorktreeId, holder: LockHolder,
                          delivery: &DeliveryId, lease: Duration) -> Result<LockToken, RepoError>; // 冲突 → WorktreeLocked{lock}
    async fn renew_lock(&self, ctx: &Ctx, token: &LockToken, lease: Duration) -> Result<LockToken, RepoError>;
    async fn release_lock(&self, ctx: &Ctx, token: &LockToken) -> Result<(), RepoError>;            // 幂等

    // —— 沙盒工作区 ——（§6.2）
    // —— 补丁闸门与提交 ——（§6.3）
}

pub enum MirrorMode { Fetch, Push, Read }
pub struct MirrorGuard {
    pub git_dir: PathBuf,
    pub fetch_refspecs: Vec<String>,   // ["+refs/heads/*:refs/mirror/heads/*"]；不含 refs/pull/*（见 §7.1）[提议]
    /* 内部持有 flock 共享锁；Drop 释放 */
}
pub struct CommitInfo { pub parents: Vec<Oid>, pub tree: Oid, pub trailers: Vec<(String, String)> }  // trailers 不可信，仅作线索

/// forge.md §6 草拟的 `MirrorPort` 由本模块以下面的薄适配实现（F-OQ-4 的答复）：
///   mirror_path(p)  = mirror_access(p, Read).git_dir
///   begin_fetch(p)  = mirror_access(p, Fetch)       // 返回 MirrorFetchGuard（即 MirrorGuard），拉取期间 repo 不 gc
///   commit_info(..) = RepoService::commit_info
/// `repo` 从不调用 forge。基线提交不在镜像仓（BaseNotInMirror）时，由 engine 请求 forge 先拉取再重试。
pub struct CreateWorktree { pub project_id: ProjectId, pub delivery_id: DeliveryId, pub branch: BranchName,
                            pub base_ref: String, pub base_sha: Oid }
pub enum LockHolder { Engine, Guardian, Human }
pub struct LockToken { pub worktree_id: WorktreeId, pub holder: LockHolder, pub delivery_id: DeliveryId,
                       pub fence: Seq,           // 取 `worktree.locked` 事件的 seq，单调
                       pub lease_until: UtcTime }
```

### 6.2 沙盒工作区

```rust
    /// 由 sandbox.stage 调用。`dest` 是 sandbox 在自己的 staging 目录下建好的空目录；
    /// repo 只往里写，之后绝不再读它（沙盒会往里写东西）。
    async fn prepare_workspace(&self, req: PrepareWorkspace) -> Result<PreparedWorkspace, RepoError>;

pub struct PrepareWorkspace {
    pub project_id: ProjectId,
    pub job_id: AttemptId,
    pub base_sha: Oid,                 // 工作区 HEAD；review / verify 时是候选 head（本地提交）
    pub pr_base_sha: Option<Oid>,      // review / verify 的变更起点
    pub dest: PathBuf,
    pub max_bytes: u64,                // 来自 sandbox 的 limits.disk 的一部分 [提议]；超出 → WorkspaceTooLarge
    pub want_diff: bool,               // true 时生成 diff.patch（pr_base_sha..base_sha）
}
pub struct PreparedWorkspace {
    pub work_dir: PathBuf,             // == dest
    pub head: Oid,                     // == base_sha
    pub tree: Oid,
    pub diff_patch: Option<CasRef>,    // 服务端 git 在镜像仓里生成；sandbox 负责放进 inputs/
    pub stats: WorkspaceStats,         // bytes / files / elapsed_ms
}
```

### 6.3 补丁闸门与提交

```rust
    /// V5。幂等：同一 job_id 重复调用得到同一结果（结果固化进 CAS，以 patch.validated / patch.rejected 为索引）。
    async fn validate_patch(&self, ctx: &Ctx, req: ValidatePatch) -> Result<PatchOutcome, RepoError>;
    /// verdict = allow，或 needs_approval 且带已 granted 的审批。不接受调用方传入的 ValidatedPatch 结构，
    /// 按 job_id 从 CAS 读回（防止调用方篡改判定）。
    async fn commit_patch(&self, ctx: &Ctx, req: CommitPatch) -> Result<CommitRecord, RepoError>;
    async fn provenance(&self, project: &ProjectId, oid: &Oid) -> Result<Option<CommitRecord>, RepoError>;

pub struct ValidatePatch {
    pub project_id: ProjectId, pub delivery_id: DeliveryId, pub job_id: AttemptId,
    pub worktree_id: Option<WorktreeId>,
    pub base_sha: Oid,                     // JobSpec.workspace.base_sha
    pub patch: CasRef,                     // changes.patch，已过 sandbox intake
    pub declared: DeclaredPatch,           // {sha256, bytes, base_sha, files_changed}
    pub job_paths: PathConstraints,        // JobSpec.constraints.paths 快照（生效规则 = 与 policy 当前规则取交，见 worker-protocol §4.5）
    pub job_patch_limits: Option<PatchLimits>, // JobSpec.constraints.patch；只会让限额更小
}
pub enum PatchOutcome {
    Validated(ValidatedPatch),             // verdict: allow | needs_approval | deny
    NotApplicable { detail: String },      // P2 → engine 分类为 exec.output_invalid（patch_not_applicable）
    BaseConflict { base_sha: Oid, branch_head: Oid }, // P10：不自动 rebase，交 engine 重新派发
    NoChange,                              // P11：声明了补丁但无变更，视为无补丁
}
pub struct ValidatedPatch {
    pub sha256: Sha256,                    // 补丁内容摘要 = 补丁审批的 subject_digest（P-20）
    pub base_sha: Oid,
    pub result_tree: Option<Oid>,          // deny 时为 None（不写 tree 对象）
    pub files: Vec<PatchFile>,             // policy.md §6.2 的 PatchFile：{path, status, old_mode, new_mode, added, deleted, is_binary, size_after}
    pub verdict: PatchVerdict,             // Allow | NeedsApproval | Deny
    pub reasons: Vec<PatchReason>,         // {code: PolicyCode(policy.symlink …), path?, rule_id?}；repo 自己发现的结构性问题用同一词表
    pub matched_approval_rules: Vec<String>, // 命中 approve 的规则 id（M2，进入补丁审批的 target.path_rules）
    pub config_rev: Seq,                   // 做出判定时 policy 的 config_rev
}
pub struct CommitPatch {
    pub project_id: ProjectId, pub job_id: AttemptId, pub worktree_id: WorktreeId, pub lock: LockToken,
    pub approval: Option<ApprovalId>,      // held 补丁必填；repo 经 policy.check_approval 复核（只读，不消费）
    pub message: CommitMessage,            // {subject, body}
    pub identity: CommitIdentity,          // {name, email}
    pub timestamp: UtcTime,                // 确定性；取 ResultBundle.collected_at（秒）
}
pub struct CommitRecord { pub commit_oid: Oid, pub tree_oid: Oid, pub parent: Oid,
                          pub patch_sha256: Sha256, pub job_id: AttemptId, pub worktree_id: WorktreeId }
```

### 6.4 使用的 `policy` 接口（[policy.md §6.2](policy.md)）

方法名与 `PatchFacts` 以 `policy.md` 为准（它已采用本文早先草案里的方法名，并把 `PatchFacts` 改成由 `repo` 提供枚举事实与新增行的惰性来源，而不是传补丁的 CAS 引用）。此处只列 `repo` 用到的部分：

```rust
pub trait PatchPolicy: Send + Sync {
    async fn evaluate_patch(&self, facts: &PatchFacts, added: &dyn AddedLineSource) -> Result<PatchPolicyVerdict, PolicyError>;
    fn patch_limits(&self, project: &ProjectId) -> PatchLimits;     // 同步；repo 在应用前做字节数 / 文件数的早期拒绝
    fn scan_text(&self, text: &str) -> Vec<SecretFinding>;          // 与 P9 同一扫描器；repo 用它扫提交说明
    async fn check_approval(&self, approval: &ApprovalId, patch_sha256: &Sha256Digest) -> Result<(), MismatchReason>; // M2 held
}
pub struct PatchFacts { project_id, delivery_id, attempt_id, patch_sha256, patch_bytes,
                        files: Vec<PatchFile>, total_changed_lines, job_paths }   // repo 在 P3 之后构造
pub struct PatchFile { path, status: Added|Modified|Deleted|ModeOnly, old_mode, new_mode, added, deleted, is_binary, size_after }
pub trait AddedLineSource { fn added_lines(&self, path: &RepoPath) -> Box<dyn Iterator<Item = io::Result<String>> + '_>; }
// repo 的实现 `GitAddedLines`：在 scratch 检出存活期间，对指定路径运行
//   git diff --cached -U0 --no-color --no-ext-diff --no-textconv --no-renames HEAD -- <path>
// 并只取以 `+` 开头（不含 `+++`）的行——解析的是 git 自己规整过的输出，不是沙盒给的补丁文本。
```

分工：`repo` 完成 P2–P4 并枚举事实；`policy` 对收到的事实复核 P1、P5、P7（纵深防御）并主责 P6、P8、P9，**收集全部命中的 reasons**，`Deny` 时追加 `policy.denied`；`patch.validated` / `patch.rejected` 由 `repo` 写。P1 / P5 / P7 在 worker-protocol §8.3 里归"闸门"，而 architecture I8 与 config §5.4 把大小校验归 `policy`——两处并存，`policy.md` 的 OQ-P13 也记了；本文的做法是两边都做（`repo` 先结构性判定，`policy` 再断言一遍）。仍有一个缺口：`repo` 在**应用之前**就发现的结构性问题（路径逃逸、`.git` 组件、过大）没有 `PatchFacts` 可交，`policy.denied` 无人追加，见 OQ-R6。

### 6.5 错误

```rust
pub enum RepoError {
    MirrorMissing, MirrorNeedsRebuild, BaseNotInMirror { oid: Oid },
    WorktreeNotFound, WorktreeLocked { lock: LockInfo }, LockRequired, StaleLock,   // fence 过期 / 租约已过
    BaseMoved { base_sha: Oid, branch_head: Oid },                                  // commit_patch 时分支头已前移（P10）
    LeaseTooLong { max: Duration },
    WorkspaceTooLarge { bytes: u64, max: u64 },
    PatchRecordMissing, PatchDigestMismatch, ApprovalInvalid(ApprovalError), VerdictNotCommittable(PatchVerdict),
    Git { op: &'static str, status: Option<i32>, stderr_tail: String },            // stderr 视为不可信文本
    Timeout { op: &'static str }, Io(String), Store(String), Policy(String),
}
```

## 7. 内部设计

### 7.1 磁盘布局与 ref 命名 **[提议]**

```
<data_dir>/mirrors/<owner>/<repo>.git/ 裸仓（路径取自 deploy.md §6.3）。hooks/ 为空目录；config 不含 url 凭据；不含 include
    gantry.lock                         flock：共享 = 读 / fetch / push / 校验 / 准备工作区；独占 = gc、重建
    refs/mirror/heads/*                 上游分支快照（forge fetch 写入）。refspec：+refs/heads/*:refs/mirror/heads/*
    refs/gantry/wt/<wtr_id>             工作树分支头（Gantry 本地推进的提交）
    refs/gantry/attempt/<att_id>        某次 Attempt 已生成的提交（提交幂等的锚点，只创建不改写）
<data_dir>/worktrees/<repo>/<dlv>/      可选的物化检出，仅展示（api.md §4.6 的 `path`，deploy.md §6.3），见 OQ-R1
<data_dir>/workspaces/<sbx_id>/…        sandbox 的作业目录（deploy.md §6.3：gantry:gantry-sbx 0710）；repo 只写 `work/`
<data_dir>/scratch/<job_id>/            validate 的一次性检出（仅 gantry 可见）；成功、失败、超时都删除；启动时清空
```

要点：本地工作放在 `refs/gantry/*`，上游快照放在 `refs/mirror/*`，两个命名空间互不覆盖，所以 `forge` 的 `fetch --prune` 不会误删本地尚未推送的提交。refspec 命名空间是 forge.md F-OQ-4 交给本规格书决定的事：只拉 `refs/heads/*`，**不拉** `refs/pull/*`（第一版不接收 fork PR，P-09；Gantry 自己的分支就在 `heads/gantry/*` 里）。`<data_dir>` 默认 `/var/lib/gantry`（`gantry:gantry 0750`）；`gantry-sbx` 读不到镜像仓、工作树、CAS（deploy.md §6.3 的读取矩阵），所以沙盒拿到的永远是复制品，而不是这些目录里的任何东西。镜像仓是缓存（architecture §6 "可重建"）；因为提交是确定性的（§7.5），丢失 `refs/gantry/*` 时可由 CAS 里的补丁加 `base_sha` 重建。

### 7.2 git 调用约束

所有 git 子进程只经 `GitCmd` 构造，禁止在其他位置直接 `Command::new("git")`（用 clippy `disallowed_methods` + 测试强制，§12.G）。`GitCmd` 固定：

| 项 | 取值 |
| --- | --- |
| 环境 | `GIT_CONFIG_NOSYSTEM=1`、`GIT_CONFIG_GLOBAL=/dev/null`、`HOME` 指向空目录、`GIT_TERMINAL_PROMPT=0`、`GIT_ATTR_NOSYSTEM=1`、`GIT_OPTIONAL_LOCKS=0`、`LC_ALL=C`；不继承服务端进程的其他环境变量 |
| `-c` | `core.hooksPath=/dev/null`、`core.fsmonitor=false`、`core.protectNTFS=true`、`core.protectHFS=true`、`core.autocrlf=false`、`core.attributesFile=/dev/null`、`apply.whitespace=nowarn` |
| 网络 | 校验 / 提交 / 工作区准备类命令一律不联网：`GIT_ALLOW_PROTOCOL=file`（工作区准备从本地镜像仓取对象需要）。联网的 fetch / push 由 `forge` 执行，不经 `GitCmd` |
| 工作目录 | 镜像仓或 `scratch/<job_id>/`；**不得**是任何由沙盒写入过的目录 |
| 资源 | 每次调用有墙钟超时（默认 60 s [建议值，待调优]）、输出上限、`RLIMIT_FSIZE` / `RLIMIT_NOFILE`；超时 → 杀进程组 → `RepoError::Timeout` |
| 并发 | 每项目 `Semaphore(git_parallelism)`（默认 4 [建议值，待调优]） |

镜像仓配置：`transfer.fsckObjects=true`、`fetch.fsckObjects=true`（拒收畸形对象与 `.git` 路径技巧，由 `forge` 的 fetch 继承，**需核实**对 `.gitmodules` / 符号链接类问题的覆盖）、`gc.auto=0`（`gc` 由 `repo` 在独占锁下定时执行）、`gc.pruneExpire=2.weeks`（防止刚写入、尚未被 ref 引用的对象被回收）。

### 7.3 `prepare_workspace` 配方 **[提议]**（后置条件以 worker-protocol §2.4 为准）

1. `has_commit(base_sha)`（及 `pr_base_sha`）；缺失 → `BaseNotInMirror`。
2. 在 `dest` 中 `git init`，`git fetch --no-tags file://<mirror> <base_sha> [<pr_base_sha>]`，仅在这条命令行上设 `-c uploadpack.allowAnySHA1InWant=true`；`git checkout --detach <base_sha>`。历史深度 / 过滤（`--depth` / `--filter`）取决于 OQ-R4，**默认不浅克隆**。备选 `git clone --no-hardlinks --no-checkout`，**需核实**两者在 git 版本上的行为。
3. **禁止** hardlink / reflink / `alternates` / `--shared` 指向镜像仓：沙盒内文件被 `chown` 到容器用户后，硬链接会让沙盒改写镜像仓的对象文件（I1 / I8 的重点反例）。必须 `--no-hardlinks` 语义，即复制。
4. 清理：删除 `remote.*`、`refs/remotes/*`、packed-refs 中的非 HEAD 引用、`.git/hooks/*`；`.git/config` 只保留 `core` 最小集合，无 url / include / credential；不写 `.git/info/attributes`。
5. 子模块只有 gitlink（不初始化），LFS 只有 pointer 文件，不执行 `git lfs`（worker-protocol §2.4）。
6. `want_diff`：在镜像仓里 `git diff --no-color --no-ext-diff --no-textconv --no-renames --full-index -U3 <pr_base_sha> <base_sha>`，输出入 CAS 返回（服务端 git、服务端对象）。
7. 自检（后置条件，失败 → `Git{...}`）：`git status --porcelain` 空；HEAD 游离且等于 `base_sha`；`.git` 下所有普通文件 `st_nlink == 1`；无 `objects/info/alternates`；`git fsck --connectivity-only` 通过；总字节 ≤ `max_bytes`。
8. 返回后由 `sandbox` 负责目录属主 / 权限适配（用户映射见 [sandbox.md §7.7](sandbox.md#77-用户映射与文件属主-提议)）；`repo` 不 `chown`。

### 7.4 补丁闸门流水线（V5）

原则（worker-protocol §8.3）：**不自己写 patch 解析器**，事实来自 git 自己的输出；顺序是"原始字节数 → git 解析（不落盘）→ 应用 → 应用后枚举 → 规则"。

```
validate_patch(req):
  0. 幂等：job_id 已有结论 → 直接返回
  1. 取补丁字节（CAS）；重算 sha256；与 declared.sha256 / declared.bytes 不符 → 整体丢弃（engine 归 exec.protocol）
     declared.base_sha != req.base_sha → 同上
  2. P1a  字节数 > max_bytes → Deny policy.size（尚未解析）
  3. 预解析（git 自己的解析，不碰文件系统）：
        git apply --numstat -z --summary -p1 <patch>
     取 文件数 / 总变更行 / 路径 / 模式（create|delete|mode change）；
     文件数 > max_files 或 总行 > max_changed_lines → Deny policy.size
     对预解析出的每条路径与模式先做 P4 / P5 / P7（结构性），命中即 Deny（给出 policy.path_invalid / dotgit /
     symlink / submodule / binary）。命令本身失败 → NotApplicable{patch_malformed}。
     —— 之所以在应用前做：路径逃逸类补丁会被 git apply 直接拒绝，如果只靠应用后的事实，
        这类补丁会被归为 exec.output_invalid 而不是契约要求的 policy_denied（§11.3）。需核实 numstat / summary
        对 `../../x`、`.GIT/config` 这类路径是否稳定列出（OQ-R5）。
  4. P10  若 req.worktree_id 存在且 branch_head != base_sha → BaseConflict（不自动 rebase）
  5. 建一次性检出：git worktree add --detach <scratch>/<job_id> <base_sha>（或等价方式，见 OQ-R5）
  6. P2   git apply --index --whitespace=nowarn -p1 <patch>   （严格模式，不加 --3way / --reject / --recount /
                                                              --unidiff-zero / --ignore-whitespace）
          失败 → 删除 scratch → NotApplicable{git 的原因}
  7. P3   git diff --cached --raw -z --no-renames --full-index HEAD  +  --numstat -z  → 事实清单 files[]
          以此为准重做 P4 / P5 / P7（应用后事实是权威）；检查大小写 / Unicode 规范化冲突（P4 末项）
          单文件应用后大小 ≤ max_file_bytes（git cat-file --batch-check 读 index 里 blob 大小）→ 否则 Deny policy.size
  8. P11  files 为空 → NoChange
  9. policy.evaluate_patch(PatchFacts, GitAddedLines)   → P1 复核、P5–P9；policy 收集全部命中的 reasons，
     Deny 时由 policy 追加 policy.denied。应用后枚举到的结构性问题（第 7 步）**同样**交给它（让 reasons 完整、
     审计齐全），而不是在 repo 里提前返回
 10. 合并结论：Deny > NeedsApproval > Allow；repo 只会使结论更严（policy 说 Allow 而 repo 发现结构性问题 → 仍 Deny）；
     应用前（第 2–3 步）就拒绝的情形没有 PatchFacts 可交，见 OQ-R6
 11. verdict != Deny → git write-tree → result_tree；固化 ValidatedPatchRecord 与补丁字节进 CAS
     verdict == Deny → 不写 tree；补丁原文进 CAS 隔离区
 12. 追加事件（Allow / NeedsApproval → patch.validated；Deny / NotApplicable → patch.rejected），清理 scratch
```

P1–P11 的执行者与失败码一览：

| # | 检查 | 执行者 | 判定 / 码 |
| --- | --- | --- | --- |
| P1 | 字节数 / 文件数 / 总变更行 / 应用后单文件大小 | `repo` 在应用前用 `policy.patch_limits()` 与 `job_patch_limits` 的较小者做早期拒绝（避免应用巨型补丁），并把统计值放进 `PatchFacts`；`policy` 复核（policy.md §7.4 步 a） | Deny `policy.size` |
| P2 | 严格可应用 | `repo` | `NotApplicable` → `exec.output_invalid`（`patch_not_applicable`） |
| P3 | 枚举事实 | `repo` | — |
| P4 | 路径规范：UTF-8；无绝对路径、`..`、空组件、`\`、NUL；任何组件不得是 `.git`（含大小写变体、NTFS 8.3 `GIT~1`、NTFS 备用数据流、HFS 可忽略字符）；不得有两条路径仅大小写或 Unicode 规范化（NFC / NFD）不同 | `repo`（结构性） | Deny `policy.path_invalid` / `policy.dotgit` |
| P5 | 仅允许模式 `100644`、`100755`；任一侧为 `120000` 或 `160000` 即拒；`.gitmodules` 任何改动同处理 | `repo`（结构性）；`policy` 再断言一次（policy.md §7.4 步 b、c；P-41：符号链接无审批路径） | Deny `policy.symlink` / `policy.submodule`（边界解释见 OQ-R5） |
| P6 | 新文件 `100755`：允许并记录；已有文件 `100644 ↔ 100755`：交 `policy` | `policy` | `needs_approval`（M1 等同 deny，decisions P-19） |
| P7 | 二进制：`numstat` 为 `- -`，或补丁出现 `Binary files` / `GIT binary patch` | `repo` 置 `PatchFile.is_binary`；`policy` 处置（policy.md §7.4 步 d：暂一律 deny） | Deny `policy.binary`（P-41 的 allowlist 见 OQ-R5） |
| P8 | 路径规则（新旧路径都算；`allow` / `deny` / `approval_required`；取 `policy` **当前**规则） | `policy` | Deny `policy.path_outside_allow` / `path_denied` / `path_protected`（M1）；`needs_approval`（M2） |
| P9 | 敏感文件名与新增行密钥扫描 | `policy` | Deny `policy.secret` |
| P10 | base 一致 | `repo` | `BaseConflict` |
| P11 | 空补丁 | `repo` | `NoChange` |

`repo` 把失败码原样放进 `PatchReason.code`；`Attempt.failure_class` / `failure_detail` 的映射由 `engine` 按 [worker-protocol §9](../interfaces/worker-protocol.md#9-失败分类) 完成。一次性检出里**不执行任何仓库内容**（hooks 关闭、不构建不测试、不运行 filter / textconv），`deny` 或 `needs_approval` 时整体丢弃或冻结。

### 7.5 提交生成与幂等

```
commit_patch(req):
  1. 读 ValidatedPatchRecord（按 job_id）；缺失 → PatchRecordMissing
     verdict == Deny → VerdictNotCommittable
     verdict == NeedsApproval → 必须有 req.approval：经 ApprovalService::get 读出，要求 status == granted、未过期、
                                action == push_branch、subject_digest == record.sha256（即 policy.check_approval(approval, sha256)）；
                                并让 policy 以当前规则重新 evaluate_patch 一次，若已变为 Deny → 拒绝（config.md §2.6）。
                                repo 只读审批，不消费——消费在 forge 推送成功之后（policy.md §6.5 的 ApprovalGate）
  2. 校验锁：req.lock.fence == 当前锁的 fence 且 now < lease_until 且 holder ∈ {Engine, Guardian}；Human 持锁时不得提交
  3. 幂等锚点：若 refs/gantry/attempt/<job_id> 已存在 → 取其指向的提交，校验 tree == record.result_tree，返回 CommitRecord
  4. P10：branch_head == record.base_sha，否则返回 RepoError::BaseMoved（不自动 rebase，交 engine 重新派发）
  5. 净化提交说明：去 NUL / 控制符 / bidi；中和 `@用户` / `@团队`、跨仓库引用、关闭关键字；剥离正文中形似 trailer 的行
     （`Co-authored-by`、`Signed-off-by`、`Gantry-*` 等，防伪造）；`policy.scan_text` 命中密钥 → 用模板说明替代；
     最后追加固定 trailer：`Gantry-Attempt: <job_id>`
  6. git commit-tree <result_tree> -p <base_sha> -F <msg>
        GIT_AUTHOR_* / GIT_COMMITTER_* = identity，日期 = req.timestamp  （确定性：同输入得同 OID）
  7. git update-ref refs/gantry/attempt/<job_id> <commit> 0000…   （仅创建）
     git update-ref refs/gantry/wt/<wtr_id>      <commit> <base_sha>（比较并交换）
  8. 返回 CommitRecord
```

- engine.md 的崩溃恢复一节写"`repo` 按 `Gantry-Attempt` trailer 查已有提交"；本模块按 `refs/gantry/attempt/<job_id>` 查。trailer 可以被 GitHub 来源的提交伪造，只能给人看，不能当信任锚（`provenance` 也以此 ref 加 `patch.validated` 事件为准）。
- 崩溃在第 6–7 步之间：重放得到同一 OID（日期、身份、说明、tree、parent 全部确定），`update-ref` 对同值幂等；孤儿提交对象无害。
- 提交说明由 `repo` 净化的原因：提交一旦创建，OID 即被审批 / `push_branch` 绑定，`forge` 在 `git` 传输下不能再改写它（forge-actions §8），所以 [forge-actions §2.4](../interfaces/forge-actions.md#24-标记marker与出站文本净化) 的出站净化规则在这里必须先执行一遍。共享净化实现的归属见 OQ-R8。
- 提交身份与说明来源见 OQ-R3；多提交粒度见 worker-protocol OQ-10（本文：单补丁、单提交）。

### 7.6 写权锁

- 状态来自事件，**裁决在 `store`**：[store.md §7.3](store.md) 的状态机在追加 `worktree.locked` 时用 `worktree_lock::acquire(current, req, event.ts)` 判定冲突与租约是否到期（以 `event.ts` 为准），他人未过期的锁 → `PreconditionFailed`，`repo` 映射为 `WorktreeLocked{lock}`。因此 `repo` 不需要自己的互斥锁来保证排他：单写者事务 + 状态机即是仲裁点；`repo` 负责到期计时器与 `LockToken` 的围栏校验。
- `fence` = `worktree.locked` 事件的 `seq`，天然单调。`commit_patch` / `advance_head` / `remove_worktree` 必须带 `LockToken` 并与当前锁比对，租约过期后的旧持有者不能提交（围栏令牌）。
- 重入：同 `holder` + 同 `delivery` 再次 `acquire` 视为续租。人工锁 `lease` 默认 1800 s、最长 14400 s（api.md §3.7）；`engine` / `guardian` 的租约时长与续租节奏骨架没定，见 OQ-R7。
- 不可抢占：`engine` / `guardian` 持有时，`human` 取锁返回 `WorktreeLocked{lock}`（api `409 worktree_locked`）；反之 `human` 持有时 `engine` 提交返回同一错误，由 `engine` 等待或上报。
- 到期：每个有锁的工作树登记一个计时器；到期追加 `worktree.unlocked`（`actor.kind = system`，`data.reason = "expired"`）。启动时扫描投影，对已过期的补发。
- 只读操作（`prepare_workspace`、`validate_patch`）不要求持锁；是否持锁跨整个 `build` 尝试由 `engine` 决定。

### 7.7 并发与资源

- 全部 git 操作异步包装（`tokio::process`），不阻塞运行时；每项目 git 并行度受限；`scratch/` 总占用有上限（默认 `max_concurrent_validations × (max_bytes + 检出体积)`，超限排队）。
- 镜像仓读写靠 git 自身的原子 ref / 对象写入；`gc`、重建需要独占 `gantry.lock`，期间 `mirror_access` / `prepare_workspace` 等待（有超时，超时返回可重试错误）。
- 校验结果与补丁字节写入 CAS 后再追加事件；崩溃后重放以事件为索引，缺事件则重新校验（校验是纯函数，同输入同输出）。

### 7.8 持久化

`repo` 不自带数据库：镜像仓与 `scratch/` 在文件系统上；工作树与锁的事实在事件日志；校验记录与补丁字节在 CAS。启动恢复：清空 `scratch/`；`git worktree prune`；补发过期锁的 `worktree.unlocked`；对状态为 `NeedsRebuild` 的镜像仓通知 `forge`（经返回值，不经回调）。

## 8. 不变式与安全要求

| 不变式 | 本模块如何保证 |
| --- | --- |
| **I8** 执行产物先验后推 | 补丁只在服务端自建的一次性检出上用服务端 git 应用；服务端不在沙盒产出的 `.git` 上运行 git，不遍历 `/gantry/work`——接口里没有接受沙盒目录的函数（`validate_patch` 吃 `CasRef`，`prepare_workspace` 的 `dest` 只写）；提交只用 `result_tree`，提交的 OID 就是 `forge` 之后推送与审批绑定的对象；`R8 provenance` 供推送前核对 |
| **I1** 沙盒只计算 | 工作区副本自包含、无 remote / 凭据 / hooks，不与镜像仓共享任何 inode（禁 hardlink / alternates / reflink）；镜像仓不被挂进沙盒 |
| **I3** 放行唯一决策点 | `repo` 只做结构性检查，路径规则 / 权限位 / 敏感内容的放行只来自 `policy`；`repo` 对 `policy` 的结论只会更严（Deny > NeedsApproval > Allow），不会放宽；`needs_approval` 的补丁没有 `granted` 且 digest 一致的审批、且 `policy` 以当前规则复评仍非 Deny，就不能提交 |
| **I5** 外部文本不可信 | 仓库内容、补丁、git 的 stderr、提交说明都当数据：不执行、不拼进命令行的 shell 字符串（参数数组传递）、进日志前去控制符；提交说明净化后才写入对象 |
| **I2** GitHub 写入唯一路径 | `repo` 不持 token、不联网、不推送；`MirrorGuard` 只给路径与 refspec |
| **I4** 状态各有事实源 | 镜像仓是 GitHub 的缓存，可重建；工作树 / 锁 / 校验结论的事实源是事件日志 |

附加要求：

- `GitCmd` 之外不得出现 git 子进程；任何 git 调用不得以沙盒写入过的目录为 `cwd`、`--git-dir` 或 `--work-tree`。
- 一次性检出内不执行仓库内容：测试用带陷阱的仓库（`.gitattributes` filter、`core.fsmonitor`、可执行 hook、符号链接指向检出外）验证标记文件不被创建（§12.F）。
- 拒收的补丁不得在任何持久检出或 ref 上留下痕迹；仅 CAS 隔离区保留原文。
- 日志与事件里的路径 / 说明文本都是不可信文本，展示端按纯文本渲染。

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 镜像仓不存在 / 损坏（`fsck` 失败） | `MirrorNeedsRebuild`；`forge` 重新 clone；`repo` 把旧目录改名保留；`refs/gantry/*` 由 CAS 补丁 + `base_sha` 确定性重放恢复，无法恢复的提交上报 |
| `base_sha` 不在镜像仓 | `BaseNotInMirror`；`engine` 让 `forge` 先 fetch 再重试，仍缺则 `infra.workspace_prep` |
| `prepare_workspace` 超限 / 超时 / 自检失败 | 删除 `dest` 内容，返回 `WorkspaceTooLarge` / `Timeout` / `Git`；`sandbox` 归 `infra.workspace_prep`，新沙盒重试 |
| git 子进程超时或崩溃 | 杀进程组，清理 `scratch/<job_id>`，返回 `Timeout` / `Git`；`validate_patch` 对同输入可安全重放 |
| 补丁不可应用 | `NotApplicable`，`patch.rejected`；`engine` 可带反馈重试 |
| 校验后、提交前服务端崩溃 | 重放 `validate_patch` 命中 CAS 记录直接返回；`commit_patch` 靠 `refs/gantry/attempt/<job_id>` 幂等 |
| 分支头已前移（校验时 `BaseConflict`、提交时 `BaseMoved`） | 不自动 rebase，返回给 `engine` 重新派发；不产生失败事件 |
| 锁过期后旧持有者提交 | `StaleLock`，不写任何东西 |
| `policy` 不可用 | 失败闭合：`validate_patch` 返回 `Policy` 错误，不给 `allow`；`engine` 视为 `infra_error` 重试 |
| CAS / 事件写入失败 | 返回 `Store`；已创建的 git 对象为无引用孤儿，等 `gc` |
| `gc` 与读者冲突 | 独占锁互斥；读者等待超时返回可重试错误 |
| 磁盘不足 | 拒绝新的 `prepare_workspace` / `validate_patch`（`Io`），不删除既有镜像仓 |

## 10. 配置

`repo` 没有自己的设置项（设置只在 settings 路由，I7）。它消费：

| 配置 | 来源 | 用途 |
| --- | --- | --- |
| `policies.patch.max_files` / `max_bytes` / `max_changed_lines` / `max_file_bytes`（默认 200 / 5,000,000 / 10000 / 1,000,000） | [config §5.4](../interfaces/config.md#54-policiesapproval-与-policiespatch)，经 `policy.patch_limits()` | P1 阈值。config §1.3 的"各模块读哪里"未把 `repo` 列为消费者，见 OQ-R6 |
| `policies.paths` / `policies.allow_globs` | 同上，仅 `policy` 读取 | P8 |
| `sandbox.limits.disk_mb` | 经 `sandbox` 折算为 `PrepareWorkspace.max_bytes` | 工作区体积上限 |

引导配置（`deploy` 拥有，文件、重启生效；config §1.1）**[提议]**：

| 项 | 默认 | 说明 |
| --- | --- | --- |
| `repo.data_dir` | `/var/lib/gantry` | 镜像仓、工作树、`scratch/` 的根；属于服务端系统用户 |
| `repo.git_parallelism` | 4 | 每项目 git 并行度 [建议值，待调优] |
| `repo.git_timeout_s` | 60 | 单次 git 调用超时 [建议值，待调优] |
| `repo.gc_interval` | 24 h | 独占锁下的 `gc` 周期 [建议值，待调优] |
| `repo.scratch_max_concurrent` | 4 | 并发校验数 [建议值，待调优] |

## 11. 可观测性

事件（见 [data-model §5.2](../data-model.md#52-事件类型目录)）：

| 事件 | `data` 至少含 |
| --- | --- |
| `worktree.created` | `worktree_id`、`project_id`、`delivery_id`、`branch`、`base_ref`、`base_sha` |
| `worktree.locked` / `worktree.unlocked` | `worktree_id`、`holder`、`delivery_id`、`lease_until`（locked）；`reason`（`released` / `expired`）（unlocked） |
| `worktree.removed` | `worktree_id`、`branch` |
| `patch.validated` | `job_id`、`sha256`、`base_sha`、`result_tree`、`verdict`、`files[]`、`reasons[]`、`config_rev` |
| `patch.rejected` | `job_id`、`sha256`、`reasons[]`（含 `policy.*` 码与路径）或 `not_applicable` 的 git 原因、`config_rev` |

`policy.denied` 由谁追加见 OQ-R6；`commit_oid` 写在哪个事件见 OQ-R2。

日志（结构化，字段 `project_id`、`job_id`、`worktree_id`、`op`）：每次 git 调用的命令名、退出码、耗时、stderr 尾部（视为不可信文本，去控制符）；锁的获取 / 冲突 / 过期；校验各步的耗时与结论。不记录补丁正文与文件内容。

指标：`repo_git_duration_seconds{op}`、`repo_patch_outcome_total{outcome,reason}`、`repo_workspace_prepare_seconds`、`repo_workspace_bytes`、`repo_lock_conflicts_total{holder}`、`repo_lock_expired_total`、`repo_mirror_bytes{project}`、`repo_scratch_inflight`、`repo_commit_replays_total`。

## 12. 测试与验收

测试替身：`ScriptedPolicy`（`policy` 的 `testing` feature 导出，可脚本化 `PatchPolicyVerdict` / `PatchLimits` / 扫描结果 / `check_approval`；M0 替身清单里没有 policy 替身，见 OQ-R6）、`Store::open_temp` / `open_in_memory`（`store` 的 testing feature，含事件、投影、锁状态机与 CAS，见 [store.md §6.1](store.md)）、**真实 `git`** 构造临时镜像仓（本模块的测试以真实 git 为主，因为校验的事实源就是 git 的行为）。恶意补丁语料放在 `crates/gantry-repo/tests/fixtures/patches/`，手写文本补丁（`git diff` 产生不出的畸形补丁必须手工构造），同时作为 `schemas/worker/fixtures/` 的候选。

### A. 替身一致性（M0 / M1）

- [ ] `FakeRepo` 与真实 `RepoService` 通过同一份 trait 用例集：锁语义、幂等、`PatchOutcome` 各分支的返回形状一致。
- [ ] 用例集覆盖 `FakeRepo` 的脚本化 "通过 / 拒绝" 能在不启动 git 的情况下驱动 `engine` 测试。

### B. 镜像仓、工作树、锁（M1）

- [ ] `ensure_mirror` 幂等；创建中途崩溃不留下半成品（临时目录 + 原子 rename）。
- [ ] 镜像仓 `hooks/` 为空；`config` 不含带凭据的 URL（逐行断言）；`gc.auto=0`、`transfer.fsckObjects=true` 已设置。
- [ ] `create_worktree` 同一 `delivery_id` 重复调用返回同一工作树；产生一条 `worktree.created`。
- [ ] 锁：同时 `acquire` 只有一个成功；`human` 不能抢占 `engine`；`engine` 在 `human` 持锁时提交得到 `WorktreeLocked`；人工锁 `lease` > 14400 s 被拒。
- [ ] 租约到期自动释放并追加 `worktree.unlocked(reason=expired)`；用假时钟推进验证。
- [ ] 围栏：旧持有者租约过期、新持有者取锁后，旧令牌 `commit_patch` 得到 `StaleLock`，且没有任何 ref / 对象变化。
- [ ] 重启后从事件重建锁状态；已过期的锁在启动时补发 `worktree.unlocked`。

### C. 沙盒工作区（M1；`diff.patch` 为 M2）

- [ ] 产物满足 worker-protocol §2.4：`git status --porcelain` 为空；HEAD 游离且等于 `base_sha`；`.git/config` 无 `remote` / `url` / `include` / `credential`；`.git/hooks` 无任何文件；无 `objects/info/alternates`；`.git` 下所有普通文件 `st_nlink == 1`；`git fsck` 通过；仓库里除 HEAD 外没有 `refs/mirror/*` 与 `refs/gantry/*`。
- [ ] 修改工作区中的任意 `.git` 文件不影响镜像仓（对镜像仓目录做前后哈希比对）。
- [ ] 子模块目录只有 gitlink；LFS pointer 文件内容不变。
- [ ] `base_sha` 不在镜像仓 → `BaseNotInMirror`；超过 `max_bytes` → `WorkspaceTooLarge` 且 `dest` 已清理。
- [ ] `want_diff`（M2）：`diff.patch` 与独立运行的 `git diff pr_base..base` 逐字节相同，且确定性（重复生成哈希一致）。

### D. 补丁闸门（M1；覆盖 worker-protocol §11.3 的对抗用例）

- [ ] 黄金补丁 → `Validated(Allow)`；`result_tree` 等于测试中独立 `git apply` 得到的 tree；镜像仓的 `refs/` 前后不变。
- [ ] 符号链接（`120000`）→ Deny `policy.symlink`；gitlink / `.gitmodules` 改动 → `policy.submodule`。
- [ ] `.git/hooks/pre-commit`、`.GIT/config`、`GIT~1/config`、NTFS 备用数据流 `.git::$INDEX_ALLOCATION`、HFS 可忽略字符 `.g\u{200c}it/config` → `policy.dotgit`。
- [ ] `a/../../x`、绝对路径、含 `\`、含 NUL、空组件 → `policy.path_invalid`（**这一条同时验证"预解析先于应用"，否则会被归为 `NotApplicable`**）。
- [ ] 仅大小写不同的两条路径、NFC / NFD 同形的两条路径 → `policy.path_invalid`。
- [ ] 二进制补丁（`GIT binary patch` 与 `Binary files`）→ `policy.binary`。
- [ ] 边界值：补丁字节、文件数、总变更行、单文件字节各取 `limit` 通过、`limit + 1` 拒绝（`policy.size`）；超大补丁在应用前就被拒（断言没有创建 scratch 检出）。
- [ ] `base_sha` 错误或上下文不匹配 → `NotApplicable`；应用是全有或全无（失败后 scratch 已删除、镜像仓无新 ref）。
- [ ] 声明的 `sha256` / `base_sha` 与重算不符 → 整体拒绝，不进入应用。
- [ ] 空补丁 → `NoChange`。
- [ ] `ScriptedPolicy` 返回 Deny `policy.path_protected`（M1：workflow / 依赖清单 / lockfile）→ `Validated(Deny)`，`patch.rejected` 含该码与路径；返回 `NeedsApproval`（M2）→ `Validated(NeedsApproval)`，`result_tree` 已计算。
- [ ] P6：已有文件 `100644 → 100755` 交 `policy`；新文件 `100755` 记录进 `files[]`。
- [ ] P9：新增行含密钥 → Deny `policy.secret`（`ScriptedPolicy` 回放扫描结果）。
- [ ] 应用前（字节数 / 文件数 / 路径逃逸）的结构性拒绝不调用 `policy.evaluate_patch`，也不创建 scratch 检出；应用后枚举到的结构性问题（符号链接、gitlink、二进制）**仍然**调用 `evaluate_patch`，`ScriptedPolicy` 收到包含这些文件的 `PatchFacts`，最终 reasons 是 repo 与 policy 的并集。
- [ ] `repo` 对 `policy` 结论只会更严：`ScriptedPolicy` 返回 Allow 而 `repo` 发现结构性问题 → 仍 Deny。
- [ ] 幂等：同一 `job_id` 重复 `validate_patch` 返回相同结果、只追加一次事件。
- [ ] 不可信路径名进入事件 / 日志前去除控制符与 bidi 字符。

### E. 提交（M1；`held` 路径为 M2）

- [ ] 提交的 parent = `base_sha`、tree = `result_tree`、作者 / 提交者 = 传入的身份、日期 = 传入的时间戳、末尾 trailer 含 `Gantry-Attempt: <job_id>`。
- [ ] 提交说明净化：伪造的 `Co-authored-by:` / `Signed-off-by:` / `Gantry-*:` 行被剥离；`@user`、`owner/repo#N`、`Fixes #N`、NUL、bidi 被中和或去除；含密钥 → 用模板说明替代。
- [ ] 确定性与幂等：在 `commit-tree` 之后、`update-ref` 之前注入崩溃，重放得到同一 OID，`refs/gantry/attempt/<job_id>` 与分支头一致，无第二个提交。
- [ ] 分支头 ≠ `base_sha` → `BaseMoved`，无提交。
- [ ] 无锁 / 过期锁 / `human` 持锁 → 对应错误，无提交。
- [ ] `verdict = Deny` 不可提交（`VerdictNotCommittable`）；`provenance(commit_oid)` 对本模块产生的提交返回记录，对镜像仓里来自 GitHub 的提交返回 `None`（即使其说明里伪造了 `Gantry-Attempt` trailer）。
- [ ] （M2）`held`：`NeedsApproval` 不提交；审批 `granted` 且 `subject_digest == sha256` 后提交，`result_tree` 不变；`subject_digest` 不符、审批过期、或 `policy` 以当前规则复评为 Deny → 拒绝。

### F. 安全与隔离（M1）

- [ ] 一次性检出内不执行仓库内容：仓库带 `.gitattributes` 的 `filter` / `diff` 驱动、镜像仓 `config` 里的 `core.fsmonitor`、可执行的 hook、指向检出外的符号链接——标记文件始终不被创建。
- [ ] `GitCmd` 是唯一的 git 入口：clippy `disallowed_methods` 通过；另有测试扫描源码，断言没有 `Command::new("git")`。
- [ ] 类型 / 签名检查：`RepoService` 没有任何函数接受"沙盒写入过的目录"作为读取来源（以 API 形状评审 + 编译期类型保证）。
- [ ] `GIT_*` 环境与 `-c` 固定项逐项出现在每条子进程命令中（用假 `git` 可执行文件记录 argv / env）。
- [ ] 被拒补丁不在任何 ref、持久检出上留痕；仅 CAS 隔离区有原文。

### G. 并发与恢复（M1）

- [ ] 20 个并发 `validate_patch` 对同一镜像仓，结果互不干扰，结束后 `scratch/` 为空、`git worktree list` 无残留。
- [ ] `gc` 持独占锁时 `prepare_workspace` 等待而不是失败；`gc` 后 `refs/gantry/*` 引用的对象仍在。
- [ ] 重启：`scratch/` 被清空；崩溃遗留的 worktree 元数据被 prune。
- [ ] 镜像仓损坏（删除一个 pack）→ `check_mirror` 报 `NeedsRebuild`；重建后用 CAS 补丁重放，得到与原来相同的 `commit_oid`。

### H. 与里程碑退出标准的对应

- [ ] M1 "改动 `.github/workflows/`、依赖清单或 lockfile 的补丁被策略拒绝并上报"：`repo` + 真实 `policy` v0 的集成用例，断言 `patch.rejected` 含 `policy.path_protected`；上报 InboxItem 由 `engine` 负责（worker-protocol §8.5），在 `engine` 侧验收。
- [ ] M1 "服务端在……崩溃，重启后不会重复创建 PR" 的 `repo` 部分：提交生成对重放幂等（§12.E）。

## 13. 开放问题

| # | 问题 | 影响 | 本文暂用 |
| --- | --- | --- | --- |
| OQ-R1 | 工作树是否需要物化出检出目录？骨架有 `Worktree.path`（api.md 示例是服务端路径，"仅作展示"），但补丁在一次性检出上应用、人看代码用 `git fetch`（P-01），似乎不需要常驻检出 | 磁盘、`remove_worktree` 语义、`path` 的含义 | 分支头只存在于 `refs/gantry/wt/<id>`；`path` 为可选的惰性物化检出，丢失可重建 |
| OQ-R2 | 提交 OID 记录在哪个事件。`forge` 的 `push_branch` 需要 `local_head_oid`，但 data-model 的 `patch.validated` 只含 `result_tree`，`attempt.finished` 无此字段；而提交发生在校验之后（要等提交说明） | 事件 `data` 契约（需单独契约 PR） | `engine` 把 `CommitRecord.commit_oid` 放进 `attempt.finished`（或新增事件）；`repo` 不依赖它，靠 `refs/gantry/attempt/<id>` 幂等 |
| OQ-R3 | 提交作者 / 提交者身份与说明来源。forge-actions §8 给了 bot 名与 noreply 邮箱格式（"需核实"），而 `app_slug` 是 `forge` 内部状态；§2.4 的表格还写着"提交由 `engine` 在补丁校验后创建"，与 decisions P-25 / architecture §4（`repo` 生成提交）不一致；说明来源是 `classify.commit_message` 还是 `build` 建议（worker-protocol OQ-10） | 谁调用 `commit_patch`、身份从哪来 | 调用方（`engine`）传入 `CommitIdentity` 与 `CommitMessage`；`repo` 负责创建提交对象 |
| OQ-R4 | 工作区的历史深度、子模块 / LFS 策略（worker-protocol OQ-13）；大仓库每次 Attempt 都复制全量对象的成本 | `prepare_workspace` 耗时与磁盘；review 需要的历史 | 不浅克隆，复制所需对象，不初始化子模块 |
| OQ-R5 | 补丁闸门的几处解释需契约确认：① 契约写 `git apply --index`（物理检出），是否允许用 `--cached` + 临时 index 以缩小攻击面、省去检出；② P5 对"删除一个符号链接"的处置（本文：任一侧为 `120000` / `160000` 即拒，失败闭合）；③ 预解析（`git apply --numstat -z --summary`）是否稳定列出恶意路径（**需核实**）；④ decisions P-41 的 `binary_allowlist` 与 worker-protocol OQ-4（"一律拒"）、§5.3（不生成 `--binary`，二进制变更本无可应用内容）、config.md（无 `binary_allowlist` 键）互相不一致 | 闸门的判定与失败分类 | 严格按契约：物理检出 + `--index`；二进制、符号链接、子模块一律 Deny |
| OQ-R6 | `policy` 侧的剩余缺口（方法名与 `PatchFacts` 已与 policy.md §6.2 对齐，`policy.denied` 由 `policy` 在 `evaluate_patch` 内追加，`patch.*` 由 `repo` 写）：① `repo` 在**应用之前**拒绝的补丁（路径逃逸、`.git` 组件、过大）没有 `PatchFacts` 可交，`policy.denied` 无人追加——建议 `policy` 增加 `record_structural_denial(project, attempt, reasons)`，或允许用不完整的事实调用 `evaluate_patch`；② config §1.3 没有把 `repo` 列为 `policies.patch.*` 的消费者（本文经 `patch_limits()` 取）；③ M0 替身清单没有 policy 替身（用 `ScriptedPolicy`）；④ M1 受保护路径的 InboxItem 上报由谁发（policy.md 的 `ReportHint` 交给 `engine`） | 模块边界、审计完整性 | 应用前的拒绝只写 `patch.rejected`；上报由 `engine` |
| OQ-R7 | `engine` / `guardian` 持锁的租约时长、续租节奏、`guardian` 在 `engine` 持锁时的退避（骨架只给了人工锁的数值） | 锁争用、长尝试期间的续租 | 内部常量，默认 900 s，持锁方在尝试期间续租 [建议值，待调优] |
| OQ-R8 | 密钥扫描器已由 `policy.scan_text` 暴露（policy.md §6.2 / OQ-P3，`forge` 也复用）；但 `@mention` / 跨仓库引用 / 关闭关键字 / 命令串的**净化规则**在 `forge`（`outbox/sanitize`）与本模块（提交说明）各写一份，无共享实现 | 重复实现与规则漂移 | `repo` 内联同一规则表（forge-actions §2.4），待共享实现落地后替换；一致性测试对同一语料断言两处输出相同 |
| OQ-R9 | CAS 归属：architecture §6 有、§4 没有；`store.md` 提议归 `store`（其 OQ-3）。仍待定：被拒补丁（`CasKind::Quarantine`）/ `held` 补丁 / 校验记录的保留期（worker-protocol OQ-12），以及 `store.cas.max_object_bytes`（默认 64 MiB）与补丁上限的关系 | `store` 与 `repo` 的边界、存储增长 | 使用 `store` 的 `Cas`；不删除任何对象 |
| OQ-R10 | 人工或外部推送后，工作树分支头如何与 GitHub 对齐（`head_diverged`、`forge` fetch 之后）——本文提供 `advance_head`（仅快进）供 `engine` / `guardian` 调用，是否接受 | P10 在人工参与后的行为 | M2 提供 `advance_head`，由 `engine` 决定何时调用 |
| OQ-R11 | git 最低版本，以及 `core.protectNTFS` / `core.protectHFS`、`apply` 对路径的校验、`uploadpack.allowAnySHA1InWant` 在本地 fetch 上的行为 | P4 / P5 的实现细节 | 实现前用目标 Ubuntu 的 git 做一次核实性验证（spike） |
