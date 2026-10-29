#!/usr/bin/env bash
# install.sh 的参数校验、步骤选择与预检检查器（deploy.md §6.1、§7.1、§7.2）。
# 需要 root 与 Ubuntu 的副作用步骤留给 M1.4 的虚拟机（B 级）。
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=../install.sh
. "$DEPLOY_DIR/install.sh"

FIX="$TESTS_DIR/fixtures"

good_args() {
  printf '%s\n' --release /nonexistent/gantry.tar.gz \
    --api-host gantry.example.internal \
    --webhook-url https://hooks.example.com/webhook \
    --admin-nets 192.0.2.0/24 \
    --egress-addr 10.89.0.1 --sandbox-cidrs 10.89.0.0/24 \
    --backup-recipient age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq \
    --backup-journal-target /mnt/offsite/revocations.journal
}

# expect_validate_fail <期望退出码> <参数…>：在子 shell 里解析并校验，断言退出码。
expect_validate_fail() {
  local want="$1" rc=0 args=() line
  shift
  while IFS= read -r line; do args+=("$line"); done <<EOF
$(good_args)
EOF
  (parse_args "${args[@]}" "$@" && validate_args) 2>/dev/null || rc=$?
  assert_eq "$rc" "$want" "参数 [$*] 的退出码"
}

test_valid_args_pass() {
  local args=() line
  while IFS= read -r line; do args+=("$line"); done <<EOF
$(good_args)
EOF
  parse_args "${args[@]}"
  validate_args
}

test_invalid_args_exit_10() {
  expect_validate_fail 10 --webhook-mode smee
  expect_validate_fail 10 --webhook-url http://hooks.example.com/webhook
  expect_validate_fail 10 --webhook-url https://hooks.example.com/other
  expect_validate_fail 10 --api-host 'bad host'
  expect_validate_fail 10 --admin-nets 192.0.2.0
  expect_validate_fail 10 --egress-addr 300.1.1.1
  expect_validate_fail 10 --backup-recipient not-an-age-key
  expect_validate_fail 10 --backup-journal-target relative/path
  expect_validate_fail 10 --app-owner team
  expect_validate_fail 10 --assume-exported
  expect_validate_fail 10 --non-interactive
}

test_unknown_flag_and_step_rejected() {
  expect_validate_fail 1 --only no_such_step
  local rc=0
  (parse_args --frobnicate) 2>/dev/null || rc=$?
  assert_eq "$rc" 1
}

test_steps_follow_spec_order() {
  # §7.1：第 13 步配置 webhook 入口先于第 14 步创建 App（P-65）。
  assert_eq "$(step_index webhook_ingress)" 13
  assert_eq "$(step_index github_app)" 14
  assert_eq "$(step_index verify_release)" 0
  assert_eq "$(step_index next_steps)" 19
  local s
  for s in $STEPS; do
    declare -F "step_$s" >/dev/null || fail "缺少步骤函数 step_$s"
  done
}

test_steps_to_run_resume_and_rerun() {
  local f s
  f="$(mktemp -d)/state.json"
  assert_eq "$(steps_to_run "$f" | head -n1)" verify_release
  for s in verify_release preflight packages users dirs; do state_mark_step "$f" 0.1.0 "$s"; done
  assert_eq "$(steps_to_run "$f" | head -n1)" release "续跑应从第一个未完成步骤开始"
  for s in $STEPS; do state_mark_step "$f" 0.1.0 "$s"; done
  assert_eq "$(steps_to_run "$f")" "" "全部完成后重跑不应再执行任何步骤"
  ONLY=firewall
  assert_eq "$(steps_to_run "$f")" firewall "--only 总是执行指定步骤"
}

test_check_os_release() {
  check_os_release "$FIX/os-release-ubuntu-2404" || fail "24.04 应通过"
  ! check_os_release "$FIX/os-release-ubuntu-2204" || fail "22.04 应失败"
  ! check_os_release "$FIX/os-release-debian-12" || fail "Debian 应失败"
}

test_check_ports_free() {
  check_ports_free "$(cat "$FIX/ss-free.txt")" || fail "无冲突时应通过"
  local busy rc=0
  busy="$(check_ports_free "$(cat "$FIX/ss-busy.txt")")" || rc=$?
  assert_eq "$rc" 1
  assert_eq "$busy" "8443 8787"
}

test_check_ntp() {
  check_ntp "$(printf 'Timezone=UTC\nNTP=yes\nNTPSynchronized=yes\n')" || fail "已同步应通过"
  ! check_ntp "$(printf 'NTP=yes\nNTPSynchronized=no\n')" || fail "未同步应失败"
}

test_check_disk() {
  check_disk_gib $((20 * 1024 * 1024)) 20 || fail "恰好 20 GiB 应通过"
  ! check_disk_gib $((19 * 1024 * 1024)) 20 || fail "19 GiB 应失败"
}

test_check_user_conflict() {
  check_user_conflict "" /var/lib/gantry || fail "不存在的用户应通过"
  check_user_conflict "gantry:x:998:998::/var/lib/gantry:/usr/sbin/nologin" /var/lib/gantry || fail "属性一致应通过"
  ! check_user_conflict "gantry:x:1000:1000::/home/gantry:/bin/bash" /var/lib/gantry || fail "可登录用户应判冲突"
  ! check_user_conflict "gantry:x:998:998::/srv/gantry:/usr/sbin/nologin" /var/lib/gantry || fail "家目录不同应判冲突"
}

test_webhook_host_parsing() {
  assert_eq "$(webhook_host https://hooks.example.com/webhook)" hooks.example.com
  assert_eq "$(webhook_host https://hooks.example.com:8443/webhook)" hooks.example.com:8443
  ! webhook_host https://hooks.example.com/a/webhook >/dev/null || fail "带额外路径应拒绝"
  ! webhook_host https://smee.io/abc >/dev/null || fail "非 /webhook 路径应拒绝"
}

test_next_subid_start() {
  local f
  f="$(mktemp)"
  : >"$f"
  assert_eq "$(next_subid_start "$f")" 131072
  printf 'alice:100000:65536\nbob:165536:65536\n' >"$f"
  assert_eq "$(next_subid_start "$f")" 262144
  rm -f "$f"
}

test_single_area_doctor_warns_do_not_abort() {
  local stub rc=0
  stub="$(mktemp -d)"
  printf '#!/bin/sh\nexit 1\n' >"$stub/gantry"
  chmod +x "$stub/gantry"
  PATH="$stub:$PATH" doctor_area webhook 40 "不应中止" || rc=$?
  assert_eq "$rc" 0
  rm -rf "$stub"
}

test_single_area_doctor_failure_aborts() {
  local stub rc=0
  stub="$(mktemp -d)"
  printf '#!/bin/sh\nexit 2\n' >"$stub/gantry"
  chmod +x "$stub/gantry"
  ( PATH="$stub:$PATH" doctor_area webhook 40 "应中止" ) 2>/dev/null || rc=$?
  assert_eq "$rc" 40
  rm -rf "$stub"
}

test_install_help_lists_steps() {
  local out
  out="$(bash "$DEPLOY_DIR/install.sh" --help)"
  assert_contains "$out" "13 webhook_ingress"
  assert_contains "$out" "--backup-journal-target"
}

run_tests "${BASH_SOURCE[0]}"
