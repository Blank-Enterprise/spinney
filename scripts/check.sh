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
if command -v cargo-deny >/dev/null 2>&1; then
  step "cargo deny check"
  cargo deny --locked check
else
  step "cargo deny skipped (install with: brew install cargo-deny)"
fi
if command -v zizmor >/dev/null 2>&1; then
  step "zizmor (workflow lint)"
  zizmor --offline .github/workflows
else
  step "zizmor skipped (install with: brew install zizmor)"
fi
printf '\nAll checks passed.\n'
