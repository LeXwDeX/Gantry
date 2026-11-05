# 服务端进程（`gantryd`）

> 状态：设计 v0.1（按 #101 的实现回写）· 里程碑：M1 · 泳道：`gantryd` · crate / 目录：`crates/gantryd`

把 architecture §3 的单进程模块按 `core::ports` 装配成一个服务端进程，并提供主机 CLI `gantry` 与本机控制通道。

## 1. 职责

- 读取引导配置 `/etc/gantry/gantry.toml`（deploy.md §10），校验后启动；配置非法时在绑定任何套接字之前退出。
- 装配：为每个跨模块端口构造一个实现（真实或替身），放进 `Assembly`；模块之间只经端口相连（P-51）。`Assembly` 只放 `core::ports` 端口：`api` 作为 `serve` 的服务任务启动（与控制通道并列）；`pki`、`secrets` 的构造入口在 `assembly` 内，供 `assemble_real` 与测试共用；当前生产装配尚未打开它们（配置未定），测试装配打开后注入消费者。两者都不进 `Assembly` 字段。
- 启动并托管引擎运行时 `EngineRuntime`（engine.md §7.15）：`run` 到停机，停机时与控制通道一起退出。
- 本机控制通道 `server.control_socket`（默认 `/run/gantry/control.sock`）：鉴权、行 JSON 协议、命令分派（OPS-07 的落地方案，见 §6.2）。
- 主机 CLI `gantry`：解析参数后分派给各模块的库函数（`gantry-deploy` 的 `github setup` / `doctor` / `settings bootstrap-set`），与运行中的 `gantryd` 只经控制通道交互。

## 2. 不负责

- 任何模块的业务逻辑：引擎主循环、settings 到引擎配置的换算、默认预留策略归 `engine`；GitHub App 回调归 `forge`；引导期写入白名单归设置模块（`core::bootstrap_writable`）。gantryd 只构造、启动、转发。
- 模块配置（模型、档位、策略……）：全部在 settings（I7），不出现在引导配置里，写进来即拒绝。
- 引导配置以外的热加载：引导配置重启生效；settings 热加载由各模块订阅 `settings.updated` 自行完成（config.md §2.6）。
- 进程管理与目录权限：systemd 单元、`/run/gantry` 的创建与属主归 `deploy`（deploy.md §6.3、§7.7）。

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `core` | 全部端口 trait、ID 生成、时钟与熵源接口 | `core::ports` |
| `store` | `Store`（事件日志、CAS） | store.md §6 |
| `engine` | `EngineRuntime::new` / `run` / `engine()` / `degraded()` / `mailbox()` | engine.md §7.15 |
| `deploy` | `gantry-deploy` 的 `github_setup`、`doctor`、`bootstrap_set`、`UnixControlClient`、`CurlProbe` | deploy/README.md「与 gantry CLI 的接线」 |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 引导配置 | `/etc/gantry/gantry.toml`（`--config` 可改） | TOML，deploy.md §10，未知键拒绝 | 可信（root 写入） |
| 控制通道请求 | 本机进程，经 Unix 套接字 | §6.2 | 对端 uid 校验后可信；内容仍逐字段校验 |
| SIGTERM / SIGINT | systemd / 终端 | — | 可信 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| 控制通道应答 | 本机调用方 | §6.2 |
| 进程退出码 | systemd / 调用方 | 0 正常停机；1 启动或运行失败；2 用法错误 |
| 启动 / 停机日志 | stderr（journald） | 单行文本，不含密钥 |

## 6. 对外接口

### 6.1 进程与 CLI

```text
gantryd [--config <path>]
gantry [--config <path>] [--socket <path>] <command>
  github setup …            → gantry_deploy::github_setup::run
  doctor [区域] [--json]     → gantry_deploy::doctor::run
  settings bootstrap-set …  → gantry_deploy::bootstrap_set::run
```

