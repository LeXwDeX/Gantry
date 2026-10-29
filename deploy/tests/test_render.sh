#!/usr/bin/env bash
# 模板渲染、TOML 取值与安装状态文件（deploy.md §7.15「模板渲染快照测试」）。
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=../lib/common.sh
. "$DEPLOY_DIR/lib/common.sh"
# render_with_sample <相对 deploy/ 的模板路径>：用 render-sample.sh 的示例参数渲染。
render_with_sample() {
  bash "$TESTS_DIR/render-sample.sh" "$1"
}

check_golden() {
  local template="$1" golden="$2" out
  out="$(render_with_sample "$template")"
  if [ -n "${UPDATE_GOLDEN:-}" ]; then
    printf '%s\n' "$out" >"$TESTS_DIR/fixtures/$golden"
  fi
  assert_eq "$out" "$(cat "$TESTS_DIR/fixtures/$golden")" "$template 渲染结果与快照不符（UPDATE_GOLDEN=1 可更新）"
}

test_golden_gantry_toml() { check_golden config/gantry.toml.in gantry.toml.golden; }
test_golden_egress_toml() { check_golden config/egress.toml.in egress.toml.golden; }
test_golden_nft() { check_golden nftables/gantry.nft.in gantry.nft.golden; }
test_golden_caddyfile() { check_golden webhook/Caddyfile.in Caddyfile.golden; }
test_golden_cloudflared() { check_golden webhook/cloudflared.yml.in cloudflared.yml.golden; }

test_render_rejects_injection() {
  local t
  t="$(mktemp)"
  printf 'x = "@V@"\n' >"$t"
  ! render_template "$t" 'V=a"b' 2>/dev/null || fail "引号应被拒绝"
  # shellcheck disable=SC2016 # 故意传入字面量 $(id)
  ! render_template "$t" 'V=$(id)' 2>/dev/null || fail "\$ 应被拒绝"
  ! render_template "$t" "V=$(printf 'a\nb')" 2>/dev/null || fail "换行应被拒绝"
  ! render_template "$t" 'V=a&b' 2>/dev/null || fail "& 应被拒绝（bash 5.2 patsub_replacement）"
  assert_eq "$(render_template "$t" 'V=ok-1.2:3/4')" 'x = "ok-1.2:3/4"'
  rm -f "$t"
}

test_render_rejects_leftover_placeholder() {
  local t
  t="$(mktemp)"
  printf '@A@ @B@\n' >"$t"
  ! render_template "$t" A=1 2>/dev/null || fail "未替换的 @B@ 应失败"
  rm -f "$t"
}

test_toml_get_reads_rendered_config() {
  local f
  f="$(mktemp)"
  render_with_sample config/gantry.toml.in >"$f"
  assert_eq "$(toml_get "$f" server data_dir)" /var/lib/gantry
  assert_eq "$(toml_get "$f" forge.webhook listen)" 127.0.0.1:8787
  assert_eq "$(toml_get "$f" backup keep_daily)" 7
  assert_eq "$(toml_get "$f" backup journal_target)" /mnt/offsite/gantry/revocations.journal
  ! toml_get "$f" backup missing_key >/dev/null || fail "缺失的键应返回非 0"
  rm -f "$f"
}

test_rendered_configs_have_no_secret_like_strings() {
  local t out
  for t in config/gantry.toml.in config/egress.toml.in; do
    out="$(render_with_sample "$t")"
    if printf '%s\n' "$out" | grep -Eiq 'BEGIN [A-Z ]*PRIVATE KEY|AGE-SECRET-KEY|ghp_|ghs_|sk-[A-Za-z0-9]{10,}|(secret|token|password|private_key) *= *"[^"]+'; then
      fail "$t 渲染结果含形似密钥的字符串"
    fi
  done
}

test_webhook_listener_is_loopback_only() {
  local out
  out="$(render_with_sample config/gantry.toml.in)"
  assert_contains "$out" 'listen = "127.0.0.1:8787"'
  out="$(render_with_sample config/egress.toml.in)"
  assert_contains "$out" 'svc_model = "127.0.0.1:8082"'
}

test_state_file_roundtrip() {
  local f
  f="$(mktemp -d)/install-state.json"
  ! state_has_step "$f" preflight || fail "空状态不应有步骤"
  state_mark_step "$f" 0.1.0 preflight false
  state_mark_step "$f" 0.1.0 packages false
  state_mark_step "$f" 0.1.0 preflight false
  assert_eq "$(state_completed_steps "$f" | tr '\n' ' ')" "preflight packages"
  state_has_step "$f" packages || fail "应记录 packages"
  jq -e '.schema == 1 and .version == "0.1.0" and (.completed | length == 2)' "$f" >/dev/null || fail "状态文件不是预期的 JSON"
  ! state_mark_step "$f" 0.1.0 'bad step' false 2>/dev/null || fail "非法步骤名应被拒绝"
}

run_tests "${BASH_SOURCE[0]}"
