#!/usr/bin/env bash
# Install the latest Spinney release into /Applications. SPINNEY_VERSION picks another: a tag
# such as v1.0.0 or v1.1.0-beta.1, or "beta" for the newest release, pre-releases included.
#   curl -fsSL https://raw.githubusercontent.com/Blank-Enterprise/spinney/main/install.sh | bash
set -euo pipefail

# The newest release's tag, pre-releases included; GitHub lists releases newest first.
newest_release() {
  if command -v gh >/dev/null 2>&1 && gh auth status --hostname github.com >/dev/null 2>&1; then
    gh api "repos/$1/releases?per_page=1" -q '.[0].tag_name' </dev/null
  else
    curl -fsSL "https://api.github.com/repos/$1/releases?per_page=1" \
      | sed -nE 's/^ *"tag_name": *"([^"]+)".*/\1/p' | head -1
  fi
}

main() {
  local REPO="${SPINNEY_REPO:-Blank-Enterprise/spinney}"
  local APP_DIR="${SPINNEY_APP_DIR:-/Applications}"
  local VERSION="${SPINNEY_VERSION:-}"
  local ASSET="Spinney-macos-arm64.zip"
  local dest version base

  if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "Spinney supports macOS on Apple Silicon only." >&2
    return 1
  fi
  if [ "$VERSION" = beta ]; then
    VERSION="$(newest_release "$REPO")" || true
    [ -n "$VERSION" ] && [ "$VERSION" != null ] || { echo "Could not find the newest release of $REPO." >&2; return 1; }
  fi
  if [ -n "$VERSION" ] && [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]]; then
    echo 'SPINNEY_VERSION must be a release tag like v1.0.0, or "beta".' >&2
    return 1
  fi
  if [ -n "$VERSION" ]; then
    base="https://github.com/$REPO/releases/download/$VERSION"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi

  tmp="$(mktemp -d)" # global: the EXIT trap runs after main returns
  trap 'rm -rf "$tmp"' EXIT

  # The zip and its checksum; gh first when signed in (no anonymous rate limit), then plain curl.
  if [ -n "${SPINNEY_ZIP:-}" ]; then
    cp "$SPINNEY_ZIP" "$tmp/$ASSET"
  elif command -v gh >/dev/null 2>&1 && gh auth status --hostname github.com >/dev/null 2>&1 \
    && gh release download ${VERSION:+"$VERSION"} --repo "$REPO" --dir "$tmp" \
      --pattern "$ASSET" --pattern "$ASSET.sha256" </dev/null; then
    :
  elif curl -fsSL -o "$tmp/$ASSET" "$base/$ASSET" \
    && curl -fsSL -o "$tmp/$ASSET.sha256" "$base/$ASSET.sha256"; then
    :
  else
    echo "Could not download ${VERSION:-the latest} $ASSET from $REPO." >&2
    echo "Check the version exists: https://github.com/$REPO/releases" >&2
    return 1
  fi
  if [ -z "${SPINNEY_ZIP:-}" ] && ! (cd "$tmp" && shasum -a 256 -c -s "$ASSET.sha256"); then
    echo "$ASSET does not match its published checksum; not installing." >&2
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

  # SpinneyVersion has a pre-release's full version (1.1.0-beta.1); older bundles lack it.
  version="$(/usr/libexec/PlistBuddy -c 'Print SpinneyVersion' "$dest/Contents/Info.plist" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$dest/Contents/Info.plist")"
  echo "Installed Spinney $version to $dest"
}

main "$@"
