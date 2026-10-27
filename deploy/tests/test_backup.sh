#!/usr/bin/env bash
# gantry-backup 的本机替身测试（deploy.md §7.10、§12「备份 (M1)」）：真实 sqlite3 与 age，
# gantry CLI 用 tests/stubs/gantry 替身。持续写入负载、定时器补做等归 M1.4 虚拟机。
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=../lib/rotate.sh
. "$DEPLOY_DIR/lib/rotate.sh"

SENTINEL="SENTINEL-SECRET-VALUE-7f3a9c"

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# make_env：在临时目录里搭出数据目录、配置与 age 密钥，输出根目录路径。
make_env() {
  local root recipient
  root="$(mktemp -d)"
  mkdir -p "$root/var/lib/gantry/pki" "$root/var/lib/gantry/cas/ab" "$root/var/lib/gantry/secrets" \
    "$root/var/lib/gantry/mirrors/o/r.git" "$root/var/lib/gantry/worktrees/r/d" \
    "$root/var/lib/gantry-egress/secrets" "$root/etc/gantry" "$root/etc/gantry-egress" "$root/backups"
  sqlite3 "$root/var/lib/gantry/gantry.db" "PRAGMA journal_mode=WAL; CREATE TABLE events(seq INTEGER PRIMARY KEY, body TEXT); INSERT INTO events(body) VALUES ('a'),('b');" >/dev/null
  printf 'object\n' >"$root/var/lib/gantry/cas/ab/cdef"
  printf '%s\n' "$SENTINEL" >"$root/var/lib/gantry/secrets/github.app.private_key"
  printf '%s\n' "$SENTINEL" >"$root/var/lib/gantry-egress/secrets/model.key"
  printf 'mirror-object\n' >"$root/var/lib/gantry/mirrors/o/r.git/HEAD"
  printf 'worktree-file\n' >"$root/var/lib/gantry/worktrees/r/d/file"
  # 最后一行不完整（没有换行），应被丢弃。
  printf '{"device_id":"dev_1","revoked_at":"2026-10-01T00:00:00Z"}\n{"device_id":"dev_2"' >"$root/var/lib/gantry/pki/revocations.journal"
  age-keygen -o "$root/age.key" 2>/dev/null
  recipient="$(age-keygen -y "$root/age.key")"
  {
    printf '[server]\ndata_dir = "%s"\nsecrets_dir = "%s"\n\n' "$root/var/lib/gantry" "$root/var/lib/gantry/secrets"
    printf '[backup]\ndest = "%s"\nrecipients = "%s"\npost_hook = ""\n' "$root/backups" "$recipient"
    printf 'journal_target = "%s"\nkeep_daily = 7\nkeep_weekly = 4\nkeep_monthly = 6\n' "$root/offsite/revocations.journal"
  } >"$root/etc/gantry/gantry.toml"
  printf '{"schema":1,"version":"0.1.0","completed":["preflight"],"updated_at":"2026-10-27T00:00:00Z"}\n' >"$root/etc/gantry/install-state.json"
  printf '[egress]\ndata_dir = "%s"\n' "$root/var/lib/gantry-egress" >"$root/etc/gantry-egress/egress.toml"
  printf '%s' "$root"
}

set_post_hook() {
  local root="$1" hook="$2" tmp
  tmp="$(mktemp)"
  sed "s|^post_hook = \"\"|post_hook = \"$hook\"|" "$root/etc/gantry/gantry.toml" >"$tmp"
  mv "$tmp" "$root/etc/gantry/gantry.toml"
}

run_backup() {
  local root="$1"
  shift
  PATH="$TESTS_DIR/stubs:$PATH" GANTRY_BACKUP_TEST=1 STUB_LOG="$root/stub.log" \
    GANTRY_VERSION_FILE="$root/VERSION" \
    "$DEPLOY_DIR/bin/gantry-backup" --config "$root/etc/gantry/gantry.toml" \
    --egress-config "$root/etc/gantry-egress/egress.toml" "$@"
}

archives() {
  find "$1/backups" -maxdepth 1 -name 'gantry-*.tar.gz.age' | LC_ALL=C sort
}

# decrypt_latest <根> <输出目录>
decrypt_latest() {
  mkdir -p "$2"
  age -d -i "$1/age.key" "$(archives "$1" | tail -n1)" | tar -xzf - -C "$2"
}

