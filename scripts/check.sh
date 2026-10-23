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
step "build on the declared rust-version"
bash scripts/msrv-check.sh --if-installed
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
if command -v actionlint >/dev/null 2>&1; then
  step "actionlint"
  actionlint
else
  step "actionlint skipped (install with: brew install actionlint)"
fi
# Not run here: bundle.sh rebuilds target/release, which may be your credential helper (AGENTS.md).
step "bundle smoke test skipped (CI's bundle job runs it; locally, in a worktree: bash scripts/bundle.sh 0.0.0 && bash scripts/smoke-install.sh 0.0.0)"
printf '\nAll checks passed.\n'
