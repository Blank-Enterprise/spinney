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

/// Replace the last commit with `message` plus whatever is staged (nothing staged: message only).
pub fn amend(worktree: &Path, message: &str) -> Result<()> {
    if message.trim().is_empty() {
        bail!("empty commit message");
    }
    let args = ["commit", "--amend", "-F", "-"];
    git::run_stdin(worktree, &args, message.as_bytes())?;
    Ok(())
}

/// The full message of HEAD, without git's trailing newline.
pub fn head_message(worktree: &Path) -> Result<String> {
    let out = git::run(worktree, &["log", "-1", "--format=%B", "HEAD"])?;
    Ok(out.trim_end().to_string())
}

/// The current branch tracks an upstream.
fn has_upstream(worktree: &Path) -> bool {
    git::run(worktree, &["rev-parse", "--abbrev-ref", "@{u}"]).is_ok()
}

/// Whether HEAD is already on its upstream, so amending it rewrites pushed history.
/// No upstream means nothing was pushed.
pub fn head_pushed(worktree: &Path) -> Result<bool> {
    if !has_upstream(worktree) {
        return Ok(false);
    }
    let ahead = git::run(worktree, &["rev-list", "--count", "@{u}..HEAD"])?;
    Ok(ahead.trim() == "0")
}

/// Commits that rebasing HEAD onto `onto` rewrites although they are already on the
/// upstream: reachable from HEAD and `@{u}` but not from `onto`. No upstream means none.
pub fn rebase_rewrites_pushed(worktree: &Path, onto: &str) -> Result<usize> {
    if !has_upstream(worktree) {
        return Ok(0);
    }
    // Exit 1: no common history, so nothing pushed is rewritten.
    let base = git::run_ok_codes(worktree, &["merge-base", "HEAD", "@{u}"], &[0, 1])?;
    let base = base.trim();
    if base.is_empty() {
        return Ok(0);
    }
    let range = format!("{onto}..{base}");
    let n = git::run(worktree, &["rev-list", "--count", &range])?;
    Ok(n.trim().parse()?)
}

/// How `merge` brings the branch in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MergeMode {
    /// `--ff`: move the branch when possible, else create a merge commit.
    FastForward,
    /// `--no-ff`: always create a merge commit.
    NoFf,
    /// `--squash`: stage the combined changes for the user to commit.
    Squash,
}

/// How a merge or rebase ended.
#[derive(Debug, PartialEq)]
pub enum MergeOutcome {
    /// Merged, rebased, or (squash) staged without committing.
    Done,
    /// Nothing to bring in: HEAD and the index are unchanged.
    UpToDate,
    /// Stopped on conflicts in these paths and was aborted, so nothing changed.
    Conflicts(Vec<String>),
}

/// The merge, rebase, cherry-pick or revert already under way in `worktree`, if any.
pub fn in_progress(worktree: &Path) -> Result<Option<&'static str>> {
    let states = [
        ("MERGE_HEAD", "merge"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ];
    let mut args = vec!["rev-parse"];
    for (file, _) in &states {
        args.extend(["--git-path", file]);
    }
    // Per-worktree paths: in the worktree layout they live under `.bare/worktrees/<name>`.
    let out = git::run(worktree, &args)?;
    Ok(out
        .lines()
        .zip(states)
        .find(|(path, _)| worktree.join(path).exists())
        .map(|(_, (_, what))| what))
}

/// Refuse to merge or rebase on top of another operation or uncommitted changes to
/// tracked files. Untracked files are left to git, which refuses to overwrite them.
pub fn ensure_clean(worktree: &Path) -> Result<()> {
    if let Some(what) = in_progress(worktree)? {
        bail!("A {what} is already in progress; finish or abort it first");
    }
    if is_dirty(worktree)? {
        bail!("Commit or stash your changes first");
    }
    Ok(())
}

/// Staged or unstaged changes to tracked files (conflicts included).
fn is_dirty(worktree: &Path) -> Result<bool> {
    let out = git::run(worktree, &["status", "--porcelain", "--untracked-files=no"])?;
    Ok(!out.trim().is_empty())
}

