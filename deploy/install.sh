#!/usr/bin/env bash
# Gantry 服务端安装脚本（deploy.md §7.1）。幂等、可续跑：每步完成后写入
# /etc/gantry/install-state.json，失败即停，`--resume` 从失败步继续。
#
# 用法见 `install.sh --help`。退出码（deploy.md §6.1）：0 成功；10 预检失败；20 用户中止；
# 30 CA 引导失败；40 App 引导失败；50 服务未就绪；60 自检失败；其余 1。
#
# 依赖 gantry CLI 的步骤（secrets、pki、github、doctor）只编排调用，命令本身归对应模块；
# 接线约定见 deploy/README.md「与 gantry CLI 的接线」。
set -Eeuo pipefail

DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
. "$DEPLOY_DIR/lib/common.sh"
# shellcheck source=lib/layout.sh
. "$DEPLOY_DIR/lib/layout.sh"

# shellcheck disable=SC2034 # 由 lib/common.sh 的 log() 使用
GANTRY_LOG_TAG=gantry-install

readonly EXIT_PREFLIGHT=10 EXIT_ABORTED=20 EXIT_CA=30 EXIT_APP=40 EXIT_NOT_READY=50 EXIT_DOCTOR=60

# 固定端口（deploy.md §6.4 [提议]）。改端口需同时改引导配置并重启。
readonly API_PORT=8443 PAIR_PORT=8444 WEBHOOK_PORT=8787
readonly EGRESS_MODEL_PORT=8081 EGRESS_SVC_PORT=8082 EGRESS_CONNECT_PORT=3128

readonly STATE_FILE=/etc/gantry/install-state.json
readonly CA_BOOTSTRAP_DIR=/run/gantry-ca-bootstrap
readonly OPT_DIR=/opt/gantry

# 步骤顺序即执行顺序（deploy.md §7.1 的 0–19）。
readonly STEPS="verify_release preflight packages users dirs release config secrets_init ca_bootstrap server_cert firewall units start webhook_ingress github_app bootstrap_writes sandbox_smoke doctor backup next_steps"

# 系统依赖（§7.1 第 2 步）。runsc 随 M2，caddy / cloudflared 按入口形态由管理员自装（OPS-18）。
readonly APT_PACKAGES="podman uidmap passt nftables git sqlite3 age jq openssl ca-certificates curl"

# ---- 参数 ----
API_HOSTS=""
WEBHOOK_MODE=tunnel
WEBHOOK_URL=""
EXPORT_TO=""
APP_OWNER=user
APP_NAME=""
NON_INTERACTIVE=0
RESUME=0
ONLY=""
TEST_MODE=0
RESET_CA=0
ASSUME_EXPORTED=0
PASSPHRASE_FILE=""
RELEASE=""
ADMIN_NETS=""
EGRESS_ADDR=""
SANDBOX_CIDRS=""
BACKUP_RECIPIENTS=""
BACKUP_JOURNAL_TARGET=""
BACKUP_POST_HOOK=""
TUNNEL_ID=""

usage() {
  cat <<'EOF'
用法：install.sh [参数]

必填：
  --release <目录或 .tar.gz>     发布包（同目录须有 SHA256SUMS）
  --api-host <名字或 IP>         服务端证书 SAN 与 api_base_url；可重复，第一个作为 public_url 主机
  --webhook-url <https://…/webhook>  公网可达的 webhook 地址（tunnel 模式为隧道的稳定主机名）
  --admin-nets <网段,…>          允许访问 mTLS API（8443）的来源网段，逗号分隔
  --egress-addr <IPv4>           沙盒侧 Egress 监听地址（取决于 rootless 网络模式，OPS-11）
  --sandbox-cidrs <网段,…>       沙盒网段，写入 egress.sandbox_cidrs（P-49）
  --backup-recipient <age 公钥>  备份收件人；可重复；私钥永不放在服务端
  --backup-journal-target <路径> revocations.journal 的同步副本（备份目标上的可追加路径，P-80）

可选：
  --webhook-mode tunnel|public   webhook 入口形态（默认 tunnel）
  --tunnel-id <id>               tunnel 模式：cloudflared 命名隧道 ID（渲染模板用）
  --backup-post-hook <命令路径>  备份完成后的异地同步命令
  --export-to <目录>             根私钥（加密）导出位置，须是已挂载的外部介质；缺省仅屏幕显示
  --app-owner user|org:<名>      GitHub App 的拥有者（默认 user）
  --app-name <名称>              GitHub App 名称
  --non-interactive              无人值守；口令走 --passphrase-file，确认走 --assume-exported
  --passphrase-file <文件>       根私钥导出口令（用后删除）
  --assume-exported              仅 --non-interactive：跳过“已离线保存”的人工确认
  --resume                       从上次失败的步骤继续
  --only <步骤>                  只执行某一步（步骤名见下）
  --reset-ca                     重建 CA（使所有已配对设备失效），需二次确认
  --test-mode                    仅测试：允许假 GitHub；doctor 会对此标红
  -h, --help                     显示本帮助

步骤：
EOF
  local i=0 s
  for s in $STEPS; do
    printf '  %2d %s\n' "$i" "$s"
    i=$((i + 1))
  done
}

