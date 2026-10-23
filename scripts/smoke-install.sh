#!/usr/bin/env bash
# Check what bundle.sh built: the plist lints, install.sh installs the zip, and the binary is arm64.
# Usage: smoke-install.sh <version>, after bundle.sh <version>. Installs into a temp folder only.
set -euo pipefail

version="${1:?usage: smoke-install.sh <version>}"
cd "$(dirname "$0")/.."

plutil -lint dist/Spinney.app/Contents/Info.plist
tmp="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
apps="$(mktemp -d "${tmp%/}/spinney-apps.XXXXXX")"
trap 'rm -rf "$apps"' EXIT
out="$(SPINNEY_ZIP=dist/Spinney-macos-arm64.zip SPINNEY_APP_DIR="$apps" bash install.sh </dev/null)"
echo "$out"
[ "$out" = "Installed Spinney $version to $apps/Spinney.app" ] || { echo "unexpected install result" >&2; exit 1; }
lipo -archs "$apps/Spinney.app/Contents/MacOS/spinney" | grep -qw arm64 || { echo "binary is not arm64" >&2; exit 1; }
echo "smoke test passed ($version)"