/// The checked-out branch, refusing a detached HEAD or `branch` itself.
fn current_for(worktree: &Path, branch: &str) -> Result<String> {
    let Some(current) = refs::current_branch(worktree)? else {
        bail!("Check out a branch first");
    };
    if current == branch {
        bail!("{branch} is the checked-out branch");
    }
    Ok(current)
}

fn rev_parse(worktree: &Path, rev: &str) -> Result<String> {
    Ok(git::run(worktree, &["rev-parse", rev])?.trim().to_string())
}

/// Merge `branch` (local or remote-tracking) into the current branch. A squash stages the
/// result without committing. Conflicts abort the merge: see `MergeOutcome::Conflicts`.
pub fn merge(worktree: &Path, branch: &str, mode: MergeMode) -> Result<MergeOutcome> {
    current_for(worktree, branch)?;
    ensure_clean(worktree)?;
    let before = rev_parse(worktree, "HEAD")?;
    // `--ff` explicitly, so a `merge.ff` setting cannot change the chosen mode.
    let flag = match mode {
        MergeMode::FastForward => "--ff",
        MergeMode::NoFf => "--no-ff",
        MergeMode::Squash => "--squash",
    };
    if let Err(e) = git::run(worktree, &["merge", flag, "--no-edit", branch]) {
        // A squash records no MERGE_HEAD, so there is no merge for `--abort` to find.
        let abort: &[&str] = match mode {
            MergeMode::Squash => &["reset", "--merge"],
            _ => &["merge", "--abort"],
        };
        return stopped(worktree, e, abort, &before, None);
    }
    let unchanged = match mode {
        MergeMode::Squash => !has_staged(worktree)?,
        _ => rev_parse(worktree, "HEAD")? == before,
    };
    Ok(if unchanged {
        MergeOutcome::UpToDate
    } else {
        MergeOutcome::Done
    })
}

/// Replay the current branch's commits on top of `onto` (local or remote-tracking).
/// Conflicts abort the rebase: see `MergeOutcome::Conflicts`.
pub fn rebase(worktree: &Path, onto: &str) -> Result<MergeOutcome> {
    let current = current_for(worktree, onto)?;
    ensure_clean(worktree)?;
    let before = rev_parse(worktree, "HEAD")?;
    if let Err(e) = git::run(worktree, &["rebase", onto]) {
        let abort = &["rebase", "--abort"];
        return stopped(worktree, e, abort, &before, Some(&current));
    }
    Ok(if rev_parse(worktree, "HEAD")? == before {
        MergeOutcome::UpToDate
    } else {
        MergeOutcome::Done
    })
}

/// Anything staged (`diff --cached --quiet` exits 1 when there is).
fn has_staged(worktree: &Path) -> Result<bool> {
    let out = git::output(worktree, &["diff", "--cached", "--quiet"])?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!("git diff failed: {}", stderr(&out)),
    }
}

/// After a failed merge or rebase: without conflicts, git's own error. With conflicts, run
/// `abort` and make sure HEAD (on `branch` again, for a rebase) and the tracked files are
/// back where they started.
fn stopped(
    worktree: &Path,
    err: anyhow::Error,
    abort: &[&str],
    before: &str,
    branch: Option<&str>,
) -> Result<MergeOutcome> {
    let args = [
        "-c",
        "core.quotepath=off",
        "diff",
        "--name-only",
        "--diff-filter=U",
    ];
    let conflicts: Vec<String> = git::run(worktree, &args)?
        .lines()
        .map(str::to_string)
        .collect();
    if conflicts.is_empty() {
        // E.g. an untracked file in the way; a rebase can stop on it partway through.
        if in_progress(worktree)?.is_some() {
            git::run(worktree, abort)?;
        }
        return Err(err);
    }
    // Until #53/#58: nothing can resolve conflicts and continue yet, so never leave the
    // operation half done; abort it and report the conflicted files instead.
    git::run(worktree, abort)?;
    let on_branch = match branch {
        Some(b) => refs::current_branch(worktree)?.as_deref() == Some(b),
        None => true,
    };
    let restored = on_branch
        && rev_parse(worktree, "HEAD")? == before
        && !is_dirty(worktree)?
        && in_progress(worktree)?.is_none();
    if !restored {
        bail!(
            "conflicts in {}, and aborting did not restore {before}; check the repository",
            conflicts.join(", ")
        );
    }
    Ok(MergeOutcome::Conflicts(conflicts))
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

/// A push the remote turned down because it has commits this branch lacks.
#[derive(Clone, Debug, PartialEq)]
pub struct PushRejected {
    pub worktree: PathBuf,
    pub branch: String,
}

impl std::fmt::Display for PushRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "push of {} rejected (non-fast-forward)", self.branch)
    }
}