append_csv() {
  if [ -z "$1" ]; then printf '%s' "$2"; else printf '%s,%s' "$1" "$2"; fi
}

need_value() {
  [ "$#" -ge 2 ] && [ -n "$2" ] || die 1 "参数 $1 需要一个值"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --release) need_value "$@"; RELEASE="$2"; shift 2 ;;
      --api-host) need_value "$@"; API_HOSTS="$(append_csv "$API_HOSTS" "$2")"; shift 2 ;;
      --webhook-mode) need_value "$@"; WEBHOOK_MODE="$2"; shift 2 ;;
      --webhook-url) need_value "$@"; WEBHOOK_URL="$2"; shift 2 ;;
      --tunnel-id) need_value "$@"; TUNNEL_ID="$2"; shift 2 ;;
      --admin-nets) need_value "$@"; ADMIN_NETS="$2"; shift 2 ;;
      --egress-addr) need_value "$@"; EGRESS_ADDR="$2"; shift 2 ;;
      --sandbox-cidrs) need_value "$@"; SANDBOX_CIDRS="$2"; shift 2 ;;
      --backup-recipient) need_value "$@"; BACKUP_RECIPIENTS="$(append_csv "$BACKUP_RECIPIENTS" "$2")"; shift 2 ;;
      --backup-journal-target) need_value "$@"; BACKUP_JOURNAL_TARGET="$2"; shift 2 ;;
      --backup-post-hook) need_value "$@"; BACKUP_POST_HOOK="$2"; shift 2 ;;
      --export-to) need_value "$@"; EXPORT_TO="$2"; shift 2 ;;
      --app-owner) need_value "$@"; APP_OWNER="$2"; shift 2 ;;
      --app-name) need_value "$@"; APP_NAME="$2"; shift 2 ;;
      --passphrase-file) need_value "$@"; PASSPHRASE_FILE="$2"; shift 2 ;;
      --only) need_value "$@"; ONLY="$2"; shift 2 ;;
      --non-interactive) NON_INTERACTIVE=1; shift ;;
      --assume-exported) ASSUME_EXPORTED=1; shift ;;
      --resume) RESUME=1; shift ;;
      --reset-ca) RESET_CA=1; shift ;;
      --test-mode) TEST_MODE=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) die 1 "未知参数：$1（见 --help）" ;;
    esac
  done
}

# ---- 参数校验（纯函数，可在本机测试）----

valid_host() {
  case "$1" in
    '' | -* | *[!A-Za-z0-9.:-]*) return 1 ;;
    *) return 0 ;;
  esac
}

valid_ipv4() {
  printf '%s\n' "$1" | grep -Eq '^([0-9]{1,3}\.){3}[0-9]{1,3}$' || return 1
  local IFS=. o
  # shellcheck disable=SC2086
  set -- $1
  for o in "$@"; do [ "$o" -le 255 ] || return 1; done
}

valid_cidr() {
  local ip="${1%/*}" bits="${1#*/}"
  [ "$ip" != "$1" ] || return 1
  valid_ipv4 "$ip" || return 1
  case "$bits" in '' | *[!0-9]*) return 1 ;; esac
  [ "$bits" -le 32 ]
}

valid_cidr_list() {
  local item IFS=,
  [ -n "$1" ] || return 1
  for item in $1; do valid_cidr "$item" || return 1; done
}

