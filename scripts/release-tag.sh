#!/usr/bin/env bash
# Release tags, checked against the Cargo.toml version and the repo's tags.
#   release-tag.sh check <tag>       may this tag be released? (release.yml runs this)
#   release-tag.sh next <beta|rc> [version]
#                                    print the next pre-release tag, e.g. v1.1.0-beta.2
#                                    (version defaults to the Cargo.toml one)
# A stable tag (v1.1.0) must equal the Cargo.toml version. A pre-release (v1.1.0-beta.1,
# v1.1.0-rc.1) is for the release Cargo.toml is heading to, so it needs no version bump,
# but not once that release is out. Runs in the current repo, not the script's.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

die() { echo "$*" >&2; exit 1; }
cargo_version() { cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version'; }
has_tag() { [ -n "$(git tag -l "$1")" ]; }
# Highest N among v<version>-<channel>.N tags, or 0.
last_n() {
  git tag -l "v$1-$2.*" | sed -nE "s/^v[0-9.]+-$2\.([0-9]+)$/\1/p" | sort -n | tail -1 | grep . || echo 0
}

case "${1:-}" in
  check)
    tag="${2:?usage: release-tag.sh check <tag>}"
    version="$(cargo_version)"
    if [[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
      [ "${BASH_REMATCH[1]}" = "$version" ] || die "Cargo.toml is $version but the tag is $tag"
    elif [[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)-(beta|rc)\.[1-9][0-9]*$ ]]; then
      [ "${BASH_REMATCH[1]}" = "$version" ] || die "$tag is a pre-release of ${BASH_REMATCH[1]}, but Cargo.toml is $version"
      ! has_tag "v$version" || die "v$version is already released; bump Cargo.toml for the next one"
    else
      die "not a release tag: $tag (vX.Y.Z, vX.Y.Z-beta.N or vX.Y.Z-rc.N)"
    fi
    ;;
  next)
    channel="${2:-}"
    [[ "$channel" == beta || "$channel" == rc ]] || die "usage: release-tag.sh next <beta|rc> [version]"
    version="${3:-$(cargo_version)}"
    ! has_tag "v$version" || die "v$version is already released; bump Cargo.toml for the next one"
    if [ "$channel" = beta ] && [ "$(last_n "$version" rc)" != 0 ]; then
      die "v$version already has a release candidate; cut an rc instead"
    fi
    echo "v$version-$channel.$(($(last_n "$version" "$channel") + 1))"
    ;;
  *) die "usage: release-tag.sh check <tag> | next <beta|rc> [version]" ;;
esac
