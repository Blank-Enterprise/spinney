#!/usr/bin/env bash
# Install the latest Spinney release into /Applications.
set -euo pipefail

main() {
  local REPO="${SPINNEY_REPO:-SameetKeshav/spinney}"
  local APP_DIR="${SPINNEY_APP_DIR:-/Applications}"
  local ASSET="Spinney-macos-arm64.zip"
  local dest version

  if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "Spinney supports macOS on Apple Silicon only." >&2
    return 1
  fi

  tmp="$(mktemp -d)" # global: the EXIT trap runs after main returns
  trap 'rm -rf "$tmp"' EXIT

  if [ -n "${SPINNEY_ZIP:-}" ]; then
    cp "$SPINNEY_ZIP" "$tmp/$ASSET"
  elif command -v gh >/dev/null 2>&1 && gh auth status --hostname github.com >/dev/null 2>&1 \
    && gh release download --repo "$REPO" --pattern "$ASSET" --dir "$tmp" </dev/null; then
    :
  elif curl -fsSL -o "$tmp/$ASSET" "https://github.com/$REPO/releases/latest/download/$ASSET"; then
    :
  else
    echo "Could not download $ASSET from $REPO." >&2
    echo "If the repo is private, install the GitHub CLI and run 'gh auth login'." >&2
    return 1
  fi

  mkdir "$tmp/out"
  ditto -x -k "$tmp/$ASSET" "$tmp/out"
  [ -d "$tmp/out/Spinney.app" ] || { echo "Archive does not contain Spinney.app." >&2; return 1; }

  # Quit a running copy (only when installing to the real location).
  if [ -z "${SPINNEY_APP_DIR:-}" ]; then
    osascript -e 'quit app "Spinney"' >/dev/null 2>&1 || true
    sleep 1
  fi

  dest="$APP_DIR/Spinney.app"
  if ! { rm -rf "$dest.new" && mv "$tmp/out/Spinney.app" "$dest.new"; } 2>/dev/null; then
    rm -rf "$dest.new" 2>/dev/null || true
    echo "Cannot write to $APP_DIR; re-run with sudo or set SPINNEY_APP_DIR." >&2
    return 1
  fi
  rm -rf "$dest"
  mv "$dest.new" "$dest"
  xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true

  version="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$dest/Contents/Info.plist")"
  echo "Installed Spinney $version to $dest"
}

main "$@"
