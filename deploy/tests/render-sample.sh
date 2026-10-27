#!/usr/bin/env bash
# 用测试示例参数渲染一个模板到 stdout（供快照测试、nft -c、caddy validate 等使用）。
# 示例参数只用文档保留地址（RFC 5737）与 example 域名，仓库里不放真实主机地址。
#   deploy/tests/render-sample.sh nftables/gantry.nft.in
set -Eeuo pipefail
DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export GANTRY_NO_JOURNAL=1
# shellcheck source=../install.sh
. "$DEPLOY_DIR/install.sh"

parse_args \
  --release /nonexistent/gantry.tar.gz \
  --api-host gantry.example.internal --api-host 192.0.2.10 \
  --webhook-mode tunnel --webhook-url https://hooks.example.com/webhook \
  --tunnel-id 00000000-0000-0000-0000-000000000000 \
  --admin-nets 192.0.2.0/24,198.51.100.0/24 \
  --egress-addr 10.89.0.1 --sandbox-cidrs 10.89.0.0/24 \
  --backup-recipient age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq \
  --backup-journal-target /mnt/offsite/gantry/revocations.journal
validate_args

args=()
while IFS= read -r line; do args+=("$line"); done < <(render_args)
render_template "$DEPLOY_DIR/$1" "${args[@]}"
