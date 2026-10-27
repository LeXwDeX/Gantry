# shellcheck shell=bash
# 极简测试框架：每个 test_* 函数在子 shell 里跑，失败不影响其他用例。

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC2034 # 供 source 本文件的测试使用
DEPLOY_DIR="$(cd "$TESTS_DIR/.." && pwd)"
export GANTRY_NO_JOURNAL=1

fail() {
  printf '    断言失败：%s\n' "$*" >&2
  exit 1
}

assert_eq() {
  [ "$1" = "$2" ] || fail "${3:-值不等}：期望 [$2]，实际 [$1]"
}

assert_contains() {
  case "$1" in *"$2"*) ;; *) fail "${3:-不含期望内容}：[$2]" ;; esac
}

assert_not_contains() {
  case "$1" in *"$2"*) fail "${3:-含不应出现的内容}：[$2]" ;; esac
}

# run_tests <文件>：执行文件里所有 test_ 开头的函数，打印结果，返回失败数。
run_tests() {
  local file="$1" t failed=0 out
  while IFS= read -r t; do
    if out="$( (set -Eeuo pipefail; "$t") 2>&1)"; then
      printf '  ok   %s\n' "$t"
    else
      printf '  FAIL %s\n%s\n' "$t" "$out"
      failed=$((failed + 1))
    fi
  done < <(grep -Eo '^test_[A-Za-z0-9_]+' "$file")
  return "$failed"
}
