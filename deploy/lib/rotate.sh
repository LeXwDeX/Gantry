# shellcheck shell=bash
# 备份轮转（deploy.md §7.10 第 6 步）：日 / 周 / 月三档保留（祖父-父-子）。
# 只处理未带 tag 的归档 gantry-YYYYmmddTHHMMSSZ.tar.gz.age；带 tag 的（如 pre-upgrade-*）由管理员清理。

# iso_week <YYYYmmdd>：输出 ISO 年周（如 202643）。兼容 GNU 与 BSD date。
iso_week() {
  date -u -d "$1" +%G%V 2>/dev/null || date -u -j -f %Y%m%d "$1" +%G%V
}

# rotate_plan <每日> <每周> <每月>：stdin 为归档文件名（任意顺序），stdout 为应删除的文件名。
# 每档保留各自最近 N 个不同的日 / 周 / 月中最新的一份；任一档保留即保留。
rotate_plan() {
  local keep_d="$1" keep_w="$2" keep_m="$3" name day
  { grep -E '^gantry-[0-9]{8}T[0-9]{6}Z\.tar\.gz\.age$' || true; } | LC_ALL=C sort -r | while IFS= read -r name; do
    day="${name#gantry-}"
    day="${day%%T*}"
    printf '%s %s %s %s\n' "$name" "$day" "$(iso_week "$day")" "${day%??}"
  done | awk -v kd="$keep_d" -v kw="$keep_w" -v km="$keep_m" '
    {
      keep = 0
      if (!($2 in d) && nd < kd) { d[$2] = 1; nd++; keep = 1 } else if (!($2 in d)) { d[$2] = 1 }
      if (!($3 in w) && nw < kw) { w[$3] = 1; nw++; keep = 1 } else if (!($3 in w)) { w[$3] = 1 }
      if (!($4 in m) && nm < km) { m[$4] = 1; nm++; keep = 1 } else if (!($4 in m)) { m[$4] = 1 }
      if (!keep) print $1
    }'
}

# rotate_backups <目录> <每日> <每周> <每月>：按计划删除过期归档。
rotate_backups() {
  local dir="$1" victim
  (cd "$dir" && ls -1) | rotate_plan "$2" "$3" "$4" | while IFS= read -r victim; do
    rm -f -- "${dir:?}/$victim"
  done
}
