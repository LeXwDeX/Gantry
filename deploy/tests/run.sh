#!/usr/bin/env bash
# deploy 的本机检查入口（deploy.md §7.15 的静态部分 + 本机替身测试，A 级）。
#   deploy/tests/run.sh          全部
#   deploy/tests/run.sh static   只跑静态检查
#   deploy/tests/run.sh unit     只跑测试用例
# 工具缺失时明确打印 SKIP 及原因，不静默通过；DEPLOY_STRICT=1 时工具缺失即失败（Linux CI 用）。
set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$TESTS_DIR/.." && pwd)"
MODE="${1:-all}"
failed=0
skipped=""

skip() {
  skipped="$skipped
  SKIP $1"
  if [ -n "${DEPLOY_STRICT:-}" ]; then
    printf 'FAIL（DEPLOY_STRICT）：%s\n' "$1"
    failed=$((failed + 1))
  fi
}

check_shellcheck() {
  echo "== shellcheck"
  if ! command -v shellcheck >/dev/null 2>&1; then
    skip "shellcheck 未安装"
    return 0
  fi
  if (cd "$DEPLOY_DIR" && shellcheck -x install.sh bin/gantry-backup lib/*.sh tests/*.sh tests/stubs/gantry); then
    echo "  ok"
  else
    failed=$((failed + 1))
  fi
}

check_systemd() {
  echo "== systemd-analyze verify"
  if ! command -v systemd-analyze >/dev/null 2>&1; then
    skip "systemd-analyze 不可用（非 Linux / 无 systemd），单元约束改由 test_units.sh 的结构检查兜底"
    return 0
  fi
  # 开发机上没有 /opt/gantry/current/bin/* 与 gantry 用户，verify 会就此报错；只把其余错误当失败。
  local out
  out="$(systemd-analyze verify "$DEPLOY_DIR"/systemd/* 2>&1 || true)"
  out="$(printf '%s\n' "$out" | grep -v -E 'is not executable|No such file or directory|Failed to resolve user|Failed to resolve group' | sed '/^$/d' || true)"
  if [ -z "$out" ]; then echo "  ok"; else printf '%s\n' "$out"; failed=$((failed + 1)); fi
}

check_nft() {
  echo "== nft -c"
  if ! command -v nft >/dev/null 2>&1 || [ "$(id -u)" != 0 ]; then
    skip "nft 不可用或非 root（nft -c 需要 CAP_NET_ADMIN）"
    return 0
  fi
  local rendered
  rendered="$(mktemp)"
  bash "$TESTS_DIR/render-sample.sh" nftables/gantry.nft.in >"$rendered"
  if nft -c -f "$rendered"; then echo "  ok"; else failed=$((failed + 1)); fi
  rm -f "$rendered"
}

check_caddy() {
  echo "== caddy adapt"
  if ! command -v caddy >/dev/null 2>&1; then
    skip "caddy 未安装"
    return 0
  fi
  local dir
  dir="$(mktemp -d)"
  bash "$TESTS_DIR/render-sample.sh" webhook/Caddyfile.in >"$dir/Caddyfile"
  if caddy adapt --adapter caddyfile --config "$dir/Caddyfile" >/dev/null; then echo "  ok"; else failed=$((failed + 1)); fi
  rm -rf "$dir"
}

static_checks() {
  check_shellcheck
  check_systemd
  check_nft
  check_caddy
}

unit_tests() {
  local t
  for t in "$TESTS_DIR"/test_*.sh; do
    echo "== $(basename "$t")"
    bash "$t" || failed=$((failed + 1))
  done
}

case "$MODE" in
  all) static_checks; unit_tests ;;
  static) static_checks ;;
  unit) unit_tests ;;
  *) echo "用法：$0 [all|static|unit]" >&2; exit 2 ;;
esac

[ -z "$skipped" ] || printf '\n已跳过：%s\n' "$skipped"
if [ "$failed" -ne 0 ]; then
  printf '\n%d 组失败\n' "$failed"
  exit 1
fi
printf '\n全部通过\n'
