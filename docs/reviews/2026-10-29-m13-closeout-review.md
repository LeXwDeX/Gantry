# M1.3 收尾评审（2026-10-29）

> 只读记录。评审对象：`main@37c61ef`。方法：4 个并行只读切片（`gantryd` 装配与引擎前置、`gantry-deploy`、桌面外壳 `shell-core`、前端三种适配器），随后由主力回读关键源码复核。本文不声称逐行全审，也不是真实环境验收报告：没有启动真实 Podman、没有执行真实 GitHub 集成，也没有构建或运行 Tauri 应用本身。

## 1. 结论

- **M1.3 的 issue 与里程碑已关闭。** #101、#102、#103、#104、#289、#291 全部关闭（6/6）。#289 依用户 2026-10-29 确认关闭：引擎侧三项前置已由 PR #323 合入；真实消费路径归 #105，自动订阅 `settings.updated` 与原子热加载归 #318，并行在途预留归 #317。
- **生产装配仍未闭合，且失败闭合成立。** `crates/gantryd/src/assembly.rs:93-99` 的 `assemble_real` 固定返回 `NotReady { module: "store" }`，`crates/gantryd/src/lib.rs:68-69` 经 `?` 传播；`gantryd` 二进制没有替身参数，`gantry-fakes` 及其他模块 crate 只在 dev-dependencies。
- **#323 的前置没有伪装接线，但也还没有任何接线。** `from_settings`、`EqualShareReservation` 在 `crates/gantry-engine` 之外零引用；替身装配手写配置，控制通道 `status` 未用 `EngineRuntime::status()`。签名只依赖 core/store/engine 类型，#105 装配时都能取得输入，可以直接调用；但目前的证据只有 engine 内部测试。
- **桌面端边界设计扎实，真实联调未证明。** 命令白名单三处一致，CSP 与规格逐字一致，生产包把 scenario/mock 编译期折叠掉；Tauri 适配器 → 真实 `shell-core` → 真实 `gantryd` 的链路没有任何运行证据，归 #105/#106。
- **本轮新登记 5 个 issue（#367–#371），另在 #105、#315 补充评论。** 没有发现高严重度问题。
- **验收口径不变。** A + B 级全绿才记为「M1 本机验收通过」，C 级全绿才记为「M1 退出」（#87）。下一条主线是 #105；#280、#267 可并行。

里程碑计数于 2026-10-29 用 `gh` 只读核对：

| 里程碑 | 状态 | open | closed |
| --- | --- | ---: | ---: |
| M0.9 M0 遗留收口 | open | 1（#86） | 8 |
| M1.1 决策器 | closed | 0 | 18 |
| M1.2 服务端泳道 | closed | 0 | 16 |
| M1.3 桌面端、组装与部署 | closed | 0 | 6 |
| M1.4 M1 验收 | open | 3（#105–#107） | 0 |
| M2-prep Agent 扩展性预留 | open | 3（#108–#110） | 0 |

## 2. 框架结构评审

### 2.1 `gantryd` 装配边界

| 维度 | 结论 | 证据 |
| --- | --- | --- |
| 边界 | 生产路径无法退回内存 store 或替身；装配只经 `core::ports`；`Provider` 与 `ModuleInfo` 让 `status` 如实报告哪些模块仍是替身 | `assembly.rs:73-99`；`lib.rs:147-153` 单测断言 `--profile fakes`、`--control-uid` 被拒 |
| 稳固性 | 控制通道先校验对端 uid 再读命令，取不到 `peer_cred` 也拒绝；套接字 0600，`.lock` 加 flock 防并发启动；请求结构 `deny_unknown_fields`，单行 64 KiB 上限 | `control.rs:101-160`、`control.rs:183-188` |
| 扩展性 | `Assembly` 没有 api、pki、secrets 的承载位置，#105 需先决定它们进字段还是经 `CommandSet` / 服务任务注入 | `assembly.rs:73-91` |
| 证据与实现脱节 | 替身装配是独立手写夹具，#323 的 API 没有跨 crate 调用方；第一次真实调用会同时暴露 settings 来源、模型解析与时钟，风险集中在 #105 | `tests/support/mod.rs:225-231`、`tests/support/engine_cfg.rs:39-45` |

### 2.2 #323 引擎前置能否被 #105 直接调用

