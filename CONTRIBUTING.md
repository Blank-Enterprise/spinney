# Contributing to Spinney

Thanks for helping. Spinney is a small project run by two people, so issues and PRs that follow the
notes below are the easiest to review.

By taking part you agree to follow the [code of conduct](CODE_OF_CONDUCT.md). To report a security
problem, see [SECURITY.md](SECURITY.md). Don't open a public issue.

## Before you start

- **Bugs:** open an issue with the bug report template.
- **Features:** open an issue first, so we can agree on the shape before you write code. The
  [roadmap issue (#31)](https://github.com/Blank-Enterprise/spinney/issues/31) and the
  [milestones](https://github.com/Blank-Enterprise/spinney/milestones) show what's planned.
- Issues labelled `good first issue` are a good place to start. Issues labelled
  `needs decision` aren't ready for code yet.

## Building and testing

You need macOS on Apple Silicon and Rust 1.95 or newer.

```sh
cargo run                      # run the app (debug build)
cargo test --locked <name>     # one test, or every test whose name contains <name>
bash scripts/check.sh          # everything CI runs
```

`scripts/check.sh` runs what CI runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test`, `scripts/leak-check.sh`, and, if they're installed, a build on the declared
`rust-version`, `cargo deny check` (dependency advisories, licences and sources; see
`deny.toml`), `zizmor` and `actionlint` (workflow lint): `rustup toolchain install 1.95` and
`brew install cargo-deny zizmor actionlint`. Run it before opening a PR. CI also bundles the app
and installs it with `install.sh`; to try that locally, run `scripts/bundle.sh` and
`scripts/smoke-install.sh` in a worktree.

- **Tests** create temporary git repos and need a global git identity
  (`git config --global user.name` and `user.email`).
- **The leak check** fails if a tracked file contains a home-directory path, your username,
  hostname or git email. Use neutral paths such as `/p/project` or `/tmp/x` in tests and docs.

[AGENTS.md](AGENTS.md) describes the architecture, the patterns to follow and the safety rules,
for human and AI contributors alike. Read it before your first code change.

## Pull requests

- **One issue per PR.** Put `Resolves #N` in the description.
- **Branches:** `feat/…`, `fix/…`, `docs/…`, `ci/…` or `chore/…`.
- **Commits:** [conventional commits](https://www.conventionalcommits.org/), e.g.
  `fix(stage): …`, with a short body saying why.
- **Tests:** add tests for new logic. Keep decisions in pure functions and test those against
  real temporary repos. The UI isn't tested by rendering.
- **Safety:** anything that can lose work (discard, delete, reset, force push, stash drop) must ask
  for confirmation and name what is lost. Force pushes use `--force-with-lease`, never `--force`.
- **Testing section:** say what you checked, and plainly what you didn't check in the running app.
- **Changelog:** if users will notice the change, add a line under **Unreleased** in
  [CHANGELOG.md](CHANGELOG.md), under Added, Changed, Fixed, Removed or Security, and link the
  issue. Write it for users: what they can now do or no longer hit, not how the code changed. A
  change nobody will notice (tests, CI, refactors) gets the `skip changelog` label instead. The
  `changelog` check reminds you.
- **Email:** commits are public. Use your GitHub `noreply` address if you don't want your email
  published (`git config user.email <id>+<login>@users.noreply.github.com`).

CI must pass before a PR merges. A first-time contributor's CI run waits for a maintainer to
approve it.

## Releases

Maintainers release by bumping `version` in `Cargo.toml`, merging to `main`, and pushing a
`vX.Y.Z` tag on `main`. Before that, a beta or release candidate of the version in `Cargo.toml`
needs no bump: run the **prerelease** workflow from the Actions tab (pick `beta` or `rc`), or
`bash scripts/tag-prerelease.sh beta` locally. Either tags `main` as the next `vX.Y.Z-beta.N` or
`vX.Y.Z-rc.N`, and the release workflow publishes it as a pre-release.

Release notes come from [CHANGELOG.md](CHANGELOG.md), followed by GitHub's list of merged PRs. A
beta or release candidate uses the **Unreleased** section as it stands. For a stable release, the
version-bump PR also runs `bash scripts/changelog.sh release X.Y.Z`, which moves Unreleased into a
dated `X.Y.Z` section; the release workflow refuses a stable tag without one.

## Labels

| Kind | Labels |
|---|---|
| Type | `bug`, `enhancement`, `security`, `documentation`, `release`, `ci`, `chore`, `epic` |
| Priority | `priority: critical`, `high`, `medium`, `low` |
| Size | `size: XS` (under an hour) to `size: XL` (about a week) |
| State | `needs decision`, `blocked`, `needs QA` |
| PRs | `skip changelog`: no CHANGELOG.md entry, since users won't notice the change |

## License

Spinney is dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). Unless you
say otherwise, any contribution you intentionally submit is dual licensed the same way, without
additional terms or conditions.