# webhook_host <https://主机[:端口]/webhook>：校验并输出主机部分。
webhook_host() {
  local url="$1" rest host
  case "$url" in https://*/webhook) ;; *) return 1 ;; esac
  rest="${url#https://}"
  host="${rest%/webhook}"
  case "$host" in */* | '') return 1 ;; esac
  valid_host "$host" || return 1
  printf '%s' "$host"
}

valid_age_recipient() {
  printf '%s\n' "$1" | grep -Eq '^age1[0-9a-z]{58}$'
}

validate_args() {
  local h r IFS
  [ -n "$RELEASE" ] || die "$EXIT_PREFLIGHT" "缺少 --release"
  [ -n "$API_HOSTS" ] || die "$EXIT_PREFLIGHT" "缺少 --api-host"
  IFS=,
  for h in $API_HOSTS; do valid_host "$h" || die "$EXIT_PREFLIGHT" "--api-host 不合法：$h"; done
  for r in $BACKUP_RECIPIENTS; do valid_age_recipient "$r" || die "$EXIT_PREFLIGHT" "--backup-recipient 不是 age X25519 公钥"; done
  unset IFS
  case "$WEBHOOK_MODE" in tunnel | public) ;; *) die "$EXIT_PREFLIGHT" "--webhook-mode 只能是 tunnel 或 public" ;; esac
  webhook_host "$WEBHOOK_URL" >/dev/null || die "$EXIT_PREFLIGHT" "--webhook-url 须形如 https://<主机>/webhook"
  if [ "$WEBHOOK_MODE" = tunnel ] && [ -n "$TUNNEL_ID" ]; then
    case "$TUNNEL_ID" in *[!A-Za-z0-9-]*) die "$EXIT_PREFLIGHT" "--tunnel-id 不合法" ;; esac
  fi
  valid_cidr_list "$ADMIN_NETS" || die "$EXIT_PREFLIGHT" "--admin-nets 须是逗号分隔的 IPv4 网段"
  valid_ipv4 "$EGRESS_ADDR" || die "$EXIT_PREFLIGHT" "--egress-addr 须是 IPv4 地址"
  valid_cidr_list "$SANDBOX_CIDRS" || die "$EXIT_PREFLIGHT" "--sandbox-cidrs 须是逗号分隔的 IPv4 网段"
  [ -n "$BACKUP_RECIPIENTS" ] || die "$EXIT_PREFLIGHT" "缺少 --backup-recipient"
  case "$BACKUP_JOURNAL_TARGET" in /*) ;; *) die "$EXIT_PREFLIGHT" "--backup-journal-target 须是绝对路径（P-80，必填）" ;; esac
  render_value_ok "$BACKUP_JOURNAL_TARGET" || die "$EXIT_PREFLIGHT" "--backup-journal-target 含不允许的字符"
  if [ -n "$BACKUP_POST_HOOK" ]; then
    case "$BACKUP_POST_HOOK" in /*) ;; *) die "$EXIT_PREFLIGHT" "--backup-post-hook 须是绝对路径" ;; esac
    render_value_ok "$BACKUP_POST_HOOK" || die "$EXIT_PREFLIGHT" "--backup-post-hook 含不允许的字符"
  fi
  case "$APP_OWNER" in user | org:?*) ;; *) die "$EXIT_PREFLIGHT" "--app-owner 只能是 user 或 org:<名>" ;; esac
  if [ "$ASSUME_EXPORTED" = 1 ] && [ "$NON_INTERACTIVE" != 1 ]; then
    die "$EXIT_PREFLIGHT" "--assume-exported 只能与 --non-interactive 一起用"
  fi
  if [ "$NON_INTERACTIVE" = 1 ] && [ -z "$PASSPHRASE_FILE" ]; then
    die "$EXIT_PREFLIGHT" "--non-interactive 需要 --passphrase-file"
  fi
  if [ -n "$ONLY" ]; then
    step_index "$ONLY" >/dev/null || die 1 "未知步骤：$ONLY"
  fi
}

step_index() {
  local i=0 s
  for s in $STEPS; do
    if [ "$s" = "$1" ]; then printf '%s' "$i"; return 0; fi
    i=$((i + 1))
  done
  return 1
}

# steps_to_run <状态文件>：按 --only / --resume / 全量 决定要执行的步骤。
# 全量重跑时已完成的步骤一律跳过（重复安装不改变已有状态，§12）；--only 总是执行指定步骤。
steps_to_run() {
  local state="$1" s
  if [ -n "$ONLY" ]; then
    printf '%s\n' "$ONLY"
    return 0
  fi
  for s in $STEPS; do
    if state_has_step "$state" "$s"; then
      continue
    fi
    printf '%s\n' "$s"
  done
}

# ---- 预检检查器（纯函数：输入是文件或命令输出，便于本机测试）----

# check_os_release <os-release 文件>：只支持 Ubuntu 24.04（OPS-05，首选验证版本）。
check_os_release() {
  local id version
  id="$(sed -n 's/^ID=//p' "$1" | tr -d '"')"
  version="$(sed -n 's/^VERSION_ID=//p' "$1" | tr -d '"')"
  [ "$id" = ubuntu ] && [ "$version" = 24.04 ]
}

# check_ports_free <ss -ltnH 输出>：要用到的端口都没被占用；占用的端口打印到 stdout。
check_ports_free() {
  local busy="" p
  for p in $API_PORT $PAIR_PORT $WEBHOOK_PORT $EGRESS_MODEL_PORT $EGRESS_SVC_PORT $EGRESS_CONNECT_PORT; do
    if printf '%s\n' "$1" | awk '{print $4}' | grep -Eq "[:.]${p}\$"; then
      busy="$busy $p"
    fi
  done
  [ -z "$busy" ] || { printf '%s\n' "${busy# }"; return 1; }
}

# check_ntp <timedatectl show 输出>：NTP 已同步。不同步 = 失败（§7.2）。
check_ntp() {
  printf '%s\n' "$1" | grep -qx 'NTPSynchronized=yes'
}

# check_disk_gib <df -Pk 输出的可用 KiB> <最低 GiB>
check_disk_gib() {
  [ "$1" -ge $(($2 * 1024 * 1024)) ]
}

# check_user_conflict <getent passwd 行或空> <期望家目录>：已存在的同名用户须是 nologin 且家目录一致。
check_user_conflict() {
  local entry="$1" home="$2" shell actual_home
  [ -n "$entry" ] || return 0
  actual_home="$(printf '%s' "$entry" | cut -d: -f6)"
  shell="$(printf '%s' "$entry" | cut -d: -f7)"
  case "$shell" in */nologin | */false) ;; *) return 1 ;; esac
  [ "$actual_home" = "$home" ]
}

