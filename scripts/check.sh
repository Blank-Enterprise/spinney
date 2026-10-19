#!/usr/bin/env bash
# Run every check CI runs, in CI's order, stopping at the first failure.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

step() { printf '\n== %s\n' "$*"; }

step "leak check"
bash scripts/leak-check.sh
step "cargo fmt --check"
cargo fmt --check
step "cargo clippy (-D warnings)"
cargo clippy --locked --all-targets -- -D warnings
step "cargo test"
cargo test --locked
if command -v cargo-audit >/dev/null 2>&1; then
  step "cargo audit"
  cargo audit
else
  step "cargo audit skipped (install with: cargo install cargo-audit --locked)"
fi
printf '\nAll checks passed.\n'
