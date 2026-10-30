# Gantry 交接（handoff）——2026-10-30（换机交接版；下一步见 §4.S 的 S0–S6）

本文件写给**接手的协调会话与编码 agent / 模型**，随仓库提交在 `docs/handoff.md`。2026-10-30 起项目换机器、重新 clone：旧机器上的本地状态（未追踪文件、Git 钩子、Claude 记忆）都不会带过来，需要的内容已全部写进本文件。**只读本文件就能开工**；规格细节以仓库文档为准。

需要规格细节或规约原文时，按顺序查：

1. 仓库根目录 `AGENTS.md`：项目现状、目录入口、开发规约（权威）。
2. 本文件：基线、协作约定、任务队列、每项任务的切入点与边界。
3. 领到的 issue 正文和评论（`gh issue view <N> --comments`），以及对应的 `docs/modules/<模块>.md`。

### 交接时的状态（2026-10-30）

- **所有未做的 issue 都没有开工**：§4.S 的 S0–S6 状态都是"未开始"，没有进行中的分支、工作树或半成品。
- 旧机器上的未提交改动已全部入库：#375 的 Azure CI 配置由 PR #399 合入 `main@3a2fc9e`；本文件由随后的 docs PR 入库。
- 远端只剩 4 个有意保留的未合并分支（见 §0），其余已合并分支全部删除。

2026-10-30 新机器接手：工具链和 `gh` 登录已核对，schema 冒烟检查通过；S0 已建 #401 并建立独立工作树，诊断与复现进行中。上面三条是交接时的历史快照，当前状态以 §0 和 §4.S 为准。

### 新机器准备（第一次 clone 后做一次）

```bash
git clone https://github.com/LeXwDeX/gantry.git
cd gantry
gh auth login                      # 需要能开 PR、合并、评论 issue
rustup toolchain install stable 1.85
rustup component add rustfmt clippy
node --version                     # 需要 22 以上
cargo --locked xtask schemas --check   # 冒烟：工具链可用
```

建议把本地工具目录加进 `.git/info/exclude`，避免误提交（旧机器的做法）：

```
/.opencode/
/.agents/
/.claude/
/.codex/
/.codegraph/
/.memory/
/.serena/
/.mcp.json
/.specgit.yaml
/START-*.md
.DS_Store
```

**SpecGit（可选）**：旧机器装了 SpecGit 的 Git guard（`pre-commit` / `pre-push` 检查 checkpoint 与 `Issue:` trailer）。Git 钩子不随 clone 带过来，新机器默认**没有**这道检查。想恢复时：安装 `specgit` CLI（https://github.com/LeXwDeX/SpecGit），用附录 A 的声明运行 `specgit init --config-file <附录 A 存成的文件>`，再 `specgit setup --hook-purpose guard`（先加 `--dry-run` 预览）。不装 SpecGit 时跳过下文所有 `specgit issue` 步骤，但提交信息末行仍写 `Issue: #<N>`。注意：旧机器上 `.specgit.yaml` 是未追踪文件，所以 `git worktree` 新建的工作树里 `specgit issue` 会报"未初始化声明"，提交本身不受影响。

### 最短开工路径

1. 看 §0 确认基线，看「协作约定」确认权限，看 §4.S 确定领哪一步（单会话就从 **S0** 开始；多会话按 §4「推荐并行分配」）。
2. 若该步需要新 issue（S0、S1），先用 §4.S 该步里的 `gh issue create` 命令开 issue，记下编号 `<N>`。
3. 建独立工作树，主检出始终停在干净的 `main`：

   ```bash
   cd <仓库根>
   git fetch --prune origin
   git worktree add -b <分支名> ../gantry-wt/<分支名> origin/main
   cd ../gantry-wt/<分支名>
   # 装了 SpecGit 时：specgit issue <N> --dry-run && specgit issue <N>
   ```

4. 按该步的「步骤」「完成标准」做；提交信息用 `feat:` / `fix:` / `test:` / `docs:` / `contract:` / `chore:` 前缀，末行 `Issue: #<N>`。
5. 合并前跑完 §3 本机门槛，结果写进 PR 描述；PR 描述末尾加 `🤖 Generated with [Claude Code](https://claude.com/claude-code)`。全绿后按 §2 第 6 条自行合并并清理：

   ```bash
   gh pr merge <PR> --merge --delete-branch
   cd <仓库根>
   git worktree remove ../gantry-wt/<分支名> && git branch -D <分支名>
   git pull --ff-only
   ```

6. 回写本文件 §4.S 该步的「状态」与 §0（本文件已入库，回写走 `docs:` PR，可以和该步的收尾一起提），在相关 issue 评论结果。

遇到规格不清、要改契约、要做规格外决定：按 §2「必须停下来」处理；`needs-decision` 由协调会话按下面「协作约定」拍板。

### 协作约定（旧机器 Claude 记忆的落地版，权威）

| 约定 | 内容 | 来源 |
| --- | --- | --- |
| 全自动授权 | 用户已预先批准所有 PR 与合并，不用再问。`needs-decision` 事项和 `decisions.md` 待决项由**协调会话**按文档推荐方案代为拍板，选可逆、最小的方案，并在 issue 里写明"协调会话依用户 2026-10-27 授权代为拍板" | 用户 2026-10-27 |
| 分工 | 协调会话负责规划、拍板、评审、验收，原则上不亲自写实现代码：改动先开成 issue，再交给编码会话 / 子会话做。编码会话按本机门槛自测后自行合并 | 用户 2026-10-27 |
| 合并门槛 | GitHub CI 不可用期间，以"本机 TDD + 本机复现全部 CI 任务通过"为合并依据（§3 的命令）；在**合并后的结果**上跑，多个 PR 按合并顺序模拟。本机确实跑不了的项要明说，不能当作通过。远端 CI 失败不阻塞合并，也不用去修 | 用户 2026-10-27 |
| 里程碑循环 | 每个里程碑完成后必须做四项收尾：框架结构评审、代码评审（发现问题开修正 PR，记录进 `docs/reviews/`）、合并到 `main` 并清理分支与工作树、更新 AGENTS.md（跑一次 init）。收尾后再开下一个里程碑和 issue | 用户 2026-10-26 |
| 交付形态 | 每轮结束交付：干净的 `main`（已合并分支与工作树清掉）、清晰的 issue 计划（挂里程碑、写明依赖）、更新过的本文件。**换机后本文件入库**，不再用未追踪的 `handoff.md` | 用户 2026-10-27、2026-10-30 |
| 扩展性方向 | agent 的 skill、MCP、hooks、上下文管理都要预留扩展空间，但扩展只来自服务端可信配置，不来自仓库；照搬 `ConfigView.injection`"先定义类型、M1 拒绝非空"；先调研前身项目 GraphAgent（AGPL，只借鉴设计）与 SpecGit 的边界设计，再写 ADR、再改契约。对应 M2-prep #108 → #109 → #110 | 用户 2026-10-26 |

