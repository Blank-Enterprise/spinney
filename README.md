# Spinney

A lightweight Git desktop client for macOS on Apple Silicon, written in Rust with
[egui](https://github.com/emilk/egui).

Spinney is built around **worktrees**. Each project is a `.bare` repository plus one folder per
branch, so switching branches never means stashing, and two branches can be open, built and run
side by side. Spinney creates projects in that layout, converts an existing clone to it (with an
undo journal if anything goes wrong), and manages the worktrees for you. A normal clone works too.

<!-- Screenshot of the main window goes here once docs/screenshot.png is added:
![Spinney's main window: branch sidebar, commit graph and staged changes](docs/screenshot.png)
-->

## Features

- **Spaces:** keep work and personal accounts apart. Each space has its own GitHub or GitHub
  Enterprise account (device-flow sign-in or a token), SSH key, author identity and clone folder.
  Tokens live in the macOS Keychain, and Spinney gives them only to the space's own host.
- **Projects:** clone from a URL or by searching your repositories, create, open recent, and one
  tab per project.
- **Branches:** a sidebar grouped into folders, checkout, create, delete local and remote, and
  merge or rebase from the branch menu.
- **Commit graph** with branch and tag labels.
- **Changes:** stage, unstage or discard a file, a hunk or single lines; commit and amend.
- **Sync:** fetch, pull (fast-forward only) and push. When a push is rejected, Spinney offers a
  force push with lease, pinned to the commit you were shown.
- **Stash:** stash and pop from the toolbar; apply, pop and drop from the sidebar.

Anything that can lose work asks first and says what will be lost.

## Install

Requires macOS on Apple Silicon.

    curl -fsSL https://raw.githubusercontent.com/SameetKeshav/spinney/main/install.sh | bash

This installs the latest release into `/Applications` after checking it against its published
SHA-256 checksum. To install a specific release, set `SPINNEY_VERSION`:

    curl -fsSL https://raw.githubusercontent.com/SameetKeshav/spinney/main/install.sh | SPINNEY_VERSION=v1.1.0 bash

The app isn't notarized yet ([#18](https://github.com/SameetKeshav/spinney/issues/18)). The
install script is the supported way in. If you download the zip from the releases page in a
browser, Gatekeeper refuses to open it until you allow it in System Settings → Privacy & Security.

While the repo is private, the curl command can't reach it. Use the GitHub CLI
(`gh auth login`) instead:

    gh api repos/SameetKeshav/spinney/contents/install.sh -H "Accept: application/vnd.github.raw" | bash

## Build from source

Requires Rust 1.95 or newer.

    cargo run                      # debug build
    cargo build --release          # target/release/spinney
    bash scripts/bundle.sh 1.2.0   # Spinney.app and a zip in dist/

## Status

Spinney is early and under active development. The
[roadmap (#31)](https://github.com/SameetKeshav/spinney/issues/31) lists what's planned, and the
[milestones](https://github.com/SameetKeshav/spinney/milestones) show what's next. Merge and
rebase conflicts currently abort and list the files rather than leaving a half-done operation
([#53](https://github.com/SameetKeshav/spinney/issues/53)).

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