- `crates/gantry-engine/src/runtime_settings.rs:70-91` 的 `from_settings` 是纯函数，输入是 core 的 typed settings、`ProjectFacts`、`ModelTable`、`Timestamp` 与 build schema 字节；build schema 来自 core 的内置注册表（#319 已合入），`now` 来自 `Clock`，store 的 `settings_at_rev_for` 已提供同修订读取（`crates/gantry-store/src/lib.rs:1441`）。#105 装配时没有拿不到的输入。
- 换算规则抽查：模型修订不匹配、缺 profile 或 group、build schema 字节不符、租约 `wall + grace + margin` 溢出、proposals 取 settings 与 20 的较小值，均有测试。复核：`cargo test --locked -p gantry-engine --all-features --test runtime_settings --test runtime_settings_integration --test runtime_status --test reservation_policy`，19 项通过。
- 已知边界：`kill_grace_s` 不在 `positive()` 列表，0 可通过换算，落点待 #321；`runtime_settings.rs:267` 写死 `Trust::Trusted`，是 M1 假设；`status()` 的一致性检查只阻止错误上报，不代替 #318。
- **结论：可以直接调用，没有伪装接线；但"直接调用"目前只有 engine 内部证据。** 推荐 #105 第一步让替身装配也走 `from_settings` 与 `EqualShareReservation`，已写入 #105 评论。

### 2.3 `gantry-deploy`

- 边界清晰：`github_setup`、`doctor`、`bootstrap_set` 都经 `ControlClient` 与 `HttpProbe` trait 注入，本 crate 不接触 pem 或 webhook secret，`reject_secrets` 递归拒绝应答中的密钥字段。
- systemd 单元的加固超过规格骨架（`SystemCallFilter`、`MemoryDenyWriteExecute`、`InaccessiblePaths`），`deploy/lib/layout.sh` 的属主与模式和 deploy.md §6.3 表一致，创建与审计共用一张表。
- 弱点在"替身证据与生产"之间：生产控制通道只有 `status`，`settings.bootstrap_set` 的替身不查白名单，因此 deploy 的绿测试只证明客户端形状，不能作为 I7 证据（#371）。doctor 的"未实现"被映射为 warn，与安装脚本的消费方式不一致（#367）。
- 复核：`cargo test --locked -p gantry-deploy --all-features` 通过；`deploy/tests/run.sh` 与 shell、systemd、nft 资源只做静态阅读。实机项（两次幂等、`systemd-analyze verify`、`nft -c`、依赖 root 的 doctor）仍按 #104 评论延期到 #106。

### 2.4 桌面外壳 `shell-core` 可信边界

- IPC：`apps/desktop/src-tauri/src/lib.rs:313-337` 注册 23 条命令，与 `shell-core/src/ipc.rs:13-37` 的 `COMMANDS`、`capabilities/main.json` 逐项一致；`build.rs` 由 `COMMANDS` 生成权限并有对账测试。`tauri.conf.json` 的 CSP 与 desktop-shell.md §7.6 一致，`withGlobalTauri=false`、`freezePrototype=true`，导航与新窗口拒绝，单实例锁。
- 路由：`routes.rs:42-91` 恰好 48 条，与 `schemas/openapi.yaml` 的 48 个操作一致；外壳专用路由对 `api_request` 不可见，`approval_sign` / `guarded_write_submit` 只收句柄并用 `exact_args` 拒绝多余键。
- 密钥与 TLS：软件密钥只在 `test-software-keys` feature 下编译（`keys/mod.rs:14`），Tauri 应用未启用；无硬件时返回 `HardwareUnavailable`，不降级。`tls.rs` 固定根指纹、仅 TLS 1.3、无关闭校验分支。
- 缺口：配对 token 未按 §7.6 清零保存（#369）；已授权审批的恢复路径不核对内容（并入 #315）；Windows 本机文件权限依赖目录继承（`storage.rs:384-385`），归 #106/#107 实机验证；Tauri 应用不在根 workspace，capabilities 与 CSP 的运行效果没有自动化证据。
- 复核：`cargo test --locked -p gantry-desktop-shell --all-features` 通过（lib 单测 20 通过、3 忽略，忽略项为硬件相关）。

### 2.5 前端三种适配器