## 0. 一页速览

| 项 | 现状 |
| --- | --- |
| 基线 | `main@26107d5`（PR #402 的 merge commit）；此前 PR #399 的 Azure CI 配置与 docs PR 已合入 |
| 本地分支 / 工作树 | 主检出为干净的 `main`；S0 的诊断和测试客户端修改在 `../gantry-wt/test/401-process-flaky`。PR #402 的分支和工作树已清理；当前状态以 Git 和 §4.S 为准 |
| 远端分支 | PR #402 的分支已删除；保留 4 个诊断 / 备份 / 半成品分支：`backup/362-stash-pre-364`、`diag/346-linux-linker`、`diag/352-windows-events`、`wip/280-project-projection` |
| 里程碑 | M1.1–M1.3 已关闭；**当前是 M1.4**（开放：#105、#106、#107、#280、#401）；M0.9 只剩 #86 |
| 上一轮成果（已评审） | #369（docs #389 / 实现 #390）、#371（contract #391 / 实现 #392）、#267（#393 / #394 / #395）、#105 C2-a（#396）、#105 C2-c policy / notify（#397）与 pki / secrets（#398）。结论见 §4.0 与 §4 A |
| **#105 C2-c 已拍板** | **选 B**：`assemble_real` 继续 `NotReady`；在 `gantryd/src/` 写各模块的生产构造函数，替身装配逐片改为调用它们来替换替身。详见 §4 A 与 #105 评论 |
| **下一步（按 §4「S」编号执行）** | **S0 #401 进行中**：先补诊断，再复现进程测试偶发失败（36/37）→ **S1** 把生产主密钥来源与 `pki.ca_dir` 映射提为 `needs-decision` → **S2** #305 补齐 `ForgeReader`（forge 片的前置）→ **S3** #105 forge / repo 片。**S4** #280 契约片可与 S0–S3 并行。详见 §4.S |
| 2026-10-30 协调核对 | 已核实：#397 / #398 已合入，#105 开放。**新发现**：policy 调用 `ForgeReader::pull_request`（`gantry-policy/src/engine.rs:167`、`service.rs:360`）与 `project_snapshot`（`engine.rs:862`），而真实 `gantry_forge::Reader` 这两个方法仍返回 `not_yet`（`gantry-forge/src/reader.rs:95-112`）——直接换真实 forge 会让审批失败，所以 S2 必须先于 S3 |
| 远端 CI | GitHub Actions 不可用（#86）。Azure Pipelines 配置已由 PR #399 入库（`.azure-pipelines/`，`.github/workflows` 已删），PR #402 上已出现 Azure 实际检查：MSRV 检查失败，其余检查未确认。#375 保持开放，完成标准是"迁移 PR 上 Azure 全绿、合并后 main 自动触发"。**合并依据仍是本机门槛** |
| 生产装配 | `gantryd::assemble_real` 已打开 SQLite store、注册 policy / pki 私有表并构造 notify，仍返回 `NotReady`（有意的失败闭合）；policy 构造尚待 forge 调用，pki / secrets 构造尚未在生产路径调用。所有"通过"都是替身与模块测试证据 |

## 1. 基线与环境

- 远端 `github.com/LeXwDeX/gantry`，主分支 `main`。下文 `<仓库根>` 指新机器上的 clone 目录。
- 工具链：Rust stable，外加 MSRV `1.85`（`cargo +1.85` 要能用）；Node 22 以上；`gh` 已登录。
- 两个容易踩的坑：
  - 在 macOS 上跑测试时**不要把 `TMPDIR` 设到 `/tmp/...`**。macOS 的 `/tmp` 会清掉 setgid 位，让 `gantry-worker` 的 setid 测试失败。用默认 TMPDIR。
  - `gantry-store` 的集成测试依赖 `testing` feature，必须带 `--all-features`，否则几乎全部被跳过。
- `git fetch` 请带 `--prune`，否则会看到已删除的远端分支残留。
- 新机器若是 Linux（Ubuntu），systemd / nft 相关的 deploy 实机检查不再跳过，Windows 交叉检查仍需 Windows SDK；结果如实写进 PR。

## 2. 工作流与合并规则

1. 从最新 `main` 开分支，一个分支只做一个 issue 的一个可独立验证的切片。分支名建议 `fix/<N>-<短名>`、`feat/<N>-<短名>`、`contract/<N>-<短名>`、`test/<N>-<短名>`。
2. 装了 SpecGit 时，在该分支运行 `specgit issue <N>`；没装就跳过（见「新机器准备」）。
3. 先读规格，再写测试，再实现。测试按模块规格「测试与验收」逐条写；**不要把错误行为写成测试预期**。
4. 跑完第 3 节的本机门槛（在 rebase 到最新 `main` 之后的结果上跑）。
5. 开 PR：标题用 `feat:` / `fix:` / `docs:` / `contract:` / `test:` 前缀，正文写 `Closes #N`（最后一个切片）或 `Refs #N`（中间切片），并把门槛命令和结果贴进 PR 描述。远端检查以 Azure 为准；失败先核对具体检查项，不归因计费。
6. **合并授权**：用户在 2026-10-27 已预先批准所有 PR 与合并。本机门槛全绿后可以自行合并：`gh pr merge --merge --delete-branch`，然后删除本地分支、移除工作树，回到干净的 `main`。
7. 每完成一个 issue，更新本文件的「0. 一页速览」和「4. 任务队列」。
8. 跨"写端 / 读端"的文件格式（状态文件、提交页、套接字），测试要用写端的真实产物驱动读端（#378 的教训）。
9. **替身要和契约一样宽**：替身实现契约时，不要比契约更窄或更宽（#392 的教训，见 §4.0）。

