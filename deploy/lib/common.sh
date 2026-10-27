# shellcheck shell=bash
# deploy 脚本共用函数：日志、模板渲染、TOML 取值、安装状态文件。
# 只被 source，不单独执行。要求 bash ≥ 3.2（macOS 本机测试用 3.2，Ubuntu 是 5.x），
# 所以不用关联数组、mapfile、${var,,} 等 bash 4 特性。

# 日志：同时写 stderr 与 journald（有 systemd-cat 时）。永不记录机密（deploy.md §8、§11）。
GANTRY_LOG_TAG="${GANTRY_LOG_TAG:-gantry-deploy}"

log() {
  local level="$1"
  shift
  printf '%s [%s] %s\n' "$GANTRY_LOG_TAG" "$level" "$*" >&2
  if command -v systemd-cat >/dev/null 2>&1 && [ -z "${GANTRY_NO_JOURNAL:-}" ]; then
    printf '%s\n' "$*" | systemd-cat -t "$GANTRY_LOG_TAG" -p "$(journal_prio "$level")" 2>/dev/null || true
  fi
}

journal_prio() {
  case "$1" in
    error) printf 'err' ;;
    warn) printf 'warning' ;;
    *) printf 'info' ;;
  esac
}

info() { log info "$@"; }
warn() { log warn "$@"; }
error() { log error "$@"; }

# die <退出码> <消息>：退出码表见 deploy.md §6.1。
die() {
  local code="$1"
  shift
  error "$@"
  exit "$code"
}

# 渲染值只允许保守字符集，杜绝模板注入（换行、引号、反斜杠、$、反引号都拒绝）。
render_value_ok() {
  case "$1" in
    *[!A-Za-z0-9._:/@,=+\ -]*) return 1 ;;
    *) return 0 ;;
  esac
}

# render_template <模板> KEY=VALUE...：把 @KEY@ 替换为 VALUE，输出到 stdout。
# 值不合法、键名不合法或模板里留有未替换的 @KEY@ 都失败。
render_template() {
  local template="$1"
  shift
  [ -r "$template" ] || { error "模板不可读：$template"; return 1; }
  local content pair key value
  content="$(cat "$template")"
  for pair in "$@"; do
    key="${pair%%=*}"
    value="${pair#*=}"
    case "$key" in
      '' | *[!A-Z0-9_]*) error "模板键名不合法：$key"; return 1 ;;
    esac
    if ! render_value_ok "$value"; then
      error "模板值含不允许的字符：$key"
      return 1
    fi
    content="${content//@${key}@/$value}"
  done
  if printf '%s\n' "$content" | grep -Eq '@[A-Z][A-Z0-9_]*@'; then
    error "模板 $template 有未替换的占位符：$(printf '%s\n' "$content" | grep -Eo '@[A-Z][A-Z0-9_]*@' | sort -u | tr '\n' ' ')"
    return 1
  fi
  printf '%s\n' "$content"
}

# toml_get <文件> <section> <key>：读取 [section] 下 key = "字符串" 或 key = 数字 / 布尔。
# 只覆盖引导配置用到的简单形态；数组、内联表、多行字符串不支持（返回原样文本）。
toml_get() {
  local file="$1" section="$2" key="$3"
  awk -v want_section="$section" -v want_key="$key" '
    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
    /^[ \t]*#/ { next }
    /^[ \t]*\[/ {
      s = $0; sub(/^[ \t]*\[/, "", s); sub(/\][ \t]*(#.*)?$/, "", s)
      current = trim(s); next
    }
    current == want_section {
      line = $0
      eq = index(line, "=")
      if (eq == 0) next
      k = trim(substr(line, 1, eq - 1))
      if (k != want_key) next
      v = trim(substr(line, eq + 1))
      if (v ~ /^"/) { sub(/^"/, "", v); sub(/"[ \t]*(#.*)?$/, "", v) }
      else { sub(/[ \t]*#.*$/, "", v) }
      print v; found = 1; exit
    }
    END { exit found ? 0 : 1 }
  ' "$file"
}

# ---- 安装状态文件（/etc/gantry/install-state.json，deploy.md §5）----
# 形态：{"schema":1,"version":"…","completed":["step",…],"updated_at":"…"}。
# 只含步骤名（固定标识符）、版本与时间，不含密钥。

state_completed_steps() {
  local file="$1"
  [ -f "$file" ] || return 0
  # completed 数组里的元素都是 [a-z0-9_-]+，逐个取出。
  tr -d '\n' <"$file" | sed -n 's/.*"completed":\[\([^]]*\)\].*/\1/p' | tr ',' '\n' | sed -n 's/^ *"\([a-z0-9_-]*\)" *$/\1/p'
}

state_has_step() {
  local file="$1" step="$2"
  state_completed_steps "$file" | grep -qx "$step"
}

# state_mark_step <文件> <版本> <步骤>：追加步骤并原子替换文件（0640）。
state_mark_step() {
  local file="$1" version="$2" step="$3"
  case "$step" in '' | *[!a-z0-9_-]*) error "步骤名不合法：$step"; return 1 ;; esac
  render_value_ok "$version" || { error "版本号不合法"; return 1; }
  local steps list s tmp now
  steps="$(state_completed_steps "$file")"
  if ! printf '%s\n' "$steps" | grep -qx "$step"; then
    steps="$(printf '%s\n%s\n' "$steps" "$step")"
  fi
  list=""
  for s in $steps; do
    if [ -z "$list" ]; then list="\"$s\""; else list="$list,\"$s\""; fi
  done
  now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  tmp="$(mktemp "${file}.XXXXXX")"
  printf '{"schema":1,"version":"%s","completed":[%s],"updated_at":"%s"}\n' "$version" "$list" "$now" >"$tmp"
  chmod 0640 "$tmp"
  mv -f "$tmp" "$file"
}