- 编译期折叠成立：`apps/desktop/src/ipc/index.ts:67-90` 以 `import.meta.env.MODE` 判断，生产只落到 `TauriIpcAdapter` 或 `UnavailableIpcAdapter`（`unavailable.ts:17-21` 不回落到示例）。`npm run build` 后产物中 `ScenarioIpcAdapter`、`127.0.0.1:8787` 出现 0 次；URL 参数与 `localStorage` 不能在生产切回模拟。
- 联网规则成立：全 `src` 只有 `src/ipc/mock/adapter.ts` 使用 `fetch`；`scripts/check-boundaries.mjs` 以 AST 静态检查禁止其他联网与 `innerHTML`、`eval`、`window.open`。
- 不可信文本：无 `dangerouslySetInnerHTML`；`src/render/Markdown.tsx` 用 `marked` 的 token 生成 React 元素，`SafeLink` 只放行 `https:`。
- 缺口：`TauriIpcAdapter` 没有任何测试，前端 `routes.ts` 与枚举未对 openapi 对账（#370）。`apps/desktop/VERIFICATION.md` 自己把证据定为 A 级替身，`README.md:5` 写明不是已完成真实后端联调的客户端——这是口径，不是缺陷。
- 复核：`npm run typecheck`、`lint`、`build` 通过；`npm test -- factory mock-adapter boundaries scenario openapi-types route-body` 8 个文件 91 项通过。

## 3. 代码评审：新发现

已与 #293–#322、#346、#347、#352 去重。行号为 `main@37c61ef` 的位置。