必须停下来、不能自行决定的情况：

- 规格写得不够、互相矛盾，或者要做规格外的决定：在 issue 里评论提问，加 `needs-decision` 标签，然后换下一个任务。
- 要改 `docs/interfaces/`、`schemas/`、`gantry-core` 的事件或端口类型（`core::ports`）：**先单独开 `contract:` PR**，不要在实现 PR 里顺手改。改 core 类型后运行 `cargo --locked xtask schemas` 重新生成 `schemas/`，生成文件不要手改。
- #106、#107 需要用户先确认环境或真实资源，**不要开工**。
- #86 是 GitHub Actions 的计费问题，只能由用户在 GitHub 的计费设置里处理，不要尝试；PR 的远端检查走 Azure。

语言：文档、注释、PR 描述、issue 评论一律用中文；英文只用于代码标识符、协议字段、命令和提交前缀。

## 3. 本机门槛（合并前全部通过）

在仓库根目录运行：

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo +1.85 check --locked --workspace --all-features
cargo +1.85 check --locked --workspace --all-targets --all-features
cargo --locked xtask schemas --check
npm ci --prefix tools/mock-api && npm test --prefix tools/mock-api
npm ci --prefix apps/desktop && npm run lint --prefix apps/desktop && npm run typecheck --prefix apps/desktop && npm test --prefix apps/desktop
npm run build --prefix apps/desktop
```

PR #398 合入前的本机结果：全工作区 Rust fmt / clippy / test、MSRV 1.85 两项、schema 检查通过；mock-api 43 项、desktop 406 项与构建、deploy 脚本测试通过。macOS 跳过 systemd / nft 实机检查。改 store 时再用 `GANTRY_STORE_BACKEND=sqlite` 跑一遍 store 测试。

单测示例：`cargo test --locked -p gantry-engine --test engine_level lowering`；前端单文件：`npm test --prefix apps/desktop -- <文件名片段>`。

改到 `crates/gantryd/tests/support/` 的恢复逻辑时，再跑一次独立联测（不在根 workspace 内）：

```bash
cargo test --locked --offline --manifest-path crates/gantryd/tests/support/registry-recovery/Cargo.toml --lib
```

PR #398 补齐了独立锁文件中 `gantry-store` 的 `tokio` 依赖；原命令在合入前为 8 passed。

## 4. 任务队列（按推荐顺序）

### 4.S 下一步全量清单（2026-10-30 协调会话重写，**以本节为准**）

总顺序：**S0 → S1 → S2 → S3a → S3b → S5…**；S4 与 S0–S3 并行；S0 与 S2 碰的文件不重叠，也可以并行。每一步一个分支、一个 PR，从最新 `origin/main` 新建独立工作树，主检出保持干净的 `main`。每步完成后回写本节对应条目的「状态」一行，并更新 §0。

#### S0 进程测试偶发失败：复现、补诊断、定位（先做）

- **为什么先做**：#398 的门槛里出现过一次 gantryd 进程测试 36/37，完整输出丢失，失败用例与原因都不知道；#397 也报过 SIGTERM 进程测试偶发失败。S3 会往进程测试里加真实 forge / repo 路径，偶发失败不先查清，后面无法区分"新回归"和"老毛病"，"37 项通过"也不能完全当合并依据。
- **issue**：新开，挂 M1.4。若定位到生命周期问题且属于 #314 的范围，在 #314 评论关联。

  ```bash
  gh issue create --repo LeXwDeX/gantry --milestone "M1.4 M1 验收（B / C 级）" \
    --title "test(gantryd): 定位进程测试偶发失败" \
    --body "PR #398 合入前的门槛中，gantryd 进程测试曾报告 36/37，完整输出丢失，失败用例与原因未知；PR #397 也记录过 SIGTERM 进程测试偶发失败（套接字绑定早于信号注册，失败断言未输出退出状态）。

  范围：先补失败诊断，再在默认并行、单线程、带负载三种条件下循环复现并定位。
  完成标准：复现并修复后循环 30+30 次全绿；或未复现时只合入诊断改进并记录轮次，issue 保持开放。

  Refs #105"
  ```
- **分支**：`test/<新 issue 号>-process-flaky`；只改 `crates/gantryd/tests/`（必要时 `crates/gantryd/src/` 的根因修复，另见下）。
- **步骤**：
  1. **先补诊断，再复现**。现有断言失败时不打印子进程退出状态与 stderr（#397 已指出），先把 `crates/gantryd/tests/process.rs` 里等待子进程 / 等套接字（`wait_for_socket`，约 288 行）/ 停机断言的失败信息补全：退出码或信号、子进程 stderr 尾部、套接字路径状态、耗时。只改诊断，不改断言语义。
  2. 循环复现，每轮输出各自落盘（写到工作树外的临时目录，**不要把 `TMPDIR` 设到 `/tmp/...`**）：
     - 默认并行：`for i in $(seq 1 30); do cargo test --locked -p gantryd --all-features --test process > <dir>/par-$i.log 2>&1 || echo "FAIL $i"; done`
     - 单线程：同上加 `-- --test-threads=1`，用来区分"并发争用"和"单测自身时序"。
     - 带负载：失败最初出现在全工作区门槛期间，另开一个终端同时跑 `cargo test --locked --workspace --all-features`，再循环 10 次。
  3. 重点怀疑对象（按可疑度）：`sigterm_shuts_down_and_removes_the_socket`（约 435 行，套接字绑定早于信号注册，存在时序窗口，见 PR #397）、`concurrent_starts_leave_exactly_one_owner`（约 463 行）、`stale_socket_is_replaced_but_a_live_one_is_not`（约 443 行）、`sqlite_store_survives_process_crash_and_restart`（约 1528 行）、`real_executor_*`（回环 `FakeGitHub`）。
- **完成标准（三选一，如实写进 PR 与 issue）**：
  - 复现并定位：给出失败用例名、失败日志和根因；修复放在同一 PR（测试侧时序问题）或单独 `fix:` PR（`gantryd` 生产代码，例如先注册信号再绑定套接字，须对照 gantryd.md §7.2）。修复后再循环 30 + 30 次全绿。
  - 未复现：只合入"补诊断"PR，issue 保持开放并写明跑了多少轮、哪些条件；**不得写成"已修复"或"已排除"**。
  - 发现是环境问题（如 macOS 临时目录、端口耗尽）：写明证据，改测试的环境依赖，不改生产代码。
- **状态**：修复与复测已完成，诊断切片待终审合入；[#401](https://github.com/LeXwDeX/gantry/issues/401) 保持开放直到合入。生产信号顺序修复由 [PR #402](https://github.com/LeXwDeX/gantry/pull/402) 合入 `main@26107d5`；诊断与测试客户端修复在 `test/401-process-flaky@175c64b`，已基于该结果复测。
- **初次复现**：默认并行 30/30、单线程 30/30 通过；负载组 8/10。`load-3.log` 的 SIGTERM 以信号 15 退出并留下套接字，对应信号注册晚于绑定；`load-6.log` 的读取超时设置返回 EINVAL，本机实验确认 macOS 对端关闭后设置套接字选项会失败。后者通过连接前设置原有 10 秒超时修复，不忽略错误，不放宽鉴权断言。#398 未保留日志的 36/37 仍不能直接归因于这两次。
- **修复后证据**：默认并行 30/30、单线程 30/30、带负载 10/10 全通过，每轮 43 项。负载组每轮都有与独立工作区编译实际重叠的进程快照，不代表整轮持续重叠。§3 全部门槛通过：Rust 工作区 1,918 项通过、8 项忽略；Mock API 43 项、桌面端 406 项及构建通过。证据针对提交 `175c64b`，之后仅更新交接文本。
- **证据位置与边界**：本机日志目录 `/var/folders/jm/w5g1j5td4vl8164rqwl96fn80000gn/T/opencode/gantry-401.wyx7Mv/`，修复后使用 `after-*` 前缀，汇总见 `after-final-round-audit.log`。没有覆盖 Ubuntu 实机与真实生产装配；概率性信号测试不能保证每次命中旧窗口。

#### S1 生产凭据配置待决项：只提问，不实现

- **内容**：把 #398 留下的两组生产配置缺口提成一个 `needs-decision` issue，挂 M1.4：
  1. 主密钥来源及凭据名称（secrets.md OQ-2、deploy.md OPS-08）：`build_secrets` 已只接受调用方注入的主密钥，`assemble_real` 还没有任何来源可以调用它。
  2. `pki.ca_dir` 在 `BootConfig` 里没有字段映射（gantryd.md §7.1）。
  3. 顺带列出：`Pki::tick` 尚未由 `serve` 调度；生产打开顺序要求 `harden_process` → `build_secrets` → `build_pki`。
- **开 issue**（正文先读 secrets.md、deploy.md 中 OQ-2 / OPS-08 的原文，把出处、可选方案和影响补进 `<…>` 处）：

  ```bash
  gh issue create --repo LeXwDeX/gantry --milestone "M1.4 M1 验收（B / C 级）" --label needs-decision \
    --title "decision(gantryd): 生产主密钥来源与 pki.ca_dir 引导配置映射" \
    --body "背景：#398 已提供 build_secrets / build_pki 生产构造入口，但 assemble_real 无法调用。

  1. 主密钥来源及凭据名称（secrets.md OQ-2、deploy.md OPS-08）：<规格原文摘要；可选方案 A/B/…及影响>
  2. pki.ca_dir 在 BootConfig（crates/gantryd/src/config.rs）没有字段映射：<可选方案及影响>
  3. 一并确认：Pki::tick 由 serve 调度的方式；生产打开顺序 harden_process → build_secrets → build_pki。

  不阻塞 #305 与 #105 forge / repo 片（测试装配用测试主密钥）。

  Refs #105"
  ```

- **写法**：每项列出规格原文出处、现状代码位置（`crates/gantryd/src/assembly.rs` 的 `build_secrets` / `build_pki`、`crates/gantryd/src/config.rs`）、可选方案和各自影响；**不要自己选**。开完 issue 即完成 S1，不开分支、不写代码。拍板由协调会话做，拍板后若涉及 `docs/interfaces/` 或引导配置契约，先开 `contract:` PR。
- **不阻塞**：S2、S3 只动测试装配，用测试主密钥与 `StaticTokenProvider`，不依赖 S1。S1 只阻塞"`assemble_real` 打开 secrets / pki / forge 真实凭据"，这一步本来就排在 #106 之后。
- **状态**：未开始。

#### S2 #305：补齐真实 `ForgeReader`（S3 的前置）

- **为什么是前置**：`build_policy` 接收 `Arc<dyn ForgeReader>`，policy 实际调用 `pull_request`（`crates/gantry-policy/src/engine.rs:167`、`service.rs:360`）和 `project_snapshot`（`engine.rs:862`）；真实 `gantry_forge::Reader`（由 `Forge::reader(ReaderDeps)` 构造，`outbox/mod.rs:62`）这两个方法和 `issue` 都返回 `not_yet`（`reader.rs:95-112`）。不补齐就把 `FakeForge` 换成真实 forge，签名审批会失败。
- **范围**：只改 `crates/gantry-forge/`。按 forge.md §6、§7.10 实现 `pull_request`、`issue`、`project_snapshot`（按项目绑定读取已有快照）和 `sync_mirror`（调用执行器里已有的受保护镜像拉取，`outbox/executor.rs` 约 218 行，并传播错误）。数据来自 store 投影还是 GitHub API，以 forge.md 为准，**不要猜**；规格不清就按 §2 停下来提问。
- `ci_log`、`changed_files`、`review_thread`、`check_annotations`、`reconcile_now` 不在 #305 验收范围内：先查 forge.md 和调用方，M1 路径用不到的保持 `not_yet`，在 PR 里列明去向，不要顺手扩大范围。
- **测试**：用真实 forge + `FakeGitHub` / 本机临时仓库，**经 `ForgeReader` 端口测试**，不调内部方法；覆盖：存在、不存在（`Ok(None)`）、未绑定项目、镜像拉取失败、401 重试一次（已有 `with_token` 路径）。
- **分支 / PR**：`fix/305-forge-reader`，`Closes #305`，`Refs #105`。
- **状态**：未开始。

