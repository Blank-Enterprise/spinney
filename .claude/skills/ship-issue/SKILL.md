---
name: ship-issue
description: Take a Spinney GitHub issue from reading it to an open pull request, following the repo's conventions (branch, tests, checks, PR body, milestone, board status). Use when asked to implement, fix or "put up a PR for" an issue in this repo.
---

# Ship an issue

Read `AGENTS.md` first if it isn't already in context.

## 1. Understand
- `gh issue view <n> --comments`. Note the acceptance criteria and any linked issues ("Part of #…", "moved to #…").
- If it's an epic (label `epic`), stop and ask which sub-issue to take.
- If it's labelled `needs decision` or `blocked`, ask before starting.
- Read the code it touches, and the closest existing feature to copy patterns from (`git log --oneline` helps).

## 2. Branch
Start from the latest main. Use a worktree when other work may be happening in parallel:
```sh
git fetch origin
git switch -c feat/<short-name> origin/main    # or fix/…, ci/…, docs/…, chore/…
```

## 3. Implement
- Follow "Patterns to follow" and "Safety rules" in AGENTS.md: `Action` variants, jobs for network work, confirmations for anything destructive, pure functions with tests.
- Write the tests named in the acceptance criteria, using real temp repos.
- If an acceptance criterion can't be met, or you change behaviour beyond the issue, write down why for the PR.

## 4. Verify
```sh
bash scripts/check.sh
```
Everything must pass. To try UI changes, use a debug build (`cargo run`) and not `target/release` (see Pitfalls in AGENTS.md). If you couldn't look at it on screen, say so in the PR.

## 5. Commit and PR
- **Commit:** a conventional message (`feat(scope): …`) with a body saying why, and `Resolves #<n>` when it closes the issue.
- **Push:** `git push -u origin <branch>`.
- **Open the PR:** `gh pr create --milestone "<the issue's milestone>"`, with the body following `.github/pull_request_template.md`.
  - **Behaviour:** what changed.
  - **Notes:** decisions and deviations from the issue.
  - **Testing:** tests added, check results, and what was **not** checked in the running app.
  - End with the "🤖 Generated with Claude Code" line only. No session links.

## 6. Board
Move the issue to **In review** on the [Spinney roadmap board](https://github.com/users/SameetKeshav/projects/4):
```sh
P=PVT_kwHOBktr-c4Bny3h
ITEM=$(gh api graphql -f query='{ node(id:"'$P'"){ ... on ProjectV2 { items(first:100){ nodes{ id content{ ... on Issue { number } } } } } } }' \
  -q '.data.node.items.nodes[] | select(.content.number==<n>) | .id')
gh api graphql -f query='mutation{ updateProjectV2ItemFieldValue(input:{projectId:"'$P'",itemId:"'$ITEM'",
  fieldId:"PVTSSF_lAHOBktr-c4Bny3hzhmV1Ho",value:{singleSelectOptionId:"da59e90c"}}){ clientMutationId } }'
```
The board has more than 100 items, so page with `after:` if the issue isn't in the first 100.

Status option ids:

| Status | Id |
|---|---|
| Backlog | `f75ad846` |
| Ready | `209ce186` |
| In progress | `47fc9ee4` |
| In review | `da59e90c` |
| Blocked | `2e979f1b` |
| Done | `98236657` |

A new issue also needs adding (`addProjectV2ItemById`), plus its Start date, Target date and Area. Field ids: `gh project field-list 4 --owner SameetKeshav`.

## 7. Report
Give the PR link, what changed, the check results, and anything unverified or deviating from the issue.
