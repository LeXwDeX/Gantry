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
| 真实装配 `assemble_real` | `src/assembly.rs` | 服务端二进制。已接入 SQLite store 与 notify 生产构造函数；policy 构造函数已就绪但依赖 forge，待 forge 片接入；secrets / pki 的生产构造入口（`build_secrets`、`build_pki`）已就绪，但生产凭据来源（主密钥来源与名称）按 secrets.md OQ-2、deploy OPS-08 未定，`pki.ca_dir` 也尚无 BootConfig 字段映射；因此尚未打开。其余真实模块未接线时仍拒绝启动，不退回内存日志（失败闭合） |
| 替身装配 `assemble_fakes` | `tests/support/`（只用 dev-dependency） | 进程级测试。测试二进制带 `GANTRYD_FAKES_CHILD` 重新执行自身，作为"用替身装配的 gantryd"子进程 |

替身装配的当前构成（`status` 原样报告）：

| 模块 | 提供方 | 说明 |
| --- | --- | --- |
| engine | 真实 | `EngineRuntime`，配置取 engine 测试夹具 |
| notify | 真实 | `assembly::build_notify`（`Notify`） |
| store | 替身 / 真实 | 默认内存日志 + CAS；设置 SQLite 目录时用真实 store |
| policy | 真实 | `assembly::build_policy`（`PolicyEngine` + `PolicyApprovals`）；审批由测试设备签名 |
| secrets | 真实 | `assembly::build_secrets`：测试临时目录里的 `FileSecrets`，M1 只承载 Forge / Pki；Notify 自 M4 才加入，**不含 Egress**；测试代码显式 `init`，生产构造函数只 `open` |
| pki | 真实 | `assembly::build_pki`：测试 CA（`bootstrap::run_with` + 可替换内存盘探测）与真实 `Pki::open`；测试设备经公开 `pairing().open_session` / `redeem` 登记，私钥只留在测试进程内存；吊销经真实 `StoreAdmin::revoke` 写本地与备份吊销日志（都在 scratch 内） |
| forge | 替身 | `FakeForge` |
| repo | 替身 | `EventedRepo`（`FakeRepo` 外包一层，写 `worktree.*` / `patch.validated`） |
| sandbox | 替身 | `GoldenSandbox`（受理时写入 `single` 黄金结果） |
| worker / models / egress / conversation | 替身 | `gantry-fakes` 对应替身 |
| api | 未装配 | 不计入 `Assembly` 字段：api 是 `serve` 的服务任务；M1.4 接线时补齐 |

某个模块换成真实实现时，生产构造函数写进 `src/assembly.rs`，测试装配与
`assemble_real` 共用同一条构造路径；替换时只改调用点与 `status` 表，不改模块代码。
模块私有表由 `assembly::module_specs` 统一登记（policy 与 pki）；SQLite 数据目录先用
空列表打开、再以完整列表重开必须可用（本片用例）。

**测试里的 `real` 不等于生产已在运行。** `assemble_real` 仍返回 `NotReady`；测试装配
在同一进程/子进程里构造真实的 secrets、pki、policy 与 notify，并用它们跑完整路径，但
生产凭据来源（主密钥来源与名称，secrets.md OQ-2 / deploy OPS-08）、`pki.ca_dir` 的
引导配置映射与 `Pki::tick` 的周期调度都还没有接到 `serve`。吊销日志在 `revoke` 内同步
追加并 fsync 本地与备份目标，没有另外的周期备份调度。生产接线前必须先确定上面两组
配置，再按 `harden_process` → `build_secrets` → `build_pki` 的顺序打开。`status` 报 `real` 只表示
实例来自生产构造函数并确实被使用，不代表后台循环已在运行。

### 7.2 启动与停机

读配置 → 装配 → 注册 SIGTERM / SIGINT 接收器 → 绑定控制通道 → 并发运行 `EngineRuntime::run` 与控制通道 accept 循环 → 收到 SIGTERM / SIGINT（或引擎主循环因存储错误退出）时置停机信号 → 等引擎处理完已入队的回报后返回。在途尝试留在沙盒，下次启动由 `Tick(Recover)` 接管（engine.md §7.15）。

注册与接收分开看：注册在绑定之前完成，`tokio::signal::unix::signal` 同步安装进程级处理器并建立接收器；接收发生在绑定之后的 `select` 里，首次 `poll` 之前送达的信号先记为 pending，待运行时信号驱动分发后由接收器取得，同类信号可能合并。因此修复后套接字一旦可连接，SIGTERM / SIGINT 都只会走正常停机，不会按进程原有处置终止进程（#401）。SIGINT 与 SIGTERM 走同一路径，不使用 `ctrl_c()`——它要到首次 `poll` 才注册，覆盖不了绑定与首次轮询之间的窗口。装配和信号注册之前（进程刚启动时）送达的信号按进程原有处置，不一定是默认动作（可继承忽略）。

### 7.3 进程级测试的驱动