#### S3 #105 C2-c 第三片：forge / repo（拆成 S3a、S3b 两个 PR）

前提：S2 已合入。每个 PR 都 `Refs #105`。总规则沿用 §4 A 的「拍板 B」：`assemble_real` 继续 `NotReady`，正常依赖树不得含 `gantry-fakes`。

**S3a forge**（分支 `feat/105-forge`）

1. `crates/gantryd/Cargo.toml`：把 `gantry-forge` 从 dev-dependencies 挪进 `[dependencies]`。
2. `crates/gantryd/src/assembly.rs` 新增 `build_forge(...)`，入参只来自引导配置、settings、已打开的 store 与调用方注入的 `TokenProvider` / 时钟；内部用 `Forge::new(store, ForgeConfig)`，再经 `forge.reader(ReaderDeps)`、`forge.executor(ExecutorDeps)`、`forge.dispatcher(...)` 构造，返回一个包含 reader（`Arc<dyn ForgeReader>`）、executor、dispatcher 的结构体（命名参照 `PolicyStack`）。
3. `module_specs()` 永久登记 `gantry_forge::module_spec()`；目前只在测试分支里追加（`tests/support/mod.rs` 约 346 行），改为生产登记，并保留"空列表打开 → 完整列表重开"的用例。
4. 测试装配（`crates/gantryd/tests/support/mod.rs`）：删除 `FakeForge` 与 `ForgeSnapshots`，改为调用 `build_forge`，令牌用 `StaticTokenProvider`，GitHub 用回环 `FakeGitHub`。`build_policy` 改收真实 reader；`fake.drive` 不再扮演 forge 执行器（真实执行器认领 Outbox），只保留扮演 GitHub 和审批设备的部分，并同步改 gantryd.md §7.3。已有的 `real_executor` 句柄合进主路径，不再是可选分支。
5. `status`：forge 报 `real`，说明写"real forge against loopback FakeGitHub"；改 gantryd.md §7.1 表。
6. 接线时逐项核对 #298（写串行、预算耗尽排除）、#303（`head_epoch` 从事件日志恢复）、#308（未定义审批形状失败闭合）、#312（出站归因疑点）：本片暴露出来的一起修，或在 PR 里写明留给哪个 issue。#301（真实 HTTPS 传输）**不要**为了回环测试放宽生产传输限制。
7. `assemble_real`：可以构造 `ForgeConfig` 等不需要凭据的部分，但**不调用**需要 GitHub App 私钥的 `AppTokenProvider`（依赖 S1）；仍返回 `NotReady`。