# ---- 步骤 ----

step_verify_release() {
  local dir sums
  if [ -d "$RELEASE" ]; then dir="$RELEASE"; else dir="$(dirname "$RELEASE")"; fi
  sums="$dir/SHA256SUMS"
  [ -f "$sums" ] || die "$EXIT_PREFLIGHT" "发布包旁没有 SHA256SUMS：$sums"
  # 发布签名方案未定（OPS-06）；定下来之前只校验 SHA256SUMS，且没有“忽略校验”开关。
  (cd "$dir" && sha256sum --check --strict --quiet SHA256SUMS) ||
    die "$EXIT_PREFLIGHT" "发布包校验失败（SHA256SUMS 不符），安装中止"
  warn "发布签名校验尚未实现（OPS-06），当前只校验了 SHA256SUMS"
}

step_preflight() {
  local failed=0 out avail entry
  [ "$(id -u)" = 0 ] || { error "须以 root 运行"; failed=1; }
  check_os_release /etc/os-release || { error "只支持 Ubuntu 24.04（OPS-05）"; failed=1; }
  [ -d /run/systemd/system ] || { error "systemd 不是 PID 1"; failed=1; }
  [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" = cgroup2fs ] || { error "需要 cgroup v2"; failed=1; }
  if ! runuser -u nobody -- unshare -U true 2>/dev/null; then
    error "非特权用户无法创建用户命名空间（检查 AppArmor 的 userns 限制，需核实，OPS-05）"
    failed=1
  fi
  out="$(timedatectl show 2>/dev/null || true)"
  check_ntp "$out" || { error "NTP 未同步（证书与审批签名依赖时钟）"; failed=1; }
  avail="$(df -Pk /var/lib | awk 'NR==2 {print $4}')"
  check_disk_gib "$avail" 20 || { error "/var/lib 可用空间不足 20 GiB"; failed=1; }
  if [ "$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)" -lt $((8 * 1024 * 1024)) ]; then
    warn "内存少于推荐的 8 GiB"
  fi
  if ! out="$(check_ports_free "$(ss -ltnH)")"; then
    error "端口已被占用：$out"
    failed=1
  fi
  local url
  for url in https://api.github.com https://github.com; do
    curl -sS --max-time 10 -o /dev/null "$url" || { error "无法访问 $url"; failed=1; }
  done
  if [ -n "$(swapon --noheadings 2>/dev/null)" ]; then
    warn "已启用 swap：内存盘上的根私钥可能被换出到磁盘（若 swap 未加密）"
  fi
  local spec user home
  for spec in gantry:/var/lib/gantry gantry-egress:/var/lib/gantry-egress gantry-sbx:/var/lib/gantry-sbx; do
    user="${spec%%:*}"
    home="${spec#*:}"
    entry="$(getent passwd "$user" || true)"
    check_user_conflict "$entry" "$home" || { error "已有用户 $user 的属性与预期不符，不覆盖"; failed=1; }
  done
  [ "$failed" = 0 ] || die "$EXIT_PREFLIGHT" "预检失败，系统未改动"
}

step_packages() {
  DEBIAN_FRONTEND=noninteractive apt-get update -q
  # shellcheck disable=SC2086
  DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends $APT_PACKAGES
}

ensure_group() {
  getent group "$1" >/dev/null || groupadd --system "$1"
}

ensure_user() {
  local user="$1" home="$2"
  ensure_group "$user"
  if ! getent passwd "$user" >/dev/null; then
    useradd --system --gid "$user" --home-dir "$home" --no-create-home --shell /usr/sbin/nologin "$user"
  fi
  passwd --lock "$user" >/dev/null
}

ensure_member() {
  id -nG "$1" | tr ' ' '\n' | grep -qx "$2" || usermod --append --groups "$2" "$1"
}

ensure_subids() {
  local user="$1" file
  for file in /etc/subuid /etc/subgid; do
    if ! grep -q "^${user}:" "$file" 2>/dev/null; then
      if [ "$file" = /etc/subuid ]; then
        usermod --add-subuids "$(next_subid_start "$file")-$(($(next_subid_start "$file") + 65535))" "$user"
      else
        usermod --add-subgids "$(next_subid_start "$file")-$(($(next_subid_start "$file") + 65535))" "$user"
      fi
    fi
  done
}

# next_subid_start <subuid 文件>：已有区间之后的第一个 65536 对齐起点（至少 100000）。
next_subid_start() {
  awk -F: 'BEGIN {max = 100000} { end = $2 + $3; if (end > max) max = end } END { print int((max + 65535) / 65536) * 65536 }' "$1" 2>/dev/null || printf '100000'
}

step_users() {
  ensure_user gantry /var/lib/gantry
  ensure_user gantry-egress /var/lib/gantry-egress
  ensure_user gantry-sbx /var/lib/gantry-sbx
  ensure_group gantry-link
  ensure_group gantry-sbxctl
  ensure_member gantry gantry-link
  ensure_member gantry-egress gantry-link
  ensure_member gantry gantry-sbxctl
  ensure_subids gantry-sbx
  loginctl enable-linger gantry-sbx
}

step_dirs() {
  umask 0077
  layout_apply ""
  systemd-tmpfiles --create "$DEPLOY_DIR/tmpfiles.d/gantry.conf"
  local report
  if ! report="$(layout_audit "")"; then
    die 1 "权限审计未通过：$report"
  fi
}

release_version() {
  local dir="$1"
  [ -f "$dir/VERSION" ] || die 1 "发布包缺少 VERSION 文件"
  local v
  v="$(tr -d '[:space:]' <"$dir/VERSION")"
  case "$v" in '' | *[!A-Za-z0-9._+-]*) die 1 "VERSION 不合法" ;; esac
  printf '%s' "$v"
}

