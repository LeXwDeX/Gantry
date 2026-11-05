#!/usr/bin/env bash
# 自建 CI 全量门槛。从干净检出运行，工作目录必须是仓库根（脚本会自行定位）。
# 用法：./ci/run.sh
#
# 容器需要 `--cap-add SYS_PTRACE`（gantry-egress 的 daemon 测试读取子进程 /proc）
# 与足够的 /dev/shm（gantryd 进程测试）；建议 `--init --shm-size=1g`。
set -euo pipefail

cd "$(dirname "$0")/.."

step() {
  printf '\n========== %s ==========\n' "$1"
}

step "cargo fmt"
cargo fmt --all -- --check

step "cargo clippy"
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

step "cargo test"
cargo test --locked --workspace --all-features

step "cargo check (MSRV 1.85)"
cargo +1.85 check --locked --workspace --all-features
cargo +1.85 check --locked --workspace --all-targets --all-features

step "cargo check (all targets)"
cargo check --workspace --all-targets

step "xtask schemas"
cargo --locked xtask schemas --check
git diff --exit-code -- schemas fixtures

step "mock-api"
npm ci --prefix tools/mock-api
npm test --prefix tools/mock-api

step "desktop"
npm ci --prefix apps/desktop
npm run lint --prefix apps/desktop
npm run typecheck --prefix apps/desktop
npm test --prefix apps/desktop
npm run build --prefix apps/desktop

step "gantryd process tests (5 rounds)"
for round in 1 2 3 4 5; do
  printf '\n---------- round %s/5 ----------\n' "$round"
  cargo test --locked -p gantryd --test process --all-features
done

step "store tests on SQLite backend"
GANTRY_STORE_BACKEND=sqlite cargo test --locked -p gantry-store --all-features

step "registry-recovery"
# 独立 workspace 的锁文件依赖不在主 workspace 缓存里，不能离线跑。
cargo test --locked \
  --manifest-path crates/gantryd/tests/support/registry-recovery/Cargo.toml --lib

printf '\nCI passed.\n'