**S3b repo**（分支 `feat/105-repo`，S3a 合入后开）

1. `gantry-repo` 挪进 `[dependencies]`；`assembly.rs` 新增 `build_repo(RepoConfig, RepoDeps) -> GitRepo`（`RepoDeps` 要的 store、clock、ids、policy、cas 都来自已有构造函数，**时钟必须与 store 共用同一个**）。
2. 测试装配删除 `EventedRepo` / `FakeRepo`，改用本机临时 Git 仓库上的 `GitRepo`；临时仓库与镜像的搭建复用 `crates/gantryd/tests/real_repo.rs` 已有的做法，不另写一套。
3. `sync_mirror`（S2）在这里走通真实镜像拉取。核对 #322（租约截止可序列化）、#312 的仓库恢复疑点。
4. 改到 `tests/support/` 的恢复逻辑时，加跑独立联测（§3 的 `registry-recovery` 命令）。

**S3 每个 PR 的验收**

- engine.md §12.2 端到端：`end_to_end_low_delivery_reaches_closed_with_fakes`、`exhausted_reservation_does_not_dispatch` 及其正向对照通过。
- 签名审批全组（`real_policy_*`、`real_executor_*`、`revoked_*`、`unregistered_device_*`、`challenge_then_submission_*`）和 `sqlite_store_survives_process_crash_and_restart` 通过。
- `cargo tree -p gantryd -e normal --all-features` 不含 `gantry-fakes`。
- 进程测试**连续跑 5 次**全绿（S0 之后的新规矩），PR 里写清轮次。
- §3 全部门槛通过；gantryd.md §7.1 / §7.3 表格和 `NEXT-STEPS.md` 的替换清单同步。
- **状态**：未开始（等待 S2）。

#### S4 #280 契约片（并行会话）

照 §4 B 执行：先 `contract:` PR（`PageQuery` 加方向、`list_conversations` 返回 `PageWithSeq`，同步 `docs/interfaces/`、`schemas/openapi.yaml`、core 类型、`cargo --locked xtask schemas`），再开实现 PR。改 `gantry-core` 合入后，S2 / S3 的会话 rebase 再继续。**状态**：未开始。

#### S5 #105 后续片（S3 之后按序）