- `gantryd` 没有替身相关的参数：服务端二进制只有真实装配（§7.1）。
- `gantry` 的控制通道路径按 `--socket` > 引导配置的 `server.control_socket` > 默认值取。`pair` / `device` / `pki` / `secrets` / `store` 子命令归对应模块，接入前报用法错误并写明归属。
- 两个二进制都只在 Unix 上编译；Windows 上是提示后退出的桩，使 workspace 能在 Windows CI 上编译。

### 6.2 控制通道（OPS-07）

- **传输**：Unix 流套接字，文件权限 0600。进程在整个生命周期内持有 `<socket>.lock` 上的排他 `flock`：检查、清理残留、绑定都在锁内，并发启动时恰好一个实例成功，退出时只删自己的套接字（锁文件保留）。路径存在但不是套接字时拒绝启动。
- **鉴权**：每个连接取对端 uid（`SO_PEERCRED` / `getpeereid`），生产只放行 0。不符时第一行即应答 `forbidden` 并断开，不读任何命令。客户端（`gantry` CLI，经 `UnixControlClient`）在连接后、发送任何请求前也校验对端 uid：必须等于 `gantry` 用户；解析失败或 uid 不符都拒绝连接（#368，防止请求发给 `gantry-egress` 或冒名套接字）。
- **协议**：每行一个 JSON。请求 `{"cmd": "…", "args": {…}}`（未知字段拒绝）；应答 `{"ok": true, "result": …}` 或 `{"ok": false, "error": {"code", "message"}}`。单行上限 64 KiB，超出应答 `too_large` 并断开；非 JSON 应答 `bad_request` 后连接保持；未知命令应答 `unknown_command`。
- **命令**：内置只有 `status`（`profile`、`head_seq`、各模块的提供方 real / fake / absent、`engine.degraded`）。其余命令组经 `CommandSet` 注册：

```rust
#[async_trait]
pub trait CommandSet: Send + Sync {
    /// 不认识该命令时返回 `None`，交给下一组。
    async fn handle(&self, cmd: &str, args: &Value) -> Option<Response>;
}
```

  已约定、待对应模块接入的命令：`github.setup.begin` / `github.setup.status`（`forge`，形状见 deploy/README.md）、`settings.bootstrap_set`（设置模块，参数与错误码见 deploy.md §7.9）；`PkiControl`（pki.md §7.8）。

## 7. 内部设计

### 7.1 两种装配

| 装配 | 在哪里 | 用途 |
| --- | --- | --- |
| 真实装配 `assemble_real` | `src/assembly.rs` | 服务端二进制。已接入 SQLite store 与 notify 生产构造函数；forge / policy / repo / models / conversation / egress / settings 的构造函数（`build_forge`、`build_policy`、`build_repo`、`build_models`、`build_conversation`、`egress_config_view`、`build_egress`、`settings_ctx::RuntimeSettingsContext`）已就绪，但 `assemble_real` 尚未调用；secrets / pki 的生产构造入口（`build_secrets`、`build_pki`）已就绪；生产凭据来源按 P-87 由 BootConfig 显式提供（`[secrets.master_key]` 与 `[pki] ca_dir`，缺失即拒绝启动），但字段映射与装配顺序尚未接线（#405 C3）。其余真实模块未接线时仍拒绝启动，不退回内存日志（失败闭合） |
| 替身装配 `assemble_fakes` | `tests/support/`（只用 dev-dependency） | 进程级测试。测试二进制带 `GANTRYD_FAKES_CHILD` 重新执行自身，作为"用替身装配的 gantryd"子进程 |

替身装配的当前构成（`status` 原样报告）：

