# deploy：安装脚本、systemd 单元、防火墙与 webhook 入口模板、备份脚本

规格见 [docs/modules/deploy.md](../docs/modules/deploy.md)（以下简称“规格”）。本目录是 `ops` 泳道 M1 的交付（issue #104），当前是**骨架**：脚本与模板齐备、本机替身测试（A 级）全绿；依赖 `gantry` CLI 的步骤只编排调用，命令本身由对应模块实现（见下文「与 gantry CLI 的接线」）。实机幂等执行归 M1.4 的 Ubuntu 24.04 虚拟机（B 级，#106）。

## 目录

| 路径 | 内容 | 规格 |
| --- | --- | --- |
| `install.sh` | 一键安装 / 续跑，0–19 步，状态写 `/etc/gantry/install-state.json` | §6.1、§7.1、§7.2 |
| `lib/common.sh` | 日志、模板渲染（`@KEY@` 占位符，值走白名单字符集）、TOML 取值、安装状态文件 | §8 补充要求 |
| `lib/layout.sh` | 目录布局与权限表；创建与权限审计共用同一张表 | §6.3 |
| `lib/rotate.sh` | 备份轮转（日 7 / 周 4 / 月 6） | §7.10 第 6 步 |
| `bin/gantry-backup` | 一致性快照 + `age` 加密归档 + 异地同步钩子 + 轮转 | §7.10 |
| `systemd/` | `gantry.target`、`gantryd.service`、`gantry-egress.service`（及其加固）、`gantry-firewall.service`、`gantry-backup.service` / `.timer` | §7.7 |
| `tmpfiles.d/gantry.conf` | `/run/gantry`（`gantry:gantry-link 0770`），两个服务共用，都不用 `RuntimeDirectory=` | §6.3 |
| `nftables/gantry.nft.in` | 独立表 `inet gantry`：沙盒用户只能到 Egress 沙盒侧端口；8443 只收管理网段 | §7.8 |
| `config/*.toml.in` | `/etc/gantry/gantry.toml`、`/etc/gantry-egress/egress.toml` 模板（无密钥） | §10 |
| `webhook/Caddyfile.in` | `public` 形态：Caddy 终止 TLS，只转发 `POST /webhook` 与 `GET /setup/github/callback` | §7.6，P-65 |
| `webhook/cloudflared.yml.in` | `tunnel` 形态：cloudflared 命名隧道，同样只转发两条路径，兜底 404；不用 smee.io | §7.6，P-13、P-65 |
| `tests/` | 本机检查入口 `run.sh`、用例、快照、替身 | §7.15、§12 |

## 本机检查

```bash
deploy/tests/run.sh
```

`run.sh` 先跑静态检查（`shellcheck`、`systemd-analyze verify`、`nft -c`、`caddy adapt`），再跑 `tests/test_*.sh`。工具缺失时打印 `SKIP` 与原因，不静默通过；在 Linux 环境设 `DEPLOY_STRICT=1`，工具缺失即失败。改了模板后用 `UPDATE_GOLDEN=1 bash deploy/tests/test_render.sh` 更新快照，并在 PR 里说明差异。

macOS 上没有 systemd 与 nftables，`systemd-analyze verify` 与 `nft -c` 会被跳过，单元约束改由 `test_units.sh` 的结构检查兜底；这两项在 M1.4 的虚拟机上补跑（#106）。

本机替身覆盖的规格 §12 条目（A 级）：模板渲染快照、渲染值注入防护、配置无密钥形字符串、webhook 只监听回环、两种入口模板只转发两条路径且不暴露 API / 配对端口、单元加固项与依赖关系、`/run/gantry` 共用、防火墙表独立且幂等、布局表与权限审计、参数校验与退出码 10、步骤顺序（入口先于 App，P-65）、续跑与“重跑不再执行已完成步骤”、预检检查器（系统版本、端口、NTP、磁盘、同名用户）、备份范围 / `MANIFEST.json` 哈希 / 明文无哨兵 / 错误私钥无法解密 / 吊销日志不完整行丢弃 / `post_hook` 失败即失败且不删上一份 / 带 tag 的归档不参与轮转 / 轮转数量。

需要 root、systemd、真实 GitHub 或沙盒的条目（读取矩阵、`ss -ltnp` 监听集合、金丝雀沙盒、`systemd-analyze security` 基线、两次幂等执行、定时器补做等）归 M1.4。

## 与 gantry CLI 的接线

`install.sh` 与 `gantry-backup` 调用下面这些子命令。命令名与参数是 deploy 侧的**约定草案**，以 `gantryd`（#101）、`pki`、`secrets`、`store` 的实现为准；任何一边改名，两边一起改。

| 调用 | 谁实现 | 调用方 | 约定 |
| --- | --- | --- | --- |
| `gantry secrets init --dir <dir>` | `secrets` | `install.sh` 第 7 步，分别以 `gantry`、`gantry-egress` 身份执行 | 幂等：已初始化则校验后返回 0 |
| `gantry secrets export --dir <dir> --out -` | `secrets` | `gantry-backup`，以对应实例的用户执行 | stdout 输出加密包；收件人独立于主机主密钥（OPS-08） |
| `gantry pki bootstrap-ca --workdir <tmpfs> --ca-dir /etc/gantry/ca --san <h>… --passphrase-fd 3 [--export-to <dir>] [--assume-exported]` | `pki` | `install.sh` 第 8 步 | 口令只从 fd 3 读；完成规格 §7.3 第 3–8 步（导出、验证导出、键入指纹前 8 位、清除、断言）；产出 `/etc/gantry/ca/{root,intermediate,server}.crt`、`FINGERPRINTS.txt`，中间 CA 与服务端私钥交给 `secrets` |
| `gantry github setup --webhook-url <url> --app-owner user\|org:<名> [--name <名>] [--test-mode]` | `deploy`（CLI）+ `forge`（回调） | `install.sh` 第 14 步 | 经 `control.sock` 让 `forge` 登记 setup 会话，生成 Manifest 提交页，轮询结果；成功返回 0，否则非 0（install.sh 映射为退出码 40） |
| `gantry doctor [webhook\|github\|egress\|…] [--json]` | `deploy`（Rust 子命令） | `install.sh` 第 13、14、16、17 步 | 退出码 `0` / `1`（有警告）/ `2`（有失败） |
| `gantry store info --db <快照> --json` | `store` | `gantry-backup` | 输出 `{"schema_version","log_id","log_epoch","seq_head"}`，只读打开快照文件，不碰运行中的库 |
| `gantryd --config <file>`、`gantry-egress --config <file>` | `gantryd`、`egress` | systemd 单元 | `Type=notify`，就绪后发 `READY=1` |

## 安装参数里尚未定的值

- `--egress-addr`、`--sandbox-cidrs` 没有默认值：沙盒到 Egress 的路径取决于 rootless 网络模式，规格标为 OPS-11，未定之前由管理员显式给出。
- 发布包只校验 `SHA256SUMS`，签名方案未定（OPS-06）；没有“忽略校验”开关。
- 入口软件（Caddy / cloudflared）由管理员安装与维护，脚本只渲染模板并做可达检查（OPS-18）。
- `nftables` 的 `destroy table` 需要 nftables ≥ 1.0.8（Ubuntu 24.04 为 1.0.9，需在 M1.4 虚拟机上核实）。
