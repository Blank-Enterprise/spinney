#!/usr/bin/env bash
# Fails if tracked or staged files contain machine/personal identifiers.
# Patterns are computed at runtime; none are hard-coded.
cd "$(git rev-parse --show-toplevel)" || exit 2
pats=(-e '/Users/' -e '/home/')
for v in "$(whoami)" "$(hostname -s)" "$(git config --global user.email)"; do
  [ -n "$v" ] && pats+=(-e "$v")
done
# Tracked files (working tree) plus staged blobs; this script itself is excluded
# since it names the generic path prefixes.
out=$( { git grep -nI -F "${pats[@]}" -- . ':!scripts/leak-check.sh'
         git grep -nI -F --cached "${pats[@]}" -- . ':!scripts/leak-check.sh'; } 2>/dev/null | sort -u)
if [ -n "$out" ]; then echo "$out"; echo "leak-check: FAILED"; exit 1; fi
echo "leak-check: ok"