| 模块 | 提供方 | 说明 |
| --- | --- | --- |
| engine | 真实 | `EngineRuntime`；配置经 `from_settings` 换算；模型表由测试 helper 用硬编码设置经 `ModelsView` 推导，不是生产接线 |
| notify | 真实 | `assembly::build_notify`（`Notify`） |
| store | 替身 / 真实 | 默认内存日志 + CAS；设置 SQLite 目录时用真实 store |
| policy | 真实 | `assembly::build_policy`（`PolicyEngine` + `PolicyApprovals`）；审批由测试设备签名 |
| secrets | 真实 | `assembly::build_secrets`：测试临时目录里的 `FileSecrets`，M1 只承载 Forge / Pki；Notify 自 M4 才加入，**不含 Egress**；测试代码显式 `init`，生产构造函数只 `open` |
| pki | 真实 | `assembly::build_pki`：测试 CA（`bootstrap::run_with` + 可替换内存盘探测）与真实 `Pki::open`；测试设备经公开 `pairing().open_session` / `redeem` 登记，私钥只留在测试进程内存；吊销经真实 `StoreAdmin::revoke` 写本地与备份吊销日志（都在 scratch 内） |
| forge | 真实 | `assembly::build_forge` 两阶段构造：同一 `Forge` 先给 policy 无 mirror 的 reader，再接 repo/policy 构造执行器与完整 reader；GitHub 用回环 `FakeGitHub`，由父测试进程持有 |
| repo | 真实 | 测试装配经 `assembly::build_repo` 构造真实 `GitRepo` + 测试裸远端；`assemble_real` 尚未调用 `build_repo` |
| sandbox | 替身 | `GoldenSandbox`（显式注入可应用到 base 的完整补丁） |
| worker | 替身 | `gantry-fakes` 对应替身 |
| models | 真实 | `assembly::build_models` 的 `ModelRouter`；设置来自测试 store 的 `settings.updated`（utility / planner / worker 与 engine 模型表同源），凭据由 `EgressStub` 持有（模型调用尚未切到真实 egress） |
| egress | 真实 | 父测试进程用 `gantry_egress::testing` 起登记表、模型代理与控制面；子进程经 `assembly::build_egress` 连控制套接字，启动时下发一次 `egress_config_view` |
| conversation | 真实 | `assembly::build_conversation`：真实 `Conversation` + `EngineApiAdapter`（共享 `EngineRuntime` 的 `Arc<Engine>`）+ 上面的 `ModelRouter` |
| api | 真实（测试装配） | `Assembly.api` 携带 `ApiCore`、TLS 配置与回环监听器；`serve` 启动 `serve_api`。`Assembly.pairing` 携带 `PairingListener`、单向 TLS 与回环监听器；`serve` 启动配对监听。测试装配使用测试 CA 与 mTLS 客户端校验；生产 `assemble_real` 尚未接线 |

某个模块换成真实实现时，生产构造函数写进 `src/assembly.rs`，测试装配与
`assemble_real` 共用同一条构造路径；替换时只改调用点与 `status` 表，不改模块代码。
模块私有表由 `assembly::module_specs` 统一登记（policy、pki 与 forge）；SQLite 数据目录先用
空列表打开、再以完整列表重开必须可用（本片用例）。

**测试里的 `real` 不等于生产已在运行。** `assemble_real` 仍返回 `NotReady`；测试装配
在同一进程/子进程里构造真实的 secrets、pki、policy 与 notify，并用它们跑完整路径。
测试装配已把 models 与 conversation 切到真实构造：models 设置写入测试 store，
`ModelRouter` 与 engine 模型表使用同一份设置；conversation 与 engine 共享同一 `Engine`。
`assemble_real` 未调用这两个构造函数，模型请求仍经 `EgressStub` 回环，因此不代表真实
模型 / Egress 验收；生产装配接线归后续切片。
生产凭据来源已由 P-87 定（`[secrets.master_key]` 与 `[pki] ca_dir` 必须显式配置），但
BootConfig 字段映射、装配顺序与 `Pki::tick` 的周期调度都还没有接到 `serve`（#405 C3）。吊销日志在 `revoke` 内同步
追加并 fsync 本地与备份目标，没有另外的周期备份调度。生产接线前必须先确定上面两组
配置，再按 `harden_process` → `build_secrets` → `build_pki` 的顺序打开。`status` 报 `real` 只表示
实例来自生产构造函数并确实被使用，不代表后台循环已在运行。

### 7.2 启动与停机

读配置 → 装配 → 注册 SIGTERM / SIGINT 接收器 → 绑定控制通道 → 并发运行 `EngineRuntime::run` 与控制通道 accept 循环 → 收到 SIGTERM / SIGINT（或引擎主循环因存储错误退出）时置停机信号 → 等引擎处理完已入队的回报后返回。在途尝试留在沙盒，下次启动由 `Tick(Recover)` 接管（engine.md §7.15）。