impl std::error::Error for PushRejected {}

/// What a force push may overwrite: `remote_ref` on `remote`, only while it is still at
/// `expect`, the commit our remote-tracking ref pointed at when the user asked.
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub worktree: PathBuf,
    pub branch: String,
    pub remote: String,
    pub remote_ref: String,
    /// The remote-tracking branch, e.g. `origin/main`.
    pub tracking: String,
    pub expect: String,
    /// Commits at `expect` that are not on the branch: what the force push drops.
    pub lost: usize,
}

/// A non-fast-forward rejection fails with `PushRejected`.
pub fn push(worktree: &Path) -> Result<()> {
    let Some(branch) = refs::current_branch(worktree)? else {
        bail!("detached HEAD");
    };
    let rejected = || PushRejected {
        worktree: worktree.to_path_buf(),
        branch: branch.clone(),
    };
    if has_upstream(worktree) {
        push_porcelain(worktree, &["push", "--porcelain"], rejected)
    } else {
        let args = ["push", "--porcelain", "-u", "origin", &branch];
        push_porcelain(worktree, &args, rejected)
    }
}

/// Snapshot the lease for force pushing the current branch to its upstream (or
/// `origin/<branch>` when it has none).
pub fn lease(worktree: &Path) -> Result<Lease> {
    let Some(branch) = refs::current_branch(worktree)? else {
        bail!("detached HEAD");
    };
    let format =
        "--format=%(upstream)%00%(upstream:short)%00%(upstream:remotename)%00%(upstream:remoteref)";
    let local = format!("refs/heads/{branch}");
    let up = git::run(worktree, &["for-each-ref", format, &local])?;
    let fields: Vec<&str> = up.trim_end().split('\0').collect();
    let (tip, tracking, remote, remote_ref) = match fields[..] {
        [tip, short, remote, remote_ref] if tip.starts_with("refs/remotes/") => (
            tip.to_string(),
            short.to_string(),
            remote.to_string(),
            remote_ref.to_string(),
        ),
        _ => (
            format!("refs/remotes/origin/{branch}"),
            format!("origin/{branch}"),
            "origin".to_string(),
            local,
        ),
    };
    let Ok(expect) = git::run(worktree, &["rev-parse", "--verify", "--quiet", &tip]) else {
        bail!("{tracking} is unknown; fetch first");
    };
    let expect = expect.trim().to_string();
    let range = format!("HEAD..{expect}");
    let lost = git::run(worktree, &["rev-list", "--count", &range])?;
    Ok(Lease {
        worktree: worktree.to_path_buf(),
        branch,
        remote,
        remote_ref,
        tracking,
        expect,
        lost: lost.trim().parse()?,
    })
}