step_release() {
  local src="$RELEASE" tmp version dest bin
  if [ ! -d "$src" ]; then
    tmp="$(mktemp -d)"
    tar -xzf "$src" -C "$tmp" --no-same-owner
    src="$tmp"
  fi
  version="$(release_version "$src")"
  dest="$OPT_DIR/releases/$version"
  if [ ! -d "$dest" ]; then
    install -d -o root -g root -m 0755 "$OPT_DIR/releases"
    cp -R "$src" "$dest.partial"
    chown -R root:root "$dest.partial"
    chmod -R go-w "$dest.partial"
    mv "$dest.partial" "$dest"
  fi
  ln -sfn "$dest" "$OPT_DIR/current"
  for bin in gantryd gantry gantry-egress; do
    ln -sfn "$OPT_DIR/current/bin/$bin" "/usr/local/sbin/$bin"
  done
  ln -sfn "$OPT_DIR/current/deploy/bin/gantry-backup" /usr/local/sbin/gantry-backup
  [ -z "${tmp:-}" ] || rm -rf "$tmp"
}

current_version() {
  if [ -f "$OPT_DIR/current/VERSION" ]; then
    release_version "$OPT_DIR/current"
  else
    printf 'unknown'
  fi
}

# render_args：所有模板共用的占位符取值。
render_args() {
  local first_host webhook_host_value
  first_host="${API_HOSTS%%,*}"
  webhook_host_value="$(webhook_host "$WEBHOOK_URL")"
  printf '%s\n' \
    "API_PORT=$API_PORT" \
    "PAIR_PORT=$PAIR_PORT" \
    "WEBHOOK_PORT=$WEBHOOK_PORT" \
    "EGRESS_MODEL_PORT=$EGRESS_MODEL_PORT" \
    "EGRESS_SVC_PORT=$EGRESS_SVC_PORT" \
    "EGRESS_CONNECT_PORT=$EGRESS_CONNECT_PORT" \
    "API_PUBLIC_URL=https://${first_host}:${API_PORT}" \
    "API_SAN=$API_HOSTS" \
    "WEBHOOK_MODE=$WEBHOOK_MODE" \
    "WEBHOOK_URL=$WEBHOOK_URL" \
    "WEBHOOK_HOST=$webhook_host_value" \
    "TUNNEL_ID=${TUNNEL_ID:-REPLACE-WITH-TUNNEL-ID}" \
    "ADMIN_NETS=$(printf '%s' "$ADMIN_NETS" | sed 's/,/, /g')" \
    "EGRESS_ADDR=$EGRESS_ADDR" \
    "SANDBOX_CIDRS=$SANDBOX_CIDRS" \
    "BACKUP_RECIPIENTS=$BACKUP_RECIPIENTS" \
    "BACKUP_POST_HOOK=$BACKUP_POST_HOOK" \
    "BACKUP_JOURNAL_TARGET=$BACKUP_JOURNAL_TARGET"
}

