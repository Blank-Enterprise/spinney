#!/usr/bin/env bash
# Check the crate builds on the rust-version Cargo.toml declares.
# Usage: msrv-check.sh [--install | --if-installed]
#   --install       install that toolchain first (CI)
#   --if-installed  skip, with a note, when it isn't installed (check.sh)
set -euo pipefail
cd "$(dirname "$0")/.."

msrv="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].rust_version')"
case "${1:-}" in
  --install) rustup toolchain install "$msrv" --profile minimal ;;
  --if-installed)
    # Not `rustup run`: newer rustup may install a missing toolchain instead of failing.
    if ! rustup toolchain list | grep -Eq "^${msrv//./\\.}([.-]|$)"; then
      echo "rust $msrv not installed, skipped (install with: rustup toolchain install $msrv --profile minimal)"
      exit 0
    fi
    ;;
  "") ;;
  *) echo "usage: msrv-check.sh [--install | --if-installed]" >&2; exit 2 ;;
esac
cargo +"$msrv" check --locked --all-targets
