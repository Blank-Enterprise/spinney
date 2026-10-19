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

#[derive(Debug, PartialEq)]
pub enum DeleteOutcome {
    Deleted,
    /// `git branch -d` refused: the branch has commits its upstream (or HEAD) lacks.
    NotMerged,
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

/// Fails when any worktree of the project (not just `dir`'s) has `branch` checked out.
pub fn ensure_not_checked_out(dir: &Path, branch: &str) -> Result<()> {
    if let Some(w) = list_worktrees(dir)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        let name = w.path.file_name().unwrap_or(w.path.as_os_str());
        bail!(
            "{branch} is checked out in worktree {}",
            name.to_string_lossy()
        );
    }
    Ok(())
}

/// `git branch -d` (`-D` when `force`), refusing first when any worktree has `branch` out.
pub fn delete_branch(worktree: &Path, branch: &str, force: bool) -> Result<DeleteOutcome> {
    ensure_not_checked_out(worktree, branch)?;
    let flag = if force { "-D" } else { "-d" };
    match git::run(worktree, &["branch", flag, branch]) {
        Ok(_) => Ok(DeleteOutcome::Deleted),
        // git runs with LC_ALL=C; older versions capitalise "The branch", so compare lowercase.
        Err(e) if e.to_string().to_lowercase().contains("not fully merged") => {
            Ok(DeleteOutcome::NotMerged)
        }
        Err(e) => Err(e),
    }
}

/// `git push <remote> --delete <name>` for `remote_branch` (`origin/feature/x`); the push
/// also drops the remote-tracking ref.
pub fn delete_remote_branch(worktree: &Path, remote_branch: &str) -> Result<()> {
    let Some((remote, name)) = remote_branch.split_once('/') else {
        bail!("{remote_branch} is not a remote branch");
    };
    // The full ref, so a tag with the same name is never the one deleted.
    let full = format!("refs/heads/{name}");
    git::run(worktree, &["push", remote, "--delete", &full])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Space;
    use crate::project::{add_worktree_existing, clone_into};

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

    #[test]
    fn delete_merged_branch() {
        let (_tmp, _root, main) = cloned();
        create_branch(&main, "done", "main").unwrap();
        let out = delete_branch(&main, "done", false).unwrap();
        assert_eq!(out, DeleteOutcome::Deleted);
        assert!(git::run(&main, &["rev-parse", "--verify", "done"]).is_err());
    }

    #[test]
    fn delete_unmerged_needs_force() {
        let (_tmp, _root, main) = cloned();
        // No upstream, so `-d` judges against HEAD (main), which lacks feature/x's commit.
        git::run(
            &main,
            &["branch", "--no-track", "topic", "origin/feature/x"],
        )
        .unwrap();
        let out = delete_branch(&main, "topic", false).unwrap();
        assert_eq!(out, DeleteOutcome::NotMerged);
        git::run(&main, &["rev-parse", "--verify", "topic"]).unwrap();
        let out = delete_branch(&main, "topic", true).unwrap();
        assert_eq!(out, DeleteOutcome::Deleted);
        assert!(git::run(&main, &["rev-parse", "--verify", "topic"]).is_err());
    }

    #[test]
    fn delete_refuses_branch_checked_out_elsewhere() {
        let (_tmp, root, main) = cloned();
        add_worktree_existing(&root, "feature/x").unwrap();
        let err = delete_branch(&main, "feature/x", true).unwrap_err();
        assert!(
            err.to_string()
                .contains("checked out in worktree feature-x"),
            "{err}"
        );
        git::run(&main, &["rev-parse", "--verify", "feature/x"]).unwrap();
        let err = delete_branch(&main, "main", true).unwrap_err();
        assert!(err.to_string().contains("worktree main"), "{err}");
    }

    #[test]
    fn delete_remote_branch_on_file_remote() {
        let (tmp, _root, main) = cloned();
        delete_remote_branch(&main, "origin/feature/x").unwrap();
        let remote = remote_dir(&tmp);
        assert!(git::run(&remote, &["rev-parse", "--verify", "feature/x"]).is_err());
        git::run(&remote, &["rev-parse", "--verify", "main"]).unwrap();
        let list = refs::branches(&main).unwrap();
        assert!(
            list.iter().all(|b| b.name != "origin/feature/x"),
            "{list:?}"
        );
        assert!(delete_remote_branch(&main, "nope").is_err());
    }
}
