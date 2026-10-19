//! Stash list and the local stash actions. Stashes are shared by all worktrees of a repo.

use crate::git;
use anyhow::Result;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Stash {
    pub index: usize,
    /// Commit hash: stable identity, unlike the positional `index`.
    pub hash: String,
    pub message: String,
    pub branch: Option<String>,
}

pub fn list(worktree: &Path) -> Result<Vec<Stash>> {
    let out = git::run(worktree, &["stash", "list", "--format=%gd%x1f%H%x1f%gs"])?;
    Ok(out.lines().filter_map(parse_line).collect())
}

/// `stash@{2}\x1fWIP on main: abc1234 subject` or `...\x1fOn main: message`.
fn parse_line(line: &str) -> Option<Stash> {
    let mut parts = line.splitn(3, '\u{1f}');
    let (name, hash, subject) = (parts.next()?, parts.next()?, parts.next()?);
    let index = name
        .strip_prefix("stash@{")?
        .strip_suffix('}')?
        .parse()
        .ok()?;
    let parsed = subject
        .strip_prefix("WIP on ")
        .or_else(|| subject.strip_prefix("On "))
        .and_then(|rest| rest.split_once(": "));
    let (branch, message) = match parsed {
        Some((b, m)) if b.starts_with("(no branch") => (None, m.to_string()),
        Some((b, m)) => (Some(b.to_string()), m.to_string()),
        None => (None, subject.to_string()),
    };
    Some(Stash {
        index,
        hash: hash.to_string(),
        message,
        branch,
    })
}

/// Current index of the stash with commit `hash`, if it still exists.
pub fn index_of(worktree: &Path, hash: &str) -> Result<Option<usize>> {
    Ok(list(worktree)?
        .into_iter()
        .find(|s| s.hash == hash)
        .map(|s| s.index))
}

/// Stash tracked and untracked changes. `false` (and no stash) when there is nothing to save.
pub fn save(worktree: &Path, message: Option<&str>) -> Result<bool> {
    if git::run(worktree, &["status", "--porcelain"])?
        .trim()
        .is_empty()
    {
        return Ok(false);
    }
    let mut args = vec!["stash", "push", "-u"];
    if let Some(m) = message {
        args.extend(["-m", m]);
    }
    git::run(worktree, &args)?;
    Ok(true)
}

fn at(index: usize) -> String {
    format!("stash@{{{index}}}")
}

pub fn apply(worktree: &Path, index: usize) -> Result<()> {
    git::run(worktree, &["stash", "apply", &at(index)]).map(|_| ())
}

/// Apply and remove the stash. On conflicts git keeps it and this returns an error.
pub fn pop(worktree: &Path, index: usize) -> Result<()> {
    git::run(worktree, &["stash", "pop", &at(index)]).map(|_| ())
}