/// `git push --force-with-lease` bound to the commit in `lease`, never a plain `--force`:
/// if the remote moved since then (even if a later fetch saw it), git refuses.
pub fn force_push(lease: &Lease) -> Result<()> {
    let with_lease = format!("--force-with-lease={}:{}", lease.remote_ref, lease.expect);
    let refspec = format!("refs/heads/{}:{}", lease.branch, lease.remote_ref);
    let args = ["push", "--porcelain", &with_lease, &lease.remote, &refspec];
    let out = git::output(&lease.worktree, &args)?;
    if out.status.success() {
        return Ok(());
    }
    if porcelain_rejected(&out.stdout, &["(stale info)"]) {
        bail!(
            "{} has changed since {} was fetched; fetch and review it first",
            lease.remote_ref.trim_start_matches("refs/heads/"),
            lease.tracking
        );
    }
    bail!("git push failed: {}", stderr(&out));
}

/// Run a `--porcelain` push; a non-fast-forward rejection becomes `rejected()`.
fn push_porcelain(
    worktree: &Path,
    args: &[&str],
    rejected: impl FnOnce() -> PushRejected,
) -> Result<()> {
    let out = git::output(worktree, args)?;
    if out.status.success() {
        return Ok(());
    }
    // Porcelain status lines are stable across git versions and locales, unlike the hints.
    if porcelain_rejected(&out.stdout, &["(non-fast-forward)", "(fetch first)"]) {
        return Err(rejected().into());
    }
    bail!("git push failed: {}", stderr(&out));
}