# render_to <模板> <目标> <所有者:组> <模式>：目标不存在则写入；存在且内容相同则不动；
# 存在但不同（管理员改过）则保留原文件，另写 <目标>.new 并警告，不静默覆盖。
render_to() {
  local template="$1" target="$2" owner="$3" mode="$4" tmp args=()
  local line
  while IFS= read -r line; do args+=("$line"); done <<EOF
$(render_args)
EOF
  tmp="$(mktemp)"
  render_template "$template" "${args[@]}" >"$tmp"
  if [ ! -f "$target" ]; then
    install -o "${owner%%:*}" -g "${owner#*:}" -m "$mode" "$tmp" "$target"
  elif ! cmp -s "$tmp" "$target"; then
    install -o "${owner%%:*}" -g "${owner#*:}" -m "$mode" "$tmp" "$target.new"
    warn "$target 与渲染结果不同，已保留原文件，新内容写到 $target.new"
  fi
  rm -f "$tmp"
}

step_config() {
  render_to "$DEPLOY_DIR/config/gantry.toml.in" /etc/gantry/gantry.toml root:gantry 0640
  render_to "$DEPLOY_DIR/config/egress.toml.in" /etc/gantry-egress/egress.toml root:gantry-egress 0640
  render_to "$DEPLOY_DIR/nftables/gantry.nft.in" /etc/gantry/gantry.nft root:root 0600
  install -d -o root -g root -m 0755 /usr/local/share/gantry/webhook
  case "$WEBHOOK_MODE" in
    public) render_to "$DEPLOY_DIR/webhook/Caddyfile.in" /usr/local/share/gantry/webhook/gantry.caddyfile root:root 0644 ;;
    tunnel) render_to "$DEPLOY_DIR/webhook/cloudflared.yml.in" /usr/local/share/gantry/webhook/cloudflared.yml root:root 0644 ;;
  esac
}

step_secrets_init() {
  # 两个实例各自初始化（architecture §4）；命令归 secrets（幂等：已初始化则校验）。
  runuser -u gantry -- gantry secrets init --dir /var/lib/gantry/secrets
  runuser -u gantry-egress -- gantry secrets init --dir /var/lib/gantry-egress/secrets
}

ca_cleanup() {
  if [ -d "$CA_BOOTSTRAP_DIR" ]; then
    find "$CA_BOOTSTRAP_DIR" -type f -exec shred -u {} + 2>/dev/null || true
    umount "$CA_BOOTSTRAP_DIR" 2>/dev/null || true
    rmdir "$CA_BOOTSTRAP_DIR" 2>/dev/null || true
  fi
}

# read_passphrase：口令只经 fd 传给 pki，不进命令行参数、环境变量或日志（§7.3 第 2 步）。
read_passphrase() {
  if [ -n "$PASSPHRASE_FILE" ]; then
    cat "$PASSPHRASE_FILE"
    shred -u "$PASSPHRASE_FILE" 2>/dev/null || rm -f "$PASSPHRASE_FILE"
    return 0
  fi
  local p1 p2
  read -r -s -p "根私钥导出口令：" p1 </dev/tty
  printf '\n' >/dev/tty
  read -r -s -p "再输入一遍：" p2 </dev/tty
  printf '\n' >/dev/tty
  [ -n "$p1" ] && [ "$p1" = "$p2" ] || die "$EXIT_CA" "两次口令不一致或为空"
  printf '%s' "$p1"
}