pub fn drop(worktree: &Path, index: usize) -> Result<()> {
    git::run(worktree, &["stash", "drop", &at(index)]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn g(d: &Path, args: &[&str]) {
        git::run(d, args).unwrap();
    }

    /// Temp repo with one commit of `a.txt`.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        g(d, &["init", "-q"]);
        g(d, &["config", "user.name", "Test User"]);
        g(d, &["config", "user.email", "test@example.com"]);
        g(d, &["config", "commit.gpgsign", "false"]);
        fs::write(d.join("a.txt"), "one\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-q", "-m", "init"]);
        tmp
    }

    #[test]
    fn save_includes_untracked() {
        let tmp = repo();
        let d = tmp.path();
        fs::write(d.join("new.txt"), "x\n").unwrap();
        fs::write(d.join("a.txt"), "two\n").unwrap();
        assert!(save(d, None).unwrap());
        assert!(!d.join("new.txt").exists());
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "one\n");
        assert_eq!(list(d).unwrap().len(), 1);
    }

    #[test]
    fn save_untracked_only_saves() {
        let tmp = repo();
        fs::write(tmp.path().join("new.txt"), "x\n").unwrap();
        assert!(save(tmp.path(), None).unwrap());
        assert_eq!(list(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn save_ignored_only_returns_false() {
        let tmp = repo();
        let d = tmp.path();
        fs::write(d.join(".gitignore"), "*.log\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-q", "-m", "ignore"]);
        fs::write(d.join("x.log"), "x\n").unwrap();
        assert!(!save(d, None).unwrap());
        assert!(list(d).unwrap().is_empty());
    }

    #[test]
    fn pop_conflict_keeps_stash_and_errors() {
        let tmp = repo();
        let d = tmp.path();
        fs::write(d.join("a.txt"), "stashed\n").unwrap();
        save(d, None).unwrap();
        fs::write(d.join("a.txt"), "committed\n").unwrap();
        g(d, &["commit", "-q", "-am", "other"]);
        assert!(pop(d, 0).is_err());
        assert_eq!(list(d).unwrap().len(), 1);
    }

    #[test]
    fn drop_by_hash_after_new_stash_pushed_in_front() {
        let tmp = repo();
        let d = tmp.path();
        fs::write(d.join("a.txt"), "first\n").unwrap();
        save(d, Some("first")).unwrap();
        let target = list(d).unwrap()[0].hash.clone();
        fs::write(d.join("a.txt"), "second\n").unwrap();
        save(d, Some("second")).unwrap();
        let i = index_of(d, &target).unwrap().unwrap();
        assert_eq!(i, 1);
        drop(d, i).unwrap();
        let msgs: Vec<_> = list(d).unwrap().into_iter().map(|s| s.message).collect();
        assert_eq!(msgs, ["second"]);
        assert_eq!(index_of(d, &target).unwrap(), None);
    }

    #[test]
    fn parse_line_variants() {
        let p = |s: &str| parse_line(&format!("stash@{{3}}\u{1f}abc123\u{1f}{s}")).unwrap();
        let s = p("WIP on main: abc subject");
        assert_eq!((s.index, s.hash.as_str()), (3, "abc123"));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.message, "abc subject");
        let s = p("On feat/x: my work");
        assert_eq!(s.branch.as_deref(), Some("feat/x"));
        assert_eq!(s.message, "my work");
        let s = p("WIP on (no branch): abc detached");
        assert_eq!(s.branch, None);
        assert_eq!(s.message, "abc detached");
        let s = p("On main: a: b {} café ✓");
        assert_eq!(s.message, "a: b {} café ✓");
        let s = p("something custom");
        assert_eq!((s.branch, s.message.as_str()), (None, "something custom"));
        assert!(parse_line("garbage").is_none());
        assert!(parse_line("nope\u{1f}h\u{1f}x").is_none());
    }

    #[test]
    fn save_nothing_returns_false() {
        let tmp = repo();
        assert!(!save(tmp.path(), Some("noop")).unwrap());
        assert!(list(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn list_parses_messages() {
        let tmp = repo();
        let d = tmp.path();
        fs::write(d.join("a.txt"), "two\n").unwrap();
        save(d, Some("my work")).unwrap();
        fs::write(d.join("a.txt"), "three\n").unwrap();
        save(d, None).unwrap();
        let s = list(d).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].index, 0);
        assert!(s[0].message.contains("init"), "{:?}", s[0]);
        assert_eq!(s[1].index, 1);
        assert_eq!(s[1].message, "my work");
        assert!(s[1].branch.is_some());
    }

    #[test]
    fn pop_restores_and_removes() {
        let tmp = repo();
        let d = tmp.path();
        fs::write(d.join("new.txt"), "x\n").unwrap();
        save(d, None).unwrap();
        pop(d, 0).unwrap();
        assert!(d.join("new.txt").exists());
        assert!(list(d).unwrap().is_empty());
    }

    #[test]
    fn drop_removes_only_that_index() {
        let tmp = repo();
        let d = tmp.path();
        for m in ["first", "second", "third"] {
            fs::write(d.join("a.txt"), format!("{m}\n")).unwrap();
            save(d, Some(m)).unwrap();
        }
        drop(d, 1).unwrap(); // "second"
        let msgs: Vec<_> = list(d).unwrap().into_iter().map(|s| s.message).collect();
        assert_eq!(msgs, ["third", "first"]);
    }
}