注册与接收分开看：注册在绑定之前完成，`tokio::signal::unix::signal` 同步安装进程级处理器并建立接收器；接收发生在绑定之后的 `select` 里，首次 `poll` 之前送达的信号先记为 pending，待运行时信号驱动分发后由接收器取得，同类信号可能合并。因此修复后套接字一旦可连接，SIGTERM / SIGINT 都只会走正常停机，不会按进程原有处置终止进程（#401）。SIGINT 与 SIGTERM 走同一路径，不使用 `ctrl_c()`——它要到首次 `poll` 才注册，覆盖不了绑定与首次轮询之间的窗口。装配和信号注册之前（进程刚启动时）送达的信号按进程原有处置，不一定是默认动作（可继承忽略）。

### 7.4 后台调度

`serve` 是后台任务的唯一调度方；模块只提供可测的单步动作（`pki.md` §6.1）。
M1 只启动 `Pki::tick`，其余任务在对应模块提供公开的单步入口后再接入。

| 任务 | 节奏 | 失败处理 |
| --- | --- | --- |
| `Pki::tick`（关闭过期配对窗口、按需续签服务端证书） | 启动后立即一次，之后每 6 小时 | 记录 `pki` 的 `health` 与 stderr；下一周期重试，不终止 `serve` |
| policy 审批过期清扫 | 事件驱动；周期清扫等 `PolicyApprovals` 提供公开单步入口后接入 | 归 #314 跟踪，不让 `serve` 退出 |
| `Notify::AutoClose` | 事件驱动（每批新事件后 `run_once`）；订阅循环归 notify 后续切片 | 记录 stderr，不终止 `serve` |

总则：

- 后台任务失败只影响对应模块的 `health` 或日志，不改变 `serve` 的退出条件；
  停机时随 `watch` 信号结束。
- `Pki::tick` 的 6 小时是 `pki.md` 的建议值；配对窗口仍按设置窗口工作，tick
  只负责关闭过期窗口。
- 本规格不提供 `Pki::spawn_background`：调度归 `gantryd`，避免模块自己持有
  任务生命周期。

### 7.3 进程级测试的驱动

替身装配额外注册 `fake.*` 命令组（只在测试里编译）：`fake.create_delivery`、`fake.events`、`fake.end_bootstrap`，以及 `fake.drive`——每次调用驱动真实 forge 执行器处理 Outbox、用真实 `Dispatcher` 回读 GitHub 写快照，并为测试分支注入成功的 CI 检查；`auto_approve` 时再以 challenge + 测试设备签名授予待决审批，`manual` 模式关掉自动授予，用于签名正负对照。FakeGitHub 与测试裸远端由父测试进程持有，子进程经 HTTP 控制端点同步分支与检查；跨 daemon 重启复用同一外部事实。`fake.device_info` 实时读取设备目录，`fake.revoke_device` 用真实 `StoreAdmin::revoke` 吊销测试设备。父进程只经控制通道反复调用并观察阶段，不触碰执行体内部。测试设备经真实配对服务登记；每次授权都从 `DeviceDirectory::get` 与 `verifying_key` 取身份、角色、状态与公钥，不缓存，吊销后下一次授权即失败。测试签名私钥只留在测试进程内存。

## 8. 不变式与安全要求

- **I1 沙盒只计算**：控制套接字 0600、仅 root，永不挂入沙盒（deploy.md §12）。
- **I7 配置分层**：引导配置 `deny_unknown_fields`，settings 键写进来即拒绝。
- **失败闭合**：真实装配缺 SQLite store 或模块接线时拒绝启动；对端 uid 不符时不读命令；引导配置非法时不绑定套接字。
- **替身不进生产**：`gantry-fakes` 与 `gantry-store/testing` 只作为 dev-dependency；`cargo tree -p gantryd -e normal --all-features` 不含它们。

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 引导配置缺失或非法 | 打印原因，退出码 1，不绑定套接字 |
| 已有实例在运行（拿不到锁） | 打印 `another gantryd is listening`，退出码 1 |
| 套接字路径被非套接字文件占用 | 拒绝启动，不删除该文件 |
| 引擎主循环因存储错误退出 | 停止控制通道，退出码 1；单个交付单元的引擎缺陷只让它进入 degraded，经 `status` 报告 |
| 被 SIGKILL | 残留套接字由下次启动在锁内替换 |

