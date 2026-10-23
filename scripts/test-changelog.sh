#!/usr/bin/env bash
# Tests for changelog.sh, against temporary changelogs.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/changelog.sh"
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
export CHANGELOG="$dir/CHANGELOG.md"
url=https://github.com/Blank-Enterprise/spinney

failed=0
expect() { # expect <want> <args...>: "ok" or "error"
  local want="$1" got=ok
  shift
  bash "$script" "$@" >/dev/null 2>&1 || got=error
  if [ "$got" = "$want" ]; then echo "ok   $* -> $want"; else echo "FAIL $* -> $got, want $want"; failed=1; fi
}
expect_out() { # expect_out <want> <args...>: stdout, exactly
  local want="$1" got
  shift
  got="$(bash "$script" "$@" 2>/dev/null)" || got=error
  if [ "$got" = "$want" ]; then echo "ok   $*"; else printf 'FAIL %s\n got: %s\nwant: %s\n' "$*" "$got" "$want"; failed=1; fi
}
write() { printf '%s\n' "$@" > "$CHANGELOG"; }

write "# Changelog" "" "## [Unreleased]" "" "### Fixed" "" "- A thing." "" \
  "## [1.0.0] - 2026-10-23" "" "### Added" "" "- Everything." "" \
  "[Unreleased]: $url/compare/v1.0.0...HEAD" "[1.0.0]: $url/releases/tag/v1.0.0"
expect ok check
expect_out "$(printf '### Fixed\n\n- A thing.')" notes v1.1.0-rc.1
expect_out "$(printf '### Fixed\n\n- A thing.')" notes v1.1.0-beta.3
expect_out "$(printf '### Added\n\n- Everything.')" notes v1.0.0
expect error notes v1.1.0 # a stable release needs its own section
expect error notes 1.1.0
expect error release 1.0.0 # already there
expect error release 1.1

expect ok release 1.1.0 2026-11-19
expect ok check
expect_out "$(printf '### Fixed\n\n- A thing.')" notes v1.1.0
expect_out "" notes v1.2.0-beta.1 # nothing unreleased yet
grep -qx "\[1.1.0\]: $url/compare/v1.0.0...v1.1.0" "$CHANGELOG" || { echo "FAIL compare link"; failed=1; }
grep -qx "\[Unreleased\]: $url/compare/v1.1.0...HEAD" "$CHANGELOG" || { echo "FAIL unreleased link"; failed=1; }
expect error release 1.2.0 # Unreleased is empty

# Malformed files.
write "# Changelog" "## [1.0.0] - 2026-10-23" "[1.0.0]: x"
expect error check # no Unreleased
write "## [Unreleased]" "## [1.0.0]" "[Unreleased]: x" "[1.0.0]: x"
expect error check # no date
write "## [Unreleased]" "## [1.0.0] - 2026-10-23" "## [1.1.0] - 2026-11-19" "[Unreleased]: x" "[1.0.0]: x" "[1.1.0]: x"
expect error check # oldest first
write "## [Unreleased]" "## [1.0.0] - 2026-10-23" "[Unreleased]: x"
expect error check # no link

# pr: a temporary repo where a branch changes the app with and without the changelog.
repo="$dir/repo"
git init --quiet "$repo" && cd "$repo"
git config user.name t && git config user.email t@example.com
mkdir src && : > src/main.rs && : > README.md && : > CHANGELOG.md
git add -A && git commit --quiet -m base && base="$(git rev-parse HEAD)"
echo "docs" >> README.md && git commit --quiet -am docs
expect ok pr "$base" # docs only
echo "fn main() {}" >> src/main.rs && git commit --quiet -am app
expect error pr "$base" # the app changed, the changelog didn't
echo "- A thing." >> CHANGELOG.md && git commit --quiet -am changelog
expect ok pr "$base"

exit "$failed"
