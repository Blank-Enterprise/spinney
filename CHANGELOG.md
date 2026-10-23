# Changelog

What changed in each Spinney release. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

A pull request that changes what users see adds a line under **Unreleased**. Betas and release
candidates take their release notes from **Unreleased**; a stable release moves it into its own
section. CONTRIBUTING.md has the details.

## [Unreleased]

### Added

- Beta and release-candidate builds (`v1.1.0-beta.1`, `v1.1.0-rc.1`). Install one with
  `SPINNEY_VERSION=v1.1.0-rc.1`, or the newest with `SPINNEY_VERSION=beta`; the plain install
  keeps installing the latest stable release. ([#228](https://github.com/Blank-Enterprise/spinney/issues/228))
- Release zips carry signed build provenance: check one with
  `gh attestation verify Spinney-macos-arm64.zip --repo Blank-Enterprise/spinney`.
  ([#227](https://github.com/Blank-Enterprise/spinney/issues/227))

### Changed

- Switching spaces is fast: a space keeps its tabs when you switch away, and only the active tab
  loads until you open another. Launching with many tabs open is faster for the same reason.
  ([#231](https://github.com/Blank-Enterprise/spinney/issues/231))
- Refreshing after a commit, checkout, stash, fetch, pull or push runs about a quarter of the git
  work it did. ([#234](https://github.com/Blank-Enterprise/spinney/issues/234))
- The Worktrees view counts an agent's new files without starting a git process for each one, so
  a worktree with hundreds of them opens at once. ([#229](https://github.com/Blank-Enterprise/spinney/issues/229))
- The sidebar, Changes list and diff stay responsive with thousands of branches, files or lines:
  they're no longer rebuilt on every frame. ([#233](https://github.com/Blank-Enterprise/spinney/issues/233))

## [1.0.0] - 2026-10-23

The first public release.

### Added

- **Spaces:** separate work and personal accounts. Each space has its own GitHub or GitHub
  Enterprise account (device-flow sign-in or a token), SSH key, author identity and clone folder.
  Tokens live in the macOS Keychain and go only to the space's own host.
- **Projects and tabs:** clone from a URL or by searching your repositories, create, open recent,
  pin favourites, and one tab per project. Reorder tabs by dragging, middle-click to close, and
  reach every project from the Repositories button.
- **Worktrees:** each project is a normal clone, with its worktrees beside it in
  `<project>.worktrees/`. Worktrees made by `git worktree add`, IDEs or agents show up too, and
  moved ones are relinked. The Worktrees view lists every worktree, the agents running in it and
  what each changed since it branched.
- **Branches:** a sidebar grouped into folders; checkout, create, rename (also on the remote),
  delete local and remote branches or a whole folder, fast-forward without checking out, reset to
  the upstream, and merge or rebase from the menu or by dragging one branch onto another.
- **Commit graph:** branch and tag labels, and a commit menu to copy, check out, branch, tag or
  open a worktree, cherry-pick, revert, reset (soft, mixed or hard) or start an interactive rebase.
- **Changes:** stage, unstage or discard a file, a hunk or single lines; commit and amend; inline
  or side-by-side diffs.
- **Sync:** fetch (pruning deleted remote branches), push, and pull by fast-forward, merge or
  rebase. A rejected push offers a force push with lease, naming the commits it would replace.
- **Conflicts:** a stopped merge, rebase, cherry-pick, revert or pull stays in progress with
  Continue, Skip and Abort; take your side or theirs, use the built-in merge tool, or open an
  external one.
- **Tags and stash:** create, push and delete tags; stash, apply, pop and drop.
- A one-line installer that checks the download against its published SHA-256 checksum.

[Unreleased]: https://github.com/Blank-Enterprise/spinney/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Blank-Enterprise/spinney/releases/tag/v1.0.0