step_ca_bootstrap() {
  if [ -f /etc/gantry/ca/root.crt ] && [ "$RESET_CA" != 1 ]; then
    info "CA 已存在，不重建（重建需 --reset-ca）"
    return 0
  fi
  if [ "$RESET_CA" = 1 ]; then
    [ "$NON_INTERACTIVE" != 1 ] || die "$EXIT_CA" "--reset-ca 不能无人值守执行"
    local answer
    read -r -p "重建 CA 会使所有已配对设备失效。键入 RESET-CA 确认：" answer </dev/tty
    [ "$answer" = RESET-CA ] || die "$EXIT_ABORTED" "已中止"
  fi
  if [ -n "$EXPORT_TO" ]; then
    # 导出位置必须是已挂载的外部介质，不能在服务端系统盘上。
    local mnt
    mnt="$(findmnt -n -o TARGET --target "$EXPORT_TO")" || die "$EXIT_CA" "--export-to 不是挂载点"
    [ "$mnt" != / ] || die "$EXIT_CA" "--export-to 在系统盘上，拒绝导出根私钥"
  fi
  trap ca_cleanup EXIT
  ulimit -c 0
  install -d -o root -g root -m 0700 "$CA_BOOTSTRAP_DIR"
  mountpoint -q "$CA_BOOTSTRAP_DIR" || mount -t tmpfs -o size=16m,mode=0700,noexec,nosuid,nodev tmpfs "$CA_BOOTSTRAP_DIR"

  local -a args=(pki bootstrap-ca --workdir "$CA_BOOTSTRAP_DIR" --ca-dir /etc/gantry/ca --passphrase-fd 3)
  local IFS=, h
  for h in $API_HOSTS; do args+=(--san "$h"); done
  unset IFS
  [ -z "$EXPORT_TO" ] || args+=(--export-to "$EXPORT_TO")
  [ "$ASSUME_EXPORTED" != 1 ] || args+=(--assume-exported)
  # 口令经 fd 3 传入；导出、验证导出、人工确认指纹、清除与断言都由 pki 子命令完成（§7.3 第 3–8 步）。
  gantry "${args[@]}" 3< <(read_passphrase) || die "$EXIT_CA" "CA 引导失败"
  ca_cleanup
  trap - EXIT
  [ ! -e "$CA_BOOTSTRAP_DIR" ] || die "$EXIT_CA" "$CA_BOOTSTRAP_DIR 未清除"
}

step_server_cert() {
  # 服务端证书由 bootstrap-ca 一并签发；这里只核对 SAN 覆盖每个 --api-host。
  local h IFS=,
  [ -f /etc/gantry/ca/server.crt ] || die "$EXIT_CA" "缺少服务端证书"
  for h in $API_HOSTS; do
    openssl x509 -in /etc/gantry/ca/server.crt -noout -checkhost "$h" 2>/dev/null | grep -q 'does match' ||
      openssl x509 -in /etc/gantry/ca/server.crt -noout -checkip "$h" 2>/dev/null | grep -q 'does match' ||
      die "$EXIT_CA" "服务端证书 SAN 不含 $h"
  done
}

step_firewall() {
  nft -c -f /etc/gantry/gantry.nft || die 1 "防火墙规则校验失败"
}

step_units() {
  local unit
  for unit in gantry.target gantryd.service gantry-egress.service gantry-firewall.service gantry-backup.service gantry-backup.timer; do
    install -o root -g root -m 0644 "$DEPLOY_DIR/systemd/$unit" "/etc/systemd/system/$unit"
  done
  install -o root -g root -m 0644 "$DEPLOY_DIR/tmpfiles.d/gantry.conf" /etc/tmpfiles.d/gantry.conf
  systemd-analyze verify /etc/systemd/system/gantry*.service /etc/systemd/system/gantry*.timer /etc/systemd/system/gantry.target
  systemctl daemon-reload
  systemctl enable gantry.target gantry-firewall.service gantry-egress.service gantryd.service
}

step_start() {
  # 先防火墙，再 Egress，再 gantryd；Type=notify，start 返回即已收到 READY=1。
  local unit
  for unit in gantry-firewall.service gantry-egress.service gantryd.service; do
    if ! systemctl start "$unit"; then
      journalctl -u "$unit" -n 50 --no-pager >&2 || true
      die "$EXIT_NOT_READY" "$unit 未就绪，见上方 journalctl 输出"
    fi
  done
}

# 单区域 doctor：0 通过，1 只警告，≥2 失败。未实现区域在 M1 期间返回 1，不能当失败中止。
doctor_area() {
  local area="$1" code="$2" message="$3" rc=0
  gantry doctor "$area" || rc=$?
  case "$rc" in
    0) ;;
    1) warn "doctor $area 有警告" ;;
    3) warn "未检查区域：${area}（M1.4 补齐后生效，见上方输出）" ;;
    *) die "$code" "$message" ;;
  esac
}

