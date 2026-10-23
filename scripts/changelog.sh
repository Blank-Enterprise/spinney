#!/usr/bin/env bash
# CHANGELOG.md, in Keep a Changelog form.
#   changelog.sh check                 is it well formed? (check.sh and CI run this)
#   changelog.sh notes <tag>           print a release's notes (release.yml runs this): a stable
#                                      tag's own section, a beta's or rc's the Unreleased one
#   changelog.sh release <X.Y.Z> [date]
#                                      move Unreleased into a new X.Y.Z section, for the
#                                      version-bump PR before a stable release
#   changelog.sh pr <base>             does a PR that changes the app since <base> also change
#                                      CHANGELOG.md? (the changelog workflow runs this)
# Runs on the CHANGELOG.md of the current repo, or $CHANGELOG.
set -euo pipefail
file="${CHANGELOG:-$(git rev-parse --show-toplevel)/CHANGELOG.md}"
repo_url="https://github.com/Blank-Enterprise/spinney"

die() { echo "$*" >&2; exit 1; }
[ -f "$file" ] || die "no $file"

# The body of the `## [<name>]` section: everything up to the next `## ` or the link list.
section() {
  awk -v name="$1" '
    /^## \[/ { inside = (index($0, "## [" name "]") == 1); next }
    /^\[[^]]+\]: / { inside = 0 }
    inside { print }
  ' "$file" | sed -e '/./,$!d' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}'
}

case "${1:-}" in
  check)
    grep -q '^## \[Unreleased\]$' "$file" || die "$file needs a '## [Unreleased]' section"
    # Each release heading is `## [X.Y.Z] - YYYY-MM-DD`, newest first, with a link below.
    prev=""
    while read -r line; do
      [[ "$line" =~ ^##\ \[([0-9]+\.[0-9]+\.[0-9]+)\]\ -\ [0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] ||
        die "bad release heading: '$line' (want '## [X.Y.Z] - YYYY-MM-DD')"
      v="${BASH_REMATCH[1]}"
      if [ -n "$prev" ] && [ "$(printf '%s\n%s\n' "$v" "$prev" | sort -V | tail -1)" != "$prev" ]; then
        die "$v is listed above $prev; newest goes first"
      fi
      prev="$v"
      grep -q "^\[$v\]: " "$file" || die "no link for [$v] at the bottom of $file"
    done < <(grep '^## \[' "$file" | grep -v '^## \[Unreleased\]$')
    grep -q '^\[Unreleased\]: ' "$file" || die "no link for [Unreleased] at the bottom of $file"
    ;;
  notes)
    tag="${2:?usage: changelog.sh notes <tag>}"
    if [[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
      notes="$(section "${BASH_REMATCH[1]}")"
      [ -n "$notes" ] || die "$file has no section for ${BASH_REMATCH[1]}: run changelog.sh release ${BASH_REMATCH[1]} in the version-bump PR"
    elif [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+-(beta|rc)\.[0-9]+$ ]]; then
      notes="$(section Unreleased)"
    else
      die "not a release tag: $tag"
    fi
    printf '%s\n' "$notes"
    ;;
  release)
    version="${2:?usage: changelog.sh release <X.Y.Z> [date]}"
    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a version: $version"
    date="${3:-$(date -u +%Y-%m-%d)}"
    ! grep -q "^## \[$version\]" "$file" || die "$file already has a $version section"
    [ -n "$(section Unreleased)" ] || die "Unreleased is empty"
    # The previous release, for the compare link.
    prev="$(grep -m1 -oE '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' "$file" | tr -d '#[] ' || true)"
    tmp="$(mktemp)"
    awk -v v="$version" -v d="$date" -v url="$repo_url" -v prev="$prev" '
      $0 == "## [Unreleased]" { print; print ""; print "## [" v "] - " d; next }
      /^\[Unreleased\]: / {
        print "[Unreleased]: " url "/compare/v" v "...HEAD"
        if (prev != "") print "[" v "]: " url "/compare/v" prev "...v" v
        else print "[" v "]: " url "/releases/tag/v" v
        next
      }
      { print }
    ' "$file" > "$tmp"
    mv "$tmp" "$file"
    bash "$0" check
    echo "Moved Unreleased into $version ($date)."
    ;;
  pr)
    base="${2:?usage: changelog.sh pr <base>}"
    changed="$(git diff --name-only "$base"...HEAD)"
    # What reaches users: the app, its bundle and the installer.
    app="$(grep -E '^(src/|packaging/|install\.sh$)' <<<"$changed" || true)"
    if [ -n "$app" ] && ! grep -qx 'CHANGELOG.md' <<<"$changed"; then
      echo "This PR changes the app but not CHANGELOG.md:" >&2
      sed 's/^/  /' <<<"$app" | head -10 >&2
      die "Add a line under Unreleased, or label the PR 'skip changelog' if users won't notice."
    fi
    ;;
  *)
    die "usage: changelog.sh check | notes <tag> | release <X.Y.Z> [date] | pr <base>"
    ;;
esac
