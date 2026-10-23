#!/usr/bin/env bash
# Tests for release-tag.sh, against a temporary repo and its tags.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/release-tag.sh"
repo="$(mktemp -d)"
trap 'rm -rf "$repo"' EXIT
cd "$repo"
git init --quiet
mkdir src && : > src/lib.rs
set_version() { printf '[package]\nname = "x"\nversion = "%s"\nedition = "2021"\n' "$1" > Cargo.toml; }
set_version 1.1.0
git add -A && git commit --quiet -m init

failed=0
expect() { # expect <want> <args...>: stdout, or "error" when it fails
  local want="$1" got
  shift
  got="$(bash "$script" "$@" 2>/dev/null)" || got=error
  [ -n "$got" ] || got=ok
  if [ "$got" = "$want" ]; then echo "ok   $* -> $want"; else echo "FAIL $* -> $got, want $want"; failed=1; fi
}

git tag v1.0.0
expect v1.1.0-beta.1 next beta
expect v1.1.0-rc.1 next rc
expect error next alpha
expect v2.0.0-beta.1 next beta 2.0.0
git tag v1.1.0-beta.1 && git tag v1.1.0-beta.2 && git tag v1.1.0-beta.10 && git tag v1.0.0-beta.7
expect v1.1.0-beta.11 next beta
expect v1.1.0-rc.1 next rc
git tag v1.1.0-rc.1
expect v1.1.0-rc.2 next rc
expect error next beta

expect ok check v1.1.0
expect error check v1.0.0
expect ok check v1.1.0-beta.3
expect ok check v1.1.0-rc.2
expect error check v1.2.0-beta.1
expect error check v1.1.0-alpha.1
expect error check v1.1.0-beta.0
expect error check v1.1.0-beta
expect error check 1.1.0
git tag v1.1.0
expect ok check v1.1.0
expect error check v1.1.0-rc.3
expect error next rc

exit "$failed"
