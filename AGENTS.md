# Working on Spinney

Spinney is a lightweight Git desktop client for macOS on Apple Silicon, written in Rust with egui (eframe 0.37). It's built around worktrees: each project is a normal clone, with its worktrees beside it in `<project>.worktrees/`. Worktrees made by anything else (CLI, IDEs, agents) are found through `git worktree list`, never by folder name. Old `.bare` projects still open; #160 removes that layout.

This file is for anyone changing the code, human or agent. Read it before your first change.

## Commands

```sh
bash scripts/check.sh          # everything CI runs: fmt, clippy -D warnings, tests, leak check
cargo test --locked <name>     # one test, or every test whose name contains <name>
cargo build --locked           # debug build: target/debug/spinney
cargo run                      # run the app (debug)
bash scripts/bundle.sh 1.2.0   # release .app and .zip in dist/ (what the release workflow does)
```

- **Rust 1.95+** (`rust-version` in `Cargo.toml`). The release workflow checks that the declared version still builds.
- **Tests** create temporary git repos and need a global git identity (`git config --global user.name` and `user.email`). CI sets one.
- **The leak check** (`scripts/leak-check.sh`) fails if any tracked file contains a macOS or Linux home-directory path, your username, your hostname or your global git email. Use neutral paths such as `/p/project` or `/tmp/x` in tests and docs.

## Architecture

| File | What it does |
|---|---|
| `src/main.rs` | Entry point. `spinney credential <space> get` makes the binary act as git's credential helper. |
| `src/git.rs` | Runs git: `run`, `run_ok_codes`, `run_stdin`, `output`. Always sets `LC_ALL=C`, `GIT_TERMINAL_PROMPT=0` and `GIT_EDITOR=true`. |
| `src/actions.rs` | Git operations behind UI actions: commit and amend, fetch, pull, push and force push with lease, checkout, branches, merge and rebase. |
| `src/stage.rs` | Line and hunk patches for staging, unstaging and discarding. |
| `src/todo.rs` | Interactive rebase: the editable todo list, run through `GIT_SEQUENCE_EDITOR` with `exec` amends for new messages. |
| `src/stash.rs` | Stash list and actions, addressed by commit hash and not index. |
| `src/mergetool.rs` | "Open in merge tool": runs `git mergetool --no-prompt` for a conflicted file with the configured tool, stdin closed. |
| `src/convert.rs` | Converts a normal clone to the worktree layout, with an undo journal and rollback. |
| `src/project.rs` | Clone and create, worktrees (where they go, listing, repair and relink), and finding the project for any worktree folder. |
| `src/refs.rs`, `src/status.rs`, `src/diff.rs`, `src/graph.rs` | Parse `for-each-ref`, `status`, `diff` and `log` output. |
| `src/conflict.rs` | Parses a conflicted file's `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` blocks, and writes them back with a side picked for each (the built-in merge tool). |
| `src/creds.rs`, `src/ssh.rs`, `src/github.rs` | Keychain tokens and the credential helper; per-space SSH keys; the GitHub API and device-flow sign-in. |
| `src/config.rs` | `config.toml`: spaces and settings. New fields need `#[serde(default)]`, so older configs still load. |
| `src/ui/mod.rs` | `SpinneyApp`, the `Action` enum and `apply()`, background `Jobs`, and the top confirmation bars. |
| `src/ui/*.rs` | Panels: `toolbar`, `sidebar`, `graph_view`, `changes`, `diff_view`, `conflict_view`, `merge_view` (built-in merge tool), `dialogs` (modals), `in_progress` (merge/rebase banner), `branch_drop` (drag a branch onto another), `spaces` (account editor), `new_tab`, `theme`. |

## Patterns to follow

