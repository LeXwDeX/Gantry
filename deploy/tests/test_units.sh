#!/usr/bin/env bash
# systemd 单元、防火墙模板、webhook 入口模板与目录布局的结构检查（deploy.md §6.3、§7.6–§7.8）。
# 本机没有 systemd 时，这些检查替代不了 systemd-analyze verify，只锁住规格里写明的约束。
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=../lib/layout.sh
. "$DEPLOY_DIR/lib/layout.sh"

U="$DEPLOY_DIR/systemd"

# unit_values <文件> <键>：输出某键的所有取值（每行一个）。
unit_values() {
  sed -n "s/^$2=//p" "$1"
}

assert_unit_has() {
  grep -qxF "$2" "$1" || fail "$(basename "$1") 缺少 $2"
}

test_service_hardening() {
  local svc
  for svc in gantryd.service gantry-egress.service; do
    assert_unit_has "$U/$svc" 'Type=notify'
    assert_unit_has "$U/$svc" 'NoNewPrivileges=yes'
    assert_unit_has "$U/$svc" 'ProtectSystem=strict'
    assert_unit_has "$U/$svc" 'ProtectHome=yes'
    assert_unit_has "$U/$svc" 'PrivateTmp=yes'
    assert_unit_has "$U/$svc" 'PrivateDevices=yes'
    assert_unit_has "$U/$svc" 'CapabilityBoundingSet='
    assert_unit_has "$U/$svc" 'AmbientCapabilities='
    assert_unit_has "$U/$svc" 'UMask=0077'
    assert_unit_has "$U/$svc" 'LimitCORE=0'
    assert_unit_has "$U/$svc" 'SupplementaryGroups=gantry-link'
    assert_unit_has "$U/$svc" 'Restart=on-failure'
    assert_unit_has "$U/$svc" 'PartOf=gantry.target'
    # 两个单元共用 /run/gantry：都不得用 RuntimeDirectory=（§6.3）。
    ! grep -q '^RuntimeDirectory' "$U/$svc" || fail "$svc 不得使用 RuntimeDirectory="
    ! grep -Eiq 'Environment=.*(secret|token|key|password)' "$U/$svc" || fail "$svc 不得在单元里放密钥"
  done
  assert_eq "$(unit_values "$U/gantryd.service" User)" gantry
  assert_eq "$(unit_values "$U/gantry-egress.service" User)" gantry-egress
  assert_eq "$(unit_values "$U/gantryd.service" ReadWritePaths)" "/var/lib/gantry /run/gantry"
  assert_eq "$(unit_values "$U/gantry-egress.service" ReadWritePaths)" "/var/lib/gantry-egress /run/gantry"
}

test_processes_cannot_see_each_other() {
  assert_contains "$(unit_values "$U/gantryd.service" InaccessiblePaths)" "-/var/lib/gantry-egress"
  assert_contains "$(unit_values "$U/gantry-egress.service" InaccessiblePaths)" "-/var/lib/gantry "
}

test_gantryd_wants_but_not_requires_egress() {
  # Egress 不可用时 gantryd 仍应能运行（沙盒失败闭合），所以用 Wants= 而非 Requires=（§7.7）。
  assert_contains "$(unit_values "$U/gantryd.service" Wants)" gantry-egress.service
  assert_contains "$(unit_values "$U/gantryd.service" After)" gantry-egress.service
  ! grep -q '^Requires=.*gantry-egress' "$U/gantryd.service" || fail "gantryd 不得 Requires= Egress"
}

test_target_aggregates_services() {
  local wants
  wants="$(unit_values "$U/gantry.target" Wants)"
  assert_contains "$wants" gantryd.service
  assert_contains "$wants" gantry-egress.service
  assert_contains "$wants" gantry-firewall.service
}

test_backup_timer() {
  assert_unit_has "$U/gantry-backup.timer" 'Persistent=true'
  assert_unit_has "$U/gantry-backup.timer" 'OnCalendar=*-*-* 03:17:00'
  assert_unit_has "$U/gantry-backup.timer" 'RandomizedDelaySec=30min'
  assert_unit_has "$U/gantry-backup.service" 'Type=oneshot'
  ! grep -q '^User=' "$U/gantry-backup.service" || fail "备份以 root 运行"
}

test_firewall_unit_checks_before_load() {
  assert_unit_has "$U/gantry-firewall.service" 'Type=oneshot'
  assert_unit_has "$U/gantry-firewall.service" 'ExecStartPre=/usr/sbin/nft -c -f /etc/gantry/gantry.nft'
  assert_contains "$(unit_values "$U/gantry-firewall.service" Before)" gantryd.service
}