test_backup_archive_scope_and_manifest() {
  local root out path sha n=0
  root="$(make_env)"
  printf '0.1.0\n' >"$root/VERSION"
  run_backup "$root" 2>/dev/null
  out="$root/out"
  decrypt_latest "$root" "$out"
  # 范围：含数据库、吊销日志、CAS、两个 secrets 导出、引导配置；不含镜像仓、工作树、工作区。
  [ -f "$out/gantry.db" ] || fail "缺数据库快照"
  [ -f "$out/cas/ab/cdef" ] || fail "缺 CAS"
  [ -f "$out/secrets-gantry.export" ] && [ -f "$out/secrets-egress.export" ] || fail "缺 secrets 导出"
  [ -f "$out/etc/gantry/gantry.toml" ] && [ -f "$out/etc/gantry/install-state.json" ] && [ -f "$out/etc/gantry-egress/egress.toml" ] || fail "缺引导配置"
  [ ! -e "$out/mirrors" ] && [ ! -e "$out/worktrees" ] && [ ! -e "$out/workspaces" ] || fail "不应含可重建的目录"
  assert_eq "$(sqlite3 "$out/gantry.db" 'SELECT count(*) FROM events;')" 2
  assert_eq "$(sqlite3 "$out/gantry.db" 'PRAGMA integrity_check;')" ok
  # 吊销日志：完整行保留、末尾不完整行丢弃。
  assert_eq "$(wc -l <"$out/revocations.journal" | tr -d ' ')" 1
  assert_eq "$(cat "$out/revocations.journal")" '{"device_id":"dev_1","revoked_at":"2026-10-01T00:00:00Z"}'
  # MANIFEST：元数据与每个组件的哈希都吻合。
  jq -e '.format == 1 and .gantry_version == "0.1.0" and .schema_version == 1 and .log_id == "log_test" and .log_epoch == 1 and .seq_head == 42' "$out/MANIFEST.json" >/dev/null ||
    fail "MANIFEST 元数据不符"
  while IFS=$'\t' read -r path sha; do
    assert_eq "$(sha256_file "$out/$path")" "$sha" "$path 哈希"
    n=$((n + 1))
  done < <(jq -r '.components[] | [.path, .sha256] | @tsv' "$out/MANIFEST.json")
  [ "$n" -ge 7 ] || fail "MANIFEST 组件太少：$n"
  jq -e '.archive and .size_bytes > 0 and .post_hook == false' "$root/backups/last-success.json" >/dev/null || fail "last-success.json 不符"
}

test_backup_never_contains_plaintext_secrets() {
  local root
  root="$(make_env)"
  run_backup "$root" 2>/dev/null
  # 加密归档本身与解密后的明文都不含哨兵值（secrets 只以 secrets 模块的加密导出进入归档）。
  ! grep -rq "$SENTINEL" "$root/backups" || fail "加密归档里出现了哨兵"
  decrypt_latest "$root" "$root/out"
  ! grep -rq "$SENTINEL" "$root/out" || fail "归档明文部分出现了哨兵"
  ! grep -rq 'AGE-SECRET-KEY-1' "$root/backups" || fail "备份目录里不应有 age 私钥"
  # 没有对应私钥无法解密。
  age-keygen -o "$root/other.key" 2>/dev/null
  ! age -d -i "$root/other.key" "$(archives "$root")" >/dev/null 2>&1 || fail "错误的私钥不应能解密"
}

test_backup_secrets_exported_per_instance() {
  local root
  root="$(make_env)"
  run_backup "$root" 2>/dev/null
  grep -qxF "secrets export --dir $root/var/lib/gantry/secrets --out -" "$root/stub.log" || fail "未导出 gantryd 的 secrets"
  grep -qxF "secrets export --dir $root/var/lib/gantry-egress/secrets --out -" "$root/stub.log" || fail "未导出 Egress 的 secrets"
}

test_backup_post_hook_failure_fails_backup() {
  local root first rc=0
  root="$(make_env)"
  run_backup "$root" 2>/dev/null
  first="$(archives "$root")"
  printf '#!/bin/sh\nexit 1\n' >"$root/hook.sh"
  chmod +x "$root/hook.sh"
  set_post_hook "$root" "$root/hook.sh"
  sleep 1
  run_backup "$root" 2>/dev/null || rc=$?
  [ "$rc" -ne 0 ] || fail "post_hook 失败时备份应判失败"
  [ -f "$first" ] || fail "上一份有效备份不应被删除"
}

