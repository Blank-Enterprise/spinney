#![allow(dead_code)]

use crate::project::{list_worktrees, Layout};
use crate::{git, refs};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum CheckoutOutcome {
    Switched,
    JumpTo(PathBuf),
    NeedsWorktree,
}

pub fn commit(worktree: &Path, message: &str) -> Result<()> {
    if message.trim().is_empty() {
        bail!("empty commit message");
    }
    git::run_stdin(worktree, &["commit", "-F", "-"], message.as_bytes())?;
    Ok(())
}

pub fn fetch(dir: &Path) -> Result<()> {
    git::run(dir, &["fetch", "--all", "--prune"])?;
    Ok(())
}

/// Fast-forward only; a diverged branch surfaces git's error (merge/rebase come later).
pub fn pull(worktree: &Path) -> Result<()> {
    git::run(worktree, &["pull", "--ff-only"])?;
    Ok(())
}

pub fn push(worktree: &Path) -> Result<()> {
    let Some(branch) = refs::current_branch(worktree)? else {
        bail!("detached HEAD");
    };
    if git::run(worktree, &["rev-parse", "--abbrev-ref", "@{u}"]).is_ok() {
        git::run(worktree, &["push"])?;
    } else {
        git::run(worktree, &["push", "-u", "origin", &branch])?;
    }
    Ok(())
}

pub fn checkout(
    root: &Path,
    layout: Layout,
    active_worktree: &Path,
    branch: &str,
) -> Result<CheckoutOutcome> {
    match layout {
        Layout::NormalClone => {
            git::run(active_worktree, &["switch", branch])?;
            Ok(CheckoutOutcome::Switched)
        }
        Layout::WorktreeRoot => Ok(list_worktrees(root)?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .map_or(CheckoutOutcome::NeedsWorktree, |w| {
                CheckoutOutcome::JumpTo(w.path)
            })),
    }
}

/// Open Terminal.app at `dir`.
pub fn open_terminal(dir: &Path) -> Result<()> {
    let out = std::process::Command::new("open")
        .args(["-a", "Terminal"])
        .arg(dir)
        .output()?;
    if !out.status.success() {
        bail!(
            "open Terminal: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn create_branch(worktree: &Path, name: &str, base: &str) -> Result<()> {
    git::run(worktree, &["branch", name, base])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Space;
    use crate::project::clone_into;

    fn space() -> Space {
        Space {
            id: "test-space".into(),
            name: "Test".into(),
            host: "github.com".into(),
            author_name: "Test User".into(),
            author_email: "test@example.com".into(),
            default_clone_dir: None,
            projects: vec![],
            ssh_key: None,
        }
    }

    /// Temp "remote" with `main` and `feature/x`, plus a place to clone into.
    /// The remote is non-bare with `receive.denyCurrentBranch=updateInstead`
    /// so pushes to its checked-out `main` are accepted.
    fn fixture() -> (tempfile::TempDir, String, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        let g = |args: &[&str]| git::run(&remote, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["config", "receive.denyCurrentBranch", "updateInstead"]);
        std::fs::write(remote.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        g(&["checkout", "-q", "-b", "feature/x"]);
        std::fs::write(remote.join("b.txt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "x"]);
        g(&["checkout", "-q", "main"]);
        let url = format!("file://{}", remote.display());
        let parent = tmp.path().join("clones");
        std::fs::create_dir(&parent).unwrap();
        (tmp, url, parent)
    }

    /// Worktree-layout project; returns (tmp, root, main worktree).
    fn cloned() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, url, parent) = fixture();
        let root = clone_into(&url, &parent, "proj", &space()).unwrap();
        let main = root.join("main");
        git::run(&main, &["config", "user.name", "Test User"]).unwrap();
        git::run(&main, &["config", "user.email", "test@example.com"]).unwrap();
        (tmp, root, main)
    }

    fn write_commit(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), "x\n").unwrap();
        git::run(dir, &["add", "."]).unwrap();
        commit(dir, "add file").unwrap();
    }

    fn remote_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("remote")
    }

    #[test]
    fn commit_then_push_updates_remote() {
        let (tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        push(&main).unwrap();
        let log = git::run(&remote_dir(&tmp), &["log", "-1", "--format=%s", "main"]).unwrap();
        assert_eq!(log.trim(), "add file");
    }

    #[test]
    fn commit_empty_message_err() {
        let (_tmp, _root, main) = cloned();
        std::fs::write(main.join("c.txt"), "x\n").unwrap();
        git::run(&main, &["add", "."]).unwrap();
        assert!(commit(&main, "").is_err());
        assert!(commit(&main, " \n\t").is_err());
    }

    #[test]
    fn pull_ff() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        git::run(&remote, &["config", "user.name", "Test User"]).unwrap();
        write_commit(&remote, "r.txt");
        fetch(&main).unwrap();
        pull(&main).unwrap();
        assert!(main.join("r.txt").exists());
    }

    #[test]
    fn push_sets_upstream_for_new_branch() {
        let (tmp, _root, main) = cloned();
        git::run(&main, &["switch", "-q", "-c", "topic"]).unwrap();
        write_commit(&main, "t.txt");
        push(&main).unwrap();
        let up = git::run(&main, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap();
        assert_eq!(up.trim(), "origin/topic");
        git::run(&remote_dir(&tmp), &["rev-parse", "--verify", "topic"]).unwrap();
    }

    #[test]
    fn push_detached_err() {
        let (_tmp, _root, main) = cloned();
        git::run(&main, &["checkout", "-q", "--detach"]).unwrap();
        let err = push(&main).unwrap_err();
        assert!(err.to_string().contains("detached HEAD"), "{err}");
    }

    #[test]
    fn checkout_worktree_root_jump() {
        let (_tmp, root, main) = cloned();
        let out = checkout(&root, Layout::WorktreeRoot, &main, "main").unwrap();
        let CheckoutOutcome::JumpTo(p) = out else {
            panic!("{out:?}")
        };
        assert_eq!(p.canonicalize().unwrap(), main.canonicalize().unwrap());
    }

    #[test]
    fn checkout_worktree_root_needs() {
        let (_tmp, root, main) = cloned();
        let out = checkout(&root, Layout::WorktreeRoot, &main, "feature/x").unwrap();
        assert_eq!(out, CheckoutOutcome::NeedsWorktree);
    }

    #[test]
    fn checkout_normal_switches() {
        let (tmp, url, _parent) = fixture();
        let dir = tmp.path().join("plain");
        git::run(tmp.path(), &["clone", "-q", &url, "plain"]).unwrap();
        let out = checkout(&dir, Layout::NormalClone, &dir, "feature/x").unwrap();
        assert_eq!(out, CheckoutOutcome::Switched);
        assert_eq!(
            refs::current_branch(&dir).unwrap().as_deref(),
            Some("feature/x")
        );
    }

    #[test]
    fn create_branch_from_base() {
        let (_tmp, _root, main) = cloned();
        create_branch(&main, "new", "main").unwrap();
        git::run(&main, &["rev-parse", "--verify", "new"]).unwrap();
    }
}