step_webhook_ingress() {
  # 入口软件由管理员安装与维护（OPS-18）；这里只给出已渲染的模板位置并做外部可达检查。
  case "$WEBHOOK_MODE" in
    public) info "反向代理模板：/usr/local/share/gantry/webhook/gantry.caddyfile（include 到 Caddy 配置后 reload）" ;;
    tunnel) info "隧道模板：/usr/local/share/gantry/webhook/cloudflared.yml（填入隧道 ID 与凭据后启动 cloudflared）" ;;
  esac
  if [ "$NON_INTERACTIVE" != 1 ]; then
    local answer
    read -r -p "入口配置好后按回车继续，输入 q 中止：" answer </dev/tty
    [ "$answer" != q ] || die "$EXIT_ABORTED" "已中止"
  fi
  doctor_area webhook "$EXIT_APP" "webhook 入口不可达（P-65：须先配好入口再创建 App）"
}

step_github_app() {
  local -a args=(github setup --webhook-url "$WEBHOOK_URL" --app-owner "$APP_OWNER")
  [ -z "$APP_NAME" ] || args+=(--name "$APP_NAME")
  [ "$TEST_MODE" != 1 ] || args+=(--test-mode)
  gantry "${args[@]}" || die "$EXIT_APP" "GitHub App 创建未完成"
  doctor_area github "$EXIT_APP" "doctor github 未通过"
}

step_bootstrap_writes() {
  # 默认不写任何键（sandbox.base_image 默认是官方 worker 镜像的固定摘要，P-66）。
  # 需要覆盖时由管理员手动执行 gantry settings bootstrap-set（§7.9）。
  info "引导期写入：默认无需操作"
}

step_sandbox_smoke() {
  doctor_area egress "$EXIT_DOCTOR" "金丝雀沙盒检查未通过"
}

step_doctor() {
  local rc=0
  gantry doctor || rc=$?
  case "$rc" in
    0) ;;
    1) warn "doctor 有警告" ;;
    3) warn "未检查区域：见上方 unchecked 行（M1.4 补齐后生效）" ;;
    *) die "$EXIT_DOCTOR" "doctor 有失败项" ;;
  esac
}

step_backup() {
  systemctl enable --now gantry-backup.timer
  # 立即做一次备份并校验（§7.1 第 18 步）。
  systemctl start gantry-backup.service || die 1 "首次备份失败，见 journalctl -u gantry-backup"
}

step_next_steps() {
  cat <<EOF
安装完成。下一步：
  1. 运行 sudo gantry pair，在第一台设备上完成配对（第一台设备成为 approver）。
  2. 根证书指纹见 /etc/gantry/ca/FINGERPRINTS.txt；请确认离线副本已妥善保管。
     丢失 = 无法轮换中间 CA，最坏需全体重新配对。
EOF
}

# dirs 步骤之前 /etc/gantry 还不存在：先把完成的步骤记在内存里，目录出现后一并落盘。
PENDING_STEPS=""

run_step() {
  local step="$1" s
  info "== 步骤 $(step_index "$step")：$step =="
  "step_$step"
  PENDING_STEPS="$PENDING_STEPS $step"
  if [ -d /etc/gantry ]; then
    for s in $PENDING_STEPS; do
      state_mark_step "$STATE_FILE" "$(current_version)" "$s" "$STATE_TEST_MODE"
    done
    PENDING_STEPS=""
  fi
}

# check_state_test_mode <状态文件> <true|false>：旧文件、损坏或不一致都拒绝续跑。
check_state_test_mode() {
  local file="$1" want="$2" rc=0
  [ -f "$file" ] || return 0
  state_test_mode_matches "$file" "$want" || rc=$?
  case "$rc" in
    0) ;;
    1) die "$EXIT_PREFLIGHT" "安装状态文件的 test_mode 与本次参数不一致，拒绝续跑" ;;
    *) die "$EXIT_PREFLIGHT" "安装状态文件缺少 test_mode（旧版本或已损坏），无法安全续跑" ;;
  esac
}

main() {
  parse_args "$@"
  validate_args
  STATE_TEST_MODE=false
  [ "$TEST_MODE" != 1 ] || STATE_TEST_MODE=true
  check_state_test_mode "$STATE_FILE" "$STATE_TEST_MODE"
  local step steps
  steps="$(steps_to_run "$STATE_FILE")"
  if [ "$RESUME" = 1 ] && [ ! -f "$STATE_FILE" ]; then
    info "没有安装状态文件，从头开始"
  fi
  for step in $steps; do
    run_step "$step"
  done
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
