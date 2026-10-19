#!/usr/bin/env bash
# Build dist/Spinney.app and dist/Spinney-macos-arm64.zip. Usage: bundle.sh <version>
set -euo pipefail

version="${1:?usage: bundle.sh <version>}"
cd "$(dirname "$0")/.."

cargo build --release --locked

rm -rf dist
mkdir -p dist/Spinney.app/Contents/MacOS
cp target/release/spinney dist/Spinney.app/Contents/MacOS/spinney
sed "s/__VERSION__/$version/g" packaging/Info.plist > dist/Spinney.app/Contents/Info.plist

ditto -c -k --keepParent dist/Spinney.app dist/Spinney-macos-arm64.zip
echo "built dist/Spinney-macos-arm64.zip ($version)"