test_tmpfiles_run_gantry() {
  grep -qx 'd /run/gantry 0770 gantry gantry-link -' "$DEPLOY_DIR/tmpfiles.d/gantry.conf" || fail "tmpfiles 条目不符 §6.3"
}

test_nft_template_constraints() {
  local t="$DEPLOY_DIR/nftables/gantry.nft.in"
  grep -q '^table inet gantry {' "$t" || fail "须使用独立表 inet gantry"
  grep -q '^destroy table inet gantry$' "$t" || fail "重复装载须幂等"
  ! grep -Eq '^flush ruleset' "$t" || fail "不得清空管理员已有的规则"
  grep -q 'meta skuid "gantry-sbx" counter drop' "$t" || fail "沙盒用户其余出站须丢弃"
}

test_webhook_templates_forward_only_two_paths() {
  local caddy="$DEPLOY_DIR/webhook/Caddyfile.in" cf="$DEPLOY_DIR/webhook/cloudflared.yml.in"
  assert_eq "$(grep -Eo 'path [^ ]+' "$caddy" | sort | tr '\n' ' ')" "path /setup/github/callback path /webhook "
  assert_eq "$(grep -c 'reverse_proxy 127.0.0.1:@WEBHOOK_PORT@' "$caddy")" 2
  grep -q 'respond 404' "$caddy" || fail "Caddy 兜底须 404"
  assert_eq "$(grep -Eo 'path: [^ ]+' "$cf" | sort | tr '\n' ' ')" 'path: ^/setup/github/callback$ path: ^/webhook$ '
  grep -q 'service: http_status:404' "$cf" || fail "隧道兜底须 404"
  ! grep -Eq '8443|8444|@API_PORT@|@PAIR_PORT@' "$caddy" "$cf" || fail "API / 配对端口绝不经入口暴露"
  # 配置项（非注释行）里不得出现 smee（P-13）；注释里写明“不使用 smee.io”是允许的。
  ! grep -hv '^[[:space:]]*#' "$caddy" "$cf" "$DEPLOY_DIR/config/gantry.toml.in" | grep -iq 'smee' || fail "配置里不得出现 smee（P-13）"
}

test_layout_table_matches_spec() {
  local rows
  rows="$(layout_rows)"
  assert_contains "$rows" "d /var/lib/gantry/secrets gantry gantry 0700"
  assert_contains "$rows" "d /var/lib/gantry-egress/secrets gantry-egress gantry-egress 0700"
  assert_contains "$rows" "f /var/lib/gantry/pki/revocations.journal gantry gantry 0600"
  assert_contains "$rows" "d /var/lib/gantry/workspaces gantry gantry-sbx 0710"
  assert_contains "$rows" "d /var/backups/gantry root root 0700"
  # 每行五列、路径绝对、模式四位八进制。
  printf '%s\n' "$rows" | awk 'NF != 5 || $2 !~ /^\// || $5 !~ /^0[0-7]{3}$/ || ($1 != "d" && $1 != "f") { bad = 1; print } END { exit bad }' >/dev/null ||
    fail "布局表格式不对"
}

test_layout_audit_detects_mode_drift() {
  local root kind path owner group mode
  root="$(mktemp -d)"
  while read -r kind path owner group mode; do
    [ "$kind" = d ] || continue
    mkdir -p "$root$path"
    chmod "$mode" "$root$path"
  done <<EOF
$(layout_rows)
EOF
  LAYOUT_SKIP_OWNER=1 layout_audit "$root" >/dev/null || fail "按表创建后审计应通过"
  chmod 0755 "$root/var/lib/gantry/secrets"
  local out rc=0
  out="$(LAYOUT_SKIP_OWNER=1 layout_audit "$root")" || rc=$?
  assert_eq "$rc" 1
  assert_contains "$out" "mismatch /var/lib/gantry/secrets want 0700 got 0755"
  chmod -R u+w "$root"
  rm -rf "$root"
}

test_no_secrets_committed_under_deploy() {
  # 仓库密钥扫描（§7.15）：deploy/ 下不得出现私钥块、age 私钥、GitHub 令牌。
  if grep -RInE 'BEGIN [A-Z ]*PRIVATE KEY|AGE-SECRET-KEY-1|gh[pousr]_[A-Za-z0-9]{20,}' "$DEPLOY_DIR" --exclude-dir=.git |
    grep -v "tests/test_units.sh" | grep -v "tests/test_render.sh" | grep -v "tests/test_backup.sh"; then
    fail "deploy/ 下发现形似密钥的内容"
  fi
}

run_tests "${BASH_SOURCE[0]}"