| 编号 | 严重度 | 证据 | 复核 | 处置 |
| --- | --- | --- | --- | --- |
| N1 | 中 | 替身装配不走 #323 入口：`crates/gantryd/tests/support/mod.rs:225-231` 手写 `RuntimeSettings`，`engine_cfg.rs:39-45` 的 `FixedReserve` 恒返回 1000；`engine_cfg.rs:3-4` 注释过时 | 全仓 grep `from_settings`、`EqualShareReservation`，engine 外零引用 | 属 #105 范围，已写入 [#105 评论](https://github.com/LeXwDeX/gantry/issues/105#issuecomment-6487468346)，不另开 issue |
| N2 | 低 | 控制通道 `status` 只输出 `engine.degraded`：`crates/gantryd/src/control.rs:277-289`；`status()` 唯一调用点是测试命令 `fake.runtime_status` | 回读源码 | 符合 gantryd.md §6.2 现文；扩展字段先改规格，写入 #105 评论 |
| N3 | 中 | doctor 未实现区域返回 Warn（退出码 1）：`crates/gantry-deploy/src/doctor.rs:167-174`、`:74-77`；`deploy/install.sh:617,627` 用 `\|\| die`，`:630-638` 又把 1 视为通过 | 回读源码与脚本 | [#367](https://github.com/LeXwDeX/gantry/issues/367) |
| N4 | 中 | `install-state.json` 缺失 / 损坏时 `test_mode` 按 false：`crates/gantryd/src/cli.rs:141-148`；deploy.md §7.14 要求测试开关在 host 区域标 fail | 回读源码；`crates/gantry-deploy/tests/doctor.rs:66` 只测 true | 并入 #367 |
| N5 | 低 | `--page` 文件 `std::fs::write` 未设 0600：`crates/gantry-deploy/src/github_setup.rs:201`；页面内 `state` 15 分钟内是回调凭据 | 仅静态 | 并入 #367 |
| N6 | 低（设计） | `/run/gantry` 为 `gantry:gantry-link 0770`（`deploy/tmpfiles.d/gantry.conf`），Egress 可写（`deploy/systemd/gantry-egress.service:21,46`）；root 客户端 `crates/gantry-deploy/src/control.rs:114-119` 不校验对端 | 回读源码与单元文件 | [#368](https://github.com/LeXwDeX/gantry/issues/368)，需先改规格 |
| N7 | 中 | `settings.bootstrap_set` 只在替身实现且不查白名单：`crates/gantryd/tests/support/fake_cmds.rs:158-172`；`core::bootstrap_writable`（`crates/gantry-core/src/settings/rules.rs:390`）无生产调用点；相对键前缀与类型转换无契约 | 回读源码 | [#371](https://github.com/LeXwDeX/gantry/issues/371)，契约先行 |
| N8 | 低（违反规格） | 配对 token 用普通 `String`：`apps/desktop/src-tauri/shell-core/src/pairing.rs:36-41`；desktop-shell.md §7.6「内存」行要求 `SecretString` / `zeroize` | 回读源码与规格 | [#369](https://github.com/LeXwDeX/gantry/issues/369) |
| N9 | 低 | 只比对外层 `approval_id`（`approval.rs:81`），payload 内 ID 未比对；`api_base_url` 只校验 `https://` 前缀（`pairing.rs:290-291`） | 仅静态；代码符合规格字面 | 并入 #369，需先澄清规格 |
| N10 | 低 | 已授权审批的恢复路径不核对 action / target / digest：`shell-core/src/guarded.rs:410-415`、`:480` | 回读源码 | 并入 [#315 评论](https://github.com/LeXwDeX/gantry/issues/315#issuecomment-6487468669) |
| N11 | 低 | `TauriIpcAdapter` 无测试；`tests/`、`e2e/` 无引用；前端 `routes.ts` 未对 openapi 对账 | grep 全部测试 | [#370](https://github.com/LeXwDeX/gantry/issues/370) |
| N12 | 文档 | gantryd.md §13 首条仍称 #289 跟踪引擎前置 | 回读 | 本 PR 更新 gantryd.md §13 |

### 3.1 信息项（不开 issue）

- `deploy/nftables/gantry.nft.in` 的 API 来源限制只匹配 IPv4；当前 `deploy/config/gantry.toml.in` 监听 `0.0.0.0`，没有实际暴露，改为 `[::]` 时需同步。
- `/var/lib/gantry`（0750）与 `/var/lib/gantry/workspaces`（`gantry:gantry-sbx` 0710）的可达性矛盾与规格同源，已由 deploy.md OPS-11 标为待定，#106 实机时处理。
- `CurlProbe`（`crates/gantry-deploy/src/probe.rs:43-57`）继承环境代理且从本机发起，不等于外部视角；与 #106 的入口验证一起处理。
- `control.rs:124` 在 `accept` 持续出错时无退避地打印日志；生命周期宿主语义归 #314。
- CSP 未加 `require-trusted-types-for 'script'`，规格写"可行时"，不算违规。
- 前端主包约 598 kB，构建有大 chunk 警告，不影响边界结论。

## 4. I1–I8 与本里程碑相关的证据

只记录本轮能核对到的局部证据，不是不变式验收表。

| 不变量 | 本轮证据 | 缺口 |
| --- | --- | --- |
| I3 放行只由 policy 决定 | 外壳只持句柄，`exact_args` 拒绝夹带；审批签名 self_check 要求 payload 的 JCS 与类型化字节一致（`approval.rs:73-140`） | N9、N10；真实签名设备归 #107 |
| I6 客户端不执行、不持凭据 | 命令白名单三处一致、CSP、编译期折叠、仅 mock 可联网、软件密钥 feature 隔离、TLS 固定根 | N8、N11；Tauri 应用运行效果、真实 API、Windows TPM 未验证 |
| I7 配置与工作分离 | 引导配置 `deny_unknown_fields`；`bootstrap_writable` 白名单定义在 core | N4、N7：白名单无生产调用点，替身不查白名单 |
| 生产装配失败闭合 | `assemble_real` 返回 `NotReady`，无替身回退 | 真实装配归 #105 |

## 5. 覆盖限制与已排除

- 本文是 `main@37c61ef` 上的抽样评审；deploy 的 shell、systemd、nft 资源只做静态阅读，没有运行 `deploy/tests/run.sh`。
- 没有构建 `apps/desktop/src-tauri` 的 Tauri 应用；capabilities、CSP、无 `core:*` 权限时 Channel 是否可用均未运行验证。
- 没有把 #103、#104 的关闭写成真实联调或实机部署完成；它们的证据是 A 级替身与静态资源。
- 没有把 #289 的关闭写成生产装配可运行；`from_settings` 在装配路径零覆盖。
- 已排除：控制通道"恰好 64 KiB + 1 字节且以换行结尾"被放行（`control.rs:211`）无安全影响，不登记。

## 6. 处置汇总

| 处置 | 项目 |
| --- | --- |
| 本轮新开 issue | #367 deploy doctor 衔接与失败闭合；#368 控制通道对端校验；#369 shell-core token 清零与绑定澄清；#370 前端 Tauri 适配器与路由对账测试；#371 `settings.bootstrap_set` 参数契约 |
| 补充评论 | #105（接线切入点）；#315（N10） |
| 本轮文档更新 | 本记录；`AGENTS.md` 第 1 节；`NEXT-STEPS.md`；`docs/modules/gantryd.md` §13 |
| 未处理，继续跟踪 | #293–#322 中仍 open 的项；#346、#347、#352；#267、#280 |
| 里程碑状态 | M1.3 已关闭；下一条主线 #105，#280、#267 可并行；#106、#107 先征求用户确认 |

本轮评审不实施代码修复。