/// Some ref was `!` `[rejected]` for one of `reasons` in `git push --porcelain` output.
fn porcelain_rejected(stdout: &[u8], reasons: &[&str]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|l| {
        let summary = l.strip_prefix("!\t").and_then(|l| l.split('\t').nth(1));
        summary
            .is_some_and(|s| s.starts_with("[rejected]") && reasons.iter().any(|r| s.ends_with(r)))
    })
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
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
        let root = clone_into(
            &url,
            &parent,
            "proj",
            &space(),
            crate::project::Layout::WorktreeRoot,
        )
        .unwrap();
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

    fn g(dir: &Path, args: &[&str]) -> String {
        git::run(dir, args).unwrap().trim().to_string()
    }

    #[test]
    fn amend_rewrites_message_only() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        let count = g(&main, &["rev-list", "--count", "HEAD"]);
        let parent = g(&main, &["rev-parse", "HEAD~1"]);
        amend(&main, "better\n\nwhy").unwrap();
        assert_eq!(head_message(&main).unwrap(), "better\n\nwhy");
        assert_eq!(
            g(&main, &["rev-list", "--count", "HEAD"]),
            count,
            "no new commit"
        );
        assert_eq!(g(&main, &["rev-parse", "HEAD~1"]), parent);
        let files = g(&main, &["show", "--format=", "--name-only", "HEAD"]);
        assert_eq!(files, "c.txt");
    }

    #[test]
    fn amend_adds_staged_changes_to_last_commit() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        let count = g(&main, &["rev-list", "--count", "HEAD"]);
        std::fs::write(main.join("d.txt"), "d\n").unwrap();
        git::run(&main, &["add", "d.txt"]).unwrap();
        amend(&main, "add files").unwrap();
        assert_eq!(
            g(&main, &["rev-list", "--count", "HEAD"]),
            count,
            "no new commit"
        );
        let files = g(&main, &["show", "--format=", "--name-only", "HEAD"]);
        assert_eq!(files, "c.txt\nd.txt");
        assert_eq!(head_message(&main).unwrap(), "add files");
        assert!(
            g(&main, &["status", "--porcelain"]).is_empty(),
            "nothing left staged"
        );
    }

    #[test]
    fn amend_empty_message_err() {
        let (_tmp, _root, main) = cloned();
        assert!(amend(&main, " \n\t").is_err());
        assert_eq!(head_message(&main).unwrap(), "init");
    }

    #[test]
    fn head_pushed_follows_upstream() {
        let (_tmp, _root, main) = cloned();
        assert!(
            head_pushed(&main).unwrap(),
            "fresh clone: HEAD is origin/main"
        );
        write_commit(&main, "c.txt");
        assert!(!head_pushed(&main).unwrap());
        push(&main).unwrap();
        assert!(head_pushed(&main).unwrap());
        git::run(&main, &["switch", "-q", "-c", "topic"]).unwrap();
        assert!(!head_pushed(&main).unwrap(), "no upstream");
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

    /// A bare `remote` with one commit on `main` and two clones of it, `ours` and `theirs`.
    fn two_clones() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        git::run(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        )
        .unwrap();
        let url = format!("file://{}", remote.display());
        let clone = |name: &str| {
            git::run(tmp.path(), &["clone", "-q", &url, name]).unwrap();
            let dir = tmp.path().join(name);
            git::run(&dir, &["config", "user.name", "Test User"]).unwrap();
            git::run(&dir, &["config", "user.email", "test@example.com"]).unwrap();
            dir
        };
        let ours = clone("ours");
        write_commit(&ours, "a.txt");
        git::run(&ours, &["push", "-q", "-u", "origin", "main"]).unwrap();
        let theirs = clone("theirs");
        (tmp, remote, ours, theirs)
    }

    fn head(dir: &Path, rev: &str) -> String {
        git::run(dir, &["rev-parse", rev])
            .unwrap()
            .trim()
            .to_string()
    }

    fn rejected(err: anyhow::Error) -> PushRejected {
        err.downcast::<PushRejected>().expect("non-fast-forward")
    }

    #[test]
    fn push_behind_remote_is_rejected_as_non_fast_forward() {
        let (_tmp, _remote, ours, theirs) = two_clones();
        write_commit(&theirs, "t.txt");
        push(&theirs).unwrap();
        write_commit(&ours, "o.txt");
        // Unfetched remote commits ("fetch first") and fetched ones ("non-fast-forward").
        let err = rejected(push(&ours).unwrap_err());
        assert_eq!(err.branch, "main");
        assert_eq!(err.worktree, ours);
        fetch(&ours).unwrap();
        rejected(push(&ours).unwrap_err());
    }

    #[test]
    fn push_other_failure_is_not_rejection() {
        let (_tmp, _remote, ours, _theirs) = two_clones();
        git::run(&ours, &["remote", "set-url", "origin", "/nonexistent/repo"]).unwrap();
        write_commit(&ours, "o.txt");
        let err = push(&ours).unwrap_err();
        assert!(err.downcast_ref::<PushRejected>().is_none(), "{err}");
    }

    #[test]
    fn force_push_with_lease_overwrites_when_up_to_date() {
        let (_tmp, remote, ours, theirs) = two_clones();
        write_commit(&theirs, "t.txt");
        push(&theirs).unwrap();
        fetch(&ours).unwrap();
        write_commit(&ours, "o.txt");
        rejected(push(&ours).unwrap_err());

        let lease = lease(&ours).unwrap();
        assert_eq!(lease.tracking, "origin/main");
        assert_eq!(lease.expect, head(&theirs, "HEAD"));
        assert_eq!(lease.lost, 1);
        force_push(&lease).unwrap();
        assert_eq!(head(&remote, "main"), head(&ours, "HEAD"));
    }

    #[test]
    fn force_push_with_lease_refuses_when_remote_moved_after_fetch() {
        let (_tmp, remote, ours, theirs) = two_clones();
        write_commit(&ours, "o.txt");
        let lease = lease(&ours).unwrap();
        assert_eq!(lease.lost, 0);
        write_commit(&theirs, "t.txt");
        push(&theirs).unwrap();
        // A fetch after the lease was taken must not let us drop their commit unseen.
        fetch(&ours).unwrap();

        let err = force_push(&lease).unwrap_err();
        assert!(err.to_string().contains("fetch and review"), "{err}");
        assert_eq!(head(&remote, "main"), head(&theirs, "HEAD"));
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

    /// `main` and a local `other`, both from `init`, each changing `a.txt` differently.
    fn conflicting() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, root, main) = cloned();
        std::fs::write(main.join("a.txt"), "main\n").unwrap();
        g(&main, &["commit", "-qam", "main edit"]);
        g(&main, &["switch", "-q", "-c", "other", "origin/main"]);
        std::fs::write(main.join("a.txt"), "other\n").unwrap();
        g(&main, &["commit", "-qam", "other edit"]);
        g(&main, &["switch", "-q", "main"]);
        (tmp, root, main)
    }

    /// HEAD, branch and `status` (untracked files included) before an operation.
    fn snapshot(dir: &Path) -> (String, Option<String>, String) {
        (
            head(dir, "HEAD"),
            refs::current_branch(dir).unwrap(),
            g(dir, &["status", "--porcelain", "--untracked-files=all"]),
        )
    }

    #[test]
    fn merge_fast_forwards() {
        let (_tmp, _root, main) = cloned();
        let out = merge(&main, "origin/feature/x", MergeMode::FastForward).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(head(&main, "HEAD"), head(&main, "origin/feature/x"));
        assert_eq!(
            g(&main, &["log", "-1", "--format=%P"]).split(' ').count(),
            1
        );
        assert!(main.join("b.txt").exists());
        let again = merge(&main, "origin/feature/x", MergeMode::FastForward).unwrap();
        assert_eq!(again, MergeOutcome::UpToDate);
    }

    #[test]
    fn merge_no_ff_creates_merge_commit() {
        let (_tmp, _root, main) = cloned();
        let before = head(&main, "HEAD");
        let out = merge(&main, "origin/feature/x", MergeMode::NoFf).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        let parents = g(&main, &["log", "-1", "--format=%P"]);
        let want = format!("{before} {}", head(&main, "origin/feature/x"));
        assert_eq!(parents, want, "two parents: main, then the branch");
        assert!(main.join("b.txt").exists());
    }

    #[test]
    fn merge_squash_stages_without_committing() {
        let (_tmp, _root, main) = cloned();
        let before = head(&main, "HEAD");
        let out = merge(&main, "origin/feature/x", MergeMode::Squash).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(head(&main, "HEAD"), before, "nothing committed");
        assert_eq!(g(&main, &["diff", "--cached", "--name-only"]), "b.txt");
        assert!(git::run(&main, &["rev-parse", "--verify", "-q", "MERGE_HEAD"]).is_err());
        assert_eq!(in_progress(&main).unwrap(), None);
        commit(&main, "Squash feature/x into main").unwrap();
        assert_eq!(
            g(&main, &["log", "-1", "--format=%P"]),
            before,
            "one parent"
        );
        let again = merge(&main, "origin/feature/x", MergeMode::Squash).unwrap();
        assert_eq!(again, MergeOutcome::UpToDate);
    }

    #[test]
    fn merge_conflict_aborts_and_reports_files() {
        let (_tmp, _root, main) = conflicting();
        std::fs::write(main.join("untracked.txt"), "u\n").unwrap();
        let before = snapshot(&main);
        for mode in [MergeMode::FastForward, MergeMode::NoFf, MergeMode::Squash] {
            let out = merge(&main, "other", mode).unwrap();
            assert_eq!(
                out,
                MergeOutcome::Conflicts(vec!["a.txt".into()]),
                "{mode:?}"
            );
            assert_eq!(snapshot(&main), before, "{mode:?}");
            assert_eq!(in_progress(&main).unwrap(), None, "{mode:?}");
            assert_eq!(
                std::fs::read_to_string(main.join("a.txt")).unwrap(),
                "main\n"
            );
            assert!(g(&main, &["diff", "--cached", "--name-only"]).is_empty());
        }
    }

    #[test]
    fn rebase_replays_commits_onto_branch() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        let out = rebase(&main, "origin/feature/x").unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(head(&main, "HEAD~1"), head(&main, "origin/feature/x"));
        assert_eq!(head_message(&main).unwrap(), "add file");
        assert_eq!(
            refs::current_branch(&main).unwrap().as_deref(),
            Some("main")
        );
        assert!(main.join("b.txt").exists() && main.join("c.txt").exists());
        let again = rebase(&main, "origin/feature/x").unwrap();
        assert_eq!(again, MergeOutcome::UpToDate);
    }

    #[test]
    fn rebase_conflict_aborts_and_restores_head() {
        let (_tmp, _root, main) = conflicting();
        std::fs::write(main.join("untracked.txt"), "u\n").unwrap();
        let before = snapshot(&main);
        let out = rebase(&main, "other").unwrap();
        assert_eq!(out, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(snapshot(&main), before);
        assert_eq!(in_progress(&main).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "main\n"
        );
    }

    #[test]
    fn dirty_tracked_files_refuse_merge_and_rebase() {
        let (_tmp, _root, main) = cloned();
        let x = "origin/feature/x";
        std::fs::write(main.join("a.txt"), "changed\n").unwrap();
        let err = merge(&main, x, MergeMode::FastForward).unwrap_err();
        assert!(err.to_string().contains("Commit or stash"), "{err}");
        g(&main, &["add", "a.txt"]);
        let err = rebase(&main, x).unwrap_err();
        assert!(err.to_string().contains("Commit or stash"), "staged: {err}");
        g(&main, &["reset", "-q", "--hard"]);
        // Untracked files alone are git's call; this one is not in the way.
        std::fs::write(main.join("new.txt"), "n\n").unwrap();
        let out = merge(&main, x, MergeMode::FastForward).unwrap();
        assert_eq!(out, MergeOutcome::Done);
    }

    #[test]
    fn untracked_file_in_the_way_is_gits_error() {
        let (_tmp, _root, main) = cloned();
        std::fs::write(main.join("b.txt"), "mine\n").unwrap();
        let before = snapshot(&main);
        let err = merge(&main, "origin/feature/x", MergeMode::FastForward).unwrap_err();
        assert!(err.to_string().contains("untracked"), "{err}");
        assert_eq!(snapshot(&main), before);
    }

    #[test]
    fn operation_in_progress_refuses() {
        let (_tmp, _root, main) = conflicting();
        assert!(git::run(&main, &["merge", "--no-edit", "other"]).is_err());
        assert_eq!(in_progress(&main).unwrap(), Some("merge"));
        let err = merge(&main, "origin/feature/x", MergeMode::NoFf).unwrap_err();
        assert!(
            err.to_string().contains("merge is already in progress"),
            "{err}"
        );
        let err = rebase(&main, "origin/feature/x").unwrap_err();
        assert!(
            err.to_string().contains("merge is already in progress"),
            "{err}"
        );
        g(&main, &["merge", "--abort"]);
        assert_eq!(in_progress(&main).unwrap(), None);
    }

    #[test]
    fn detached_head_or_same_branch_refuses() {
        let (_tmp, _root, main) = cloned();
        let err = merge(&main, "main", MergeMode::FastForward).unwrap_err();
        assert!(err.to_string().contains("checked-out branch"), "{err}");
        g(&main, &["checkout", "-q", "--detach"]);
        let err = rebase(&main, "origin/feature/x").unwrap_err();
        assert!(
            err.to_string().contains("Check out a branch first"),
            "{err}"
        );
    }

    #[test]
    fn rebase_counts_pushed_commits_it_rewrites() {
        let (_tmp, _root, main) = cloned();
        let x = "origin/feature/x";
        assert_eq!(
            rebase_rewrites_pushed(&main, x).unwrap(),
            0,
            "only init, on x"
        );
        write_commit(&main, "c.txt");
        assert_eq!(rebase_rewrites_pushed(&main, x).unwrap(), 0, "not pushed");
        push(&main).unwrap();
        assert_eq!(rebase_rewrites_pushed(&main, x).unwrap(), 1);
        write_commit(&main, "d.txt");
        assert_eq!(
            rebase_rewrites_pushed(&main, x).unwrap(),
            1,
            "d is not pushed"
        );
        g(&main, &["switch", "-q", "-c", "topic"]);
        assert_eq!(rebase_rewrites_pushed(&main, x).unwrap(), 0, "no upstream");
    }
}