1. models：回环桩；`ModelTable` 由 models 设置推导，`tests/support/engine_cfg.rs` 的手写表退场；测试装配已用 `gantry_engine::runtime_settings::from_settings`，补生产构造函数。同时核对 #317、#318、#320、#321（#289 留下的三个 engine 入口在这里获得真实调用方）。
2. sandbox / worker / egress：本机只能做到构造与替身端口为止，真实运行归 #106。
3. api：作为 `serve` 的服务任务，排在 #280 之后。
4. C3 控制通道命令：真实 `settings.bootstrap_set`、`github.setup.*`，要求见 §4 A 末尾。
5. 最后一片 PR 写 `Closes #105`：`docs/m1-acceptance.md` §3 表 10 条退出标准的 A 列都有对应测试并通过。

#### S6 M1.4 收尾（A 级完成后）

按 AGENTS.md「里程碑收尾」四步做：框架结构评审、代码评审（记录写进 `docs/reviews/`）、合并并清理分支 / 工作树、更新 AGENTS.md。同时决定 §4 C 里未挂里程碑的 issue 哪些进 M1 退出范围、哪些顺延 M2。#106 / #107 仍需用户确认环境后才开工；#375 只剩 Azure 侧检查跑通，需要用户操作。

### 4.0 本轮评审结论（协调会话，2026-10-29 深夜）

逐个审阅了 PR #389–#396 的 merge diff。无阻断问题。

| PR | 结论 | 发现 / 去向 |
| --- | --- | --- |
| #389（#369 docs） | 通过 | 修正了 #382 写入的端口相等句，改为只比对主机 |
| #390（#369 实现） | 通过 | payload 内 `approval_id` 比对、`api_base_url` 经 `Origin::parse`（强制 https、拒绝 userinfo / query / path）后主机忽略大小写比对、端口不比；`pairing_begin` 入口包 `Zeroizing`。正反用例齐全 |
| #391（#371 contract） | 通过 | — |
| #392（#371 实现） | **通过，替身比契约窄** | 替身把可写字符串键硬编码为 `sandbox.base_image`；契约允许的 `github.app.private_key.secret_ref` 等 `*.secret_ref` 键会被错拒为 `not_writable`；契约的「未知字段 → `bad_request`」没检查。替身不进生产，不单独修；**C3 注册真实命令时按 settings schema 的目标类型判定并补用例**（已写进 #105 评论） |
| #393（#267 事件上限） | 通过 | 上限只在 `StoreConfig` 上可设，`gantryd` 用默认 1 MiB，尚未从 `store.max_event_data_bytes` 设置读取（C2 接线时顺带） |
| #394（#267 `verify_sample`） | 通过 | 内存后端是 `BTreeMap`，排序成立。抽样熵复用 `IdGen` 的熵源：测试里用 `SeqEntropy` 时，调用 `verify_sample` 会推移后续 ID，写确定性用例时注意 |
| #395（#267 `watch_head`） | 通过，证据偏弱 | 用例的"无空洞"实际由 `scan` 证明，`watch` 本身会合并通知；与拍板的「watch_head + scan」口径一致。`tail` 的 `EventStream` 封装与 api 改用推送仍未做（api 仍轮询，归 #296 / #105） |
| #396（#105 C2-a） | 通过 | 正向对照（默认预算 30 轮内出现 `attempt.started`）与负向（预留为零不派发）分进程跑，补上了 #386 的证据缺口 |

历次教训仍有效：「写端产物驱动读端」；「正向对照」；「替身与契约同宽」。

### A. 主线：#105 C2-c 接真实模块（`Refs #105`，最后一片 `Closes #105`）

完成标准不变：`docs/m1-acceptance.md` §3 表里 10 条退出标准的 A 列都有对应测试并通过，engine.md §12.2 端到端用例通过；替换清单与结果写进 `NEXT-STEPS.md`。

**拍板（B，见 #105 最新评论）**：

- `assemble_real` 继续返回 `NotReady`，直到 #106 提供 Podman / Egress 真实环境；**不改**失败闭合语义，生产二进制**不得**依赖 `gantry-fakes`。
- 在 `crates/gantryd/src/` 新增按模块的**生产构造函数**（例如 `assembly::build_policy(...)`、`build_notify(...)`），入参只来自引导配置、settings 与已打开的 store。
- 替身装配（`crates/gantryd/tests/support/`）逐片改为**调用这些构造函数**替换对应替身；`status` 按模块如实报告 `real` / `fake`；同步改 gantryd.md §7 的「替身装配的当前构成」表。
- 每片验收：engine.md §12.2 端到端（`end_to_end_low_delivery_reaches_closed_with_fakes` 及预留为零的正负对照）仍通过；`cargo tree -p gantryd -e normal --all-features` 仍不含 `gantry-fakes`。

切片顺序（每片一个 PR）：

1. **第一片：policy / notify + 模块注册**（原 C2-b 并入）——**PR #397 已合入 `main@93dd239`**。

   交付内容：
   - `assembly.rs` 新增 `module_specs()`（第一片登记 `gantry_policy::POLICY_MODULE`，第二片又登记 pki）、`build_policy(store, forge, clock, ids) -> PolicyStack { port, approvals }`、`build_notify(store, ids)`；
   - `gantry-policy`、`gantry-notify` 从 dev-dependencies 挪进正式依赖；`gantry-fakes` 仍只在 dev-dependencies；
   - 替身装配删除 `FakePolicy` 分支与 `Snapshots`，改调 `build_policy` / `build_notify` / `module_specs`；`fake.drive` 用测试设备密钥走 `approvals.challenge` → 签名 → `grant`，`manual` 模式关掉自动授予以做签名正负对照；
   - `assemble_real` 用 `module_specs()` 打开 SQLite，并调用 `build_notify`；**未调用 `build_policy`**（需要 `ForgeReader`，留给 forge 片）；仍返回 `NotReady`。
   - 已有证据：同一差异的独立终审通过；本机 Rust 全工作区测试、clippy、fmt、MSRV 两项、`xtask schemas --check`，mock-api 43 项、desktop 406 项与构建、deploy 脚本测试通过。本轮重跑 gantryd `--all-features` 进程测试 33 项通过；normal 依赖树不含 `gantry-fakes`。systemd / nft 实机检查在 macOS 跳过。详细结果与两项基线问题见 PR #397。
   - 交付：提交 `a4963a3`、合并 `93dd239`；远端和本地分支及第一片工作树已清理；#105 已评论，保持开放。

