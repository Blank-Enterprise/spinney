#!/usr/bin/env bash
# Build dist/Spinney.app and dist/Spinney-macos-arm64.zip. Usage: bundle.sh <version>
# The version is X.Y.Z, or X.Y.Z-beta.N / X.Y.Z-rc.N for a pre-release.
set -euo pipefail

version="${1:?usage: bundle.sh <version>}"
# Apple's bundle versions allow only digits and periods, so a pre-release has X.Y.Z there and
# its full version in SpinneyVersion, which install.sh reports.
[[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)(-[0-9A-Za-z.]+)?$ ]] || { echo "not a version: $version" >&2; exit 1; }
bundle_version="${BASH_REMATCH[1]}"
cd "$(dirname "$0")/.."

cargo build --release --locked

rm -rf dist
mkdir -p dist/Spinney.app/Contents/MacOS
cp target/release/spinney dist/Spinney.app/Contents/MacOS/spinney
sed -e "s/__BUNDLE_VERSION__/$bundle_version/g" -e "s/__VERSION__/$version/g" packaging/Info.plist > dist/Spinney.app/Contents/Info.plist

ditto -c -k --keepParent dist/Spinney.app dist/Spinney-macos-arm64.zip
echo "built dist/Spinney-macos-arm64.zip ($version)"
