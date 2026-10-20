# Spinney

A lightweight Git desktop client for macOS on Apple Silicon, written in Rust with
[egui](https://github.com/emilk/egui).

Spinney is built around **worktrees**. Each project is a normal clone, so your editor, terminal
and coding agents see an ordinary repository. Its worktrees live beside it in
`<project>.worktrees/`, so two branches can be open, built and run side by side without
stashing. Worktrees made outside Spinney, by `git worktree add`, an IDE or an agent, show up
and work like its own. Moved ones are relinked when the project opens.

<!-- Screenshot of the main window goes here once docs/screenshot.png is added:
![Spinney's main window: branch sidebar, commit graph and staged changes](docs/screenshot.png)
-->

## Features

- **Spaces:** keep work and personal accounts apart. Each space has its own GitHub or GitHub
  Enterprise account (device-flow sign-in or a token), SSH key, author identity and clone folder.
  Tokens live in the macOS Keychain, and Spinney gives them only to the space's own host.
- **Projects:** clone from a URL or by searching your repositories; create, open recent, and one
  tab per project. Opening any worktree folder opens its project.
- **Worktrees:** add and remove them, or remove a worktree and delete its branch in one step.
  Missing ones can be located or removed; locked ones are left alone. The **Worktrees** view
  (the toggle above the sidebar's filter) lists every worktree, the agents running in it
  (Claude Code, Codex…) and how far it is from the default branch. Select one to list what it
  changed since it branched, committed or not, with the first file's diff open.
- **Branches:** a sidebar grouped into folders, with checkout, create, rename (also on the remote),
  delete local and remote, and fast-forward without checking out. Merge or rebase from the branch
  menu, or drag one branch onto another.
- **Commit graph** with branch and tag labels. Right-click a commit to copy it, check it out,
  branch, tag or open a worktree from it, cherry-pick, revert, reset (soft, mixed or hard) or start
  an interactive rebase. A detached HEAD is shown, and leaving commits no branch keeps asks first.
- **Changes:** stage, unstage or discard a file, a hunk or single lines; commit and amend. When line
  staging isn't possible (binary, non-UTF-8, renamed, a file turned symlink…), Spinney says why.
- **Sync:** fetch, push, and pull by fast-forward, merge or rebase. When a push is rejected,
  Spinney offers a force push with lease, pinned to the commit you were shown.
- **Conflicts:** a merge, rebase, cherry-pick, revert or pull that stops on conflicts stays in
  progress, with Continue, Skip and Abort in a banner. View each conflicted file with its blocks
  highlighted, take your version or theirs, resolve it in the built-in merge tool, or open it in
  an external one.
- **Tags:** a sidebar section; create lightweight or annotated tags, push and delete them.
- **Stash:** stash and pop from the toolbar; apply, pop and drop from the sidebar.

Anything that can lose work asks first and says what will be lost.

## Install

Requires macOS on Apple Silicon.

    curl -fsSL https://raw.githubusercontent.com/Blank-Enterprise/spinney/main/install.sh | bash

This installs the latest release into `/Applications` after checking it against its published
SHA-256 checksum. To install a specific release, set `SPINNEY_VERSION`:

    curl -fsSL https://raw.githubusercontent.com/Blank-Enterprise/spinney/main/install.sh | SPINNEY_VERSION=v1.0.0 bash

The app isn't notarized yet ([#18](https://github.com/Blank-Enterprise/spinney/issues/18)). The
install script is the supported way in. If you download the zip from the releases page in a
browser, Gatekeeper refuses to open it until you allow it in System Settings → Privacy & Security.

While the repo is private, the curl command can't reach it. Use the GitHub CLI
(`gh auth login`) instead:

    gh api repos/Blank-Enterprise/spinney/contents/install.sh -H "Accept: application/vnd.github.raw" | bash

## Build from source

Requires Rust 1.95 or newer.

    cargo run                      # debug build
    cargo build --release          # target/release/spinney
    bash scripts/bundle.sh 1.2.0   # Spinney.app and a zip in dist/

## Status

Spinney is early and under active development. The
[roadmap (#31)](https://github.com/Blank-Enterprise/spinney/issues/31) lists what's planned, and the
[milestones](https://github.com/Blank-Enterprise/spinney/milestones) show what's next.

There's no Preferences window yet ([#84](https://github.com/Blank-Enterprise/spinney/issues/84)).
Until there is, two settings live in `~/Library/Application Support/Spinney/config.toml`:
`pull_strategy` (`"ff-only"`, `"merge"` or `"rebase"`; without it, Spinney follows your git config)
and `merge_tool` (`"vscode"`, `"file-merge"`, `"kaleidoscope"`, `"beyond-compare"` or `"git"`).

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first.
Report security problems privately, as [SECURITY.md](SECURITY.md) describes. Everyone taking part
follows the [code of conduct](CODE_OF_CONDUCT.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in Spinney, as defined in the Apache-2.0 license, is dual licensed as above,
without any additional terms or conditions.