- **UI events become `Action`s.** Widgets push an `Action`, and `SpinneyApp::apply` handles it. Add a variant rather than mutating state from a widget.
- **Slow or network git runs as a job.** Use `Jobs::spawn` (fetch, pull, push, clone, conversion, remote branch delete). Check `jobs.network_busy(root)` before anything that touches the same worktree, as stash and merge do. Local operations (commit, checkout, merge, rebase, stash) currently run on the UI thread. #16 tracks moving the slow ones off it.
- **Confirmations:**
  - inline bars use `confirm_discard`, `confirm_amend`, `rejected_push`, `confirm_op` and `confirm_resolve`, found through `asking_tab`;
  - modals use `egui::Modal` with the `danger()` button in `ui/dialogs.rs`;
  - pending bars are tied to a project root, and `forget_pending` clears them when a project is converted.
- **Pure logic first.** Keep decisions (labels, enabled states, git argument lists, parsers) in pure functions with unit tests, and keep the egui code thin. The UI isn't tested by rendering, so there are no screenshot tests.
- **Tests use real temp repos.** Copy the fixtures in `src/actions.rs` (`cloned()`, a file:// remote) and `src/stage.rs`. Each issue's acceptance criteria name the tests it expects.
- **Match the surrounding code:** short doc comments that say why, the same naming, and the same comment density.

## Safety rules (these protect users' work)

- **Destructive actions always confirm**, naming what is lost: discard, branch delete, reset, force push, stash drop, use mine/theirs.
- **Never `git push --force`.** Force pushes use `--force-with-lease=<ref>:<sha>`, pinned to the commit the user was shown.
- **A stopped merge, rebase, cherry-pick or revert is always visible.** Conflicts stay in place under the in-progress banner (Continue, Skip, Abort…), never silently half-done. A squash merge has no MERGE_HEAD for the banner to drive, so its conflicts are undone instead. See `actions::stopped`.
- **Paths go to git as literal pathspecs** (`--literal-pathspecs`). Deleting untracked files only ever deletes a regular file inside the worktree.
- **Tokens only go to the space's own host.** See `creds::helper_response`.
- **Git must never wait on input.** No editor, no terminal prompt.

## Conventions

- **Branches:** `feat/…`, `fix/…`, `ci/…`, `docs/…`, `chore/…`.
- **Commits:** conventional commits, e.g. `feat(sidebar): …`, `fix(convert): …`, `ci: …`, with a short body saying why.
- **PRs:**
  - follow `.github/pull_request_template.md`, with `Resolves #N` and a **Testing** section that says plainly what wasn't checked in the running app;
  - set the issue's milestone;
  - one issue per PR;
  - never paste links to AI or chat sessions in PRs.
- **Issues:** epics (label `epic`) hold sub-issues. Labels mark type (`bug`, `enhancement`, `security`, `release`, `ci`, `chore`), `priority: …`, `size: XS…XL`, and state (`blocked`, `needs decision`, `needs QA`). Start from the roadmap, #31.
- **Board:** the [Spinney roadmap project](https://github.com/orgs/Blank-Enterprise/projects/1), with Status Backlog → Ready → In progress → In review → Blocked → Done. Move an issue to **In review** when its PR opens.

## Pitfalls we hit

- **Don't rebuild `target/release/spinney` in a checkout whose repos use it as their credential helper.** Each rebuild gets a new ad-hoc signature, so macOS asks for Keychain access again, and `git fetch`/`git push` silently hang until someone clicks **Always Allow**. To try a change in the app, build in a separate worktree, or use `cargo run` (debug), and launch that binary.
- **Pushing changes to `.github/workflows/` needs a token with the `workflow` scope.** Spinney's sign-in requests it. Older tokens and `gh` logins may not have it.
- **Terminals can't screenshot the app** without macOS screen-recording permission. Say in the PR that UI changes weren't checked on screen.
- **Releases:** bump `version` in `Cargo.toml`, merge to `main`, then push a `vX.Y.Z` tag on `main`. The release workflow refuses a tag that's off `main` or doesn't match `Cargo.toml`.
