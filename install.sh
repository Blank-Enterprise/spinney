#!/usr/bin/env bash
# Install the latest Spinney release into /Applications.
set -euo pipefail

REPO="${SPINNEY_REPO:-SameetKeshav/spinney}"
APP_DIR="${SPINNEY_APP_DIR:-/Applications}"
ASSET="Spinney-macos-arm64.zip"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "Spinney supports macOS on Apple Silicon only." >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if [ -n "${SPINNEY_ZIP:-}" ]; then
  cp "$SPINNEY_ZIP" "$tmp/$ASSET"
elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  gh release download --repo "$REPO" --pattern "$ASSET" --dir "$tmp"
elif curl -fsSL -o "$tmp/$ASSET" "https://github.com/$REPO/releases/latest/download/$ASSET"; then
  :
else
  echo "Could not download $ASSET from $REPO." >&2
  echo "If the repo is private, install the GitHub CLI and run 'gh auth login'." >&2
  exit 1
fi

mkdir "$tmp/out"
ditto -x -k "$tmp/$ASSET" "$tmp/out"
[ -d "$tmp/out/Spinney.app" ] || { echo "Archive does not contain Spinney.app." >&2; exit 1; }

# Quit a running copy (only when installing to the real location).
if [ -z "${SPINNEY_APP_DIR:-}" ]; then
  osascript -e 'quit app "Spinney"' >/dev/null 2>&1 || true
  sleep 1
fi

dest="$APP_DIR/Spinney.app"
rm -rf "$dest"
mv "$tmp/out/Spinney.app" "$dest"
xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true

version="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$dest/Contents/Info.plist")"
echo "Installed Spinney $version to $dest"
