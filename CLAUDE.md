@AGENTS.md

## Claude Code notes

- **Before you say you're done,** run `bash scripts/check.sh`. It's pre-approved in `.claude/settings.json`.
- **Taking an issue to a PR?** Use the `ship-issue` skill (`.claude/skills/ship-issue`).
- **Worktrees:** parallel work goes in worktrees under `.claude/worktrees/` (git-ignored). Each needs its own `cargo build`; never share `target/release` with the credential helper (see Pitfalls in AGENTS.md).
- **PR descriptions** end with the "Generated with Claude Code" line only, with no session links.
