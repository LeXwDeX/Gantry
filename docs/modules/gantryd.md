# 服务端进程（`gantryd`）

> 状态：设计 v0.1（按 #101 的实现回写）· 里程碑：M1 · 泳道：`gantryd` · crate / 目录：`crates/gantryd`

把 architecture §3 的单进程模块按 `core::ports` 装配成一个服务端进程，并提供主机 CLI `gantry` 与本机控制通道。

## 1. 职责

- 读取引导配置 `/etc/gantry/gantry.toml`（deploy.md §10），校验后启动；配置非法时在绑定任何套接字之前退出。
- 装配：为每个跨模块端口构造一个实现（真实或替身），放进 `Assembly`；模块之间只经端口相连（P-51）。`Assembly` 只放 `core::ports` 端口：`api` 作为 `serve` 的服务任务启动（与控制通道并列）；`pki`、`secrets` 在 `assemble_real` 内构造后注入消费者（forge、egress、notify、api 与控制通道的 `PkiControl`），不进 `Assembly` 字段。
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
| 真实装配 `assemble_real` | `src/assembly.rs` | 服务端二进制。已接入 SQLite store 与 notify 生产构造函数；policy 构造函数已就绪但依赖 forge，待 forge 片接入；其余真实模块未接线时仍拒绝启动，不退回内存日志（失败闭合） |
| 替身装配 `assemble_fakes` | `tests/support/`（只用 dev-dependency） | 进程级测试。测试二进制带 `GANTRYD_FAKES_CHILD` 重新执行自身，作为"用替身装配的 gantryd"子进程 |

替身装配的当前构成（`status` 原样报告）：

| 模块 | 提供方 | 说明 |
| --- | --- | --- |
| engine | 真实 | `EngineRuntime`，配置取 engine 测试夹具 |
| notify | 真实 | `assembly::build_notify`（`Notify`） |
| store | 替身 / 真实 | 默认内存日志 + CAS；设置 SQLite 目录时用真实 store |
| policy | 真实 | `assembly::build_policy`（`PolicyEngine` + `PolicyApprovals`）；合并审批由测试设备密钥签名 |
| forge | 替身 | `FakeForge` |
| repo | 替身 | `EventedRepo`（`FakeRepo` 外包一层，写 `worktree.*` / `patch.validated`） |
| sandbox | 替身 | `GoldenSandbox`（受理时写入 `single` 黄金结果） |
| worker / models / egress / conversation | 替身 | `gantry-fakes` 对应替身 |
| secrets / pki / api | 未装配 | 不计入 `Assembly` 字段：api 是 `serve` 的服务任务；pki、secrets 在 `assemble_real` 内构造并注入消费者；M1.4 接线时补齐 |

某个模块换成真实实现时，生产构造函数写进 `src/assembly.rs`，测试装配与
`assemble_real` 共用同一条构造路径；替换时只改调用点与 `status` 表，不改模块代码。
模块私有表由 `assembly::module_specs` 统一登记；SQLite 数据目录先用空列表打开、
再以完整列表重开必须可用（本片用例）。

**本片的真实构造不等于后台任务已运行。** policy / notify 目前只在装配期构造并被
同步调用：`gantry_policy::invalidation` 的事件订阅与过期清扫、`gantry_notify::AutoClose`
的触发循环尚未由 `serve` 启动。`status` 报 `real` 只表示实例来自生产构造函数，
不代表这些后台循环已在运行；生命周期接入归后续切片。

### 7.2 启动与停机

读配置 → 装配 → 绑定控制通道 → 并发运行 `EngineRuntime::run` 与控制通道 accept 循环 → 收到 SIGTERM / SIGINT（或引擎主循环因存储错误退出）时置停机信号 → 等引擎处理完已入队的回报后返回。在途尝试留在沙盒，下次启动由 `Tick(Recover)` 接管（engine.md §7.15）。

### 7.3 进程级测试的驱动

替身装配额外注册 `fake.*` 命令组（只在测试里编译）：`fake.create_delivery`、`fake.events`、`fake.end_bootstrap`，以及 `fake.drive`——每次调用扮演一轮 forge 执行器（认领并完成 Outbox 动作）、GitHub（PR 快照）与审批设备（challenge + 测试设备密钥签名授予待决审批）；`manual` 模式关掉自动授予，用于签名正负对照。父进程只经控制通道反复调用并观察阶段，不触碰执行体内部。

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
- [x] 控制通道：0600、uid 不符即拒、坏请求后连接保持、超长行断开、残留套接字替换、第二个实例被拒、并发启动恰好一个成功、SIGTERM 退出并删除套接字（进程级）。
- [x] 服务端二进制：没有替身参数；无持久存储时拒绝启动；配置非法时不绑定（进程级）。
- [x] CLI：从引导配置取套接字路径；`bootstrap-set` 在引导期前后的行为；uid 被拒与守护进程不在时退出码 1；用法错误退出码 2 且不连守护进程；`doctor host --json`（进程级）。
- [x] `github.setup.*` 替身命令符合 deploy 接线约定，应答不含密钥（进程级）。
- [x] engine.md §12.2「端到端（替身）」主路径在独立 gantryd 进程里跑通：阶段序列恰为 `spec → implement → propose → review → merge → closed`，`spec → implement` 的 `reason = admission_skipped`，无 degraded（进程级）。
- [x] SQLite store 先以空模块列表打开、再以生产模块列表重开，policy 私有表可读写（组件测试）。
- [x] 真实 policy / notify 经生产构造函数装配；合并审批由测试设备密钥签名，正负签名对照通过（进程级）。

## 13. 开放问题

- **真实装配接引擎**：engine 侧的 settings → `RuntimeSettings` 换算（`gantry_engine::runtime_settings::from_settings`）、默认 `ReservationPolicy`（`EqualShareReservation`）与 `status() -> RuntimeStatus` 已由 #289（PR #323）提供，但 gantryd 尚未调用：替身装配仍用手写配置，控制通道 `status` 只输出 `engine.degraded`。接线与跨泳道验收归 #105；`settings.updated` 自动订阅与原子热加载归 #318；`status` 若要暴露 `applied_rev`、游标等字段，先改 §6.2。
- **持久存储与真实模块**：SQLite store 已接入 `assemble_real`（打开后仍因缺 forge 等模块拒绝启动）。policy / notify 已有生产构造函数；测试装配调用 `assembly::build_policy` / `build_notify`，`status` 报 real；`assemble_real` 目前只调用 `build_notify`，policy 依赖 `ForgeReader`，待 forge 片接入。sandbox 的 `BuiltinSchemas` 注入留到 sandbox 片。
- **policy / notify 后台生命周期**：`PolicyEngine` / `PolicyApprovals` / `Notify` 的实例构造已接入，但 `gantry_policy::invalidation` 的订阅与过期清扫、`gantry_notify::AutoClose` 的触发循环尚未由 `serve` 启动；归后续切片。在此之前，真实构造只覆盖同步调用路径。
- **`Type=notify`**：deploy 的单元约定 `READY=1`，gantryd 尚未发送（需要 sd_notify，归后续切片）。