## 10. 配置

引导配置键与默认值见 deploy.md §10；gantryd 当前读取 `server.*`（含 `control_socket`）、`forge.webhook.*`、`sandbox.runtime_*`、`egress.control_socket`、`backup.*`、`upgrade.*`，并校验 https URL、绝对路径与保留期格式（`<正整数><s|m|h|d>`）。

`settings.updated` 的自动订阅与安装归 #105：gantryd 从同一快照构造 `RuntimeSettings` 并调用 `EngineRuntime::install`；失败时保留最后成功配置、通过控制通道上报，不推进事件游标。

## 11. 可观测性

- `status` 命令：装配来源、事件日志 head、各模块提供方、`engine.degraded`。
- stderr：`gantryd: profile=… control=… ready`、`gantryd: shutting down` 与启动失败原因。

## 12. 测试与验收

- [x] 引导配置：默认值、必填项、未知键与 settings 键拒绝、取值校验（单元测试）。
- [x] 控制通道：0600、uid 不符即拒、坏请求后连接保持、超长行断开、残留套接字替换、第二个实例被拒、并发启动恰好一个成功、套接字一旦可连接就尽快送达 SIGTERM / SIGINT 均正常退出并删除套接字（进程级；信号注册先于绑定，多轮提高命中旧窗口的机会，不保证必定命中）。
- [x] 服务端二进制：没有替身参数；无持久存储时拒绝启动；配置非法时不绑定（进程级）。
- [x] CLI：从引导配置取套接字路径；`bootstrap-set` 在引导期前后的行为；uid 被拒与守护进程不在时退出码 1；用法错误退出码 2 且不连守护进程；`doctor host --json`（进程级）。
- [x] `github.setup.*` 替身命令符合 deploy 接线约定，应答不含密钥（进程级）。
- [x] engine.md §12.2「端到端（替身）」主路径在独立 gantryd 进程里跑通：阶段序列恰为 `spec → implement → propose → review → merge → closed`，`spec → implement` 的 `reason = admission_skipped`，无 degraded（进程级）。
- [x] SQLite store 先以空模块列表打开、再以生产模块列表重开，policy 私有表可读写（组件测试）。
- [x] models 生产构造（`build_models`）独立接入真实内存 store 与 `EgressStub` 回环：utility / planner 调用各产生一次 svc 数据面调用并写入真实 `budget.charged`；未知 effort / 模型与未配置模型在发请求前失败闭合，且没有 `budget.charged`（组件测试）。测试装配已切换为真实 `ModelRouter`，设置与 engine 模型表同源；仍不代表真实模型 / Egress 验收。
- [x] 真实 policy / notify 经生产构造函数装配；合并审批由测试设备密钥签名，正负签名对照通过（进程级）。
- [x] 真实 secrets / pki 经生产构造函数（`build_secrets` / `build_pki`）装配；设备经公开配对服务登记并使用真实 CA 材料；已配对设备完成 challenge / 签名 / grant，审批 ID、设备公钥指纹、PR head 与时序绑定正确，交付到 `closed`（进程级）。
- [x] 被真实 `StoreAdmin::revoke` 吊销的设备与未登记设备均被拒；断言没有 grant、没有 merge 入队，审批保持 pending；错误签名同样被拒（进程级）。还覆盖“challenge 已发出、提交前吊销”：提交时重新读取设备，拒绝该签名（测试驱动的请求边界，不代表真实 API 或并发竞态）。
- [x] 真实 `revoke` 同步写出本地与备份 `revocations.journal`；测试读取两份文件，断言内容相同、JSON 行可解析且 `device_id` 正确（进程级）。
- [x] `build_secrets` 只打开、不初始化：缺失目录与错误主密钥分别返回错误且不改写 `secrets.meta`；M1 实例（deploy 初始化为 Forge / Pki）可用 M1 子集打开，Egress 与 Notify 均返回 `Denied`；已含 Notify 的实例仍可用 M1 子集打开，但 M1 实例不承载 Notify；真实写入的 Forge 秘密对 Pki 命名空间不可见（组件测试）。
- [x] conversation 生产构造（`build_conversation`）：真实 `Conversation` + `EngineApiAdapter` + 内存 store，空 store 上 `run_once` 返回 0 且不写事件（组件测试）。测试装配已切换为真实 `Conversation`，`status` 报 `real`；模型端口由调用方注入。
- [x] egress 配置视图（`egress_config_view`）：`egress.*` 与 `models.providers` 派生的 `model.<id>` 注入项、`levels.token_weights` 映射到 `ConfigView`；显式 `egress.injection` 保留；序列化往返通过视图复核（组件测试）。
- [x] egress 客户端（`build_egress`）：指向给定控制套接字，无进程应答时返回错误而不是 panic（组件测试）。
- [x] 测试装配接入真实 egress：父进程起 `Registry` / `ModelProxy` / `ControlServer`，控制套接字经环境变量传给子进程；子进程 `EgressClient::apply_config` 下发设置视图，`status` 报 `real`（进程级）。模型调用仍经 `EgressStub`，凭据注入与计量走真实 egress 归后续切片。
- [x] settings 上下文（`settings_ctx::RuntimeSettingsContext`）：只读快照提供 `ValidationContext`（能力表、模板、密钥存在性、资源当前值）、`catalog` 与 `status.modules`；组件测试覆盖。
- [x] settings 快照填充（`settings_ctx::build_settings_context`）：从 store 读 8 个 resource 的当前值，按 Forge / Egress 使用方查询密钥存在性，聚合 engine/egress status，其余模块报 `unhealthy` 并附原因；catalog 的能力表/适配器解析有单元测试。
- [x] 测试装配构造 `Assembly.settings_ctx`：models 的 provider `secret_ref` 与静态 catalog 注入，全部 gantryd 进程测试通过。
- [x] 测试装配启动 mTLS API 监听：`ApiCore` 注入全部端口，TLS 1.3 + 客户端证书校验，`serve` spawn `serve_api`；进程测试断言回环监听可连接。
- [x] 测试装配启动配对监听：`PairingListener` + 单向 TLS，`serve` spawn；窗口外连接被接受后立即关闭；进程测试断言回环监听可连接。
- [ ] models 调用与 sandbox job token 切换到真实 `EgressClient`（凭据注入、`budget.charged`、令牌归属）；归 A8 后续切片。
- [ ] M4 将 Notify 加入 gantryd 实例并迁移已有实例（本片不配置、不宣称）。
- [x] SQLite 子进程崩溃重启后复用已登记设备：设备数仍为 1、设备 ID 不变，secrets / CA 来自同一 scratch（进程级）。
- [ ] 生产 `assemble_real` 打开 secrets / pki 并接入 `Pki::tick`（归后续切片，需先确定凭据来源与 `pki.ca_dir` 配置映射）。

