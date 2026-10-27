# shellcheck shell=bash
# 目录布局与权限表（deploy.md §6.3）。install.sh 的第 4 步按表创建，权限审计按同一张表核对，
# 两者共用这一份数据，避免"创建"与"验收"各说各话。
#
# 每行：类型 路径 所有者 组 模式
#   类型 d = 目录（由安装创建），f = 文件（存在时核对，不由本表创建）。
# /run/gantry 由 systemd-tmpfiles 创建（tmpfiles.d/gantry.conf），这里只核对。
# /run/gantry-ca-bootstrap 只在 CA 引导期间存在，由 install.sh 第 8 步单独管理，不在表内。

GANTRY_LAYOUT='
d /etc/gantry root gantry 0750
d /etc/gantry/ca root gantry 0755
f /etc/gantry/gantry.toml root gantry 0640
f /etc/gantry/gantry.nft root root 0600
f /etc/gantry/install-state.json root gantry 0640
d /etc/gantry-egress root gantry-egress 0750
f /etc/gantry-egress/egress.toml root gantry-egress 0640
d /var/lib/gantry gantry gantry 0750
d /var/lib/gantry/pki gantry gantry 0700
f /var/lib/gantry/pki/revocations.journal gantry gantry 0600
f /var/lib/gantry/gantry.db gantry gantry 0600
d /var/lib/gantry/cas gantry gantry 0700
d /var/lib/gantry/secrets gantry gantry 0700
d /var/lib/gantry/mirrors gantry gantry 0700
d /var/lib/gantry/worktrees gantry gantry 0700
d /var/lib/gantry/workspaces gantry gantry-sbx 0710
d /var/lib/gantry-egress gantry-egress gantry-egress 0700
d /var/lib/gantry-egress/secrets gantry-egress gantry-egress 0700
d /var/lib/gantry-egress/audit gantry-egress gantry-egress 0700
d /var/lib/gantry-sbx gantry-sbx gantry-sbx 0700
d /var/backups/gantry root root 0700
'

# layout_rows：逐行输出表内容（去掉空行）。
layout_rows() {
  printf '%s\n' "$GANTRY_LAYOUT" | sed '/^[[:space:]]*$/d'
}

# path_stat <路径>：输出 "所有者:组 模式"（模式为四位八进制）。兼容 GNU 与 BSD stat。
path_stat() {
  local out
  if out="$(stat -c '%U:%G %a' "$1" 2>/dev/null)"; then
    :
  else
    out="$(stat -f '%Su:%Sg %Lp' "$1")" || return 1
  fi
  local owner="${out% *}" mode="${out##* }"
  printf '%s %04d\n' "$owner" "$mode"
}

# layout_apply [根前缀]：创建表中的目录并设置属主与模式（幂等）。需要 root。
layout_apply() {
  local root="${1:-}" kind path owner group mode
  while read -r kind path owner group mode; do
    [ "$kind" = d ] || continue
    install -d -o "$owner" -g "$group" -m "$mode" "${root}${path}"
    # install -d 对已存在目录不一定改模式，显式再设一次。
    chown "$owner:$group" "${root}${path}"
    chmod "$mode" "${root}${path}"
  done <<EOF
$(layout_rows)
EOF
}

# layout_audit [根前缀]：逐行核对，打印不符项，有不符时返回 1。
# 目录必须存在；文件不存在时跳过（例如尚未启动过 gantryd 时没有 gantry.db）。
# LAYOUT_SKIP_OWNER=1 只核对模式（本机替身测试用，测试目录的属主是当前用户）。
layout_audit() {
  local root="${1:-}" kind path owner group mode actual failed=0 target
  while read -r kind path owner group mode; do
    target="${root}${path}"
    if [ ! -e "$target" ]; then
      if [ "$kind" = d ]; then
        printf 'missing %s\n' "$path"
        failed=1
      fi
      continue
    fi
    actual="$(path_stat "$target")" || { printf 'unreadable %s\n' "$path"; failed=1; continue; }
    if [ -n "${LAYOUT_SKIP_OWNER:-}" ]; then
      if [ "${actual##* }" != "$mode" ]; then
        printf 'mismatch %s want %s got %s\n' "$path" "$mode" "${actual##* }"
        failed=1
      fi
    elif [ "$actual" != "$owner:$group $mode" ]; then
      printf 'mismatch %s want %s:%s %s got %s\n' "$path" "$owner" "$group" "$mode" "$actual"
      failed=1
    fi
  done <<EOF
$(layout_rows)
EOF
  return "$failed"
}