替身装配额外注册 `fake.*` 命令组（只在测试里编译）：`fake.create_delivery`、`fake.events`、`fake.end_bootstrap`，以及 `fake.drive`——每次调用扮演一轮 forge 执行器（认领并完成 Outbox 动作）、GitHub（PR 快照）与审批设备（challenge + 测试设备签名授予待决审批）；`manual` 模式关掉自动授予，用于签名正负对照。`fake.device_info` 实时读取设备目录，`fake.revoke_device` 用真实 `StoreAdmin::revoke` 吊销测试设备。父进程只经控制通道反复调用并观察阶段，不触碰执行体内部。测试设备经真实配对服务登记；每次授权都从 `DeviceDirectory::get` 与 `verifying_key` 取身份、角色、状态与公钥，不缓存，吊销后下一次授权即失败。测试签名私钥只留在测试进程内存。

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
- [x] 真实 policy / notify 经生产构造函数装配；合并审批由测试设备密钥签名，正负签名对照通过（进程级）。
- [x] 真实 secrets / pki 经生产构造函数（`build_secrets` / `build_pki`）装配；设备经公开配对服务登记并使用真实 CA 材料；已配对设备完成 challenge / 签名 / grant，审批 ID、设备公钥指纹、PR head 与时序绑定正确，交付到 `closed`（进程级）。
- [x] 被真实 `StoreAdmin::revoke` 吊销的设备与未登记设备均被拒；断言没有 grant、没有 merge 入队，审批保持 pending；错误签名同样被拒（进程级）。还覆盖“challenge 已发出、提交前吊销”：提交时重新读取设备，拒绝该签名（测试驱动的请求边界，不代表真实 API 或并发竞态）。
- [x] 真实 `revoke` 同步写出本地与备份 `revocations.journal`；测试读取两份文件，断言内容相同、JSON 行可解析且 `device_id` 正确（进程级）。
- [x] `build_secrets` 只打开、不初始化：缺失目录与错误主密钥分别返回错误且不改写 `secrets.meta`；M1 实例（deploy 初始化为 Forge / Pki）可用 M1 子集打开，Egress 与 Notify 均返回 `Denied`；已含 Notify 的实例仍可用 M1 子集打开，但 M1 实例不承载 Notify；真实写入的 Forge 秘密对 Pki 命名空间不可见（组件测试）。
- [ ] M4 将 Notify 加入 gantryd 实例并迁移已有实例（本片不配置、不宣称）。
- [x] SQLite 子进程崩溃重启后复用已登记设备：设备数仍为 1、设备 ID 不变，secrets / CA 来自同一 scratch（进程级）。
- [ ] 生产 `assemble_real` 打开 secrets / pki 并接入 `Pki::tick`（归后续切片，需先确定凭据来源与 `pki.ca_dir` 配置映射）。

## 13. 开放问题

- **真实装配接引擎**：engine 侧的 settings → `RuntimeSettings` 换算（`gantry_engine::runtime_settings::from_settings`）、默认 `ReservationPolicy`（`EqualShareReservation`）与 `status() -> RuntimeStatus` 已由 #289（PR #323）提供。测试装配已改为经 `from_settings` 换算，`fake.runtime_status` 读取运行时 `status()`；模型表仍是测试手写的一条映射（归 models 片）。生产 `assemble_real` 尚未调用这些入口，控制通道 `status` 仍只输出 `engine.degraded`。接线与跨泳道验收归 #105；`settings.updated` 自动订阅与原子热加载归 #318；`status` 若要暴露 `applied_rev`、游标等字段，先改 §6.2。
- **持久存储与真实模块**：SQLite store 已接入 `assemble_real`（打开后仍因缺 forge 等模块拒绝启动）。policy / notify 已有生产构造函数；测试装配调用 `assembly::build_policy` / `build_notify`，`status` 报 real；`assemble_real` 目前只调用 `build_notify`，policy 依赖 `ForgeReader`，待 forge 片接入。sandbox 的 `BuiltinSchemas` 注入留到 sandbox 片。
- **secrets / pki 的生产配置**：`build_secrets` 已按 `server.secrets_dir` 打开实例，`build_pki` 接受显式 `PkiConfig` 与按 `Consumer::Pki` 限定的 `SecretSink`；两者均由调用方注入，gantryd 不猜主密钥来源、CA 目录或 Egress 实例。`assemble_real` 尚未打开它们：生产凭据来源（主密钥来源与名称）按 secrets.md OQ-2、deploy OPS-08 未定；`pki.ca_dir` 是另一项缺口，尚无 BootConfig 字段映射。两者接线前还需解决配置与 `harden_process` 的顺序。
- **pki 后台生命周期**：`Pki::tick()`（关闭过期配对窗口、按需续签服务端证书）尚未由 `serve` 周期调用；吊销日志在 `revoke` 内同步追加并 fsync 本地与备份目标，不需要额外调度。测试只覆盖同步构造、配对、授权、吊销与崩溃重启，不代表周期任务已在运行。
- **测试 `real` 的边界**：测试装配里的 secrets / pki 使用测试临时目录、静态主密钥与软件设备密钥。它证明的是生产构造路径与跨模块语义，不证明操作系统用户隔离、真实设备配对或 Ubuntu 部署；这些证据归 B / C 级验收。
- **policy / notify 后台生命周期**：`PolicyEngine` / `PolicyApprovals` / `Notify` 的实例构造已接入，但 `gantry_policy::invalidation` 的订阅与过期清扫、`gantry_notify::AutoClose` 的触发循环尚未由 `serve` 启动；归后续切片。在此之前，真实构造只覆盖同步调用路径。
- **`Type=notify`**：deploy 的单元约定 `READY=1`，gantryd 尚未发送（需要 sd_notify，归后续切片）。
