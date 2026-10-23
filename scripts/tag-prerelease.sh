#!/usr/bin/env bash
# Tag origin/main as the next beta or release candidate and push the tag, which starts the
# release workflow. Usage: tag-prerelease.sh <beta|rc>
set -euo pipefail

channel="${1:?usage: tag-prerelease.sh <beta|rc>}"
script_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$(git rev-parse --show-toplevel)"

git fetch --quiet --tags origin main
sha="$(git rev-parse origin/main)"
# The version main's Cargo.toml has, not this checkout's: the [package] version comes first.
version="$(git show "$sha:Cargo.toml" | sed -nE 's/^version = "([^"]+)"$/\1/p' | head -1)"
tag="$(bash "$script_dir/release-tag.sh" next "$channel" "$version")"

read -r -p "Tag $(git log -1 --format='%h %s' "$sha") as $tag and push it? [y/N] " answer
[[ "$answer" == [yY] ]] || { echo "Not tagged."; exit 1; }
git tag -a "$tag" -m "$tag" "$sha"
git push origin "refs/tags/$tag"
echo "Pushed $tag. The release workflow builds it and publishes it as a pre-release."
