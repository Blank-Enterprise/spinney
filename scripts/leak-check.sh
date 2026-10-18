#!/usr/bin/env bash
# Fails if tracked or staged files contain machine/personal identifiers.
# Patterns are computed at runtime; none are hard-coded.
# Exit: 0 clean, 1 match found, 2 git error (never fails open).
cd "$(git rev-parse --show-toplevel)" || exit 2
pats=(-e '/Users/' -e '/home/')
for v in "$(whoami)" "$(hostname -s)" "$(git config --global user.email)"; do
  [ -n "$v" ] && pats+=(-e "$v")
done
# Tracked files (working tree) plus staged blobs; this script itself is excluded
# since it names the generic path prefixes.
out=""
for mode in "" "--cached"; do
  # shellcheck disable=SC2086
  res=$(git grep -nI -F $mode "${pats[@]}" -- . ':!scripts/leak-check.sh')
  rc=$?
  if [ "$rc" -gt 1 ]; then echo "leak-check: git grep failed (status $rc)" >&2; exit 2; fi
  [ -n "$res" ] && out+="$res"$'\n'
done
if [ -n "$out" ]; then printf '%s' "$out" | sort -u; echo "leak-check: FAILED"; exit 1; fi
echo "leak-check: ok"