2. **第二片：pki / secrets**——**PR #398 已合入 `main@b545553`**。

   - `build_secrets` 只打开已初始化实例，主密钥来源由调用方注入；M1 gantryd 只承载 `Forge` / `Pki`，`Notify` 自 M4，`Egress` 是独立进程与独立实例。`build_pki` 使用显式 `PkiConfig` 与 `Pki` 限定句柄，真实私有表由 `module_specs()` 登记。
   - 测试装配用真实文件密钥存储与真实 `Pki::open`；设备经公开配对服务登记，提交审批时实时读取设备和验签公钥。覆盖未登记、已吊销、challenge 后吊销、错误签名与正向对照，并核对本地和备份吊销日志。SQLite 崩溃重启复用同一设备。
   - 生产路径会在数据目录注册 pki 私有表，但尚未打开 secrets / pki；主密钥来源及凭据名称（secrets.md OQ-2 / deploy OPS-08）和 `pki.ca_dir` 的引导配置映射需在生产接线前明确，且读取密钥前须执行进程加固。`Pki::tick` 尚未由 `serve` 调度；吊销日志备份由 `revoke` 同步写入，不是周期任务。`PkiControl` 留给 C3。
   - 本机全部门槛与独立终审通过：进程测试 37 项、密钥隔离 5 项、独立恢复联测 8 项；正常依赖树不含 `gantry-fakes`。一轮早先的进程测试报告 36/37，但完整输出丢失，失败用例和原因未知；之后主力连续复跑 5 次 37/37，最终工作区测试通过，不得声称该次失败已定位。
   - Windows 交叉检查因本机缺少 Windows SDK 头文件，在 `ring` / SQLite 依赖编译阶段失败；不是 Windows 通过证据。真实 Ubuntu / Podman / GitHub / 模型 / 设备和操作系统用户隔离仍未验证。
   - 交付：提交 `958bf5f`、合并 `b545553`；远端和本地分支及第二片工作树已清理；#105 已评论，保持开放。详情见 PR #398 与 `NEXT-STEPS.md`。
3. forge、repo：`FakeGitHub` 驱动真实 forge；镜像仓用本机临时 Git 仓库。**前置是 #305（真实 `ForgeReader` 仍返回 `not_yet`）；具体步骤见 §4.S 的 S2、S3a、S3b。**
4. models：回环桩；`ModelTable` 改为由 models 设置推导（`engine_cfg.rs` 手写表退场）。测试装配已使用 `gantry_engine::runtime_settings::from_settings`，生产装配尚未接线。
5. sandbox、worker、egress：本机只能做到构造与替身端口为止，真实运行归 #106。
6. api：作为 `serve` 的服务任务；与 #280 错开（见 C）。

接线时逐项核对 #317（在途预留）、#318（热加载原子应用）、#320（M1 默认档位与 Low 模板）、#321（`kill_grace_s=0`）、#322（租约截止可序列化）；相关的一起修，或在 PR 里写明留给哪个 issue。真实 GitHub / 模型 / Podman 不可用的部分，不要宣称真实环境通过。

**C3 控制通道命令**（A 的后续片）：注册真实 `settings.bootstrap_set`、`github.setup.*`（当前生产命令组为空：`crates/gantryd/src/lib.rs` 里 `serve(..., Vec::new(), ...)`）。`bootstrap_set` 按 deploy.md §7.9 契约实现：字符串键判定来自 settings schema 的目标类型，不照抄替身的硬编码；补「`github.app.private_key.secret_ref` 可写」正例和「多余字段 → `bad_request`」负例。`status` 要暴露 `EngineRuntime::status()` 的字段时先改 gantryd.md §6.2。

### B. 并行主线：#280（api 读路由，已挂到 M1.4）

- 读路由依赖 store 的完整 `ProjectionRead`；对话列表要给 `PageQuery` 加方向，并让 `list_conversations` 返回 `PageWithSeq`，属契约变更：**先开 `contract:` PR**（`docs/interfaces/` + `schemas/openapi.yaml` + core 类型 + `cargo --locked xtask schemas`），再开实现 PR。
- 远端半成品 `origin/wip/280-project-projection` **未经验证**，只作参考，从 `main` 新开分支。
- Mock 行为用例（`tools/mock-api`）扩展到真实 api 时，同一份夹具（`fixtures/api/`）喂两边。
- 与 A 的第 6 片（api 接线）碰同一模块：#280 先做，A 的 api 片排在它之后。

### C. 已登记的修复项（可在 A、B 间穿插，按模块领取；目前都没挂里程碑）

- engine：#293、#302、#306、#309（含预留耗尽事件）、#310、#317、#318、#320、#322
- store：#300、#388（WAL 读池与 `SnapshotExpired` / `TxTimeout`，#105 之后）
- forge / repo：#298、#301、#303、#305、#308
- api / notify / conversation：#296（含 api 改用 `watch_head`）、#297、#307
- sandbox / worker / egress / models：#294、#295、#304、#321
- 契约：#319（内置 schema 注册表已合入，装配注入归 A 第一片 / sandbox 片）
- 待复核疑点（先核实、再决定修复或排除）：#311–#315
- CI 诊断：#346、#347、#352（对照分支 `origin/diag/346-linux-linker`、`origin/diag/352-windows-events`）
- 文档同步：#299

### D. 暂不开工

- **#375**（Azure Pipelines 迁移）：配置已由 PR #399 入库，PR #402 上已出现 Azure 实际检查（MSRV 失败，其余未确认）；剩下的是让迁移 PR 的检查全绿、合并后 `main` 自动触发，需要用户操作，agent 不要尝试。跑通前本机门槛仍是合并依据。
- **#106、#107**：需要用户确认环境与真实资源。#106 现在也是 #105 生产装配（`assemble_real` 放行）的前置。
- **#86**：GitHub Actions 计费问题，只能用户处理。
- **M2-prep #108 → #109 → #110**：调研 → ADR → 契约，按顺序；不阻塞 M1.4。

