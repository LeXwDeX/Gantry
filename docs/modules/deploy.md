# 部署（`deploy`）

> 状态：设计 v0.1 · 里程碑：M0（CI）· M1（安装脚本 / systemd / webhook 入口（反向代理 / 隧道）/ `gantry github setup` / 备份脚本）· M2（升级回滚 / 备份恢复演练 / gVisor 默认）· M3（轮换运行手册）· M4（APNs 出站与凭据）· 泳道：`ops` · 目录：`deploy/`（脚本、单元、模板、测试；需要 Rust 的子命令见 §6.1 与 OPS-07）

`deploy` 把一台干净的 Ubuntu 变成"可配对、可备份、可升级"的 Gantry 服务端，并让 [architecture §5](../architecture.md#5-信任边界与不变式) 的信任边界在**操作系统层面**成立：不同进程不同系统用户、密钥只给需要它的进程读、沙盒碰不到服务端的任何端点和密钥、根 CA 私钥不在服务端留存。

标注约定：**[提议]** = 骨架没定、本规格给的默认，可被推翻；**[建议值，待调优]** = 数值起点；**需核实** = 作者没有在官方文档中确认，实现前必须核实，**不要当作已知事实**；**OPS-xx** = 见 §13 的开放问题，实现者不得自行拍板。

## 1. 职责

1. **安装脚本**（`install.sh`）：预检、安装系统依赖、创建系统用户与目录、落位程序、写引导配置、装 systemd 单元与主机防火墙规则、驱动后续各步骤；幂等、可续跑（§7.1）。
2. **`secrets` 初始化**：创建两个密钥存储实例（`gantryd` 的 `secrets/`、Egress 的 `secrets/`）并登记读取方（§7.4）。密码学与存储格式归 `secrets`。
3. **离线根 CA 引导**：在内存盘上生成根 CA（offline root CA）与在线中间 CA（intermediate CA），导出根私钥到离线介质，**验证导出后从服务端删除**，并断言服务端不再持有它；签发服务端证书（§7.3）。CA 的密码学与证书策略归 `pki`。
4. **webhook 入口**（先于 App 创建，P-65）：`forge` 的 webhook 监听只在回环地址上以明文 HTTP 提供；对公网一律经终止 TLS 的反向代理或隧道（公网证书），只转发 `/webhook` 与 App 创建期间的 `/setup/github/callback`；两种形态的模板、配置与验证；**不使用 smee.io**（P-13）（§7.6）。
5. **GitHub App 创建**：主机命令 `gantry github setup` 生成一次性的 App Manifest 提交页，经同一 webhook 入口的 `/setup/github/callback` 由 `forge` 接收 `code`、换回 App ID、私钥、webhook secret 并写入 `secrets/` 与 `github.*`（含 `github.commit_identity`）；App 不申请 `workflows` 权限（P-48）；引导用户只把 App 装到选定仓库（§7.5，P-65）。
6. **systemd 单元**：`gantryd`、`gantry-egress` 两个独立进程及其加固、目标单元、备份定时器、防火墙单元（§7.7）。
7. **主机防火墙**：把沙盒用户的出站限制为只能到 Egress，并保证服务端监听端口对沙盒不可达（§7.8）。
8. **安装引导期写入**（P-66）：第一台设备配对之前，主机命令（需 root）可写入 `github.*`、`sandbox.base_image` 等"仅 approver"键并写审计事件；之后同类改动需要审批签名（§7.9）。
9. **备份与恢复**：按 [architecture §6](../architecture.md#6-状态归属) 的"需要备份"清单做一致性快照、加密、轮转、校验；恢复时 `log_epoch` 加一、由 `pki` 重放 `revocations.journal` 中晚于备份的吊销（P-67）；恢复后的核对清单（§7.10、§7.11）。
10. **升级与回滚**：版本化目录 + `current` 链接 + 升级前强制备份 + 迁移保护 + 自动回滚（§7.12）。
11. **主机应急命令的使用说明**：`gantry device recover` 等（命令本身归 `pki`，P-39）（§7.13）。
12. **自检**（`gantry doctor`）与**安装测试基础设施**（§7.14、§7.15）。

### 里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | `ops` 泳道只交付 CI：Rust fmt / clippy / test、schema 校验、前端 lint / typecheck（[milestones M0](../milestones.md#m0-契约冻结)）；本规格另加 **[提议]** 的 deploy 静态检查（`shellcheck`、`systemd-analyze verify`、`nft -c`、模板渲染、密钥扫描，§7.15）和 `deploy/` 目录骨架；不交付可安装物 |
| M1 | `install.sh` 全流程（§7.1）；用户 / 目录 / 权限；两个 systemd 单元；离线根 CA 引导；`secrets` 初始化；webhook 入口（回环监听 + 终止 TLS 的反向代理 / 隧道，两种模板与验证，C28、P-65）；`gantry github setup`（C28）；主机防火墙；引导期写入（P-66）；`backup` 脚本 + 定时器（含 `revocations.journal`，P-67）；`gantry doctor`（M1 子集）；应急命令说明；VM 级安装冒烟测试 |
| M2 | 升级与回滚（`gantry-upgrade` / `gantry-rollback`）；**备份恢复演练**（`gantry-restore` + 演练脚本，纳入定期验证；恢复后 `log_epoch` 加一、客户端全量重同步、重放 `revocations.journal`，P-67）；gVisor（`runsc`）随安装默认配置（[milestones M2 `exec`](../milestones.md#m2-pr-后台守护--medium--high)）；私有包仓库反向代理的沙盒侧端口与防火墙放行（随 M2 `exec`，C28）；`doctor` 全量检查 |
| M3 | 密钥轮换运行手册（App 私钥、webhook secret、中间 CA、模型 key，§7.13 末），与 `sec` 泳道的轮换功能联调；出网审计回放所需的日志落盘与备份范围确认 |
| M4 | APNs（归 M4 `ops`，P-63）：`gantryd` 到 APNs 的出站放行、APNs 密钥进 `secrets/`（读取方 `notify`，P-59）、备份范围纳入推送令牌表；文档化运营者自备 Apple 账号或中继的二选一（OPS-22） |

## 2. 不负责

- **CA 的密码学、证书有效期与算法、配对协议、吊销表、`gantry pair` / `gantry device recover` 的实现**：归 `pki`。`deploy` 只编排调用并验证结果。
- **密钥的静态加密格式、读取方隔离的库实现、轮换与加密备份的实现**：归 `secrets`。
- **webhook 的接收、验签、去重、对账**：归 `forge`（[forge-actions §5](../interfaces/forge-actions.md#5-webhook-接收)）。`deploy` 只提供入口、监听配置与外部可达性。
- **沙盒的创建、镜像内容、网络细节、资源限制**：归 `sandbox` / `egress` / `worker`。`deploy` 提供 OS 前提（用户、subuid、`podman`、`runsc`、防火墙规则）。
- **Egress 的转发、注入、计量与白名单**：归 `egress`。
- **模型凭据的录入**：用户在桌面端设置页写入，需 approver 设备签名（P-18），不经安装脚本。
- **settings 的读写与校验**：归 `api` + `core`。`deploy` 只在引导期经主机命令写入初值（§7.9，P-66）。
- **GitHub 仓库的分支保护 / ruleset 配置**：用户在 GitHub 上做；`deploy` 只检查并警告（[forge-actions §9.1](../interfaces/forge-actions.md#91-仓库权限最小集)）。
- **隧道服务商账号与域名**：用户自备；`deploy` 只给配置模板与验证步骤。
- **桌面端 / 手机端的安装**：归 `desktop-shell` / `mobile`。
- **监控与告警平台**：只提供日志、`doctor` 输出和指标端点的挂接说明（§11）。

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `pki` | 根 / 中间 CA 引导子命令、服务端证书签发、`gantry pair`、`gantry device recover`、恢复后重放吊销（`replay_revocations`）、引导期判定（`bootstrap_period`）；根私钥导出格式 | [architecture §7.1](../architecture.md#71-首次部署与配对)；[api.md §1.3](../interfaces/api.md#13-配对端点唯一的单向-tls-端点)；P-06、P-39、P-66、P-67；[pki.md](pki.md) |
| `secrets` | 初始化两个实例、登记读取方、`export` / `import`（加密备份）、引导期写入 | architecture §4、P-38、P-66；[secrets.md](secrets.md) |
| `forge` | webhook 监听配置与参数、`/setup/github/callback` 与 Manifest 转换（`gantry github setup` 的服务端部分）、启动自检（App 权限）、`installation_changed` | [forge-actions §5.1、§9.5、§9.6、§11 B](../interfaces/forge-actions.md)；[forge.md §7.14](forge.md)；P-65 |
| `egress` | 独立进程、沙盒侧监听（`job_token`）、回环 `svc_model` 监听、控制面 `/run/gantry/egress.sock`、`sandbox_cidrs`、配置文件 | [config.md §8](../interfaces/config.md#8-egress)；[interfaces/egress-control.md](../interfaces/egress-control.md)；P-16、P-49、P-50；[egress.md](egress.md) |
| `sandbox` / `worker` | 运行时用户、`podman` / `runsc`、官方 worker 镜像（随版本发布、固定摘要，`sandbox.base_image` 的默认值，P-66）、工作区目录 | [worker-protocol §2、§3.5](../interfaces/worker-protocol.md)；[sandbox.md](sandbox.md) |
| `store` | SQLite 路径、在线快照（`backup_to`）、恢复（`restore_from`，`log_epoch` 加一，P-67）、迁移入口、`schema_version` | [data-model §5](../data-model.md#5-事件)；[store.md](store.md) |
| `api` | 监听器（mTLS API、配对）、端口、`api_base_url` | [api.md §1](../interfaces/api.md#1-传输与版本) |
| 配置 | 引导配置的范围；`github.*` 初值由本模块写入 | [config.md §1.1、§10](../interfaces/config.md) |
| 外部 | Ubuntu apt 源、GitHub（`github.com`、`api.github.com`）、隧道 / ACME 服务（按选择） | — |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 发布产物 | 发布渠道（GitHub Releases 或离线拷贝） | 压缩包 + `SHA256SUMS` + 签名（方案 OPS-06） | 不可信，校验后使用 |
| 安装参数 | 管理员（命令行 / 交互） | `--api-host`、`--webhook-mode`、`--webhook-url`、`--export-to`、`--app-owner`… | 管理员可信；做格式校验 |
| 主机环境事实 | OS | 版本、架构、cgroup、user namespace、NTP、磁盘、端口 | 可信但要核验 |
| 根私钥口令 | 管理员（不回显 / 文件） | 交互输入；自动化用 `--passphrase-file`（用后删除） | 机密 |
| Manifest 转换响应 | `api.github.com`（TLS），由 `forge` 在 `/setup/github/callback` 中取得 | App ID、`pem`、`webhook_secret`、`client_secret`、`html_url` 等（字段需核实） | 外部数据：校验类型 / 形状后使用，永不写入日志 |
| 备份档 | 管理员指定路径 | §7.10 的加密归档 + `MANIFEST.json` | 不可信，先验完整性与版本再恢复 |
| 管理员的"已离线保存"确认 | 管理员 | 须键入根证书指纹前 8 位十六进制 | 人工确认 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| 系统用户 / 目录 / 权限 | 主机 | §6.2、§6.3 |
| systemd 单元、防火墙规则、引导配置 | `/etc`、`/opt` | §7.7、§7.8、§10 |
| 离线根 CA 导出包 | 管理员指定的离线介质 | 加密根私钥 + 根证书 + 指纹清单（格式 OPS-03） |
| 服务端证书链、CA 指纹 | `pki` 存储 / `gantry pair` 使用 | 离线根 CA 指纹（P-64） |
| GitHub App 凭据 | `secrets/`（`github.app.private_key`、`github.app.webhook_secret`）；`github.app.app_id`、`github.commit_identity` 经引导期写入（P-66、P-69） | [config.md §10](../interfaces/config.md#10-github) |
| 引导期写入的审计事件 | 事件日志（`settings.updated`，`actor.kind = system`，P-66） | [config.md §2.3](../interfaces/config.md#23-存储事件版本追溯) |
| 备份档 | 本地暂存 + 管理员配置的异地目的地 | §7.10 |
| `doctor` 报告 | 终端 / JSON | §7.14 |
| 安装状态文件 | `/etc/gantry/install-state.json` | 已完成步骤、版本、时间；不含密钥 |

## 6. 对外接口

### 6.1 命令与脚本一览

"归属"列：谁实现。"来源"列：骨架已有 / **[提议]**。

| 命令 | 作用 | 运行身份 | 归属 | 来源 |
| --- | --- | --- | --- | --- |
| `install.sh [参数]` | 一键安装 / 续跑（§7.1） | root | `deploy`（Bash） | 骨架（"安装脚本"） |
| `gantry pair` | 开启配对窗口，输出二维码 / 配对串（地址 + CA 指纹 + 一次性 token） | root | `pki` | 骨架 |
| `gantry device list` / `gantry device recover …` | 列设备 / 主机应急（§7.13） | root | `pki` | `recover` 骨架（P-39）；`list`、参数语法 **[提议]** |
| `gantry pki bootstrap-ca` | 在内存盘生成根 + 中间 CA 与服务端证书（§7.3） | root | `pki` | **[提议]** 名称 |
| `gantry secrets init` / `export` / `import` | 初始化实例；加密导出 / 导入（§7.4、§7.10） | 对应实例的用户 | `secrets` | **[提议]** |
| `gantry github setup` | 生成一次性 Manifest 提交页，经 webhook 入口的 `/setup/github/callback` 完成 App 创建（§7.5）；仅引导期或经 approver 签名授权 | root | `deploy`（CLI）+ `forge`（回调与转换，forge.md §7.14） | 骨架（P-65；细节需核实） |
| `gantry settings bootstrap-set <resource> <键>=<值>` | 引导期写入"仅 approver"键（§7.9）；第一台设备配对后拒绝 | root | `deploy`（CLI，经 `control.sock`） | **[提议]** 名称（P-66） |
| `gantry doctor [区域] [--json]` | 自检（§7.14） | root | `deploy`（Rust 子命令） | **[提议]** |
| `gantry-backup` / `gantry-restore` | 备份 / 恢复（§7.10、§7.11；恢复内含 `Store::restore_from` 与 `pki` 的 `replay_revocations`，P-67） | root | `deploy`（Bash） | **[提议]**；脚本在骨架 AGENTS.md 的 `deploy/` 范围内 |
| `gantry-upgrade` / `gantry-rollback` | 升级 / 回滚（§7.12） | root | `deploy`（Bash） | **[提议]**（M2） |
| `uninstall.sh [--purge]` | 卸载；默认保留数据 | root | `deploy`（Bash） | **[提议]** |

`gantry` CLI 通过本机控制通道（`/run/gantry/control.sock`，仅 root 可用，OPS-07）与运行中的 `gantryd` 通信；`pair`、`device recover` 的具体通道由 `pki` 规格定。

**`install.sh` 参数 [提议]**：

| 参数 | 含义 |
| --- | --- |
| `--api-host <名字或 IP>`（可重复） | 写入服务端证书 SAN 与 `api_base_url`；必填 |
| `--webhook-mode tunnel / public` | webhook 入口形态（默认 `tunnel`，对应 Q-02 的推荐） |
| `--webhook-url <https://…/webhook>` | 公网可达的 webhook 地址；写入 App Manifest（`hook_attributes.url`；`redirect_url` 为同一主机的 `/setup/github/callback`）。`tunnel` 模式下指隧道的稳定主机名 |
| `--export-to <目录>` | 根私钥（加密）导出位置，须是已挂载的外部介质；缺省为"仅屏幕显示"（§7.3） |
| `--app-owner user / org:<名>` | App 的拥有者（需核实相应创建入口，OPS-02） |
| `--non-interactive` | CI / 测试用；口令走 `--passphrase-file`，确认走 `--assume-exported` |
| `--resume` / `--only <步骤>` | 续跑 / 只做某一步 |
| `--test-mode` | 仅测试：允许覆盖 GitHub 地址为假服务；`doctor` 对此标红 |

退出码 **[提议]**：`0` 成功；`10` 预检失败；`20` 用户中止；`30` CA 引导失败；`40` App 引导失败；`50` 服务未就绪；`60` 自检失败；其余 `1`。

### 6.2 进程与运行用户

| 进程 | 系统用户:组 | 登录 / 家目录 | 能读 | 不能读 |
| --- | --- | --- | --- | --- |
| `gantryd`（含 `api`、`forge`、`engine`、`repo`、`pki`、`store`…，architecture §3 的单进程） | `gantry:gantry` | `nologin`，`/var/lib/gantry` | 自己的 `gantry.db`、`secrets/`（App 私钥、webhook secret、中间 CA 私钥）、CAS、镜像仓、工作树、`/etc/gantry` | Egress 的 `secrets/` 与审计；沙盒用户的存储 |
| `gantry-egress`（独立进程，architecture §4 的 `egress`） | `gantry-egress:gantry-egress` | `nologin`，`/var/lib/gantry-egress` | 自己的 `secrets/`（模型 key、私有包仓库凭据，architecture §6）、`/etc/gantry-egress` | `gantryd` 的 `secrets/`、数据库 |
| 沙盒运行时（rootless Podman，architecture §3："与服务端进程使用不同的系统用户"） | `gantry-sbx:gantry-sbx` | `nologin`，`/var/lib/gantry-sbx`；`subuid` / `subgid` 各 65536；`loginctl enable-linger` | 自己的容器存储；`repo` 交给它的工作区 / 输入 / 输出目录 | 其余一切 |
| 隧道 / 反向代理（可选） | 各自发行包的用户（`cloudflared`、`caddy`…） | — | 自己的凭据 / 证书 | 全部 Gantry 目录与密钥 |

附加组 **[提议]**：`gantry-link`（成员：`gantry`、`gantry-egress`）用于 Egress 控制面套接字 `/run/gantry/egress.sock`（P-50）；`gantry-sbxctl`（成员：`gantry`）用于控制 `gantry-sbx` 的运行时套接字。**任何套接字都不得挂进沙盒**（P-15）。控制面协议见 [interfaces/egress-control.md](../interfaces/egress-control.md)；沙盒运行时的控制机制由 `sandbox` 规格定（OPS-11）。

### 6.3 目录布局与权限 [提议]

只有 `/var/lib/gantry/worktrees/<repo>/<dlv>`（[api.md §4.6 示例](../interfaces/api.md#46-worktrees)）与 `secrets/` 目录名来自骨架，其余为提议。所有数据目录 `umask 0077`。

| 路径 | 所有者:组 | 模式 | 内容 | 备份 |
| --- | --- | --- | --- | --- |
| `/opt/gantry/releases/<版本>/` | root:root | 0755 | `bin/{gantryd,gantry,gantry-egress}`、`deploy/` 脚本与模板 | 否（可重装） |
| `/opt/gantry/current`、`previous` | root:root | 符号链接 | 当前 / 上一版本 | 否 |
| `/usr/local/sbin/gantry*` | root:root | 符号链接 | 指向 `current` 下的程序 | 否 |
| `/etc/gantry/` | root:gantry | 0750 | `gantry.toml`（0640，**无密钥**）、`ca/`（根 / 中间 / 服务端证书链，均为公开证书，0644）、`install-state.json` | 是 |
| `/etc/gantry-egress/` | root:gantry-egress | 0750 | `egress.toml`（0640，无密钥） | 是 |
| `/var/lib/gantry/` | gantry:gantry | 0750 | 以下各项 | — |
| `…/pki/revocations.journal` | gantry:gantry | 0600 | 设备吊销的只追加日志（P-67），在事件日志之外；每次写入同步复制到 `backup.journal_target`（P-80） | **是**（每次备份复制一份；恢复时与主机上现有文件、`backup.journal_target` 的副本**合并**，不覆盖，§7.11） |
| `…/gantry.db`（+ `-wal`、`-shm`） | gantry:gantry | 0600 | 事件日志 + 投影 + Outbox + `pki` 表（SQLite） | **是**（在线快照，§7.10） |
| `…/cas/` | gantry:gantry | 0700 | 结果库（`result_ref`、进度、被拒产物） | **是**（保留期待定，architecture §6） |
| `…/secrets/` | gantry:gantry | 0700 | 加密密钥文件：App 私钥、webhook secret、中间 CA 私钥 | **是**（只备份加密导出） |
| `…/mirrors/<owner>/<repo>.git` | gantry:gantry | 0700 | 裸镜像仓 | 否（可重建） |
| `…/worktrees/<repo>/<dlv>` | gantry:gantry | 0700 | 工作树 | 否（可重建） |
| `…/workspaces/` | gantry:gantry-sbx | 0710 | `repo` 为沙盒准备的一次性工作区 / `inputs` / `out`；作业目录的所有权与 ACL 待定（OPS-11） | 否 |
| `/var/lib/gantry-egress/` | gantry-egress:gantry-egress | 0700 | `secrets/`（0700）、`audit/`（出网审计日志，保留期 config OQ-21） | `secrets/` 是；审计待定 |
| `/var/lib/gantry-sbx/` | gantry-sbx:gantry-sbx | 0700 | 容器存储与镜像 | 否 |
| `/run/gantry/` | gantry:gantry-link | 0770（`systemd-tmpfiles` 创建，两个单元都不用 `RuntimeDirectory=`，避免一方重启删掉另一方的套接字） | `control.sock`（gantry，0600，仅 root 连接）、`egress.sock`（gantry-egress:gantry-link，0660，Egress 控制面，P-50） | 否 |
| `/var/backups/gantry/` | root:root | 0700 | 备份暂存 | — |
| `/run/gantry-ca-bootstrap/` | root:root | 0700（tmpfs） | **仅安装期**：根私钥只存在于此，结束即清空 | 绝不 |
| 日志 | — | — | journald；不另写明文日志文件（Egress 审计除外） | 否 |

**读取矩阵（验收依据）**：`gantry` 读不到 `/var/lib/gantry-egress/**`；`gantry-egress` 读不到 `/var/lib/gantry/**`；`gantry-sbx` 读不到 `/var/lib/gantry/{gantry.db,secrets,cas,mirrors,worktrees}`、`/var/lib/gantry-egress/**`、`/etc/gantry*`；任何非 root 用户读不到 `/run/gantry-ca-bootstrap`；全机不存在其他用户可读的私钥文件。

### 6.4 端口规划 [提议，端口号未在骨架中定死]

骨架给定：API 与配对是**两个独立监听端口**（[api.md §1.3](../interfaces/api.md#13-配对端点唯一的单向-tls-端点)）；API 端口由部署决定（示例用 8443）；webhook 只在回环地址上以明文 HTTP 监听、默认 `127.0.0.1:8787`、路径 `/webhook`（及 App 创建期间的 `/setup/github/callback`，P-65；forge-actions §11 B）；API 监听器对 `/webhook` 返回 404；沙盒与服务端之间没有任何网络端点（P-15）。

| 用途 | 协议 / 认证 | 默认绑定 | 进程 | 何时开 | 暴露面 |
| --- | --- | --- | --- | --- | --- |
| mTLS API `/v1/*` | HTTPS，TLS 1.3，强制客户端证书 | `0.0.0.0:8443` | `gantryd` | 常开 | 局域网 / VPN / 公网（按 Q-02），**绝不经隧道** |
| 配对端点 `POST /v1/pair` | HTTPS，单向 TLS + token | `0.0.0.0:8444` | `gantryd` | 仅 `gantry pair` 的有效窗口，窗口外端口关闭（api.md §1.3，OQ-12） | 与 API 同网络 |
| webhook | HTTP 明文，`POST /webhook`；`GET /setup/github/callback`（只在有未完成的 setup 会话时响应） | `127.0.0.1:8787` | `gantryd`（`forge`） | 常开 | **仅本机**；对外一律经终止 TLS 的反向代理或隧道转入（P-65） |
| webhook 公网入口（`public` 模式） | HTTPS（公共受信证书，ACME） | `0.0.0.0:443` | 反向代理（如 Caddy） | `public` 模式常开 | 公网，**只转发 `/webhook` 与 `/setup/github/callback`**，其余 404 |
| Egress 沙盒侧模型 API 反向代理 | HTTP 明文到 Egress（worker-protocol E3），路径 `/j/<job_token>/<provider>/…`（P-49） | `<EGRESS_ADDR>:8081` | `gantry-egress` | 常开 | 仅沙盒网络可达；令牌只接受来自 `egress.sandbox_cidrs` 的请求（强制方式需核实，P-73） |
| Egress 包仓库 CONNECT | HTTP `CONNECT` 白名单（`HTTPS_PROXY=http://job:<job_token>@…`，`Proxy-Authorization` 带 `job_token`，P-72） | `<EGRESS_ADDR>:3128` | `gantry-egress` | 常开 | 仅沙盒网络可达 |
| Egress 私有包仓库反向代理（M2） | HTTP，`/r/<job_token>/<injection_id>/…`（[提议]，egress-control OQ-1） | `<EGRESS_ADDR>:<分配>` | `gantry-egress` | M2 起 | 仅沙盒网络可达 |
| Egress `svc_model`（服务端 `models` 用） | HTTP，`/s/<svc_token>/<provider>/…`，请求头 `X-Gantry-Ledger`（P-50、P-74） | `127.0.0.1:8082` | `gantry-egress` | 常开 | **仅回环**；`gantry-sbx` 被主机防火墙挡住 |
| Egress 控制面 | Unix 套接字（`/control/…`） | `/run/gantry/egress.sock` | `gantry-egress` 监听，`gantryd` 连接 | 常开 | 本机，组 `gantry-link`（P-50） |
| `gantryd` 控制通道 | Unix 套接字 | `/run/gantry/control.sock` | `gantryd` | 常开 | 仅 root |
| 出站（`gantryd`） | HTTPS 443 | — | `gantryd` | — | `api.github.com`、`github.com`（拉镜像仓）；M4 增加 APNs |
| 出站（`gantry-egress`） | HTTPS 443 | — | `gantry-egress` | — | 模型提供方、包仓库白名单 |

`<EGRESS_ADDR>`、沙盒子网与"沙盒如何到达 Egress"取决于 rootless 网络模式，由 `sandbox` / `egress` 规格定（OPS-11）。**入站防火墙**：默认只放行 `8443/tcp`（来自配置的管理网段）、`443/tcp`（`public` 模式）；`8444/tcp` 的放行与监听窗口无关（端口未监听即无暴露），监听本身只在配对窗口内存在；其余入站丢弃。端口均为引导配置项（§10），改端口需重启。

## 7. 内部设计

### 7.1 安装步骤清单

`install.sh` 按序执行，每步完成后写入 `install-state.json`，失败即停，`--resume` 从失败步继续；**已存在的 CA 不会被静默重建**（重建会使所有已配对设备失效，必须显式 `--reset-ca` 并二次确认）。

> 首次部署顺序（P-65）：**先配置 webhook 入口（第 13 步），再用 App Manifest 创建 App（第 14 步）**——Manifest 要写入 `hook_attributes.url`，回调 `/setup/github/callback` 也经同一入口到达。

| # | 步骤 | 说明 / 完成判据 |
| --- | --- | --- |
| 0 | 获取并校验发布包 | `SHA256SUMS` + 签名校验通过才继续（OPS-06） |
| 1 | 预检（§7.2） | 全部通过，否则退出码 10 |
| 2 | 安装系统依赖 | `podman`、`uidmap`、`nftables`、`git`、`sqlite3`、`age`、`ca-certificates`、`curl`；M2 起 `runsc`（gVisor）；可选 `caddy`。包源与密钥校验需核实 |
| 3 | 创建用户 / 组 / subuid / linger | §6.2；已存在则校验属性而不是覆盖 |
| 4 | 创建目录与权限 | §6.3；写完立即做一次权限审计（与 §12 同一脚本） |
| 5 | 落位程序 | 解压到 `/opt/gantry/releases/<版本>`，建 `current` 链接与 `/usr/local/sbin` 链接 |
| 6 | 写引导配置 | `/etc/gantry/gantry.toml`、`/etc/gantry-egress/egress.toml`（§10），不含密钥 |
| 7 | `secrets` 初始化 | §7.4 |
| 8 | 离线根 CA 引导 | §7.3；**交互步骤**，需要口令与离线保存确认 |
| 9 | 签发服务端证书 | SAN 来自 `--api-host`；由 `pki` 签发 |
| 10 | 主机防火墙 | §7.8；装载后校验规则生效 |
| 11 | 安装并启用 systemd 单元 | §7.7；此时不开放配对端口 |
| 12 | 启动 `gantry-egress`，再启动 `gantryd` | 等待 `READY=1`（`sd_notify`）；超时即失败并打印 `journalctl` 提示 |
| 13 | 配置 webhook 入口 | §7.6；反向代理 / 隧道就绪，得到最终 `https://…/webhook`；`doctor webhook` 的外部可达检查通过 |
| 14 | `gantry github setup` 创建并安装 App | §7.5；**需要浏览器**；完成后 `doctor github` 通过 |
| 15 | 引导期写入（可选） | §7.9；只在需要覆盖默认值时（例如自建 worker 镜像） |
| 16 | 拉取沙盒基础镜像并做冒烟 | 官方 worker 镜像按发布包锁定的摘要拉取（`sandbox.base_image` 的默认值，P-66）；起一个金丝雀沙盒，验证"访问 `github.com` / `api.github.com` 失败、环境里没有密钥、读不到服务端端口"（M1 退出标准的主机侧前提） |
| 17 | `gantry doctor` | 全部通过或仅警告 |
| 18 | 备份 | 配置 `age` 收件人（公钥）与目的地，启用定时器，**立即做一次备份并校验** |
| 19 | 输出下一步 | 提示运行 `gantry pair`；打印 CA 指纹与离线保存提醒 |

### 7.2 预检

任一失败即停（退出码 10），并给出修复提示：

| 项 | 要求 |
| --- | --- |
| 身份 / 系统 | root；Ubuntu LTS（版本与架构见 OPS-05，首选验证 24.04 x86_64 **[提议]**）；systemd 为 PID 1 |
| cgroup / 命名空间 | cgroup v2；`gantry-sbx` 能创建用户命名空间（`unshare -U true`）；Ubuntu 24.04 默认的 AppArmor 对非特权用户命名空间限制是否影响 rootless Podman **需核实** |
| 时钟 | `timedatectl` 显示 NTP 已同步（证书有效期与审批签名过期都依赖时钟；design.md："两端开 NTP"）。**不同步 = 失败**，不是警告 |
| 资源 | `/var/lib` 可用 ≥ 20 GiB、内存 ≥ 8 GiB（推荐）**[建议值，待调优]**；与 `sandbox.max_concurrent_sandboxes × limits.mem_mb`（config V12）的过量提示 |
| 网络 | 能访问 `api.github.com:443`、`github.com:443`、apt 源；端口 8443 / 8444 / 8787 / 8081 / 8082 / 3128 未被占用 |
| swap | 若启用且未加密，提示"内存盘上的根私钥可能被换出到磁盘"（§7.3 的残余风险） |
| 冲突 | 已有同名用户 / 目录且属性不符 → 失败，不覆盖 |

### 7.3 离线根 CA 引导

目标：服务端上**永远不存在**根私钥的持久副本（architecture §6、§7.1 步骤 1；P-06）。`pki` 提供子命令，`deploy` 负责环境与流程：

1. 建内存盘目录 `/run/gantry-ca-bootstrap`（tmpfs，root 0700）；设 `LimitCORE=0`、`ulimit -c 0`，进程内尽量 `mlock`。
2. 提示输入两遍导出口令（不回显；自动化用 `--passphrase-file`，用后立即删除）。口令**不进**命令行参数、环境变量或日志。
3. 调 `gantry pki bootstrap-ca`：在内存盘产出 `root.key.enc`（口令加密的 PKCS#8，**[提议]** PBES2 + AES-256，OPS-03）、`root.crt`、`intermediate.key`、`intermediate.crt`、服务端 `server.key` / `server.crt`（SAN = `--api-host`）。算法、有效期归 `pki`。
4. **导出**：把 `root.key.enc`、`root.crt` 与 `FINGERPRINTS.txt`（根证书 SHA-256）写入 `--export-to` 指定的外部介质挂载点（脚本用 `findmnt` 确认它**不在**服务端系统盘上）；未指定时只在终端显示 `root.key.enc` 的 base64 和指纹，由管理员抄入密码管理器。**绝不**把导出文件留在服务端持久磁盘上。
5. **验证导出**：用同一口令在内存盘里解密导出副本，核对其公钥与 `root.crt` 一致；通过才继续。
6. **人工确认**：要求管理员键入根证书指纹前 8 位十六进制（防条件反射式回车）。`--assume-exported` 仅限 `--non-interactive`。
7. **清除**：覆盖并删除内存盘里的 `root.key*`，卸载 / 清空目录。把 `intermediate.key` 与服务端密钥交给 `secrets`（§7.4）；`root.crt`、中间证书与服务端证书链落到 `/etc/gantry/ca/` 与 `pki` 存储。
8. **断言**（失败即退出码 30）：扫描 `/etc`、`/var/lib/gantry*`、`/root`、`/tmp`、`/run` 与 `--export-to` 以外的所有可写位置中的 PEM 私钥块，推导公钥，**没有任何一个与根证书公钥匹配**；`intermediate.key` 只在 `secrets/` 中。
9. 打印：根证书指纹、"请确认离线副本已妥善保管；丢失 = 无法轮换中间 CA（M3），最坏需全体重新配对"。

残余风险（写进安装输出）：内存盘页可能被换出（swap 未加密时）；管理员终端的回滚缓冲与屏幕录制可能留下 base64 副本。中间 CA 轮换需要临时取回根私钥（M3，OPS-04）。

### 7.4 `secrets` 初始化

1. 创建 `/var/lib/gantry/secrets`（`gantry` 0700）与 `/var/lib/gantry-egress/secrets`（`gantry-egress` 0700）；两个实例各自初始化（`secrets` 是库，"各进程各自实例"，architecture §4）。
2. 登记读取方：`forge` → `github.app.private_key`、`github.app.webhook_secret`（及 `webhook_secret_previous`）；`pki` → 中间 CA 私钥；`egress` → 模型 key 与私有包仓库凭据。M4 增加 `notify` → APNs 密钥（architecture §4 目前没有该读取方，OPS-22）。注意：`forge` 与 `pki` 同在 `gantryd` 进程内，二者之间的隔离是库层面的 API 约束，**不是**操作系统隔离；操作系统隔离只存在于 `gantryd` / `gantry-egress` / `gantry-sbx` 之间。
3. 写入第一批机密：中间 CA 私钥（来自 §7.3）；App 私钥与 webhook secret（来自 §7.5，写入后立即读回校验）。
4. 静态加密的主密钥放在哪里（systemd 加密凭据 / TPM2 / 仅文件权限）未定（OPS-08）；无论哪种，**备份用的加密独立于主机主密钥**（§7.10）。
5. 自检：各读取方只能读到自己的命名空间；跨用户读取失败（§12 读取矩阵）。

### 7.5 GitHub App 创建（`gantry github setup`，P-65）

以下 GitHub 侧行为（Manifest 字段、`redirect_url` 的约束、`code` 有效期、转换端点、响应字段、App 名称唯一性、各权限与事件的兼容关系、`PATCH /app/hook/config`）**全部需核实**，实现前对照 GitHub 官方文档逐项确认（P-65 原文"细节需核实"）。

**为什么需要引导**：[forge-actions §9](../interfaces/forge-actions.md#9-github-app-权限与订阅) 已规定最小权限；手工填表容易多给权限（尤其 `Workflows`、`Administration`，§9.2 禁止；Gantry 不修改 `.github/workflows/`，P-48），且 `webhook_secret` 需要生成和保存。服务端是无头主机，Manifest 流程需要一个浏览器提交表单，并让 GitHub 的重定向回到服务端——P-65 规定重定向经**同一个 webhook 入口**的 `/setup/github/callback` 到达 `forge`，因此不需要额外的本机监听器或 SSH 端口转发。

**分工**：`deploy` 提供主机命令与浏览器侧的提交页；`forge` 持有 setup 会话、接收回调、调用转换端点、写入 `secrets/` 与 `github.*`（[forge.md §7.14](forge.md)）。

**流程**（`gantry github setup`，root，经 `control.sock` 与 `gantryd` 通信）：

1. 前提：webhook 入口已就绪（§7.1 第 13 步），外部 `https://<入口主机>/webhook` 可达；仍处于引导期（尚未配对任何设备，`pki` 的 `bootstrap_period`），否则命令要求 approver 签名授权（P-66）。
2. `forge` 登记一次性 setup 会话（随机 `state` ≥ 128 位、单次有效、[建议值] 15 分钟过期），命令据此生成 Manifest JSON：
   - `name`（用户可改；GitHub 上需唯一，冲突时重试）、`url`（主页）、`public: false`（私有 App）；
   - `hook_attributes: {url: https://<入口主机>/webhook, active: true}`；`redirect_url: https://<入口主机>/setup/github/callback`；
   - `default_permissions` / `default_events` **取自 [forge-actions §9.6](../interfaces/forge-actions.md#96-app-manifest-片段供-deploy-使用)**，M1 只带 `metadata:read`、`contents:write`、`pull_requests:write`、`issues:write`、`checks:read` 及对应事件；`actions:write`、`merge_queues:read` 与 `merge_group` 事件留到 M2 经"接受新权限"升级。**不得**出现 `workflows`（P-48）、`administration`；
   - 不请求 OAuth（`request_oauth_on_install: false`）。
3. 命令把一个自动提交的 HTML 页（把 Manifest 以字段 `manifest` POST 到 `https://github.com/settings/apps/new?state=<state>`，组织拥有者为 `/organizations/<org>/settings/apps/new`）写到终端指定的文件，或打印其内容；管理员在任意一台有浏览器的电脑上打开并提交。页面本身不含任何秘密（`state` 只在与回调配对时有意义）。
4. 用户在 GitHub 上确认创建；GitHub 重定向浏览器到 `https://<入口主机>/setup/github/callback?code=…&state=…`，经反向代理 / 隧道到达 `forge`。`forge` 校验 `state`（常量时间、单次），用 `code` 调转换端点，把 `pem`、`webhook_secret` 写入 `secrets/`（引导期写入路径）、把 `github.app.*` 与 `github.commit_identity`（App 机器人的提交身份，P-69）以 `actor = {kind:"system"}` 写入并追加审计事件（P-66），会话作废，回应一个不含密钥的完成页。`client_secret` 不使用、不保存。
5. 命令轮询会话结果并打印 App ID、slug、获批权限；用 App 私钥签 JWT 调 `GET /app` 确认获批权限与 Manifest **完全一致**（对应 forge-actions §9.5 的启动自检）。
6. 引导安装：输出 `https://github.com/apps/<slug>/installations/new`，提醒**只选需要的仓库**。安装完成后 `forge` 收到 `installation` 事件得到 `installation_id`。项目如何据此绑定（`project.added` 由谁触发）骨架未定（api.md OQ-8，OPS-13）。
7. 项目绑定时的保护检查：默认分支是否受保护 / 有 ruleset（forge-actions §9.1 要求 `deploy` 警告；所需权限需核实）。

**失败处理**：`state` 不符 / 过期 → 回调 404，重新运行；`code` 过期 → 重新运行；名称冲突 → `--name` 重试；**转换成功但落盘失败** → 凭据已丢（`code` 单次有效），必须到 App 设置里删除该 App 或重新生成私钥并重置 webhook secret 后重做，命令打印 `html_url` 与步骤，**不得**把 `pem` 打印到终端；入口不可达（回调到不了 `forge`）→ 先修复 §7.6，再重跑。

### 7.6 webhook 入口

两种形态都满足（P-65）：入口在公网侧用**公共受信证书终止 TLS**，**只转发 `POST /webhook` 与 `GET /setup/github/callback`**（后者只在 App 创建期间有意义，`forge` 在无 setup 会话时对它返回 404）；mTLS API 与配对端口**绝不**经隧道或公网反向代理暴露（API 监听器对 `/webhook` 返回 404，api.md §1.1）；`gantryd` 的 webhook 监听器本身只绑 `127.0.0.1:8787` 且为明文 HTTP。

| 形态 | 做法 | 注意 |
| --- | --- | --- |
| `tunnel`（默认，内网主机；Q-02 推荐） | `cloudflared` 命名隧道（需自备账号与域名，稳定主机名）：ingress 前两条规则按路径 `^/webhook$`、`^/setup/github/callback$` 转到 `http://127.0.0.1:8787`，兜底规则 `http_status:404`。备选：Tailscale Funnel 限定路径（`--set-path`）。**不使用 smee.io**（仅供开发，经第三方中继，P-13） | cloudflared 在边缘终止 TLS，Cloudflare 能看到 webhook 明文（私有仓库内容）；Tailscale Funnel 在本机终止 TLS。路径限定语法与选项需核实。快速隧道（随机主机名）不可用，主机名变化需更新 App 的 webhook 地址（`PATCH /app/hook/config` 或网页设置，需核实） |
| `public`（公网主机） | 反向代理（如 Caddy，ACME 自动申请公共受信证书）：只匹配 `POST /webhook` 与 `GET /setup/github/callback` → `127.0.0.1:8787`，其余 404。可选：用 `GET /meta` 的 `hooks` 网段收紧入站（forge-actions §5.2，需周期刷新；隧道场景不适用） | GitHub 不信任私有 CA，所以不能用 Gantry 自己的证书；由反向代理终止 TLS（P-65） |

验证（`gantry doctor webhook`）：① 本机对 `127.0.0.1:8787` 发送带正确 HMAC 的 `ping` → `200`，错误签名 → `401`；② 外部地址 `POST /webhook` 无签名 → `401`（到达了 `forge`）；无 setup 会话时外部 `GET /setup/github/callback` → `404`（由 `forge` 返回）；同一外部地址上 `GET /v1/me`、`POST /v1/pair` → `404`，且该 `404` 由隧道 / 反向代理返回、没有到达 `gantryd`（核对 `gantryd` 的访问日志计数未增加）；③ `GET /app/hook/deliveries` 看到最近 `installation` 投递状态为成功。

### 7.7 systemd 单元

| 单元 | 说明 |
| --- | --- |
| `gantry.target` | 聚合 `gantryd`、`gantry-egress`、`gantry-firewall`；`systemctl start gantry.target` 起整个服务 |
| `gantry-egress.service` | 用户 `gantry-egress`；先于 `gantryd`；`Restart=on-failure` |
| `gantryd.service` | 用户 `gantry`；`Wants=` + `After=` Egress（不用 `Requires=`：Egress 不可用时 `gantryd` 仍应能只读运行，沙盒 fail-closed 无网络，config §2.6） |
| `gantry-firewall.service` | oneshot，装载 §7.8 规则 |
| `gantry-backup.service` / `.timer` | root；每日一次（默认 03:17 本地时间，`RandomizedDelaySec=30min`，`Persistent=true`）**[提议]** |
| 沙盒运行时单元 | 以 `gantry-sbx` 运行 `podman system service`，套接字归 `gantry-sbxctl` 组；单元内容待 `sandbox` 规格（OPS-11） |

`gantryd.service` 骨架 **[提议]**（加固项以发行版 systemd 版本实测为准）：

```ini
[Unit]
Description=Gantry server
After=network-online.target gantry-egress.service
Wants=network-online.target gantry-egress.service
PartOf=gantry.target

[Service]
Type=notify
User=gantry
Group=gantry
ExecStart=/opt/gantry/current/bin/gantryd --config /etc/gantry/gantry.toml
Restart=on-failure
RestartSec=2s
TimeoutStopSec=60s
StateDirectory=gantry
StateDirectoryMode=0750
# /run/gantry 由 systemd-tmpfiles 创建（gantry:gantry-link 0770），与 Egress 共用，不用 RuntimeDirectory=
SupplementaryGroups=gantry-link
UMask=0077
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictNamespaces=yes
RestrictSUIDSGID=yes
LockPersonality=yes
SystemCallArchitectures=native
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
CapabilityBoundingSet=
AmbientCapabilities=
ReadWritePaths=/var/lib/gantry /run/gantry

[Install]
WantedBy=gantry.target
```

`gantry-egress.service` 同形，`User=gantry-egress`，`StateDirectory=gantry-egress`，`Group=gantry-egress` 且 `SupplementaryGroups=gantry-link`，`ReadWritePaths=/var/lib/gantry-egress /run/gantry`（只在其中创建 `egress.sock`）。`deploy/tmpfiles.d/gantry.conf`：`d /run/gantry 0770 gantry gantry-link -`。

约束：`gantryd` 不得持有 `CAP_NET_BIND_SERVICE`（所有监听端口 ≥ 1024）；`NoNewPrivileges` 意味着不能靠 `sudo` 切换到 `gantry-sbx`，沙盒控制必须走套接字（OPS-11）；日志只进 journald，`Storage=persistent` 与保留期交给主机配置，单元内不写任何密钥。`systemd-analyze security gantryd.service` 的评分作为回归指标（§12）。

### 7.8 主机防火墙与沙盒网络隔离

目标：满足 worker-protocol E1（"沙盒网络里除 Egress 外无路由"）并保证服务端监听端口对沙盒不可达（P-15、I1）。rootless 网络下沙盒的出站由 `gantry-sbx` 用户名下的网络辅助进程（`pasta` 或 `slirp4netns`）在宿主网络命名空间里发出，因此可以按**发起用户**过滤——**需核实**该行为随所选网络模式成立。

`nftables` 表 `inet gantry`（独立表，不清空用户的既有规则）**[提议]**：

```nft
table inet gantry {
  chain output {
    type filter hook output priority 0; policy accept;
    # 沙盒用户：只允许到 Egress 的沙盒侧端口，其余一律丢弃（含回环上的服务端端口、svc_model 8082、DNS）
    meta skuid "gantry-sbx" ip daddr $EGRESS_ADDR tcp dport { 8081, 3128 } accept   # M2 加私有包仓库端口
    meta skuid "gantry-sbx" counter drop
  }
  chain input {
    type filter hook input priority 0; policy accept;
    # 配对 / API 端口的来源网段限制（可选）
    tcp dport 8443 ip saddr != $ADMIN_NETS counter drop
  }
}
```

`$EGRESS_ADDR`、`$ADMIN_NETS` 由引导配置渲染；同一沙盒网段写入 Egress 的 `egress.sandbox_cidrs`，`job_token` 只接受来自这些网段的请求（P-49；rootless Podman 下 Egress 能否看到真实源地址需核实，P-73，金丝雀沙盒的用例负责核实）。该规则与 Egress 自身的硬性拒绝（config §8.3：IP 字面量、回环 / 私网 / 链路本地）是**两道**独立防线。`doctor` 的金丝雀沙盒负责验证（§7.14）。Egress 进程自己的出站（模型提供方、包仓库）不受此表限制。`gantryd` 的出站不做主机层过滤（不在骨架范围）。

### 7.9 安装引导期写入（P-66）

首次启动前没有任何设备，而 [config.md §2.5](../interfaces/config.md#25-修改权限设备角色) 规定 `github.*`、`sandbox.base_image` 是"仅 approver 键"、需设备签名。P-66 的裁决：**第一台设备配对之前**，主机命令（需 root）可以写入这些键，并写审计事件；之后改为需要审批签名。

- 判定：`pki` 的 `bootstrap_period()`（库里从未有过任何设备）；允许写的键由 `core::bootstrap_writable(resource, path)` 列出（[core.md §7.6](core.md)）。
- 通道：主机命令经 `control.sock` 提交，由 `gantryd` 按 `core` 校验后追加 `settings.updated`（`actor = {kind:"system", id:"deploy"}`，scope `global`），事件可审计、无 secret 值（config §2.3）；密钥值经 `secrets` 的引导期写入路径（secrets.md）。第一台设备配对之后，命令一律拒绝并提示改用桌面端设置页。

| resource | 引导期写入 | 来源 |
| --- | --- | --- |
| `github` | `app.app_id`、`app.private_key.secret_ref`、`app.webhook_secret.secret_ref`、`commit_identity` | `gantry github setup` 的回调由 `forge` 写入（§7.5，P-69） |
| `sandbox` | `base_image`：**默认不写**——默认值是随版本发布、固定摘要的官方 worker 镜像（P-66）；只有自建镜像时才用 `gantry settings bootstrap-set` 覆盖 | 镜像如何构建与分发归 `sandbox` / `worker` / CI（OPS-11） |
| `egress` | `default` 档案与 `model_api` 分组（M1 只要求模型 API，config §8.2） | **[提议]**，归 `egress` 规格确认；若内置默认已含则不写 |

其余键使用"内置默认随二进制"（config §2.1），不写。

### 7.10 备份

范围严格对应 [architecture §6](../architecture.md#6-状态归属) 的"备份"列：

| 备份 | 内容 |
| --- | --- |
| **需要** | SQLite 数据库（事件日志、投影、`outbox`、`pki` 表；`log_id` / `log_epoch` 在日志元数据里）；`revocations.journal` 的副本（P-67；恢复时只合并、不覆盖，§7.11）；CAS 结果库；`gantryd` 与 Egress 两个 `secrets/` 的**加密导出**；`/etc/gantry`、`/etc/gantry-egress`（无密钥的引导配置）；`install-state.json`；M4 起推送令牌表（在数据库内） |
| **不需要** | 镜像仓、工作树、工作区、PR / Issue 快照（可重建）；沙盒存储与镜像；journald；根 CA 私钥（离线保存，**从不**在服务端） |
| 待定 | Egress 出网审计日志的保留与备份（config OQ-21，OPS-10、OPS-14）；CAS 保留期（worker-protocol OQ-12） |

流程（`gantry-backup`，root；`gantry-backup.timer` 触发或手动）：

1. **数据库**：用 SQLite 在线备份接口（或 `VACUUM INTO`）取一致性快照，**不直接拷贝**运行中的 `-wal` 文件；在快照上跑 `PRAGMA integrity_check`，记录事件 `seq` 头。
2. **CAS**：**先 DB 快照、后复制 CAS**（对象内容寻址、写入先于被事件引用，所以后复制不会漏掉快照所引用的对象）。
3. **吊销日志**：复制 `/var/lib/gantry/pki/revocations.journal`（只追加文件，复制时读到的前缀总是完整行；末尾不完整的行丢弃）。此外，P-80 要求**每次写入同步复制到备份目标**：引导配置 `backup.journal_target`（备份目标上的一个可追加路径，如挂载的异地卷或备份盘，`gantry:gantry 0600`）由 `pki` 在吊销事务提交前追加并 `fsync`，复制失败则吊销失败——这样即使主机整体丢失、只能用旧备份恢复，备份之后的吊销也能从备份目标上的副本重放。安装时未配置 `backup.journal_target` → 安装失败（`doctor` 同样报红）。
4. **密钥**：`sudo -u gantry gantry secrets export` 与 `sudo -u gantry-egress gantry secrets export`，各自输出加密包（加密方式归 `secrets`，备份收件人独立于主机主密钥，OPS-08）。
5. 组装 `tar`（可压缩）+ `MANIFEST.json`（组件清单与 SHA-256、Gantry 版本、`schema_version`、事件 `seq` 头、`log_id` 与 `log_epoch`、创建时间、主机标识），整体用 `age` 公钥加密（**[提议]**，收件人公钥在安装时配置，**私钥永不放在服务端**，与根私钥同等对待）。
6. 写入 `/var/backups/gantry/`（0700），按保留策略轮转（日 7 / 周 4 / 月 6 **[建议值，待调优]**），然后执行 `backup.post_hook`（管理员配置的异地同步命令：rsync、rclone 等）。**只备份在同一块盘上不算备份**；`doctor` 检查最近一次异地同步成功。
7. 失败不静默：退出码非 0、journald 记录、`doctor backup` 报红。

RPO = 备份间隔（默认 24 h）；RTO 目标 ≤ 30 分钟（已准备好的新主机）**[建议值，待调优]**，由恢复演练度量。

### 7.11 恢复

`gantry-restore <归档> [--identity <age 私钥文件>]`，在**全新安装**（`install.sh --restore-from`，跳过 CA 引导与 App 创建，复用备份里的中间 CA 与 App 凭据）或原机上执行：

1. 用离线保存的 `age` 私钥解密；校验 `MANIFEST.json` 的各组件哈希；**拒绝**恢复到比备份 `schema_version` 更旧的程序。
2. 停服务；用 `Store::restore_from` 放回数据库（`log_epoch` 加一、`log_id` 不变，P-67）；放回 CAS、密钥、配置并设置属主 / 权限（§6.3）；`gantry secrets import`。
3. **合并吊销日志**：主机上已有的 `revocations.journal` 保留（原机恢复时它比备份新），与备份里的副本、`backup.journal_target` 上的同步副本（P-80）按行取并集（去重键 `device_id` + `revoked_at`）；**绝不**用备份副本覆盖现有文件。
4. **重放吊销**：离线调用 `pki` 的 `replay_revocations`（P-67）：日志里晚于备份、而恢复后的库里仍 `active` 的设备被重新吊销（`device.revoked`，`via = journal_replay`）。重放失败（坏行等）→ 恢复中止，不启动服务。
5. 先启 `gantry-egress`，再启 `gantryd`；`gantryd` 启动执行 `forge` 的全量对账（`startup_sweep`，forge-actions §7.1）。
6. **恢复后核对清单**（脚本逐项提示，不自动处理）：
   - **吊销**：第 4 步已重放日志里的吊销；只有当日志本身也丢失（主机整体损毁且无异地副本）时，恢复点之后的吊销才会回退——脚本此时列出设备清单让管理员核对，无法确认则视为需要轮换中间 CA 并全体重新配对。
   - 恢复点之后新配对的设备在数据库里不存在，其证书映射不到设备，需要重新配对。
   - 客户端重同步：`log_epoch` 已变化，客户端从响应头 `X-Log-Epoch` 或心跳的 `log_epoch` 发现后丢弃缓存、全量重同步（P-67）；不再依赖 `409 cursor_ahead` 检测恢复。
   - 恢复点之后在 GitHub 上发生的事（PR、评论、合并）由对账器补齐并上报 `forge.drift_detected`；Outbox 行可能丢失（forge-actions §4.4 规则 4 已预期这一点）。
   - 若主机地址变了：服务端证书 SAN、`api_base_url`、webhook 入口（反向代理 / 隧道）与 App 的 webhook 地址都要更新。
   - 收件箱里已失效的 open 项由 `notify` 的自动关闭规则在回放中清理。
7. `gantry doctor` 全量通过。

**恢复演练**（M2，之后每季度或每次大版本前）：在干净 VM 上用最近一次真实备份按上述流程走完，度量 RTO，核对：事件 `seq` 连续、`log_epoch` 加一且客户端全量重同步、备份后吊销的设备经日志重放仍被拒、已有设备（备份时存在且未吊销的）能直接以旧证书连上、对账补齐了备份后创建的 PR。演练结果记录在发布检查单里。

### 7.12 升级与回滚（M2）

布局：`/opt/gantry/releases/<版本>/` + `current` + `previous`。迁移只前进（forward-only）；**程序拒绝启动于 `schema_version` 比自己支持的更新的数据库**（防止无声的降级损坏）。

`gantry-upgrade <版本或压缩包>`：

1. 校验发布包（同 §7.1 第 0 步）；预检：磁盘、`doctor` 当前健康、版本兼容性（`gantryd --check-config` 用新程序校验现有引导配置与数据库版本）。
2. **强制备份**：`gantry-backup --tag pre-upgrade-<版本>`，记录事件 `seq` 头；备份失败则不升级。
3. 停 `gantryd`（`TimeoutStopSec` 内优雅退出）。沙盒由 `gantry-sbx` 持有，**不随之终止**；`gantryd` 重启后按 worker-protocol §3.5 重新接管。Egress 若版本不变不重启；若要重启，先确认无运行中沙盒，否则这些 Attempt 会以 `exec.model_api` 类失败重试（OPS-15）。
4. 切换 `current` → 新版本，`previous` → 旧版本；执行迁移（持排他锁）；启动 Egress（如需）与 `gantryd`，等待 `READY=1`。
5. 健康检查：`gantry doctor --quick` 在 `health_timeout`（默认 120 s **[建议值，待调优]**）内通过，且 `forge` 的 `startup_sweep` 完成。
6. 失败 → **自动回滚**：停服务；`current` 指回 `previous`；若迁移已执行（`schema_version` 变化）则用升级前快照恢复数据库——此时迁移后产生的事件会丢失，因此仅当"事件 `seq` 头仍等于升级前记录值"时自动执行，否则停下并交给管理员决定（OPS-15）。
7. 成功后保留最近 2 个版本目录，其余清理。

升级期间 webhook 不可达：GitHub 不会自动重投，丢失由 `forge` 的 L2 / L3 补齐（forge-actions §5.5），无需特殊处理，但升级窗口应尽量短。

`gantry-rollback [--restore-db]`：手动回滚，行为同第 6 步。

### 7.13 主机应急命令使用说明

适用：**唯一的 approver 设备丢失 / 被盗**，此时没有任何设备能签名吊销或改角色（P-39）。命令归 `pki`，需**主机 root**，会写审计事件（`actor.kind = system`），语法为 **[提议]**，以 `pki` 规格为准：

```bash
sudo gantry device list                                   # 列出设备：id、名称、平台、角色、状态、last_seen
sudo gantry device recover --revoke  dev_01JB…            # 吊销丢失的设备（不可逆；之后要重新配对）
sudo gantry device recover --promote dev_01JB…            # 把一台已有设备提升为 approver
```

处置顺序：

1. `device list` 核对要处理的设备 `id`（按名称 / 平台 / `last_seen` 判断，别凭印象）。
2. 若另有可信设备：先 `--promote` 它，再 `--revoke` 丢失的设备。
3. 吊销生效：该设备已建立的事件流在心跳内断开（上限 60 s，P-30），新握手被拒。
4. 复盘：在事件日志里按 `actor = dev_…` 查丢失期间该设备做过的写操作（settings、审批、改角色）。
5. 需要新设备时再 `gantry pair`。
6. 若**没有任何其他设备**：吊销后重新 `gantry pair` 的新设备是 `operator`（`pki` 按"库里从未有过设备"判定第一台，pki.md OQ-K11），需再 `--promote`；口径仍待确认，OPS-16。
7. 每次 `--revoke` 都会追加到 `revocations.journal`（P-67），之后从旧备份恢复也不会让它复活。

命令要求交互确认（键入设备名），`--yes` 仅供自动化。非 root 执行必须拒绝。

**轮换运行手册（M3，由 `sec` 泳道的功能支撑）**：webhook secret 按 [config.md §10](../interfaces/config.md#10-github) 的四步窗口轮换；App 私钥利用 GitHub 允许多把私钥并存先生成新钥、写入 `secrets/`（`SecretRef.value`，approver 签名）、验证后再删旧钥（App 至少保留一把，需核实）；中间 CA：临时取回离线根私钥到内存盘、签发新中间、重新签发服务端证书、清除根私钥；客户端固定的是离线根 CA 指纹，链为 `[叶子, 中间 CA, 根]`，已配对设备无需重新配对，下次续期即拿到新中间 CA 签发的证书（P-64）；模型 key：设置页写入新值。

### 7.14 `gantry doctor`

输出 `ok` / `warn` / `fail`，`--json` 供自动化，退出码 `0` / `1`（有警告）/ `2`（有失败）**[提议]**。分区：

| 区域 | 检查 |
| --- | --- |
| `host` | 用户与目录权限符合 §6.3（读取矩阵）；监听端口集合恰好等于预期（`ss -ltn`）；单元 `active`；NTP 同步；磁盘与内存；`systemd-analyze security` 评分 |
| `pki` | 根私钥不存在于服务端（§7.3 第 8 步同一断言）；证书剩余有效期 > 30 天、SAN 与 `api_base_url` 一致；配对端口当前关闭（无配对窗口时）；`revocations.journal` 存在、属主 / 权限正确、最近一次异地同步成功；库与日志一致（无未生效的吊销） |
| `github` | JWT 调 `GET /app` 成功；权限集合等于 Manifest（无 `workflows` / `administration`，P-48）；`github.commit_identity` 已写入（P-69）；已安装的仓库清单；默认分支保护检查（需核实）；最近 webhook 投递状态 |
| `webhook` | §7.6 的三项验证 |
| `egress` | 监听器就绪；控制面 `/run/gantry/egress.sock` 可连（`EgressControl::status`）且 `gantry-sbx` 不可连；模型 key `present`（`secret_present`，不读值）；金丝雀沙盒：`github.com` / `api.github.com` 不可达、服务端 8443 / 8444 / 8787 与 Egress `svc_model` 8082 不可达、环境变量里除本尝试的 `job_token`（只出现在模型 base URL 与 `HTTPS_PROXY` 里）外没有任何密钥形字符串；尝试结束后该令牌被拒 |
| `backup` | 最近一次备份龄 < 2 × 间隔；异地同步成功；收件人公钥已配置 |
| `version` | `gantryd` / `gantry-egress` / CLI 版本一致，数据库 `schema_version` 受支持 |

`--test-mode` 的安装在 `host` 区域标 `fail`（防止测试开关留在生产）。

### 7.15 CI 与安装测试（M0 起）

- **静态**（每个 PR）：`shellcheck deploy/**/*.sh`；`systemd-analyze verify deploy/systemd/*`；`nft -c -f` 校验规则模板；模板渲染快照测试；仓库密钥扫描（AGENTS.md："不在 PR 里提交密钥、证书、`.env`"）；M0 要求的 Rust / schema / 前端检查。
- **VM 级安装冒烟**（M1 起，夜间 + 发布前）：在带 systemd 的干净 Ubuntu 镜像上跑 `install.sh --non-interactive --test-mode`，GitHub 侧用 `FakeGitHubManifest`（本模块自带的假服务：实现 Manifest 表单接收并重定向到 `/setup/github/callback`、`/app-manifests/{code}/conversions`、`GET /app`、`/app/hook/deliveries`；可基于 M0 的 HTTP 级 `FakeGitHub` 扩展，P-51）；随后跑 §12 的主机侧检查。
- **沙盒隔离冒烟**：需要嵌套 / 裸金属 runner 才能起 rootless Podman + `runsc`；不具备时该组测试标 `manual-required` 并在发布检查单里手动执行，**不得静默跳过**。

## 8. 不变式与安全要求

| 不变式 | 本模块如何保证 |
| --- | --- |
| I1 沙盒只计算 | 沙盒运行时是独立系统用户 `gantry-sbx`，读不到 `gantryd` / Egress 的任何文件；主机防火墙把该用户出站限制为只能到 Egress 的沙盒侧端口（§7.8）；服务端所有监听端口与 Egress 的 `svc_model` 回环监听对沙盒不可达；控制套接字（`control.sock`、`egress.sock`）永不挂入沙盒；沙盒内唯一的令牌是本尝试的 `job_token`，只能给本尝试记账（P-49）；M1 退出标准"沙盒内访问 `github.com`、`api.github.com` 失败、读不到任何模型 key"在 §12 有主机侧测试 |
| I2 GitHub 写入唯一路径 | 安装期对 GitHub 的写（创建 App）由管理员在浏览器里亲自确认，转换与落盘由 `forge` 在回调中完成；之后所有写只经 Outbox。`deploy` 不持有也不使用 installation token；引导期写入不是 GitHub 写 |
| I3 放行唯一决策点 | `gantry device recover` 是主机 root 对"无人能签"的受控例外（P-39），写审计事件；引导期写入只在第一台设备配对之前、只限 `core::bootstrap_writable` 列出的键，并写审计事件（§7.9，P-66） |
| I4 状态各有事实源 | 备份只含本地事实源（事件日志等），镜像仓与 GitHub 快照不备份、靠对账重建；吊销另有日志外的 `revocations.journal`，恢复时合并并重放（P-67）；恢复后明确列出与 GitHub 的差异处理（§7.11） |
| I5 外部文本不可信 | Manifest 转换响应按外部数据校验形状；备份档先验完整性再恢复；脚本不 `eval` 任何外部文本；日志不含响应体 |
| I6 客户端不持凭据 | 安装不向任何客户端分发密钥；`pem`、`webhook_secret` 只进 `secrets/`；根私钥只在内存盘与离线介质 |
| I7 配置与工作分离 | 引导配置（端口、路径、入口形态）是文件配置、重启生效、不进事件日志、不能经 API 改（config §1.1）；其余运行期设置只经 settings，唯一例外是 §7.9 的引导期写入（同样产生 `settings.updated`，受审计） |
| I8 先验后推 | 与本模块无直接关系；`git` 以 `gantry` 用户在服务端运行，`gantry-sbx` 无权写镜像仓 |

补充要求：

- 脚本 `set -Eeuo pipefail`，所有外部输入加引号；不写 `curl … | sh` 到未校验内容；临时文件用 `mktemp` 且 0600；任何含机密的变量不 `export`、不进 `ps`。
- 机密永不出现在：命令行参数、环境变量、journald、`install-state.json`、备份的明文部分、shell 历史（口令输入用 `read -s`）。
- 发布产物校验失败 = 安装中止，没有"忽略校验"开关。
- 两个密钥存储实例互相不可读；`gantry-sbx` 与二者都不可读。
- Contents:write 的高风险（forge-actions §9.1）由"默认分支保护"缓解，`doctor` 对未保护的默认分支警告，但不替用户改 GitHub 设置。

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 预检失败 | 退出码 10，逐项打印原因与修复建议；不改动系统 |
| 安装中途失败 / 被中断 | `install-state.json` 记录进度；`--resume` 继续；各步骤幂等；CA 引导失败后根私钥内存盘由 `trap` 清除 |
| 根私钥导出验证失败 | 不删除内存盘里的私钥，不继续，提示重试；超时则销毁并要求整步重做（此时尚无已签发的设备证书，安全） |
| 管理员未确认离线保存 | 不进入后续步骤（避免"根私钥已删但没人保存"） |
| Manifest 转换成功但落盘失败 | 凭据已丢；打印 `html_url` 与删除 / 重建步骤，不打印 `pem`（§7.5） |
| `gantryd` 启动超时 | 打印 `journalctl -u gantryd` 尾部；不自动重试安装 |
| Egress 不可用 | `gantryd` 照常运行，沙盒因无出网而失败闭合（config §2.6）；`doctor egress` 报红 |
| 时钟不同步 | 预检失败；运行期 `doctor` 警告并提示证书 / 审批签名可能异常 |
| 备份失败 | 退出码非 0 + journald；`doctor backup` 报红；不覆盖上一份有效备份 |
| 备份档损坏 / 版本不符 | 恢复前校验失败即停，不触碰现有数据 |
| 恢复时 `revocations.journal` 重放失败 | 恢复中止、不启动服务；修复日志（或确认放弃该段）后重跑，`Pki::open` 也会拒绝在有未生效吊销时启动（P-67） |
| 引导期结束后仍尝试主机写入"仅 approver"键 | 命令拒绝，提示改用桌面端设置页并签名（P-66） |
| `/setup/github/callback` 到不了 `forge` | `gantry github setup` 超时；按 §7.6 的验证排查入口后重跑 |
| 升级健康检查失败 | 自动回滚（§7.12）；回滚需丢事件时停下交人 |
| 数据库版本新于程序 | 程序拒绝启动并说明原因 |
| 磁盘将满 | `doctor` 警告；备份与升级预检拒绝执行 |
| 应急命令在非 root 下执行 | 拒绝并提示用 `sudo` |

## 10. 配置

**引导配置**（文件，重启生效，`deploy` 拥有，config.md §1.1；forge-actions §11 B/C 的 `forge.*` 同属此类）。格式 TOML **[提议]**，`/etc/gantry/gantry.toml`：

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `server.data_dir` | `/var/lib/gantry` | 数据目录 |
| `server.secrets_dir` | `/var/lib/gantry/secrets` | 密钥存储 |
| `server.api.listen` | `0.0.0.0:8443` | mTLS API 监听 |
| `server.api.public_url` | 无（必填） | 写入 `api_base_url` |
| `server.api.san` | 来自 `--api-host` | 服务端证书 SAN |
| `server.pair.listen` | `0.0.0.0:8444` | 配对监听，仅配对窗口内开启 |
| `server.control_socket` | `/run/gantry/control.sock` | 本机控制通道 |
| `forge.webhook.listen` | `127.0.0.1:8787` | forge-actions §11 B |
| `forge.webhook.ingress` | `tunnel` | `tunnel` / `public`，仅用于 `doctor` 与文档，不影响 `forge` 行为 |
| `forge.webhook.public_url` | 无（必填） | 写入 App Manifest（`hook_attributes.url`；`redirect_url` 取同一主机的 `/setup/github/callback`） |
| `forge.webhook.max_body_bytes` / `ip_allowlist` / `dedupe_retention` / `payload_retention` | `26214400` / `false` / `30d` / `7d` | forge-actions §11 B |
| `sandbox.runtime_user` / `sandbox.runtime_socket` | `gantry-sbx` / `/run/gantry-sbx/podman.sock` | 待 `sandbox` 规格（OPS-11） |
| `egress.control_socket` | `/run/gantry/egress.sock` | `gantryd` ↔ Egress 控制面（P-50；与 egress.md §10 同名） |
| `backup.dest` / `backup.recipients` / `backup.post_hook` / `backup.journal_target` | `/var/backups/gantry` / 无（必填） / 空 / 无（必填） | §7.10；`journal_target` 是 `revocations.journal` 的同步副本路径，`pki` 每次吊销写入时同步追加（P-80） |
| `backup.keep_daily/weekly/monthly` | 7 / 4 / 6 | **[建议值，待调优]** |
| `upgrade.health_timeout_s` | 120 | **[建议值，待调优]** |

`/etc/gantry-egress/egress.toml`（键名以 [egress.md §10](egress.md) 为准）：`egress.listen.sandbox_model`（`<EGRESS_ADDR>:8081`）、`egress.listen.sandbox_connect`（`<EGRESS_ADDR>:3128`）、`egress.listen.sandbox_registry`（M2）、`egress.listen.svc_model`（`127.0.0.1:8082`）、`egress.control_socket`（`/run/gantry/egress.sock`）、`egress.sandbox_cidrs`（与 §7.8 的沙盒网段一致）、`secrets_dir`、`audit_dir`。

**不是引导配置的**：模型、档位、策略、守护、沙盒限额、出网白名单、通知——全部在 settings（I7），`deploy` 不提供它们的文件形式。

## 11. 可观测性

- **事件**：安装期不产生事件（`gantryd` 尚未运行）；步骤进度写 `install-state.json`。§7.5 / §7.9 的引导期写入产生 `settings.updated`（`actor.kind = system`，P-66）；`pki` 的日志重放产生 `device.revoked`（`via = journal_replay`）；`gantry device recover` 产生 `device.revoked` / `device.updated`（`actor.kind = system`，归 `pki`）。**不新增事件类型**。
- **日志**：安装 / 升级 / 备份脚本同时写 stdout 与 journald（`SyslogIdentifier=gantry-install` 等）；永不记录机密、Manifest 响应体、口令；`doctor --json` 可被外部监控采集。
- **指标**：`deploy` 不引入指标进程。备份与升级结果以退出码、journald 与 `doctor backup` / `doctor version` 呈现；如主机已有 node_exporter，可用 textfile collector 暴露 `gantry_backup_last_success_timestamp`、`gantry_backup_last_size_bytes`、`gantry_install_version` **[提议]**。
- **审计**：应急命令、引导期写入、升级 / 回滚、恢复均记入 journald，并在可行处写事件日志（`actor.kind = system`）。

## 12. 测试与验收

测试在带 systemd 的干净 Ubuntu VM 上执行（§7.15）；以下每条都能写成一个自动化或半自动化测试。标 **(M1)** / **(M2)** 为首次要求的里程碑。

**一键安装与幂等 (M1)**

- [ ] 在干净 Ubuntu LTS 上执行 `install.sh`（带 `--api-host`、`--webhook-mode`、`--webhook-url`），除"口令输入 / 离线保存确认 / 浏览器创建 App"三处人工步骤外无需其他操作（隧道账号与域名、`age` 收件人公钥为前置条件），最终 `gantry doctor` 返回 `0`，且输出下一步 `gantry pair`。
- [ ] `--non-interactive --test-mode` 在 CI VM 上无人值守跑通，使用 `FakeGitHubManifest`。
- [ ] 中途杀掉安装进程，`--resume` 从中断步继续并成功；重复运行完整安装不改变任何已有状态（对 `/etc`、`/opt`、`/var/lib` 做前后哈希 / 权限比较）。
- [ ] 预检：关闭 NTP、缺少 cgroup v2、端口被占、不支持的系统版本，各自让安装以退出码 10 失败且不改动系统。
- [ ] 发布包校验：篡改压缩包或 `SHA256SUMS` → 安装中止。

**用户、目录与权限 (M1)**

- [ ] 三个系统用户 / 组存在，`nologin`，无密码，`gantry-sbx` 具备 `subuid` / `subgid` 与 linger。
- [ ] 权限审计脚本逐行核对 §6.3 的所有者、组、模式（table-driven）。
- [ ] 读取矩阵：`gantry` 读 `/var/lib/gantry-egress/secrets/*` 失败；`gantry-egress` 读 `/var/lib/gantry/secrets/*` 失败；`gantry-sbx` 读 `gantry.db`、`secrets`、`cas`、`mirrors`、`/etc/gantry*`、`/var/lib/gantry-egress` 全部失败。
- [ ] 全盘没有其他用户可读的私钥文件；`/etc/gantry/gantry.toml`、`egress.toml` 里没有任何形似密钥的字符串。

**离线根 CA (M1)**

- [ ] 安装完成后，按 §7.3 第 8 步扫描：服务端所有可写位置没有与根证书公钥匹配的私钥；`/run/gantry-ca-bootstrap` 不存在；服务器上没有 `root.key*` 文件。
- [ ] 导出介质上的 `root.key.enc` 用口令解密后公钥与 `root.crt` 一致；错误口令解密失败。
- [ ] 未键入正确的指纹前 8 位，流程不进入删除与后续步骤；导出验证失败时根私钥未被删除且流程中止。
- [ ] 中间证书由根签发，服务端证书链验证通过，SAN 含每个 `--api-host`；口令不出现在 `ps`、journald、shell 历史、`install-state.json`。
- [ ] 重跑安装不会重建 CA（CA 指纹不变）；`--reset-ca` 需二次确认。

**systemd、端口、防火墙 (M1)**

- [ ] `systemd-analyze verify` 通过；`systemctl start gantry.target` 后 `gantryd` 与 `gantry-egress` 为 `active`，`gantryd` 报 `READY=1`；杀掉任一进程被自动拉起。
- [ ] `ss -ltnp` 的监听集合恰好等于 §6.4（无配对窗口时 8444 **不**监听；webhook 只在 `127.0.0.1:8787`；`svc_model` 只在 `127.0.0.1:8082`；Egress 沙盒侧监听器不在公网地址上；没有 8788 之类的临时监听）。
- [ ] `/run/gantry/egress.sock` 为 `gantry-egress:gantry-link 0660`，`gantry` 可连、`gantry-sbx` 不可连；重启任一单元后另一方的套接字仍在。
- [ ] `gantry pair` 后 8444 监听，token 被消耗或窗口结束后端口关闭。
- [ ] 无客户端证书访问 8443 → TLS 握手失败、无 HTTP 响应；8443 上请求 `/webhook` 需持有效证书且返回 `404`；8444 上请求 `/v1/me` → `404`。
- [ ] 金丝雀沙盒内：连接 `github.com`、`api.github.com`、宿主 8443 / 8444 / 8787 / 8082 / 控制套接字全部失败；`env` 与文件系统中除本尝试的 `job_token` 外没有任何密钥形字符串；用该令牌从沙盒网段以外请求 Egress → 拒绝（P-49；需核实，P-73：核实不可强制时断言 Egress `status.detail` 含 `source_check: unverified` 并记录结论）。
- [ ] 沙盒用户对 Egress 端口的连接成功、对其他任何地址失败（nft 计数器可见丢弃）。
- [ ] `systemd-analyze security gantryd.service` 评分不劣于记录的基线。

**GitHub App 创建 (M1)**

- [ ] 顺序：webhook 入口未就绪时 `gantry github setup` 拒绝执行（P-65）；第一台设备配对之后，无 approver 签名授权时拒绝执行（P-66）。
- [ ] 假服务下：经入口的 `/setup/github/callback` 完成创建；`state` 不符 → `404`；回调重放 → 第二次 `404`；无 setup 会话时外部访问 `/setup/github/callback` → `404`；`github.commit_identity` 与 `github.app.*` 写入且事件 `actor.kind = system`。
- [ ] 生成的 Manifest 的权限与事件集合等于 forge-actions §9.6 的 M1 子集，**不含** `workflows`、`administration`、`actions`、`merge_queues`、`merge_group`；M2 的 Manifest 含 `actions:write`、`merge_queues:read`、`merge_group`。
- [ ] 转换响应的 `pem` / `webhook_secret` 写入 `secrets/` 后读回一致、权限 0600 / 0700；在 `ps`、journald、`install-state.json`、命令行、shell 历史中全部搜不到（用哨兵值断言）。
- [ ] 模拟"转换成功但落盘失败" → 脚本不打印 `pem`，打印 `html_url` 与重做步骤。
- [ ] 真实 GitHub 的手工验收（发布前）：端到端创建私有 App、只装到一个测试仓库，`doctor github` 通过，`forge` 启动自检无高危权限告警；所有标"需核实"的 Manifest 行为在此逐项确认并回写本规格。

**webhook 入口 (M1，隧道与公网两种形态，C28)**

- [ ] `tunnel`：经隧道主机名 `POST /webhook` 到达 `forge`（无签名 → `401`）；`GET /setup/github/callback` 到达 `forge`（无会话 → `404`）；`GET /v1/me`、`POST /v1/pair`、任意其他路径 → 非 Gantry 响应（隧道 404）；直接访问隧道主机名无法触达 8443 / 8444。
- [ ] `public`：反向代理只转发 `POST /webhook` 与 `GET /setup/github/callback`，其余 `404`；证书为公共受信；可选 IP 白名单生效时非 GitHub 来源被拒。
- [ ] 本机带正确 / 错误 HMAC 的 `ping` → `200` / `401`。配置里不存在 smee 相关项。

**引导期写入 (M1，P-66)**

- [ ] 第一台设备配对之前：`gantry settings bootstrap-set sandbox base_image=…@sha256:…` 成功，`settings.updated` 存在且 `actor.kind = system`，`value` 不含 secret；不在 `core::bootstrap_writable` 列表里的键被拒；未写 `base_image` 时生效值为官方 worker 镜像的固定摘要。
- [ ] 第一台设备配对之后：同一命令被拒（即使吊销了全部设备）；`GET /v1/settings/status` 显示相关模块已应用。

**备份 (M1) 与恢复演练 (M2)**

- [ ] 在持续写入负载下备份：快照通过 `PRAGMA integrity_check`，事件 `seq` 连续；归档含 `MANIFEST.json`，哈希全部吻合。
- [ ] 归档明文部分（哨兵扫描）不含任何密钥值；未用 `age` 私钥无法解密；服务端上不存在 `age` 私钥。
- [ ] 范围：归档**含**数据库（含 `log_id` / `log_epoch`）、`revocations.journal` 副本、CAS、两个 `secrets` 导出、引导配置；**不含**镜像仓、工作树、工作区、根私钥。
- [ ] 每次吊销后 `backup.journal_target` 与本地 `revocations.journal` 逐行相同；把 `journal_target` 设为不可写 → 吊销失败（`503`），设备仍 `active`（P-80）。
- [ ] 轮转保留数量正确；`post_hook` 被调用且失败时备份判失败；定时器 `Persistent=true` 在关机期间错过的备份会补做。
- [ ] **(M2) 恢复演练**：在第二台干净 VM 上，用最近的真实备份 + 离线 `age` 私钥 + `backup.journal_target` 上的日志副本（P-80）恢复，服务全部启动；事件 `seq` 与备份时一致；`log_epoch` 比备份时大 1，客户端发现后全量重同步（P-67）；备份后被吊销的设备经日志重放仍握手失败；备份时已存在且未吊销的设备用旧证书可直接连上；`forge` 对账补齐备份后在 GitHub 上创建的 PR 并报告差异。度量并记录 RTO（目标 ≤ 30 分钟）。
- [ ] 原机恢复：主机上现有的 `revocations.journal` 比备份新，恢复后文件行数不减少（合并而非覆盖）。
- [ ] 恢复前校验：篡改归档、版本更旧的程序、缺组件，各自被拒绝且现有数据未改动。

**升级与回滚 (M2)**

- [ ] 从版本 N 升到 N+1：升级前备份存在；服务重启；`doctor --quick` 通过；运行中的沙盒不被终止，`gantryd` 重启后重新接管。
- [ ] 注入失败的迁移 / 健康检查 → 自动回滚到 N，数据库恢复到升级前快照，服务恢复可用；回滚后 `seq` 头等于升级前记录值。
- [ ] 迁移后已产生新事件再失败 → 不自动恢复数据库，停下并提示。
- [ ] 把旧程序指向新 schema 的数据库 → 拒绝启动并给出原因。
- [ ] 升级窗口内投递的 webhook 在重启后被 `forge` 补齐（L2 / L3），不重复处理。

**应急命令与 `doctor` (M1)**

- [ ] 非 root 执行 `gantry device recover` 被拒；root 执行 `--revoke` 后该设备握手被拒、已建立的事件流 ≤ 60 s 断开，事件日志含 `actor.kind = system` 的审计事件，`revocations.journal` 多一行；`--promote` 后该设备具备 approver 能力。
- [ ] `gantry doctor` 在故意破坏（改权限、停 Egress、关 NTP、放开 nft、让根私钥副本出现在服务端）的每种情形下都对应报 `fail`；`--test-mode` 安装被标 `fail`。

**CI (M0)**

- [ ] CI 跑通 Rust fmt / clippy / test、schema 校验、前端 lint / typecheck（M0 退出标准的 ops 部分）。
- [ ] `shellcheck`、`systemd-analyze verify`、`nft -c`、模板渲染、密钥扫描作为必过门禁。

## 13. 开放问题

骨架没定、本规格给了临时处理的一律在此；实现者不得自行拍板，需要时开 Issue。

| # | 问题 | 本文的临时处理 | 影响 |
| --- | --- | --- | --- |
| OPS-01 | **已决：见裁决 C19 / P-65**（首次部署先配置 webhook 入口，再用 App Manifest 创建 App；architecture §7.1 按此修正）。原问题：architecture 把创建 App 排在配置入口之前，且 Manifest 不是普通链接 | 已落实到 §7.1、§7.5 | — |
| OPS-02 | （回调通道**已决：见裁决 C19 / P-65**——`gantry github setup` 生成一次性提交页，经同一入口的 `/setup/github/callback` 接收 `code`）仍未决：App 拥有者是个人还是组织、跨多个拥有者的仓库是否需要多个 App、App 名称冲突处理；GitHub 侧细节需核实 | 单 App、拥有者由 `--app-owner` 指定；全部 GitHub 细节需核实（M1 `ops` spike） | `forge` |
| OPS-03 | （指纹口径**已决：见裁决 C18 / P-64**——固定离线根 CA 指纹，链 `[叶子, 中间 CA, 根]`）仍未决：导出的口令加密格式、介质形态（U 盘 / 密码管理器 / 打印） | PKCS#8 PBES2 加密 PEM；以 `pki` 规格为准 | `pki` |
| OPS-04 | （设备证书续期路由**已决：见裁决 C18 / P-64**，`POST /v1/devices/self/renew`，M1）仍未决：中间 CA 轮换（M3）临时取回离线根私钥的操作细节；根私钥丢失的恢复策略（全体重新配对） | §7.13 的运行手册草案 | `pki`、`sec` |
| OPS-05 | 支持的 Ubuntu 版本与 CPU 架构（x86_64 / aarch64）；Ubuntu 24.04 对非特权用户命名空间的 AppArmor 限制与 rootless Podman、`runsc` 的兼容性；apt 源与 gVisor 安装方式（**需核实**） | 首选验证 24.04 x86_64，其余待定 | `sandbox`、CI |
| OPS-06 | 发布与分发：压缩包 + `SHA256SUMS` + 签名（工具？minisign / cosign / GPG）；是否同时提供 `.deb`；离线安装；一行 `curl … \| sudo bash` 引导是否提供 | 压缩包 + 校验和 + 签名，方案待定；不提供对未校验内容的管道执行 | CI、发布流程 |
| OPS-07 | `gantry` CLI 的位置（`crates/gantryd` 的子命令还是独立 crate）、`github setup` / `doctor` 的 Rust 实现落在哪个 crate；本机控制通道（`control.sock`）的鉴权与协议，`pair` / `device recover` 是走套接字还是直接操作停机状态下的数据库 | 控制套接字 root-only；归属如 §6.1 | `pki`、`gantryd`、AGENTS.md 仓库布局 |
| OPS-08 | `secrets` 静态加密的主密钥放哪里（systemd 加密凭据 / TPM2 / 仅文件权限）——只靠文件权限时，防不住同一用户被攻破，只能防磁盘镜像与备份泄露；加密备份的收件人密钥如何托管（`age` 私钥与根私钥同等离线保存？） | 备份用 `age` 收件人公钥，私钥离线；主密钥方案待 `secrets` 规格 | `secrets`、`deploy` |
| OPS-09 | **已决：见裁决 C19 / P-65**（webhook 监听只在回环地址以明文 HTTP 提供，对公网一律经终止 TLS 的反向代理或隧道）。原问题：公网形态由谁终止 TLS | 已落实到 §6.4、§7.6；`deploy` 提供 Caddy 与 cloudflared 模板 | — |
| OPS-10 | （通道**已决：见裁决 C4 / P-50**——控制面 `/run/gantry/egress.sock`，secret 只写不读，`models` 走回环 `svc_model` + `svc_token`）仍未决：Egress 审计日志的位置、保留与备份（config OQ-21） | 审计日志落 `/var/lib/gantry-egress/audit` | `egress` |
| OPS-11 | 沙盒主机侧细节：`gantryd` 如何控制 `gantry-sbx` 的 rootless Podman（套接字 vs `sudo`，后者与 `NoNewPrivileges` 冲突）；`workspaces/` 的属主 / ACL；rootless 网络模式（`pasta` / `slirp4netns`）与"沙盒如何到达 Egress"、`skuid` 过滤是否成立（需核实）；基础镜像如何构建、签名、分发（`sandbox.base_image` 的默认值**已决：见裁决 C20 / P-66**——随版本发布、固定摘要的官方 worker 镜像） | §6.3、§7.8、§7.9 的草案 | `sandbox`、`worker`、`egress`、CI |
| OPS-12 | **已决：见裁决 C20 / P-66**（第一台设备配对之前，主机命令可写入"仅 approver"配置并写审计事件；之后需要审批签名）。原问题：安装期没有设备时如何写入"仅 approver 键" | 已落实到 §7.9 | — |
| OPS-13 | 项目绑定：App 装好后谁创建 `Project`、`project.added` 由什么触发（api.md OQ-8：安装回调 / API）；默认分支保护检查所需权限与端点（需核实）；`installation_id`、`bot_user_id` 初始化（forge-actions OQ-10） | 安装后由 `forge` 处理 `installation` 事件；`deploy` 只做检查与警告 | `forge`、`api` |
| OPS-14 | （恢复后吊销回退**已决：见裁决 C21 / P-67**——`revocations.journal` 只追加、恢复时重放；`log_epoch` 加一，客户端全量重同步）仍未决：备份频率与保留；异地目的地；CAS 保留期；出网审计日志是否入备份（`revocations.journal` 的同步副本已决：见 decisions.md P-80，`backup.journal_target` 必填） | 见 §7.10、§7.11 | `store` |
| OPS-15 | 升级策略：数据库迁移是否允许 N-1 兼容、`schema_version` 与 settings 的迁移（config OQ-20）；Egress 重启对进行中 Attempt 的影响；自动回滚的判据与"丢事件"的容忍；维护窗口 / 排空语义 | 迁移只前进；失败且无新事件才自动恢复数据库 | `store`、`egress`、`engine` |
| OPS-16 | 应急命令：唯一 approver 被吊销且无其他设备时，下一台配对设备能否成为 approver（"第一台"的判定口径）；命令的实现方式与参数语法（`pki` 规格定） | 语法如 §7.13，口径待定 | `pki` |
| OPS-17 | 配对窗口：监听端口是否固定 8444、窗口时长与 token TTL（design.md 示例 10 分钟；config OQ-18 未定）、配对串里的"地址"指 API 还是配对监听器（api.md OQ-12；固定根指纹已决，P-64） | 固定 8444，时长由 `pki` 定 | `pki`、桌面 / 手机端 |
| OPS-18 | 隧道软件的安装与维护是否纳入 `deploy` 的脚本范围（还是只给模板与验证）；Q-02（公网 / 内网）未决，两种形态都要支持；Q-03（worker 用 API key 还是订阅登录）若选订阅登录，会引入需要在沙盒外保管的登录态，影响 `secrets` 与备份范围 | 只给模板与验证；按 API key 假设 | 产品、`worker` |
| OPS-19 | `gantry doctor` 中 M1 与 M2 的检查集合划分；`--test-mode` 的暴露方式 | §7.14 全量，M1 取子集 | `ops` |
| OPS-20 | 单主机以外（多主机、HA）不在本版范围；卸载默认保留数据，`--purge` 的确认方式 | 单主机 | — |
| OPS-21 | Manifest / 权限随里程碑升级（M1 → M2 的 `actions:write`、`merge_queues:read`）由谁提示用户到 GitHub 上"接受新权限"（forge-actions §9.5 上报）；`deploy` 是否提供 `gantry github upgrade-permissions` 引导 | 仅文档说明 | `forge` |
| OPS-22 | （APNs 归 M4 `ops`、`secrets` 读取方加 `notify`：**已决：见裁决 C17 / P-63、C13 / P-59**）推送令牌注册路由**已决：见 decisions.md P-77**；仍未决：APNs 密钥与参数的设置键、"运营者自备 Apple 开发者账号直连还是经中继"（见 [notify.md N-11](notify.md#13-开放问题)）；`gantryd` 到 APNs 的出站放行 | 仅在部署侧预留放行与密钥登记位置 | `notify`、`secrets`、`mobile` |