## 13. 开放问题

- **真实装配接引擎**：engine 侧的 settings → `RuntimeSettings` 换算（`gantry_engine::runtime_settings::from_settings`）、默认 `ReservationPolicy`（`EqualShareReservation`）与 `status() -> RuntimeStatus` 已由 #289（PR #323）提供。测试装配已改为经 `from_settings` 换算，`fake.runtime_status` 读取运行时 `status()`；模型表由测试 helper 用硬编码设置经 `gantry-models::ModelsView` 推导，不是从生产设置自动推导。生产 `assemble_real` 尚未调用这些入口，控制通道 `status` 仍只输出 `engine.degraded`。接线与跨泳道验收归 #105；`settings.updated` 自动订阅与原子热加载归 #318；`status` 若要暴露 `applied_rev`、游标等字段，先改 §6.2。
- **models 生产构造**：`build_models` 已把 `StoreSettings` / `ViewCache`、`AnthropicProvider`、注入的 Egress / ID / 熵 / 时钟 / 能力表接到真实 `ModelRouter`；本机回环测试覆盖 utility / planner 调用与失败闭合。测试装配已切换为真实 `ModelRouter`，`assemble_real` 未调用。模型配置值（provider / model）来自 models 设置且没有内置默认；推理深度能力表由调用方注入（models.md OQ-1 / OQ-8），提供方凭据由 Egress 持有。以上不代表真实模型 / Egress 验收。
- **egress 装配**：`egress_config_view` 与 `build_egress` 已就绪；测试装配的父进程起真实 egress（Registry / ControlServer / ModelProxy）并在启动时 `apply_config`，`Assembly.egress` 为真实 `EgressClient`。仍缺：models 调用切到真实 egress（凭据注入与 `budget.charged`）、sandbox job token 注入、`settings.updated` 热下发（#318）。换算放在 `gantryd`（K11），未改 core 契约类型。
- **settings 上下文装配**：`RuntimeSettingsContext` 与 `build_settings_context` 已实现；快照内容（resource 当前值、Forge/Egress 密钥存在性、engine/egress status）由装配时聚合，其余模块暂报 `unhealthy`。catalog 由调用方传入（M1 用静态目录），能力表不在 models 设置里推断。`Assembly.settings_ctx`、`Assembly.api` 与 `Assembly.pairing` 已在测试装配中构造；`serve` 启动 mTLS API 与配对监听。生产 `assemble_real` 仍未接线，`settings.updated` 热加载与 SSE 取消订阅等生命周期项归后续切片。
- **持久存储与真实模块**：SQLite store 已接入 `assemble_real`（打开后仍因缺 sandbox 等模块拒绝启动）。policy / notify 已有生产构造函数；测试装配调用 `assembly::build_forge` / `build_policy` / `build_repo` / `build_notify` / `build_models` / `build_conversation`，其中 policy / notify / repo / models / conversation 的 `status` 报 real；`assemble_real` 目前只打开 store 并构造 notify，尚未按依赖顺序调用其余构造函数。sandbox 的 `BuiltinSchemas` 注入留到 sandbox 片。
- **secrets / pki 的生产配置**：`build_secrets` 已按 `server.secrets_dir` 打开实例，`build_pki` 接受显式 `PkiConfig` 与按 `Consumer::Pki` 限定的 `SecretSink`；两者均由调用方注入。P-87 已定：BootConfig 必须显式提供 `[secrets.master_key]`（`systemd_credential` / `key_file` 二选一）与 `[pki] ca_dir`（默认 `/etc/gantry/ca`，启动校验三张证书），缺失即拒绝启动、不生成新密钥、不新建 CA。`assemble_real` 的字段映射与 `harden_process → build_secrets → build_pki` 顺序归 #405 C3。
- **pki 后台生命周期**：`Pki::tick()`（关闭过期配对窗口、按需续签服务端证书）尚未由 `serve` 周期调用；吊销日志在 `revoke` 内同步追加并 fsync 本地与备份目标，不需要额外调度。测试只覆盖同步构造、配对、授权、吊销与崩溃重启，不代表周期任务已在运行。
- **测试 `real` 的边界**：测试装配里的 secrets / pki 使用测试临时目录、静态主密钥与软件设备密钥。它证明的是生产构造路径与跨模块语义，不证明操作系统用户隔离、真实设备配对或 Ubuntu 部署；这些证据归 B / C 级验收。
- **policy / notify 后台生命周期**：`PolicyEngine` / `PolicyApprovals` / `Notify` 的实例构造已接入，但 `gantry_policy::invalidation` 的订阅与过期清扫、`gantry_notify::AutoClose` 的触发循环尚未由 `serve` 启动；归后续切片。在此之前，真实构造只覆盖同步调用路径。
- **`Type=notify`**：deploy 的单元约定 `READY=1`，gantryd 尚未发送（需要 sd_notify，归后续切片）。