### 推荐并行分配

| 会话 | 任务 | 触碰范围 |
| --- | --- | --- |
| 1 | S0 → S1 → S3a → S3b → S5（#105 主线与 gantryd 测试） | `crates/gantryd/`（src 与 tests）、`docs/modules/gantryd.md` §7、`NEXT-STEPS.md` |
| 2 | S2（#305） | `crates/gantry-forge/`；合入后会话 1 才开 S3a |
| 3 | S4（#280：contract → 实现） | `docs/interfaces/`、`schemas/`、`crates/gantry-core`（契约片）、`crates/gantry-api/`、`crates/gantry-store/` 投影读、`tools/mock-api`、`fixtures/api/` |
| 4（可选） | C 里与上面都不碰的修复，如 engine #320 / #322 | 对应模块目录；forge 的 #298 / #303 / #308 留给 S2、S3a 一并核对，不要另开会话改 forge |

会话 3 的 contract PR 若改 `gantry-core`，会话 1、2 rebase 后再继续。

## 5. 已知遗留与风险

- **生产凭据配置待决**：主密钥来源 / 名称（secrets.md OQ-2、deploy OPS-08）与 `pki.ca_dir` 映射未定，归 §4.S 的 S1；未拍板前 `assemble_real` 不打开 secrets / pki / forge 凭据。
- **生产装配**：按 B 拍板，`assemble_real` 在 #106 前一直 `NotReady`；M1「本机验收通过」的 A 级证据来自替身装配里的真实模块，不能写成生产装配已跑通。
- **发布构建的 feature**：`gantryd` 的 `testing` feature 会让 CLI 接受 `GANTRY_CONTROL_UID` 覆盖期望 uid。发布构建只能用 `cargo build --release -p gantryd`（不带 `--all-features`）；#106 落地打包时核对。
- **#369 遗留**：硬件密钥与真实桌面外壳未验证。
- **#371 遗留**：真实 `settings.bootstrap_set` 命令归 C3；替身比契约窄（§4.0）。
- **#267 遗留**：`tail` 的 `EventStream` 封装未实现，api 仍轮询；`gantryd` 尚未从设置读取 `store.max_event_data_bytes`。
- **进程测试稳定性**：S0 #401 已复现信号注册顺序和 macOS 连接后设置读取超时两项问题。前者由 PR #402 合入，后者及诊断切片待合入；两项整合后 30+30+10 轮通过（§4.S）。PR #398 未保留输出的 36/37 仍无法归因；重复通过不代表排除了所有偶发失败。独立 `registry-recovery` 锁文件缺少 `tokio` 的问题已在 #398 修复。
- **Windows 整体编译未验证**：`deferred_cancel_survives_resubmission_after_unknown` 用 `#[cfg(unix)]` 门控；本机已安装 Windows Rust 目标，但缺少 Windows SDK 头文件，#398 的交叉检查未完成。
- **#362 遗留**（见 PR #366 描述）：`Phase::Failed` 恢复时失败分类被覆盖为 `infra.job_identity_unavailable`；`foreign_job_evidence` 遇到其他 sandbox 的损坏 meta 时跳过；`registry_recovery.rs` 的 `/tmp/g362-*`、`Drop::unwrap`、`.recovery.tmp` 残留没有清理路径。
- **收尾评审信息项**（`docs/reviews/2026-10-29-m13-closeout-review.md` §3.1）：nftables 的 API 来源限制只匹配 IPv4；`/var/lib/gantry` 与 `workspaces` 权限待 #106 实机处理；`CurlProbe` 继承环境代理；`control.rs` 的 accept 出错无退避（归 #314）。
- **积压 issue 无里程碑**：C 节所列 issue 都没挂里程碑。M1.4 收尾评审时决定哪些进 M1 退出范围、哪些顺延 M2。
- **真实环境全部未验证**：真实 Ubuntu 安装（#106）、真实 GitHub / 模型 / Podman / 密钥、桌面真实外壳与 API 联调。PR 和文档里不要把替身证据写成真实环境通过。

## 6. 参考入口

| 要找的 | 位置 |
| --- | --- |
| 架构与不变式 I1–I8 | `docs/architecture.md` |
| 已确定 / 待决决策 | `docs/decisions.md`（待决项不要自行拍板） |
| 实体、状态机、事件目录 | `docs/data-model.md` |
| 模块间契约 | `docs/interfaces/` |
| 模块规格 | `docs/modules/<模块>.md` |
| 验收分级 | `docs/m1-acceptance.md` |
| 当前进度与优先级 | `NEXT-STEPS.md` |
| 最近两次评审 | `docs/reviews/2026-10-27-m1-closeout-review.md`、`docs/reviews/2026-10-29-m13-closeout-review.md` |
| #105 拍板原文 | `gh issue view 105 --comments`（最后一条） |

文档冲突时的优先级：`decisions.md`（已确定）> `architecture.md` 不变式 > `interfaces/` > `modules/` > 其余。

## 7. 附录

### 附录 A：旧机器的 `.specgit.yaml`（恢复 SpecGit 时用）

```yaml
version: 2
remote: origin
language: zh
validation:
  titles: false
  labels: off
  bodies: false
init_policy:
  required_checks: []
tags: []
templates:
  issue:
    source: builtin
  pr:
    source: builtin
agent:
  native_auto_merge: false
  close_issues_after_merge: false
observation:
  poll_seconds: 15
  max_wait_seconds: 1800
  notify:
  - attention
  - completed
```

### 附录 B：没有迁移的本地内容

旧机器上 `.opencode/workflow-drafts/`（历史工作流草稿）、`.serena/`（索引缓存）、`apps/desktop/` 的构建产物与 `node_modules` 都没有入库，新机器不需要：草稿对应的工作都已合入或写进本文件，缓存与产物可以重新生成。