test_backup_post_hook_called_with_archive() {
  local root
  root="$(make_env)"
  # shellcheck disable=SC2016 # $1 要原样写进钩子脚本
  printf '#!/bin/sh\nprintf "%%s\\n" "$1" >"%s/hook.called"\n' "$root" >"$root/hook.sh"
  chmod +x "$root/hook.sh"
  set_post_hook "$root" "$root/hook.sh"
  run_backup "$root" 2>/dev/null
  assert_contains "$(cat "$root/hook.called")" "$root/backups/gantry-"
  jq -e '.post_hook == true' "$root/backups/last-success.json" >/dev/null || fail "last-success 应记录 post_hook"
}

test_backup_fails_loudly_without_metadata() {
  local root rc=0
  root="$(make_env)"
  STUB_STORE_INFO_FAIL=1 run_backup "$root" 2>/dev/null || rc=$?
  [ "$rc" -ne 0 ] || fail "读不到快照元数据时应失败"
  assert_eq "$(archives "$root" | wc -l | tr -d ' ')" 0 "失败时不应留下归档"
  assert_eq "$(find "$root/backups" -maxdepth 1 -name '.work.*' | wc -l | tr -d ' ')" 0 "失败时应清理工作目录"
}

test_backup_tag_excluded_from_rotation() {
  local root
  root="$(make_env)"
  run_backup "$root" --tag pre-upgrade-0.2.0 2>/dev/null
  archives "$root" | grep -Eq '/gantry-[0-9]{8}T[0-9]{6}Z-pre-upgrade-0\.2\.0\.tar\.gz\.age$' || fail "带 tag 的归档名不对"
  assert_eq "$(printf 'gantry-20261027T000000Z-pre-upgrade-0.2.0.tar.gz.age\n' | rotate_plan 0 0 0)" "" "带 tag 的归档不参与轮转"
}

# days_before <YYYYmmdd> <n>：兼容 GNU 与 BSD date。
days_before() {
  date -u -d "$1 -$2 day" +%Y%m%d 2>/dev/null || date -u -j -v-"$2"d -f %Y%m%d "$1" +%Y%m%d
}

test_rotate_plan_gfs() {
  # 40 天的每日备份（截至 2026-10-27），每天两份：只保留每天最新的一份。
  local names="" d i deleted kept n
  for i in $(seq 0 39); do
    d="$(days_before 20261027 "$i")"
    names="$names
gantry-${d}T031700Z.tar.gz.age
gantry-${d}T010000Z.tar.gz.age"
  done
  deleted="$(printf '%s\n' "$names" | rotate_plan 7 4 6)"
  kept="$(printf '%s\n' "$names" | sed '/^$/d' | grep -vxF "$deleted" | LC_ALL=C sort -r)"
  printf '%s\n' "$kept" | grep -qx 'gantry-20261027T031700Z.tar.gz.age' || fail "最新的应保留"
  printf '%s\n' "$kept" | grep -qx 'gantry-20261021T031700Z.tar.gz.age' || fail "第 7 天应保留"
  ! printf '%s\n' "$kept" | grep -qx 'gantry-20261020T031700Z.tar.gz.age' || fail "第 8 天（同周已有代表）应删除"
  ! printf '%s\n' "$kept" | grep -q 'T010000Z' || fail "同一天较旧的一份应删除"
  printf '%s\n' "$kept" | grep -qx 'gantry-20260930T031700Z.tar.gz.age' || fail "9 月的月代表（当月最新一份）应保留"
  ! printf '%s\n' "$kept" | grep -qx 'gantry-20260918T031700Z.tar.gz.age' || fail "超出日 / 周 / 月三档的应删除"
  n="$(printf '%s\n' "$kept" | wc -l | tr -d ' ')"
  [ "$n" -le $((7 + 4 + 6)) ] || fail "保留数量超出上限：$n"
}

test_rotate_backups_deletes_only_planned() {
  local dir i d
  dir="$(mktemp -d)"
  for i in $(seq 0 9); do
    d="$(days_before 20261027 "$i")"
    : >"$dir/gantry-${d}T031700Z.tar.gz.age"
  done
  : >"$dir/gantry-20260101T000000Z-pre-upgrade-0.1.0.tar.gz.age"
  : >"$dir/last-success.json"
  rotate_backups "$dir" 3 0 0
  assert_eq "$(find "$dir" -name 'gantry-2026*Z.tar.gz.age' | wc -l | tr -d ' ')" 3
  [ -f "$dir/gantry-20260101T000000Z-pre-upgrade-0.1.0.tar.gz.age" ] || fail "带 tag 的归档不应被删"
  [ -f "$dir/last-success.json" ] || fail "非归档文件不应被删"
}

run_tests "${BASH_SOURCE[0]}"
