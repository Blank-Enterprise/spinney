#![allow(dead_code)]

use crate::config::PullStrategy;
use crate::project::{list_worktrees, same_dir};
use crate::{git, refs};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum CheckoutOutcome {
    Switched,
    JumpTo(PathBuf),
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
    commit_message(worktree, "HEAD")
}

/// The full message of commit `rev`, without git's trailing newline.
pub fn commit_message(worktree: &Path, rev: &str) -> Result<String> {
    let out = git::run(worktree, &["log", "-1", "--format=%B", rev, "--"])?;
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
    pushed_not_on(worktree, onto)
}

/// Commits on both HEAD and its upstream that `onto` lacks: what moving the branch to (or
/// replaying it on) `onto` takes off pushed history. No upstream means none.
fn pushed_not_on(worktree: &Path, onto: &str) -> Result<usize> {
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

/// How `reset` moves the current branch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResetMode {
    /// Keep the index and files: the dropped commits' changes end up staged.
    Soft,
    /// Keep the files but reset the index: the dropped commits' changes end up unstaged.
    Mixed,
    /// Reset the index and tracked files: uncommitted changes to them are lost.
    Hard,
}

impl ResetMode {
    pub fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }
}

/// What resetting `branch` from `head` to `target` would do, worked out before asking so
/// the confirmation can name it and `reset` can refuse if anything changed since.
#[derive(Clone, Debug, PartialEq)]
pub struct ResetPlan {
    pub branch: String,
    pub head: String,
    pub target: String,
    /// Commits on the branch after `target`: no longer on it after the reset.
    pub dropped: usize,
    /// Of `dropped`, those already on the upstream: pushing afterwards needs a force push.
    pub pushed: usize,
    /// Tracked files with uncommitted (staged or unstaged) changes: what a hard reset loses.
    pub changed: Vec<String>,
}

/// Plan resetting the checked-out branch to `target`, which must be on its history.
pub fn reset_plan(worktree: &Path, target: &str) -> Result<ResetPlan> {
    let Some(branch) = refs::current_branch(worktree)? else {
        bail!("Check out a branch first");
    };
    if let Some(op) = in_progress(worktree)? {
        bail!("A {op} is in progress; finish or abort it first");
    }
    let head = rev_parse(worktree, "HEAD")?;
    let target = rev_parse(worktree, &format!("{target}^{{commit}}"))?;
    if !is_ancestor(worktree, &target, "HEAD")? {
        bail!("{} isn't on {branch}'s history", &target[..8]);
    }
    let range = format!("{target}..{head}");
    let dropped = git::run(worktree, &["rev-list", "--count", &range])?;
    Ok(ResetPlan {
        dropped: dropped.trim().parse()?,
        pushed: pushed_not_on(worktree, &target)?,
        changed: changed_tracked(worktree)?,
        branch,
        head,
        target,
    })
}

/// Tracked files that differ from HEAD in the index or the worktree.
fn changed_tracked(worktree: &Path) -> Result<Vec<String>> {
    let args = ["-c", "core.quotepath=off", "diff", "HEAD", "--name-only"];
    Ok(git::run(worktree, &args)?
        .lines()
        .map(str::to_string)
        .collect())
}

/// Reset as planned in `plan`, refusing when the branch, HEAD or (for a hard reset) the
/// uncommitted changes are no longer what the user was shown.
pub fn reset(worktree: &Path, plan: &ResetPlan, mode: ResetMode) -> Result<()> {
    let branch = refs::current_branch(worktree)?;
    if branch.as_deref() != Some(&plan.branch) || rev_parse(worktree, "HEAD")? != plan.head {
        bail!("{} moved since you were asked; try again", plan.branch);
    }
    if let Some(op) = in_progress(worktree)? {
        bail!("A {op} is in progress; finish or abort it first");
    }
    if mode == ResetMode::Hard && changed_tracked(worktree)? != plan.changed {
        bail!("Your uncommitted changes changed since you were asked; try again");
    }
    git::run(worktree, &["reset", "-q", mode.flag(), &plan.target])?;
    Ok(())
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
    /// Stopped on conflicts in these paths, left in place to resolve and continue.
    Conflicts(Vec<String>),
    /// A squash stopped on conflicts in these paths and was undone, so nothing changed: it
    /// records no MERGE_HEAD, so there is no merge in progress to continue or abort.
    SquashConflicts(Vec<String>),
}

/// A merge, rebase, cherry-pick or revert that stopped partway, waiting for the user.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Operation {
    /// The git command, which takes `--continue` and `--abort` (and maybe `--skip`).
    pub fn command(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
        }
    }

    /// Skip drops the commit being applied; a merge has no commit to drop. `git revert
    /// --skip` exists too, but skipping a revert just keeps the commit, so it isn't offered.
    pub fn can_skip(self) -> bool {
        matches!(self, Operation::Rebase | Operation::CherryPick)
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.command())
    }
}

/// The merge, rebase, cherry-pick or revert already under way in `worktree`, if any.
pub fn in_progress(worktree: &Path) -> Result<Option<Operation>> {
    let states = [
        ("MERGE_HEAD", Operation::Merge),
        ("rebase-merge", Operation::Rebase),
        ("rebase-apply", Operation::Rebase),
        ("CHERRY_PICK_HEAD", Operation::CherryPick),
        ("REVERT_HEAD", Operation::Revert),
    ];
    let mut args = vec!["rev-parse"];
    for (file, _) in &states {
        args.extend(["--git-path", file]);
    }
    // Per-worktree paths: a linked worktree's live under `.git/worktrees/<name>`.
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

pub(crate) fn rev_parse(worktree: &Path, rev: &str) -> Result<String> {
    Ok(git::run(worktree, &["rev-parse", rev])?.trim().to_string())
}

/// Merge `branch` (local or remote-tracking) into the current branch. A squash stages the
/// result without committing. Conflicts are left to resolve, except a squash's: see
/// `MergeOutcome`.
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
        return stopped(worktree, e, mode == MergeMode::Squash, &before);
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
/// Conflicts are left to resolve: see `MergeOutcome::Conflicts`.
pub fn rebase(worktree: &Path, onto: &str) -> Result<MergeOutcome> {
    current_for(worktree, onto)?;
    ensure_clean(worktree)?;
    let before = rev_parse(worktree, "HEAD")?;
    if let Err(e) = git::run(worktree, &["rebase", onto]) {
        return stopped(worktree, e, false, &before);
    }
    Ok(if rev_parse(worktree, "HEAD")? == before {
        MergeOutcome::UpToDate
    } else {
        MergeOutcome::Done
    })
}

/// Apply `shas` onto the current branch in the order given (the caller puts the oldest
/// first). Conflicts are left to resolve: see `MergeOutcome::Conflicts`. Merge commits are
/// refused: picking one needs a mainline parent.
pub fn cherry_pick(worktree: &Path, shas: &[String]) -> Result<MergeOutcome> {
    if shas.is_empty() {
        bail!("No commits to cherry-pick");
    }
    if refs::current_branch(worktree)?.is_none() {
        bail!("Check out a branch first");
    }
    ensure_clean(worktree)?;
    for sha in shas {
        if parent_count(worktree, sha)? > 1 {
            bail!(
                "{} is a merge commit; cherry-picking merges isn't supported",
                &sha[..8.min(sha.len())]
            );
        }
    }
    let before = rev_parse(worktree, "HEAD")?;
    let mut args = vec!["cherry-pick"];
    args.extend(shas.iter().map(String::as_str));
    if let Err(e) = git::run(worktree, &args) {
        return stopped(worktree, e, false, &before);
    }
    Ok(MergeOutcome::Done)
}

/// `git revert --no-edit` of `sha` on the current branch. A merge commit needs `mainline`,
/// the parent (1-based) whose side is kept. Conflicts are left to resolve, as for merge.
pub fn revert(worktree: &Path, sha: &str, mainline: Option<usize>) -> Result<MergeOutcome> {
    if refs::current_branch(worktree)?.is_none() {
        bail!("Check out a branch first");
    }
    ensure_clean(worktree)?;
    let parents = parent_count(worktree, sha)?;
    let m = match (parents > 1, mainline) {
        (true, Some(m)) if (1..=parents).contains(&m) => Some(m.to_string()),
        (true, Some(m)) => bail!("A merge commit with {parents} parents has no parent {m}"),
        (true, None) => bail!("Choose which parent of the merge to keep"),
        (false, Some(_)) => bail!("Only a merge commit takes a mainline parent"),
        (false, None) => None,
    };
    let before = rev_parse(worktree, "HEAD")?;
    let mut args = vec!["revert", "--no-edit"];
    if let Some(m) = &m {
        args.extend(["-m", m]);
    }
    args.push(sha);
    if let Err(e) = git::run(worktree, &args) {
        return stopped(worktree, e, false, &before);
    }
    Ok(MergeOutcome::Done)
}

/// The parents of commit `rev`, in order.
pub fn parents(worktree: &Path, rev: &str) -> Result<Vec<String>> {
    let out = git::run(worktree, &["rev-list", "--parents", "-n", "1", rev, "--"])?;
    Ok(out.split_whitespace().skip(1).map(String::from).collect())
}

/// Number of parents of commit `rev`.
fn parent_count(worktree: &Path, rev: &str) -> Result<usize> {
    Ok(parents(worktree, rev)?.len())
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

/// Conflicted (unmerged) paths in `worktree`.
pub fn unmerged(worktree: &Path) -> Result<Vec<String>> {
    let args = [
        "-c",
        "core.quotepath=off",
        "diff",
        "--name-only",
        "--diff-filter=U",
    ];
    Ok(git::run(worktree, &args)?
        .lines()
        .map(str::to_string)
        .collect())
}

/// After a failed merge or rebase: with conflicts, leave them for the in-progress banner,
/// except a squash's, which is undone and checked to be back at `before`. Without
/// conflicts (e.g. an untracked file in the way, which can stop a rebase partway), abort
/// whatever was started and return git's own error.
pub(crate) fn stopped(
    worktree: &Path,
    err: anyhow::Error,
    squash: bool,
    before: &str,
) -> Result<MergeOutcome> {
    let conflicts = unmerged(worktree)?;
    if conflicts.is_empty() {
        if let Some(op) = in_progress(worktree)? {
            git::run(worktree, &[op.command(), "--abort"])?;
        }
        return Err(err);
    }
    if !squash {
        return Ok(MergeOutcome::Conflicts(conflicts));
    }
    // A squash records no MERGE_HEAD, so there is no merge for `--abort` to find.
    git::run(worktree, &["reset", "--merge"])?;
    if rev_parse(worktree, "HEAD")? != before || is_dirty(worktree)? {
        bail!(
            "conflicts in {}, and undoing the squash did not restore {before}; check the repository",
            conflicts.join(", ")
        );
    }
    Ok(MergeOutcome::SquashConflicts(conflicts))
}

/// Run `git <op> <flag>` for the operation in progress. It may stop again on the next
/// commit's conflicts, which are left in place like the first ones.
fn step(worktree: &Path, op: Operation, flag: &str) -> Result<MergeOutcome> {
    if in_progress(worktree)? != Some(op) {
        bail!("No {op} is in progress any more");
    }
    let Err(e) = git::run(worktree, &[op.command(), flag]) else {
        return Ok(MergeOutcome::Done);
    };
    let conflicts = unmerged(worktree)?;
    if !conflicts.is_empty() && in_progress(worktree)? == Some(op) {
        return Ok(MergeOutcome::Conflicts(conflicts));
    }
    Err(e)
}

/// Finish the step that stopped (committing a merge with git's default message) and go
/// on. Refused while any file is still conflicted.
pub fn continue_op(worktree: &Path, op: Operation) -> Result<MergeOutcome> {
    if !unmerged(worktree)?.is_empty() {
        bail!("Resolve the conflicted files first");
    }
    step(worktree, op, "--continue")
}

/// Drop the commit a rebase or cherry-pick stopped on, and go on with the next one.
pub fn skip_op(worktree: &Path, op: Operation) -> Result<MergeOutcome> {
    if !op.can_skip() {
        bail!("A {op} can't skip");
    }
    step(worktree, op, "--skip")
}

/// Undo the whole operation: back to the branch and commit it started from.
pub fn abort_op(worktree: &Path, op: Operation) -> Result<()> {
    if in_progress(worktree)? != Some(op) {
        bail!("No {op} is in progress any more");
    }
    git::run(worktree, &[op.command(), "--abort"])?;
    Ok(())
}

/// Which version of a conflicted file to keep. Mine is always the user's branch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Version {
    Mine,
    Theirs,
}

/// git's name for `version` during `op`: `--ours` is HEAD, which during a rebase is the
/// branch being rebased onto, so there mine is `--theirs`. A stash pop (no operation)
/// keeps HEAD as ours too.
pub fn git_side(op: Option<Operation>, version: Version) -> &'static str {
    let rebase = op == Some(Operation::Rebase);
    match (version, rebase) {
        (Version::Mine, false) | (Version::Theirs, true) => "--ours",
        (Version::Theirs, false) | (Version::Mine, true) => "--theirs",
    }
}

/// Resolve `path` with `version`, dropping the other side and any edits to the file. When
/// that version deleted the file, it is removed (`git rm`).
pub fn use_version(
    worktree: &Path,
    op: Option<Operation>,
    path: &str,
    version: Version,
) -> Result<()> {
    let side = git_side(op, version);
    let stage = if side == "--ours" { "2" } else { "3" };
    // `<mode> <sha> <stage>\t<path>` per index entry; a side that deleted it has none.
    let out = git::run(
        worktree,
        &["--literal-pathspecs", "ls-files", "-u", "-z", "--", path],
    )?;
    if out.is_empty() {
        bail!("{path} is not conflicted");
    }
    let kept = out
        .split('\0')
        .filter_map(|e| e.split_once('\t'))
        .any(|(meta, _)| meta.rsplit(' ').next() == Some(stage));
    if !kept {
        git::run(worktree, &["--literal-pathspecs", "rm", "-q", "--", path])?;
        return Ok(());
    }
    git::run(
        worktree,
        &["--literal-pathspecs", "checkout", side, "--", path],
    )?;
    git::run(worktree, &["--literal-pathspecs", "add", "--", path])?;
    Ok(())
}

/// Lines git writes around a conflict: `<<<<<<< ours` and `>>>>>>> theirs`. A lone
/// `=======` is left out, as it is also a Markdown heading underline.
pub fn has_conflict_markers(text: &str) -> bool {
    text.lines().any(|l| {
        let marker = |m: &str| l == m || l.strip_prefix(m).is_some_and(|r| r.starts_with(' '));
        marker("<<<<<<<") || marker(">>>>>>>")
    })
}

/// Mark `path` resolved as it is in the worktree (deleted, if it is gone). Without `anyway`
/// a text file still holding conflict markers is refused: false, and nothing changes.
pub fn mark_resolved(worktree: &Path, path: &str, anyway: bool) -> Result<bool> {
    if !anyway {
        if let Ok(bytes) = std::fs::read(worktree.join(path)) {
            let text = !bytes.contains(&0);
            if text && has_conflict_markers(&String::from_utf8_lossy(&bytes)) {
                return Ok(false);
            }
        }
    }
    git::run(worktree, &["--literal-pathspecs", "add", "-A", "--", path])?;
    Ok(true)
}

/// Save the built-in merge tool's `result` over conflicted `path`, then mark it resolved.
/// `original` is the text the merge started from: if the file changed since (another
/// tool, an editor), nothing is written. Like `mark_resolved`, false means conflict
/// markers are left, the file is saved but not marked, and the caller asks first.
pub fn save_resolution(worktree: &Path, path: &str, original: &str, result: &str) -> Result<bool> {
    let full = worktree.join(path);
    // Only ever overwrite a regular file inside the worktree, never through a symlink.
    if !std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_file()) {
        bail!("{path} is not a regular file");
    }
    if std::fs::read(&full)? != original.as_bytes() {
        bail!("{path} changed on disk since the merge started; close it and open it again");
    }
    std::fs::write(&full, result)?;
    mark_resolved(worktree, path, false)
}

/// Open `path` in its default macOS app, as a double-click in Finder would.
pub fn open_file(path: &Path) -> Result<()> {
    let out = std::process::Command::new("open").arg(path).output()?;
    if !out.status.success() {
        bail!("open: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn fetch(dir: &Path) -> Result<()> {
    git::run(dir, &["fetch", "--all", "--prune"])?;
    Ok(())
}

/// `git pull` arguments for `strategy`, all explicit so the user's `pull.rebase`,
/// `pull.ff` or `branch.<name>.rebase` cannot turn it into a different kind of pull.
pub fn pull_args(strategy: PullStrategy) -> &'static [&'static str] {
    match strategy {
        PullStrategy::FastForwardOnly => &["pull", "--no-rebase", "--ff-only"],
        PullStrategy::Merge => &["pull", "--no-rebase", "--ff", "--no-edit"],
        PullStrategy::Rebase => &["pull", "--rebase=true"],
    }
}

/// The strategy the user's git config asks for: `branch.<name>.rebase` over `pull.rebase`,
/// then `pull.ff`. With neither set it stays fast-forward only, as Spinney always pulled,
/// rather than git's merge: a pull never makes a merge commit nobody asked for.
pub fn strategy_from_git(
    branch_rebase: Option<&str>,
    pull_rebase: Option<&str>,
    pull_ff: Option<&str>,
) -> PullStrategy {
    let truthy = |v: &str| {
        let v = v.trim().to_ascii_lowercase();
        // `merges` and `interactive` rebase too; Spinney replays plainly, without an editor.
        [
            "true",
            "yes",
            "on",
            "1",
            "merges",
            "m",
            "interactive",
            "i",
            "preserve",
            "p",
        ]
        .contains(&v.as_str())
    };
    let ff = pull_ff.map(|v| v.trim().to_ascii_lowercase());
    match branch_rebase.or(pull_rebase) {
        Some(v) if truthy(v) => PullStrategy::Rebase,
        _ if ff.as_deref() == Some("only") => PullStrategy::FastForwardOnly,
        // An explicit `rebase = false` or a `pull.ff` other than `only` means merge.
        Some(_) => PullStrategy::Merge,
        None if ff.is_some() => PullStrategy::Merge,
        None => PullStrategy::FastForwardOnly,
    }
}

/// `strategy_from_git` for the branch checked out in `worktree`.
pub fn git_pull_strategy(worktree: &Path) -> Result<PullStrategy> {
    // Exit 1 is an unset key.
    let get = |key: &str| -> Result<Option<String>> {
        let v = git::run_ok_codes(worktree, &["config", "--get", key], &[0, 1])?;
        Ok(Some(v.trim().to_string()).filter(|v| !v.is_empty()))
    };
    let branch_rebase = match refs::current_branch(worktree)? {
        Some(b) => get(&format!("branch.{b}.rebase"))?,
        None => None,
    };
    let (pull_rebase, pull_ff) = (get("pull.rebase")?, get("pull.ff")?);
    Ok(strategy_from_git(
        branch_rebase.as_deref(),
        pull_rebase.as_deref(),
        pull_ff.as_deref(),
    ))
}

/// A merge or rebase pull that stopped on conflicts, left in place for the in-progress
/// banner. An error so it can come back from a background job, like `PushRejected`.
#[derive(Clone, Debug, PartialEq)]
pub struct PullConflicts {
    pub strategy: PullStrategy,
    pub files: Vec<String>,
}

impl std::fmt::Display for PullConflicts {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "pull stopped on conflicts in {}", self.files.join(", "))
    }
}

impl std::error::Error for PullConflicts {}

/// Pull the current branch's upstream with `strategy`. A merge or rebase that conflicts is
/// left in progress (`MergeOutcome::Conflicts`); anything else that stops it is undone.
/// Fast-forward only refuses a diverged branch and changes nothing.
pub fn pull(worktree: &Path, strategy: PullStrategy) -> Result<MergeOutcome> {
    if strategy != PullStrategy::FastForwardOnly {
        // As for merge and rebase: never on top of another operation or local changes.
        ensure_clean(worktree)?;
    }
    let before = rev_parse(worktree, "HEAD")?;
    if let Err(e) = git::run(worktree, pull_args(strategy)) {
        if strategy == PullStrategy::FastForwardOnly {
            // It never starts an operation, so one in progress is the user's: leave it.
            if format!("{e:#}").contains("fast-forward") && diverged(worktree) {
                bail!("The branch and its upstream have diverged; pull with Merge or Rebase (▾)");
            }
            return Err(e);
        }
        return stopped(worktree, e, false, &before);
    }
    Ok(if rev_parse(worktree, "HEAD")? == before {
        MergeOutcome::UpToDate
    } else {
        MergeOutcome::Done
    })
}

/// HEAD and its upstream each have commits the other lacks.
fn diverged(worktree: &Path) -> bool {
    let counts = git::run(
        worktree,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    );
    counts.is_ok_and(|c| {
        let mut n = c
            .split_whitespace()
            .map(|n| n.parse::<usize>().unwrap_or(0));
        n.next().unwrap_or(0) > 0 && n.next().unwrap_or(0) > 0
    })
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

/// Switch `active_worktree` to `branch`, or jump to the worktree (Spinney's, an agent's) that
/// already has it, as git refuses to check a branch out twice.
pub fn checkout(root: &Path, active_worktree: &Path, branch: &str) -> Result<CheckoutOutcome> {
    let elsewhere = list_worktrees(root)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch) && !same_dir(&w.path, active_worktree));
    if let Some(w) = elsewhere {
        return Ok(CheckoutOutcome::JumpTo(w.path));
    }
    git::run(active_worktree, &["switch", branch])?;
    Ok(CheckoutOutcome::Switched)
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

#[cfg(test)]
pub fn create_branch(worktree: &Path, name: &str, base: &str) -> Result<()> {
    git::run(worktree, &["branch", name, base])?;
    Ok(())
}

/// The commit HEAD is detached at, or None when a branch (even an unborn one) is out.
pub fn detached_head(worktree: &Path) -> Result<Option<String>> {
    if refs::current_branch(worktree)?.is_some() {
        return Ok(None);
    }
    head_commit(worktree)
}

/// The commit HEAD points at, or None before the first commit.
pub fn head_commit(worktree: &Path) -> Result<Option<String>> {
    let out = git::run_ok_codes(worktree, &["rev-parse", "--verify", "-q", "HEAD"], &[0, 1])?;
    let sha = out.trim();
    Ok((!sha.is_empty()).then(|| sha.to_string()))
}

/// How far back the checkout history is searched for the last branch.
const PREVIOUS_LOOKBACK: usize = 10;

/// The most recently checked-out branch that still exists, other than the current one,
/// from the worktree's checkout history (`@{-N}`).
pub fn previous_branch(worktree: &Path) -> Result<Option<String>> {
    let current = refs::current_branch(worktree)?;
    for n in 1..=PREVIOUS_LOOKBACK {
        let rev = format!("@{{-{n}}}");
        // Fails for a deleted branch or once the history runs out; empty when that
        // checkout was detached.
        let Ok(out) = git::run(worktree, &["rev-parse", "--symbolic-full-name", &rev]) else {
            continue;
        };
        let Some(name) = out.trim().strip_prefix("refs/heads/") else {
            continue;
        };
        let full = format!("refs/heads/{name}");
        let exists = git::run(worktree, &["rev-parse", "--verify", "-q", &full]).is_ok();
        if exists && current.as_deref() != Some(name) {
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

/// Commits reachable from a detached HEAD but from no branch, tag or remote-tracking ref:
/// what leaving HEAD would make hard to find. Zero when a branch is checked out.
pub fn detached_orphans(worktree: &Path) -> Result<usize> {
    if detached_head(worktree)?.is_none() {
        return Ok(0);
    }
    let args = [
        "rev-list",
        "--count",
        "HEAD",
        "--not",
        "--branches",
        "--tags",
        "--remotes",
    ];
    Ok(git::run(worktree, &args)?.trim().parse()?)
}

/// Create `name` at `base` and switch to it. Moving to another commit refuses over
/// uncommitted changes, as `checkout_detached` does; from HEAD itself they come along.
pub fn create_and_switch(worktree: &Path, name: &str, base: &str) -> Result<()> {
    if rev_parse(worktree, &format!("{base}^{{commit}}"))? != rev_parse(worktree, "HEAD")? {
        ensure_clean(worktree)?;
    }
    git::run(worktree, &["switch", "-c", name, base])?;
    Ok(())
}

/// Check out commit `sha` with a detached HEAD, refusing over uncommitted changes or an
/// operation in progress (git would carry the changes along, mixing them into another base).
pub fn checkout_detached(worktree: &Path, sha: &str) -> Result<()> {
    ensure_clean(worktree)?;
    git::run(worktree, &["switch", "--detach", sha])?;
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

/// What deleting several local branches did, branch by branch.
#[derive(Debug, Default, PartialEq)]
pub struct BulkDelete {
    pub deleted: Vec<String>,
    /// Left alone: `-d` refused them, so they can be asked about again with force.
    pub not_merged: Vec<String>,
    pub errors: Vec<String>,
}

/// `delete_branch` for each of `branches`, carrying on past the ones that fail.
pub fn delete_branches(worktree: &Path, branches: &[String], force: bool) -> BulkDelete {
    let mut out = BulkDelete::default();
    for b in branches {
        match delete_branch(worktree, b, force) {
            Ok(DeleteOutcome::Deleted) => out.deleted.push(b.clone()),
            Ok(DeleteOutcome::NotMerged) => out.not_merged.push(b.clone()),
            Err(e) => out.errors.push(format!("{b}: {e:#}")),
        }
    }
    out
}

/// One atomic `git push <remote> --delete` per remote for `remote_branches`
/// (`origin/feat/a`, …): a remote either drops them all or none.
pub fn delete_remote_branches(worktree: &Path, remote_branches: &[String]) -> Result<()> {
    let mut by_remote: Vec<(&str, Vec<String>)> = vec![];
    for rb in remote_branches {
        let Some((remote, name)) = rb.split_once('/') else {
            bail!("{rb} is not a remote branch");
        };
        // Full refs, so a tag with the same name is never the one deleted.
        let full = format!("refs/heads/{name}");
        match by_remote.iter_mut().find(|(r, _)| *r == remote) {
            Some((_, refs)) => refs.push(full),
            None => by_remote.push((remote, vec![full])),
        }
    }
    for (remote, refs) in by_remote {
        let mut args = vec!["push", "--atomic", remote, "--delete"];
        args.extend(refs.iter().map(String::as_str));
        git::run(worktree, &args)?;
    }
    Ok(())
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

/// Why `name` can't be a new tag's name, checked with `git check-ref-format`. A leading
/// `-` passes that check but `git tag` would read it as an option.
pub fn check_tag_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Enter a tag name");
    }
    if name.starts_with('-') {
        bail!("{name} is not a valid tag name: it can't start with -");
    }
    // Outside any repo, like `check_branch_name`.
    let full = format!("refs/tags/{name}");
    if git::run(&std::env::temp_dir(), &["check-ref-format", &full]).is_err() {
        bail!("{name} is not a valid tag name");
    }
    Ok(())
}

/// Whether `refs/tags/<name>` exists (the full ref, so a same-named branch doesn't count).
pub fn tag_exists(worktree: &Path, name: &str) -> Result<bool> {
    let full = format!("refs/tags/{name}");
    let out = git::run_ok_codes(worktree, &["rev-parse", "--verify", "-q", &full], &[0, 1])?;
    Ok(!out.trim().is_empty())
}

/// `git tag` arguments for tagging `target`: annotated (`-a`, its message on stdin, so no
/// editor) when there is a message, lightweight otherwise. Never `-f`.
pub fn tag_args<'a>(name: &'a str, target: &'a str, annotated: bool) -> Vec<&'a str> {
    if annotated {
        vec!["tag", "-a", "-F", "-", name, target]
    } else {
        vec!["tag", name, target]
    }
}

/// Tag `target` as `name`, annotated when `message` has any text. An existing tag is
/// refused, never moved.
pub fn create_tag(worktree: &Path, name: &str, target: &str, message: Option<&str>) -> Result<()> {
    check_tag_name(name)?;
    if tag_exists(worktree, name)? {
        bail!("A tag named {name} already exists");
    }
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    let args = tag_args(name, target, message.is_some());
    match message {
        Some(m) => git::run_stdin(worktree, &args, format!("{m}\n").as_bytes())?,
        None => git::run(worktree, &args)?,
    };
    Ok(())
}

/// The commit `rev` (a SHA or a full ref) names, so a tag pins what the user was shown.
pub fn resolve_commit(worktree: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    let args = ["rev-parse", "--verify", "-q", "--end-of-options", &spec];
    let out = git::run_ok_codes(worktree, &args, &[0, 1])?;
    match out.trim() {
        "" => bail!("{rev} is not a commit"),
        sha => Ok(sha.to_string()),
    }
}

/// Whether the repository has a remote called `name`.
pub fn has_remote(worktree: &Path, name: &str) -> Result<bool> {
    let out = git::run(worktree, &["remote"])?;
    Ok(out.lines().any(|l| l == name))
}

/// `git push <remote> refs/tags/<tag>`: the full ref, so a branch with the same name is
/// never the one pushed. A tag already on the remote at another commit is rejected.
pub fn push_tag(worktree: &Path, remote: &str, tag: &str) -> Result<()> {
    let full = format!("refs/tags/{tag}");
    git::run(worktree, &["push", remote, &format!("{full}:{full}")])?;
    Ok(())
}

/// `git tag -d`: only the name goes; the commit stays on its branches.
pub fn delete_tag(worktree: &Path, tag: &str) -> Result<()> {
    git::run(worktree, &["tag", "-d", tag])?;
    Ok(())
}

/// `git push <remote> --delete refs/tags/<tag>`.
pub fn delete_remote_tag(worktree: &Path, remote: &str, tag: &str) -> Result<()> {
    let full = format!("refs/tags/{tag}");
    git::run(worktree, &["push", remote, "--delete", &full])?;
    Ok(())
}

/// Fails when a worktree other than `worktree` has `branch` checked out: renaming it would
/// change that worktree's branch under it.
pub fn ensure_not_checked_out_elsewhere(worktree: &Path, branch: &str) -> Result<()> {
    let here = worktree.canonicalize()?;
    let other = list_worktrees(worktree)?.into_iter().find(|w| {
        w.branch.as_deref() == Some(branch) && w.path.canonicalize().ok().as_ref() != Some(&here)
    });
    if let Some(w) = other {
        let name = w.path.file_name().unwrap_or(w.path.as_os_str());
        bail!(
            "{branch} is checked out in worktree {}",
            name.to_string_lossy()
        );
    }
    Ok(())
}

/// `git branch -m old new`, refusing an invalid name or a branch checked out in another
/// worktree. `-m`, not `-M`: an existing `new` is refused, never overwritten.
pub fn rename_branch(worktree: &Path, old: &str, new: &str) -> Result<()> {
    if new == old {
        bail!("{old} already has that name");
    }
    if crate::project::check_branch_name(new).is_err() {
        bail!("{new} is not a valid branch name");
    }
    ensure_not_checked_out_elsewhere(worktree, old)?;
    git::run(worktree, &["branch", "-m", old, new])?;
    Ok(())
}

/// A branch whose upstream has its own name on `remote`, so it can be renamed there too.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteRename {
    pub remote: String,
    /// The remote-tracking branch, e.g. `origin/feature/x`.
    pub tracking: String,
    /// Where `tracking` pointed when the user asked: the commit the new name gets, and the
    /// lease on deleting the old one.
    pub expect: String,
}

/// The remote rename on offer for `branch`: when its upstream is `<remote>/<branch>` and
/// the remote-tracking ref exists.
pub fn remote_rename(worktree: &Path, branch: &str) -> Result<Option<RemoteRename>> {
    let format = "--format=%(upstream)%00%(upstream:short)%00%(upstream:remotename)%00\
                  %(upstream:remoteref)";
    let local = format!("refs/heads/{branch}");
    let up = git::run(worktree, &["for-each-ref", format, &local])?;
    let fields: Vec<&str> = up.trim_end().split('\0').collect();
    let [tip, tracking, remote, remote_ref] = fields[..] else {
        return Ok(None);
    };
    if !tip.starts_with("refs/remotes/") || remote_ref != local {
        return Ok(None);
    }
    let Ok(expect) = git::run(worktree, &["rev-parse", "--verify", "--quiet", tip]) else {
        return Ok(None); // the upstream is gone
    };
    Ok(Some(RemoteRename {
        remote: remote.to_string(),
        tracking: tracking.to_string(),
        expect: expect.trim().to_string(),
    }))
}

/// After renaming `old` to `new` locally: push `r.expect` as `new` and delete `old` on the
/// remote in one atomic push, then track the new name. Both refs are leased: `old` must
/// still be at `r.expect` and `new` must not exist yet, or nothing changes.
pub fn rename_on_remote(worktree: &Path, r: &RemoteRename, old: &str, new: &str) -> Result<()> {
    let (old_ref, new_ref) = (format!("refs/heads/{old}"), format!("refs/heads/{new}"));
    let args = [
        "push",
        "--porcelain",
        "--atomic",
        &format!("--force-with-lease={old_ref}:{}", r.expect),
        &format!("--force-with-lease={new_ref}:"),
        &r.remote,
        &format!("{}:{new_ref}", r.expect),
        &format!(":{old_ref}"),
    ];
    let out = git::output(worktree, &args)?;
    if !out.status.success() {
        if porcelain_rejected(&out.stdout, &["(stale info)"]) {
            bail!(
                "{old} or {new} changed on {} since the last fetch; fetch and try again \
                 (renamed locally only)",
                r.remote
            );
        }
        bail!("renamed locally only; git push failed: {}", stderr(&out));
    }
    let key = |k: &str| format!("branch.{new}.{k}");
    git::run(worktree, &["config", &key("remote"), &r.remote])?;
    git::run(worktree, &["config", &key("merge"), &new_ref])?;
    Ok(())
}

/// `git merge-base --is-ancestor a b`: `a` is reachable from `b`.
pub(crate) fn is_ancestor(worktree: &Path, a: &str, b: &str) -> Result<bool> {
    let out = git::output(worktree, &["merge-base", "--is-ancestor", a, b])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base failed: {}", stderr(&out)),
    }
}

/// Fast-forward local `branch`, checked out nowhere, to its upstream without checking it
/// out (handy for `main` in the worktree layout).
pub fn fast_forward(worktree: &Path, branch: &str) -> Result<MergeOutcome> {
    let local = format!("refs/heads/{branch}");
    let up = git::run(worktree, &["for-each-ref", "--format=%(upstream)", &local])?;
    let up = up.trim();
    if up.is_empty() {
        bail!("{branch} has no upstream");
    }
    let short = up.strip_prefix("refs/remotes/").unwrap_or(up);
    fast_forward_to(worktree, branch, short)
}

/// Whether local `branch` can move forward to `target`: it is behind and not diverged.
pub fn can_fast_forward(worktree: &Path, branch: &str, target: &str) -> Result<bool> {
    let old = rev_parse(worktree, &format!("refs/heads/{branch}"))?;
    let new = rev_parse(worktree, &format!("{target}^{{commit}}"))?;
    Ok(old != new && is_ancestor(worktree, &old, &new)?)
}

/// Move local `branch` forward to `target` when that only adds commits, like
/// `git fetch . target:branch`. Refused for a branch any worktree has checked out (its
/// files would no longer match) and when the two have diverged.
pub fn fast_forward_to(worktree: &Path, branch: &str, target: &str) -> Result<MergeOutcome> {
    ensure_not_checked_out(worktree, branch)?;
    let local = format!("refs/heads/{branch}");
    let old = rev_parse(worktree, &local)?;
    let new = rev_parse(worktree, &format!("{target}^{{commit}}"))?;
    if is_ancestor(worktree, &new, &old)? {
        return Ok(MergeOutcome::UpToDate);
    }
    if !is_ancestor(worktree, &old, &new)? {
        bail!("{branch} has diverged from {target}; check it out to merge or rebase");
    }
    // Pinned to `old`: if the branch moved since it was checked, nothing changes.
    let msg = format!("fast-forward to {target}");
    git::run(worktree, &["update-ref", "-m", &msg, &local, &new, &old])?;
    Ok(MergeOutcome::Done)
}

/// What removing a worktree together with its branch loses, shown before it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoveWithBranch {
    pub path: PathBuf,
    pub branch: String,
    /// Uncommitted or untracked files in the worktree.
    pub dirty: bool,
    /// Those files, for the confirmation to name (a nested repo makes it dirty unnamed).
    pub dirty_files: Vec<String>,
    /// What `git branch -d` checks against: the upstream, else the project's HEAD.
    pub base: String,
    /// Commits on the branch that are not on `base`.
    pub unmerged: usize,
}

/// Check that the worktree at `path` can go with its branch: it has a branch, no other
/// worktree has that branch out, and it is not the main working tree.
pub fn plan_remove_with_branch(root: &Path, path: &Path) -> Result<RemoveWithBranch> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let target = canon(path);
    if target == canon(root) {
        bail!("the main working tree can't be removed");
    }
    let worktrees = list_worktrees(root)?;
    let Some(wt) = worktrees.iter().find(|w| canon(&w.path) == target) else {
        bail!("{} is not a worktree of this project", path.display());
    };
    let Some(branch) = wt.branch.clone() else {
        bail!("this worktree has no branch (detached HEAD)");
    };
    let elsewhere = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(&branch) && canon(&w.path) != target);
    if let Some(w) = elsewhere {
        let name = w.path.file_name().unwrap_or(w.path.as_os_str());
        bail!(
            "{branch} is also checked out in worktree {}",
            name.to_string_lossy()
        );
    }
    let dirty = !git::run(path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    let dirty_files = if dirty {
        crate::status::status(path)?.changed_paths()
    } else {
        vec![]
    };
    let local = format!("refs/heads/{branch}");
    let up = git::run(
        root,
        &["for-each-ref", "--format=%(upstream:short)", &local],
    )?;
    let base = match up.trim() {
        "" => refs::current_branch(root)?.unwrap_or_else(|| "HEAD".into()),
        up => up.to_string(),
    };
    let range = format!("{base}..{local}");
    let unmerged = git::run(root, &["rev-list", "--count", &range])?
        .trim()
        .parse()?;
    Ok(RemoveWithBranch {
        path: path.to_path_buf(),
        branch,
        dirty,
        dirty_files,
        base,
        unmerged,
    })
}

/// Remove the worktree (forcing only when the confirmed plan said it was dirty), then
/// delete its branch: `-D` only when the plan said it was unmerged. A branch that turns out
/// unmerged anyway is kept and reported as `NotMerged`, for the usual prompt.
pub fn remove_with_branch(root: &Path, plan: &RemoveWithBranch) -> Result<DeleteOutcome> {
    crate::project::remove_worktree(root, &plan.path, plan.dirty)?;
    delete_branch(root, &plan.branch, plan.unmerged > 0)
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

    /// Normal clone; returns (tmp, root, main worktree), which are the same folder.
    fn cloned() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, url, parent) = fixture();
        let root = clone_into(&url, &parent, "proj", &space()).unwrap();
        let main = root.clone();
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
        assert_eq!(
            pull(&main, PullStrategy::FastForwardOnly).unwrap(),
            MergeOutcome::Done
        );
        assert!(main.join("r.txt").exists());
        assert_eq!(
            pull(&main, PullStrategy::FastForwardOnly).unwrap(),
            MergeOutcome::UpToDate
        );
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
    fn create_lightweight_tag() {
        let (_tmp, _root, main) = cloned();
        let head = head_commit(&main).unwrap().unwrap();
        create_tag(&main, "v1.0", &head, None).unwrap();
        let t = git::run(&main, &["cat-file", "-t", "refs/tags/v1.0"]).unwrap();
        assert_eq!(t.trim(), "commit");
        let at = git::run(&main, &["rev-parse", "refs/tags/v1.0"]).unwrap();
        assert_eq!(at.trim(), head);
        // A blank message is no message.
        create_tag(&main, "v1.1", &head, Some("  \n")).unwrap();
        let t = git::run(&main, &["cat-file", "-t", "refs/tags/v1.1"]).unwrap();
        assert_eq!(t.trim(), "commit");
    }

    #[test]
    fn create_annotated_tag() {
        let (_tmp, _root, main) = cloned();
        let first = git::run(&main, &["rev-parse", "HEAD"]).unwrap();
        write_commit(&main, "n.txt");
        let msg = "Release 2\n\nWith notes.";
        create_tag(&main, "v2", first.trim(), Some(msg)).unwrap();
        let t = git::run(&main, &["cat-file", "-t", "refs/tags/v2"]).unwrap();
        assert_eq!(t.trim(), "tag");
        let body = git::run(&main, &["cat-file", "tag", "refs/tags/v2"]).unwrap();
        assert!(body.contains("Release 2"), "{body}");
        let at = git::run(&main, &["rev-parse", "refs/tags/v2^{commit}"]).unwrap();
        assert_eq!(at, first, "tags the commit asked for, not HEAD");
    }

    #[test]
    fn invalid_tag_names_are_refused() {
        let (_tmp, _root, main) = cloned();
        for bad in [
            "",
            "a..b",
            "has space",
            "end.",
            "x.lock",
            "-x",
            "a~1",
            "a:b",
        ] {
            assert!(check_tag_name(bad).is_err(), "{bad:?} accepted");
            assert!(create_tag(&main, bad, "HEAD", None).is_err(), "{bad:?}");
        }
        for good in ["v1.2.3", "release/2026-10", "beta-1"] {
            check_tag_name(good).unwrap();
        }
        let tags = git::run(&main, &["tag", "-l"]).unwrap();
        assert_eq!(tags, "", "nothing created");
    }

    #[test]
    fn existing_tag_is_never_overwritten() {
        let (_tmp, _root, main) = cloned();
        let first = git::run(&main, &["rev-parse", "HEAD"]).unwrap();
        create_tag(&main, "v1", "HEAD", None).unwrap();
        write_commit(&main, "n.txt");
        let err = create_tag(&main, "v1", "HEAD", Some("again")).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        let at = git::run(&main, &["rev-parse", "refs/tags/v1"]).unwrap();
        assert_eq!(at, first);
        // A branch of the same name doesn't count as a tag.
        git::run(&main, &["branch", "same"]).unwrap();
        assert!(!tag_exists(&main, "same").unwrap());
    }

    #[test]
    fn resolve_commit_peels_refs() {
        let (_tmp, _root, main) = cloned();
        let head = head_commit(&main).unwrap().unwrap();
        assert_eq!(resolve_commit(&main, "refs/heads/main").unwrap(), head);
        let x = resolve_commit(&main, "refs/remotes/origin/feature/x").unwrap();
        assert_ne!(x, head);
        assert!(resolve_commit(&main, "refs/heads/nope").is_err());
        assert!(has_remote(&main, "origin").unwrap());
        assert!(!has_remote(&main, "upstream").unwrap());
    }

    #[test]
    fn tag_args_never_force() {
        assert_eq!(tag_args("v1", "abc", false), ["tag", "v1", "abc"]);
        assert_eq!(
            tag_args("v1", "abc", true),
            ["tag", "-a", "-F", "-", "v1", "abc"]
        );
    }

    #[test]
    fn push_tag_pushes_only_the_tag() {
        let (tmp, _root, main) = cloned();
        git::run(&main, &["switch", "-q", "-c", "unpushed"]).unwrap();
        write_commit(&main, "t.txt");
        create_tag(&main, "v3", "HEAD", Some("three")).unwrap();
        create_tag(&main, "other", "HEAD", None).unwrap();
        push_tag(&main, "origin", "v3").unwrap();
        let remote = remote_dir(&tmp);
        let t = git::run(&remote, &["cat-file", "-t", "refs/tags/v3"]).unwrap();
        assert_eq!(t.trim(), "tag");
        assert!(!tag_exists(&remote, "other").unwrap(), "only the one tag");
        let branch = git::run_ok_codes(
            &remote,
            &["rev-parse", "--verify", "-q", "refs/heads/unpushed"],
            &[0, 1],
        )
        .unwrap();
        assert_eq!(branch, "", "the branch isn't pushed");
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

    /// `ours` and the remote each have a commit the other lacks: o.txt and t.txt, or with
    /// `clash`, different edits to a.txt.
    fn diverged_clones(clash: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, _remote, ours, theirs) = two_clones();
        let edit = |dir: &Path, file: &str, text: &str| {
            std::fs::write(dir.join(file), text).unwrap();
            git::run(dir, &["add", "."]).unwrap();
            commit(dir, &format!("edit {file}")).unwrap();
        };
        if clash {
            edit(&theirs, "a.txt", "theirs\n");
            edit(&ours, "a.txt", "ours\n");
        } else {
            edit(&theirs, "t.txt", "t\n");
            edit(&ours, "o.txt", "o\n");
        }
        push(&theirs).unwrap();
        (tmp, ours, theirs)
    }

    #[test]
    fn pull_args_are_explicit() {
        assert_eq!(
            pull_args(PullStrategy::FastForwardOnly),
            ["pull", "--no-rebase", "--ff-only"]
        );
        assert_eq!(
            pull_args(PullStrategy::Merge),
            ["pull", "--no-rebase", "--ff", "--no-edit"]
        );
        assert_eq!(pull_args(PullStrategy::Rebase), ["pull", "--rebase=true"]);
    }

    #[test]
    fn strategy_from_git_config() {
        use PullStrategy::*;
        let s = strategy_from_git;
        assert_eq!(s(None, None, None), FastForwardOnly, "nothing set");
        assert_eq!(s(None, Some("true"), None), Rebase);
        assert_eq!(s(None, Some("merges"), None), Rebase);
        assert_eq!(s(None, Some("Interactive"), Some("only")), Rebase);
        assert_eq!(s(None, Some("false"), None), Merge);
        assert_eq!(s(None, Some("false"), Some("only")), FastForwardOnly);
        assert_eq!(s(None, None, Some("only")), FastForwardOnly);
        assert_eq!(s(None, None, Some("true")), Merge);
        assert_eq!(s(None, None, Some("false")), Merge);
        assert_eq!(s(Some("false"), Some("true"), None), Merge, "branch wins");
        assert_eq!(s(Some("yes"), Some("false"), None), Rebase, "branch wins");
    }

    #[test]
    fn git_pull_strategy_reads_repo_config() {
        let (_tmp, _remote, ours, _theirs) = two_clones();
        let strategy = || git_pull_strategy(&ours).unwrap();
        assert_eq!(strategy(), PullStrategy::FastForwardOnly);
        git::run(&ours, &["config", "pull.rebase", "false"]).unwrap();
        assert_eq!(strategy(), PullStrategy::Merge);
        git::run(&ours, &["config", "branch.main.rebase", "true"]).unwrap();
        assert_eq!(strategy(), PullStrategy::Rebase);
    }

    #[test]
    fn pull_ff_only_refuses_diverged_branch() {
        let (_tmp, ours, _theirs) = diverged_clones(false);
        let before = head(&ours, "HEAD");
        let err = pull(&ours, PullStrategy::FastForwardOnly).unwrap_err();
        assert!(err.to_string().contains("diverged"), "{err}");
        assert_eq!(head(&ours, "HEAD"), before);
        assert_eq!(in_progress(&ours).unwrap(), None);
    }

    #[test]
    fn pull_merge_on_diverged_branch_makes_merge_commit() {
        let (_tmp, ours, _theirs) = diverged_clones(false);
        let mine = head(&ours, "HEAD");
        // A user's `pull.rebase` must not turn the chosen merge into a rebase.
        git::run(&ours, &["config", "pull.rebase", "true"]).unwrap();
        git::run(&ours, &["config", "pull.ff", "only"]).unwrap();
        assert_eq!(
            pull(&ours, PullStrategy::Merge).unwrap(),
            MergeOutcome::Done
        );
        assert_eq!(parents(&ours, "HEAD").unwrap().len(), 2);
        assert_eq!(head(&ours, "HEAD^1"), mine);
        assert_eq!(head(&ours, "HEAD^2"), head(&ours, "origin/main"));
        assert!(ours.join("o.txt").exists() && ours.join("t.txt").exists());
    }

    #[test]
    fn pull_rebase_on_diverged_branch_replays_on_upstream() {
        let (_tmp, ours, _theirs) = diverged_clones(false);
        // A user's `pull.ff = only` must not refuse the chosen rebase.
        git::run(&ours, &["config", "pull.ff", "only"]).unwrap();
        git::run(&ours, &["config", "pull.rebase", "false"]).unwrap();
        assert_eq!(
            pull(&ours, PullStrategy::Rebase).unwrap(),
            MergeOutcome::Done
        );
        assert_eq!(parents(&ours, "HEAD").unwrap().len(), 1);
        assert_eq!(head(&ours, "HEAD~1"), head(&ours, "origin/main"));
        assert_eq!(head_message(&ours).unwrap(), "edit o.txt");
        assert!(ours.join("o.txt").exists() && ours.join("t.txt").exists());
    }

    #[test]
    fn pull_merge_conflict_is_left_in_progress() {
        let (_tmp, ours, _theirs) = diverged_clones(true);
        let outcome = pull(&ours, PullStrategy::Merge).unwrap();
        assert_eq!(outcome, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(in_progress(&ours).unwrap(), Some(Operation::Merge));
        abort_op(&ours, Operation::Merge).unwrap();
        assert_eq!(in_progress(&ours).unwrap(), None);
    }

    #[test]
    fn pull_rebase_conflict_is_left_in_progress() {
        let (_tmp, ours, _theirs) = diverged_clones(true);
        let outcome = pull(&ours, PullStrategy::Rebase).unwrap();
        assert_eq!(outcome, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(in_progress(&ours).unwrap(), Some(Operation::Rebase));
        // Resolve and continue: the rebase finishes on top of the upstream.
        std::fs::write(ours.join("a.txt"), "both\n").unwrap();
        assert!(mark_resolved(&ours, "a.txt", false).unwrap());
        assert_eq!(
            continue_op(&ours, Operation::Rebase).unwrap(),
            MergeOutcome::Done
        );
        assert_eq!(head(&ours, "HEAD~1"), head(&ours, "origin/main"));
    }

    #[test]
    fn pull_ff_only_never_aborts_an_operation_in_progress() {
        let (_tmp, ours, _theirs) = diverged_clones(true);
        let outcome = pull(&ours, PullStrategy::Merge).unwrap();
        assert_eq!(outcome, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        // Conflicted, then resolved but not continued: both times git refuses the pull.
        pull(&ours, PullStrategy::FastForwardOnly).unwrap_err();
        assert_eq!(in_progress(&ours).unwrap(), Some(Operation::Merge));
        std::fs::write(ours.join("a.txt"), "both\n").unwrap();
        assert!(mark_resolved(&ours, "a.txt", false).unwrap());
        pull(&ours, PullStrategy::FastForwardOnly).unwrap_err();
        assert_eq!(in_progress(&ours).unwrap(), Some(Operation::Merge));
        assert_eq!(
            std::fs::read_to_string(ours.join("a.txt")).unwrap(),
            "both\n"
        );
    }

    #[test]
    fn pull_merge_or_rebase_refuses_local_changes() {
        let (_tmp, ours, _theirs) = diverged_clones(false);
        std::fs::write(ours.join("a.txt"), "dirty\n").unwrap();
        let before = head(&ours, "HEAD");
        for strategy in [PullStrategy::Merge, PullStrategy::Rebase] {
            let err = pull(&ours, strategy).unwrap_err();
            assert!(err.to_string().contains("Commit or stash"), "{err}");
        }
        assert_eq!(head(&ours, "HEAD"), before);
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
    fn checkout_normal_switches() {
        let (tmp, url, _parent) = fixture();
        let dir = tmp.path().join("plain");
        git::run(tmp.path(), &["clone", "-q", &url, "plain"]).unwrap();
        let out = checkout(&dir, &dir, "feature/x").unwrap();
        assert_eq!(out, CheckoutOutcome::Switched);
        assert_eq!(
            refs::current_branch(&dir).unwrap().as_deref(),
            Some("feature/x")
        );
    }

    #[test]
    fn checkout_normal_jumps_to_an_external_worktree() {
        let (tmp, url, _parent) = fixture();
        let dir = tmp.path().join("plain");
        git::run(tmp.path(), &["clone", "-q", &url, "plain"]).unwrap();
        // An agent's worktree, made with plain git, nested in the repo.
        let agent = dir.join(".claude/worktrees/agent-1");
        let agent_str = agent.to_str().unwrap();
        git::run(&dir, &["worktree", "add", "-q", agent_str, "feature/x"]).unwrap();
        let out = checkout(&dir, &dir, "feature/x").unwrap();
        let CheckoutOutcome::JumpTo(p) = out else {
            panic!("{out:?}")
        };
        assert_eq!(p.canonicalize().unwrap(), agent.canonicalize().unwrap());
        assert_eq!(refs::current_branch(&dir).unwrap().as_deref(), Some("main"));
    }

    #[test]
    fn create_branch_from_base() {
        let (_tmp, _root, main) = cloned();
        create_branch(&main, "new", "main").unwrap();
        git::run(&main, &["rev-parse", "--verify", "new"]).unwrap();
    }

    #[test]
    fn create_branch_at_commit() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        let sha = head(&main, "HEAD~1");
        create_branch(&main, "at-init", &sha).unwrap();
        assert_eq!(head(&main, "at-init"), sha);
        assert_eq!(
            refs::current_branch(&main).unwrap().as_deref(),
            Some("main"),
            "not switched"
        );
    }

    #[test]
    fn create_and_switch_carries_changes_only_from_head() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        std::fs::write(main.join("a.txt"), "changed\n").unwrap();
        let init = head(&main, "HEAD~1");
        let err = create_and_switch(&main, "old", &init).unwrap_err();
        assert!(err.to_string().contains("Commit or stash"), "{err}");
        assert!(git::run(&main, &["rev-parse", "--verify", "-q", "old"]).is_err());
        create_and_switch(&main, "here", "main").unwrap();
        assert_eq!(
            refs::current_branch(&main).unwrap().as_deref(),
            Some("here")
        );
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "changed\n"
        );
        g(&main, &["checkout", "--", "a.txt"]);
        create_and_switch(&main, "old", &init).unwrap();
        assert_eq!(head(&main, "HEAD"), init);
    }

    #[test]
    fn detached_head_previous_branch_and_orphans() {
        let (_tmp, _root, main) = cloned();
        assert_eq!(detached_head(&main).unwrap(), None);
        assert_eq!(detached_orphans(&main).unwrap(), 0);
        let init = head(&main, "HEAD");
        g(&main, &["switch", "-q", "-c", "topic"]);
        assert_eq!(previous_branch(&main).unwrap().as_deref(), Some("main"));
        checkout_detached(&main, &init).unwrap();
        assert_eq!(detached_head(&main).unwrap(), Some(init.clone()));
        assert_eq!(previous_branch(&main).unwrap().as_deref(), Some("topic"));
        assert_eq!(detached_orphans(&main).unwrap(), 0, "init is on main");
        // Detached to detached: the branch before that is still found.
        write_commit(&main, "c.txt");
        let c = head(&main, "HEAD");
        checkout_detached(&main, &init).unwrap();
        checkout_detached(&main, &c).unwrap();
        assert_eq!(previous_branch(&main).unwrap().as_deref(), Some("topic"));
        assert_eq!(detached_orphans(&main).unwrap(), 1, "c is on no branch");
        write_commit(&main, "d.txt");
        assert_eq!(detached_orphans(&main).unwrap(), 2);
        create_branch(&main, "saved", "HEAD").unwrap();
        assert_eq!(detached_orphans(&main).unwrap(), 0);
        // A deleted branch is not offered.
        g(&main, &["branch", "-q", "-D", "topic"]);
        assert_eq!(previous_branch(&main).unwrap().as_deref(), Some("main"));
    }

    /// `main` with `c.txt` and `d.txt` committed on top of `init`; returns `init`.
    fn two_ahead(main: &Path) -> String {
        let init = head(main, "HEAD");
        write_commit(main, "c.txt");
        write_commit(main, "d.txt");
        init
    }

    #[test]
    fn reset_soft_keeps_dropped_changes_staged() {
        let (_tmp, _root, main) = cloned();
        let init = two_ahead(&main);
        let plan = reset_plan(&main, &init).unwrap();
        assert_eq!((plan.branch.as_str(), plan.dropped), ("main", 2));
        reset(&main, &plan, ResetMode::Soft).unwrap();
        assert_eq!(head(&main, "HEAD"), init);
        assert_eq!(
            g(&main, &["diff", "--cached", "--name-only"]),
            "c.txt\nd.txt"
        );
        assert!(g(&main, &["diff", "--name-only"]).is_empty());
        assert!(main.join("c.txt").exists());
    }

    #[test]
    fn reset_mixed_keeps_dropped_changes_unstaged() {
        let (_tmp, _root, main) = cloned();
        let init = two_ahead(&main);
        std::fs::write(main.join("a.txt"), "edited\n").unwrap();
        g(&main, &["add", "a.txt"]);
        let plan = reset_plan(&main, &init).unwrap();
        reset(&main, &plan, ResetMode::Mixed).unwrap();
        assert_eq!(head(&main, "HEAD"), init);
        assert!(g(&main, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(
            g(&main, &["diff", "--name-only"]),
            "a.txt",
            "unstaged, kept"
        );
        let status = g(&main, &["status", "--porcelain"]);
        assert!(
            status.contains("?? c.txt") && status.contains("?? d.txt"),
            "{status}"
        );
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn reset_hard_drops_changes_it_named_and_keeps_untracked() {
        let (_tmp, _root, main) = cloned();
        let init = two_ahead(&main);
        std::fs::write(main.join("a.txt"), "edited\n").unwrap();
        std::fs::write(main.join("new.txt"), "staged new\n").unwrap();
        g(&main, &["add", "new.txt"]);
        std::fs::write(main.join("untracked.txt"), "u\n").unwrap();
        let plan = reset_plan(&main, &init).unwrap();
        assert_eq!(plan.changed, ["a.txt", "new.txt"]);
        // Another change after asking: refused, nothing touched.
        std::fs::write(main.join("c.txt"), "more\n").unwrap();
        let err = reset(&main, &plan, ResetMode::Hard).unwrap_err();
        assert!(err.to_string().contains("changed since"), "{err}");
        assert_eq!(head(&main, "HEAD~2"), init);
        g(&main, &["checkout", "--", "c.txt"]);
        reset(&main, &plan, ResetMode::Hard).unwrap();
        assert_eq!(head(&main, "HEAD"), init);
        assert!(g(&main, &["status", "--porcelain", "--untracked-files=no"]).is_empty());
        assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), "a\n");
        assert!(!main.join("c.txt").exists() && !main.join("new.txt").exists());
        assert!(main.join("untracked.txt").exists(), "untracked kept");
    }

    #[test]
    fn reset_refuses_off_history_detached_or_moved() {
        let (_tmp, _root, main) = cloned();
        let x = head(&main, "origin/feature/x");
        let err = reset_plan(&main, &x).unwrap_err();
        assert!(err.to_string().contains("isn't on main's history"), "{err}");
        let init = two_ahead(&main);
        let plan = reset_plan(&main, &init).unwrap();
        write_commit(&main, "e.txt");
        let err = reset(&main, &plan, ResetMode::Soft).unwrap_err();
        assert!(err.to_string().contains("moved since"), "{err}");
        g(&main, &["checkout", "-q", "--detach"]);
        let err = reset_plan(&main, &init).unwrap_err();
        assert!(
            err.to_string().contains("Check out a branch first"),
            "{err}"
        );
    }

    #[test]
    fn reset_counts_pushed_commits_it_drops() {
        let (_tmp, _root, main) = cloned();
        let init = head(&main, "HEAD");
        write_commit(&main, "c.txt");
        let plan = reset_plan(&main, &init).unwrap();
        assert_eq!((plan.dropped, plan.pushed), (1, 0), "not pushed");
        push(&main).unwrap();
        write_commit(&main, "d.txt");
        let plan = reset_plan(&main, &init).unwrap();
        assert_eq!((plan.dropped, plan.pushed), (2, 1), "c is pushed, d is not");
        let one_back = head(&main, "HEAD~1");
        let plan = reset_plan(&main, &one_back).unwrap();
        assert_eq!((plan.dropped, plan.pushed), (1, 0), "only d is dropped");
        g(&main, &["switch", "-q", "-c", "topic"]);
        assert_eq!(reset_plan(&main, &init).unwrap().pushed, 0, "no upstream");
    }

    #[test]
    fn checkout_detached_at_commit() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        let sha = head(&main, "HEAD~1");
        std::fs::write(main.join("a.txt"), "changed\n").unwrap();
        let err = checkout_detached(&main, &sha).unwrap_err();
        assert!(err.to_string().contains("Commit or stash"), "{err}");
        assert_eq!(
            refs::current_branch(&main).unwrap().as_deref(),
            Some("main")
        );
        g(&main, &["checkout", "--", "a.txt"]);
        checkout_detached(&main, &sha).unwrap();
        assert_eq!(head(&main, "HEAD"), sha);
        assert_eq!(refs::current_branch(&main).unwrap(), None, "detached");
        assert!(!main.join("c.txt").exists());
    }

    #[test]
    fn commit_message_of_any_commit() {
        let (_tmp, _root, main) = cloned();
        let sha = head(&main, "HEAD");
        std::fs::write(main.join("c.txt"), "c\n").unwrap();
        g(&main, &["add", "c.txt"]);
        commit(&main, "second\n\nwhy").unwrap();
        assert_eq!(commit_message(&main, &sha).unwrap(), "init");
        assert_eq!(commit_message(&main, "HEAD").unwrap(), "second\n\nwhy");
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
    fn delete_branches_reports_each() {
        let (_tmp, root, main) = cloned();
        create_branch(&main, "feat/done", "main").unwrap();
        git::run(
            &main,
            &["branch", "--no-track", "feat/topic", "origin/feature/x"],
        )
        .unwrap();
        create_branch(&main, "feat/out", "main").unwrap();
        add_worktree_existing(&root, "feat/out").unwrap();
        let names = ["feat/done", "feat/topic", "feat/out"].map(String::from);
        let out = delete_branches(&main, &names, false);
        assert_eq!(out.deleted, ["feat/done"]);
        assert_eq!(out.not_merged, ["feat/topic"]);
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].starts_with("feat/out: "), "{:?}", out.errors);
        let out = delete_branches(&main, &out.not_merged, true);
        assert_eq!(out.deleted, ["feat/topic"]);
        assert!(git::run(&main, &["rev-parse", "--verify", "feat/topic"]).is_err());
        git::run(&main, &["rev-parse", "--verify", "feat/out"]).unwrap();
    }

    #[test]
    fn delete_remote_branches_in_one_push() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        for b in ["feat/a", "feat/b"] {
            create_branch(&main, b, "main").unwrap();
            git::run(&main, &["push", "-q", "origin", b]).unwrap();
        }
        let names = ["origin/feat/a", "origin/feat/b"].map(String::from);
        delete_remote_branches(&main, &names).unwrap();
        for b in ["feat/a", "feat/b"] {
            assert!(git::run(&remote, &["rev-parse", "--verify", b]).is_err());
        }
        git::run(&remote, &["rev-parse", "--verify", "feature/x"]).unwrap();
        git::run(&main, &["rev-parse", "--verify", "feat/a"]).unwrap();
        assert!(delete_remote_branches(&main, &["nope".into()]).is_err());
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
        assert!(err.to_string().contains("worktree proj"), "{err}");
    }

    /// Whether `refs/tags/<tag>` exists in `dir`.
    fn has_tag(dir: &Path, tag: &str) -> bool {
        let r = format!("refs/tags/{tag}");
        git::run(dir, &["rev-parse", "--verify", "-q", &r]).is_ok()
    }

    #[test]
    fn push_tag_to_file_remote() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        // A branch with the tag's name must not be what gets pushed.
        git::run(&main, &["tag", "-a", "-m", "release", "v1"]).unwrap();
        git::run(&main, &["branch", "v1"]).unwrap();
        push_tag(&main, "origin", "v1").unwrap();
        assert!(has_tag(&remote, "v1"));
        assert!(git::run(&remote, &["rev-parse", "--verify", "-q", "refs/heads/v1"]).is_err());
        let kind = git::run(&remote, &["cat-file", "-t", "refs/tags/v1"]).unwrap();
        assert_eq!(kind.trim(), "tag", "pushed as the annotated tag");
        assert!(push_tag(&main, "origin", "nope").is_err());
    }

    #[test]
    fn delete_tag_locally_keeps_remote() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        git::run(&main, &["tag", "v1"]).unwrap();
        push_tag(&main, "origin", "v1").unwrap();
        let head = rev_parse(&main, "HEAD").unwrap();
        delete_tag(&main, "v1").unwrap();
        assert!(!has_tag(&main, "v1"));
        assert_eq!(rev_parse(&main, "HEAD").unwrap(), head, "commit untouched");
        assert!(has_tag(&remote, "v1"), "only local");
        assert!(delete_tag(&main, "v1").is_err());
    }

    #[test]
    fn delete_tag_on_file_remote() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        git::run(&main, &["tag", "v1"]).unwrap();
        git::run(&main, &["branch", "v1"]).unwrap();
        push_tag(&main, "origin", "v1").unwrap();
        git::run(&main, &["push", "-q", "origin", "refs/heads/v1"]).unwrap();
        delete_remote_tag(&main, "origin", "v1").unwrap();
        assert!(!has_tag(&remote, "v1"));
        git::run(&remote, &["rev-parse", "--verify", "-q", "refs/heads/v1"]).unwrap();
        assert!(has_tag(&main, "v1"), "local tag stays");
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

    #[test]
    fn rename_local_only_branch() {
        let (_tmp, _root, main) = cloned();
        create_branch(&main, "topic", "main").unwrap();
        let tip = head(&main, "topic");
        assert_eq!(remote_rename(&main, "topic").unwrap(), None, "no upstream");
        rename_branch(&main, "topic", "feat/topic").unwrap();
        assert!(git::run(&main, &["rev-parse", "--verify", "-q", "topic"]).is_err());
        assert_eq!(head(&main, "feat/topic"), tip);
        // The checked-out branch of this very worktree may be renamed.
        rename_branch(&main, "main", "trunk").unwrap();
        assert_eq!(
            refs::current_branch(&main).unwrap().as_deref(),
            Some("trunk")
        );
        for (new, why) in [
            ("feat/topic", "exists"),
            ("a..b", "invalid"),
            ("trunk", "same"),
        ] {
            assert!(rename_branch(&main, "trunk", new).is_err(), "{why}");
        }
        assert_eq!(head(&main, "feat/topic"), tip, "an existing name is kept");
    }

    #[test]
    fn rename_with_remote() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        g(&main, &["branch", "feature/x", "origin/feature/x"]);
        let tip = head(&main, "feature/x");
        // An unpushed local commit is not published by the rename: the remote keeps its tip.
        let tree = format!("{tip}^{{tree}}");
        let local = g(&main, &["commit-tree", &tree, "-p", &tip, "-m", "local"]);
        g(&main, &["update-ref", "refs/heads/feature/x", &local]);
        let r = remote_rename(&main, "feature/x").unwrap().expect("offered");
        assert_eq!(
            (r.remote.as_str(), r.tracking.as_str()),
            ("origin", "origin/feature/x")
        );
        assert_eq!(r.expect, tip);
        rename_branch(&main, "feature/x", "feature/y").unwrap();
        rename_on_remote(&main, &r, "feature/x", "feature/y").unwrap();
        assert_eq!(head(&remote, "feature/y"), tip);
        assert_eq!(head(&main, "feature/y"), local);
        assert!(git::run(&remote, &["rev-parse", "--verify", "-q", "feature/x"]).is_err());
        let up = g(&main, &["rev-parse", "--abbrev-ref", "feature/y@{u}"]);
        assert_eq!(up, "origin/feature/y");
        let list = refs::branches(&main).unwrap();
        assert!(
            list.iter().all(|b| b.name != "origin/feature/x"),
            "{list:?}"
        );
    }

    #[test]
    fn rename_on_remote_refuses_when_remote_moved() {
        let (tmp, _root, main) = cloned();
        let remote = remote_dir(&tmp);
        g(&main, &["branch", "feature/x", "origin/feature/x"]);
        let r = remote_rename(&main, "feature/x").unwrap().unwrap();
        g(&remote, &["config", "user.name", "Test User"]);
        g(&remote, &["switch", "-q", "feature/x"]);
        write_commit(&remote, "r.txt");
        let moved = head(&remote, "feature/x");
        fetch(&main).unwrap();
        rename_branch(&main, "feature/x", "feature/y").unwrap();
        let err = rename_on_remote(&main, &r, "feature/x", "feature/y").unwrap_err();
        assert!(err.to_string().contains("renamed locally only"), "{err}");
        assert_eq!(head(&remote, "feature/x"), moved, "their commit is kept");
        assert!(git::run(&remote, &["rev-parse", "--verify", "-q", "feature/y"]).is_err());
    }

    #[test]
    fn rename_refuses_branch_checked_out_elsewhere() {
        let (_tmp, root, main) = cloned();
        add_worktree_existing(&root, "feature/x").unwrap();
        let err = rename_branch(&main, "feature/x", "feature/y").unwrap_err();
        assert!(
            err.to_string()
                .contains("checked out in worktree feature-x"),
            "{err}"
        );
        git::run(&main, &["rev-parse", "--verify", "feature/x"]).unwrap();
        assert!(git::run(&main, &["rev-parse", "--verify", "-q", "feature/y"]).is_err());
    }

    /// Local `feature/x` tracking `origin/feature/x`, which then gains a commit on the remote
    /// and is fetched: the branch is one behind.
    fn behind() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, root, main) = cloned();
        let remote = remote_dir(&tmp);
        g(&main, &["branch", "feature/x", "origin/feature/x"]);
        g(&remote, &["config", "user.name", "Test User"]);
        g(&remote, &["switch", "-q", "feature/x"]);
        write_commit(&remote, "r.txt");
        g(&remote, &["switch", "-q", "main"]);
        fetch(&main).unwrap();
        (tmp, root, main)
    }

    #[test]
    fn fast_forward_moves_a_branch_that_is_not_checked_out() {
        let (_tmp, _root, main) = behind();
        let before = snapshot(&main);
        assert_eq!(
            fast_forward(&main, "feature/x").unwrap(),
            MergeOutcome::Done
        );
        assert_eq!(head(&main, "feature/x"), head(&main, "origin/feature/x"));
        assert_eq!(snapshot(&main), before, "the worktree is untouched");
        let again = fast_forward(&main, "feature/x").unwrap();
        assert_eq!(again, MergeOutcome::UpToDate);
        create_branch(&main, "topic", "main").unwrap();
        let err = fast_forward(&main, "topic").unwrap_err();
        assert!(err.to_string().contains("no upstream"), "{err}");
    }

    #[test]
    fn fast_forward_to_another_branch() {
        let (_tmp, _root, main) = cloned();
        g(&main, &["branch", "feature/x", "origin/feature/x"]);
        create_branch(&main, "base", "origin/main").unwrap();
        assert!(can_fast_forward(&main, "base", "feature/x").unwrap());
        assert!(
            !can_fast_forward(&main, "feature/x", "base").unwrap(),
            "ahead"
        );
        assert!(
            !can_fast_forward(&main, "base", "main").unwrap(),
            "same commit"
        );
        let out = fast_forward_to(&main, "base", "feature/x").unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(head(&main, "base"), head(&main, "feature/x"));
        assert!(!can_fast_forward(&main, "base", "feature/x").unwrap());
    }

    #[test]
    fn fast_forward_refuses_when_diverged() {
        let (_tmp, _root, main) = behind();
        let tip = head(&main, "feature/x");
        let tree = format!("{tip}^{{tree}}");
        let local = g(&main, &["commit-tree", &tree, "-p", &tip, "-m", "local"]);
        g(&main, &["update-ref", "refs/heads/feature/x", &local]);
        let err = fast_forward(&main, "feature/x").unwrap_err();
        assert!(err.to_string().contains("diverged"), "{err}");
        assert_eq!(head(&main, "feature/x"), local, "unchanged");
    }

    #[test]
    fn fast_forward_refuses_when_checked_out_in_a_worktree() {
        let (_tmp, root, main) = behind();
        let wt = add_worktree_existing(&root, "feature/x").unwrap();
        let tip = head(&wt, "HEAD");
        let err = fast_forward(&main, "feature/x").unwrap_err();
        assert!(
            err.to_string()
                .contains("checked out in worktree feature-x"),
            "{err}"
        );
        assert_eq!(head(&main, "feature/x"), tip);
        // The active worktree's own branch too: pull is the way there.
        let err = fast_forward_to(&main, "main", "origin/feature/x").unwrap_err();
        assert!(err.to_string().contains("worktree proj"), "{err}");
    }

    fn remove_worktree_and_its_merged_branch() {
        let (_tmp, root, _main) = cloned();
        let wt = crate::project::add_worktree(&root, "topic", "main").unwrap();
        let plan = plan_remove_with_branch(&root, &wt).unwrap();
        assert_eq!(
            (plan.branch.as_str(), plan.base.as_str()),
            ("topic", "main")
        );
        assert_eq!((plan.dirty, plan.unmerged), (false, 0));
        let out = remove_with_branch(&root, &plan).unwrap();
        assert_eq!(out, DeleteOutcome::Deleted);
        assert!(!wt.exists());
        assert!(git::run(&root, &["rev-parse", "--verify", "-q", "topic"]).is_err());
    }

    #[test]
    fn remove_worktree_and_unmerged_dirty_branch_once_confirmed() {
        let (_tmp, root, _main) = cloned();
        let wt = crate::project::add_worktree(&root, "topic", "main").unwrap();
        g(&wt, &["config", "user.name", "Test User"]);
        write_commit(&wt, "t.txt");
        std::fs::write(
            wt.join("scratch.txt"),
            "s
",
        )
        .unwrap();
        let plan = plan_remove_with_branch(&root, &wt).unwrap();
        assert_eq!((plan.dirty, plan.unmerged), (true, 1));
        assert_eq!(plan.dirty_files, ["scratch.txt"]);
        // Asked about as clean and merged, nothing is forced: the worktree stays.
        let unconfirmed = RemoveWithBranch {
            dirty: false,
            unmerged: 0,
            ..plan.clone()
        };
        assert!(remove_with_branch(&root, &unconfirmed).is_err());
        assert!(wt.join("scratch.txt").exists());
        assert_eq!(
            remove_with_branch(&root, &plan).unwrap(),
            DeleteOutcome::Deleted
        );
        assert!(!wt.exists());
        assert!(git::run(&root, &["rev-parse", "--verify", "-q", "topic"]).is_err());
    }

    #[test]
    fn remove_worktree_keeps_a_branch_found_unmerged_after_asking() {
        let (_tmp, root, _main) = cloned();
        let wt = crate::project::add_worktree(&root, "topic", "main").unwrap();
        let plan = plan_remove_with_branch(&root, &wt).unwrap();
        g(&wt, &["config", "user.name", "Test User"]);
        write_commit(&wt, "t.txt");
        let out = remove_with_branch(&root, &plan).unwrap();
        assert_eq!(out, DeleteOutcome::NotMerged, "the usual prompt follows");
        assert!(!wt.exists());
        git::run(&root, &["rev-parse", "--verify", "topic"]).unwrap();
    }

    #[test]
    fn remove_with_branch_refusals() {
        let (tmp, root, main) = cloned();
        let wt = add_worktree_existing(&root, "feature/x").unwrap();
        assert_eq!(
            plan_remove_with_branch(&root, &wt).unwrap().base,
            "origin/feature/x",
            "judged against its upstream"
        );
        let twin = tmp.path().join("twin");
        let twin_s = twin.to_str().unwrap();
        g(&root, &["worktree", "add", "-q", "-f", twin_s, "feature/x"]);
        let err = plan_remove_with_branch(&root, &wt).unwrap_err();
        assert!(
            err.to_string()
                .contains("also checked out in worktree twin"),
            "{err}"
        );
        g(&twin, &["checkout", "-q", "--detach"]);
        let err = plan_remove_with_branch(&root, &twin).unwrap_err();
        assert!(err.to_string().contains("detached"), "{err}");
        assert!(plan_remove_with_branch(&root, &root).is_err());
        assert!(main.exists() && wt.exists() && twin.exists());
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

    #[test]
    fn cherry_pick_one_commit() {
        let (_tmp, _root, main) = cloned();
        let x = head(&main, "origin/feature/x");
        let before = head(&main, "HEAD");
        assert_eq!(cherry_pick(&main, &[x]).unwrap(), MergeOutcome::Done);
        assert_eq!(head(&main, "HEAD~1"), before);
        assert_eq!(head_message(&main).unwrap(), "x");
        assert!(main.join("b.txt").exists());
        assert_eq!(in_progress(&main).unwrap(), None);
    }

    #[test]
    fn cherry_pick_several_in_the_order_given() {
        let (_tmp, _root, main) = cloned();
        g(&main, &["switch", "-q", "-c", "topic"]);
        write_commit(&main, "c.txt");
        let c = head(&main, "HEAD");
        std::fs::write(main.join("c.txt"), "c2\n").unwrap();
        g(&main, &["commit", "-qam", "edit c"]);
        let c2 = head(&main, "HEAD");
        g(&main, &["switch", "-q", "main"]);
        let out = cherry_pick(&main, &[c, c2]).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        let log = g(&main, &["log", "-3", "--format=%s"]);
        assert_eq!(log, "edit c\nadd file\ninit");
        assert_eq!(std::fs::read_to_string(main.join("c.txt")).unwrap(), "c2\n");
    }

    #[test]
    fn conflicting_cherry_pick_is_left_in_progress() {
        let (_tmp, _root, main) = conflicting();
        let other = head(&main, "other");
        let out = cherry_pick(&main, &[other]).unwrap();
        assert_eq!(out, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(in_progress(&main).unwrap(), Some(Operation::CherryPick));
        abort_op(&main, Operation::CherryPick).unwrap();
        assert_eq!(head_message(&main).unwrap(), "main edit");
    }

    #[test]
    fn cherry_pick_refuses_dirty_detached_and_merges() {
        let (_tmp, _root, main) = cloned();
        let x = head(&main, "origin/feature/x");
        std::fs::write(main.join("a.txt"), "changed\n").unwrap();
        let err = cherry_pick(&main, std::slice::from_ref(&x)).unwrap_err();
        assert!(err.to_string().contains("Commit or stash"), "{err}");
        g(&main, &["checkout", "--", "a.txt"]);
        g(
            &main,
            &["merge", "-q", "--no-ff", "--no-edit", "origin/feature/x"],
        );
        let m = head(&main, "HEAD");
        g(&main, &["reset", "-q", "--hard", "HEAD~1"]);
        let err = cherry_pick(&main, &[m]).unwrap_err();
        assert!(err.to_string().contains("merge commit"), "{err}");
        g(&main, &["checkout", "-q", "--detach"]);
        let err = cherry_pick(&main, &[x]).unwrap_err();
        assert!(err.to_string().contains("Check out a branch"), "{err}");
    }

    #[test]
    fn revert_a_normal_commit() {
        let (_tmp, _root, main) = cloned();
        write_commit(&main, "c.txt");
        let c = head(&main, "HEAD");
        assert_eq!(revert(&main, &c, None).unwrap(), MergeOutcome::Done);
        assert!(!main.join("c.txt").exists());
        assert_eq!(head(&main, "HEAD~1"), c);
        assert!(head_message(&main)
            .unwrap()
            .starts_with("Revert \"add file\""));
        let err = revert(&main, &c, Some(1)).unwrap_err();
        assert!(err.to_string().contains("Only a merge commit"), "{err}");
    }

    #[test]
    fn revert_a_merge_keeping_the_first_parent() {
        let (_tmp, _root, main) = cloned();
        let before = head(&main, "HEAD");
        g(
            &main,
            &["merge", "-q", "--no-ff", "--no-edit", "origin/feature/x"],
        );
        let m = head(&main, "HEAD");
        assert_eq!(
            parents(&main, &m).unwrap(),
            [before, head(&main, "origin/feature/x")]
        );
        let err = revert(&main, &m, None).unwrap_err();
        assert!(err.to_string().contains("Choose which parent"), "{err}");
        assert!(revert(&main, &m, Some(3)).is_err());
        assert_eq!(revert(&main, &m, Some(1)).unwrap(), MergeOutcome::Done);
        assert!(!main.join("b.txt").exists(), "feature/x's change undone");
        assert!(main.join("a.txt").exists());
        assert_eq!(head(&main, "HEAD~1"), m);
    }

    #[test]
    fn conflicting_revert_is_left_in_progress() {
        let (_tmp, _root, main) = cloned();
        std::fs::write(main.join("a.txt"), "one\n").unwrap();
        g(&main, &["commit", "-qam", "one"]);
        let one = head(&main, "HEAD");
        std::fs::write(main.join("a.txt"), "two\n").unwrap();
        g(&main, &["commit", "-qam", "two"]);
        let out = revert(&main, &one, None).unwrap();
        assert_eq!(out, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(in_progress(&main).unwrap(), Some(Operation::Revert));
        abort_op(&main, Operation::Revert).unwrap();
        assert_eq!(head_message(&main).unwrap(), "two");
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
    fn merge_conflict_is_left_in_progress() {
        for mode in [MergeMode::FastForward, MergeMode::NoFf] {
            let (_tmp, _root, main) = conflicting();
            let out = merge(&main, "other", mode).unwrap();
            assert_eq!(
                out,
                MergeOutcome::Conflicts(vec!["a.txt".into()]),
                "{mode:?}"
            );
            assert_eq!(in_progress(&main).unwrap(), Some(Operation::Merge));
            assert_eq!(unmerged(&main).unwrap(), ["a.txt"]);
        }
    }

    #[test]
    fn squash_conflict_is_undone() {
        let (_tmp, _root, main) = conflicting();
        std::fs::write(main.join("untracked.txt"), "u\n").unwrap();
        let before = snapshot(&main);
        let out = merge(&main, "other", MergeMode::Squash).unwrap();
        assert_eq!(out, MergeOutcome::SquashConflicts(vec!["a.txt".into()]));
        assert_eq!(snapshot(&main), before);
        assert_eq!(in_progress(&main).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "main\n"
        );
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
    fn abort_after_conflicting_rebase_restores_head_exactly() {
        let (_tmp, _root, main) = conflicting();
        std::fs::write(main.join("untracked.txt"), "u\n").unwrap();
        let before = snapshot(&main);
        let out = rebase(&main, "other").unwrap();
        assert_eq!(out, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(in_progress(&main).unwrap(), Some(Operation::Rebase));
        assert_eq!(refs::current_branch(&main).unwrap(), None, "detached");
        abort_op(&main, Operation::Rebase).unwrap();
        assert_eq!(snapshot(&main), before);
        assert_eq!(in_progress(&main).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "main\n"
        );
        let err = abort_op(&main, Operation::Rebase).unwrap_err();
        assert!(err.to_string().contains("No rebase"), "{err}");
    }

    /// Start each kind of operation so it stops on a conflict in `a.txt` of `dir`, where
    /// HEAD changed `a.txt` and `other` changed it differently.
    fn stop_on_conflict(dir: &Path, op: Operation) {
        let args: &[&str] = match op {
            Operation::Merge => &["merge", "--no-edit", "other"],
            Operation::Rebase => &["rebase", "other"],
            Operation::CherryPick => &["cherry-pick", "other"],
            Operation::Revert => {
                // Reverting HEAD~1 conflicts with HEAD's later edit of the same line.
                std::fs::write(dir.join("a.txt"), "later\n").unwrap();
                g(dir, &["commit", "-qam", "later"]);
                &["revert", "--no-edit", "HEAD~1"]
            }
        };
        assert!(git::run(dir, args).is_err(), "{op} should stop");
        assert_eq!(unmerged(dir).unwrap(), ["a.txt"], "{op}");
    }

    const OPERATIONS: [Operation; 4] = [
        Operation::Merge,
        Operation::Rebase,
        Operation::CherryPick,
        Operation::Revert,
    ];

    #[test]
    fn detects_each_operation_in_a_linked_worktree() {
        for op in OPERATIONS {
            let (_tmp, root, _) = conflicting();
            // Move `main` into a linked worktree: its state files live under `.git/worktrees/`.
            g(&root, &["switch", "-q", "--detach"]);
            let main = add_worktree_existing(&root, "main").unwrap();
            assert!(main.join(".git").is_file());
            assert_eq!(in_progress(&main).unwrap(), None);
            stop_on_conflict(&main, op);
            assert_eq!(in_progress(&main).unwrap(), Some(op));
            // Another worktree of the same repo has nothing in progress.
            let x = add_worktree_existing(&root, "feature/x").unwrap();
            assert_eq!(in_progress(&x).unwrap(), None, "{op}");
            abort_op(&main, op).unwrap();
            assert_eq!(in_progress(&main).unwrap(), None, "{op}");
        }
    }

    #[test]
    fn detects_each_operation_in_a_normal_clone() {
        for op in OPERATIONS {
            let (_tmp, _remote, ours, _theirs) = two_clones();
            std::fs::write(ours.join("a.txt"), "main\n").unwrap();
            g(&ours, &["commit", "-qam", "main edit"]);
            g(&ours, &["switch", "-q", "-c", "other", "HEAD~1"]);
            std::fs::write(ours.join("a.txt"), "other\n").unwrap();
            g(&ours, &["commit", "-qam", "other edit"]);
            g(&ours, &["switch", "-q", "main"]);
            assert!(ours.join(".git").is_dir());
            stop_on_conflict(&ours, op);
            assert_eq!(in_progress(&ours).unwrap(), Some(op));
        }
    }

    #[test]
    fn continue_refuses_while_conflicted_then_finishes_a_merge() {
        let (_tmp, _root, main) = conflicting();
        let before = head(&main, "HEAD");
        merge(&main, "other", MergeMode::NoFf).unwrap();
        let err = continue_op(&main, Operation::Merge).unwrap_err();
        assert!(err.to_string().contains("Resolve"), "{err}");
        std::fs::write(main.join("a.txt"), "both\n").unwrap();
        g(&main, &["add", "a.txt"]);
        let out = continue_op(&main, Operation::Merge).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(in_progress(&main).unwrap(), None);
        let parents = g(&main, &["log", "-1", "--format=%P"]);
        assert_eq!(parents, format!("{before} {}", head(&main, "other")));
        assert!(g(&main, &["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn continue_and_skip_go_on_through_a_rebase() {
        let (_tmp, _root, main) = conflicting();
        // Two commits to replay, each conflicting with `other`.
        std::fs::write(main.join("a.txt"), "main 2\n").unwrap();
        g(&main, &["commit", "-qam", "main edit 2"]);
        assert_eq!(
            rebase(&main, "other").unwrap(),
            MergeOutcome::Conflicts(vec!["a.txt".into()])
        );
        std::fs::write(main.join("a.txt"), "resolved\n").unwrap();
        g(&main, &["add", "a.txt"]);
        let out = continue_op(&main, Operation::Rebase).unwrap();
        assert_eq!(
            out,
            MergeOutcome::Conflicts(vec!["a.txt".into()]),
            "next one"
        );
        let out = skip_op(&main, Operation::Rebase).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(in_progress(&main).unwrap(), None);
        assert_eq!(
            refs::current_branch(&main).unwrap().as_deref(),
            Some("main")
        );
        assert_eq!(head(&main, "HEAD~1"), head(&main, "other"));
        assert_eq!(head_message(&main).unwrap(), "main edit");
        let err = skip_op(&main, Operation::Merge).unwrap_err();
        assert!(err.to_string().contains("can't skip"), "{err}");
    }

    fn read(dir: &Path, file: &str) -> String {
        std::fs::read_to_string(dir.join(file)).unwrap()
    }

    #[test]
    fn mine_is_ours_except_during_a_rebase() {
        use Version::*;
        for op in [None, Some(Operation::Merge), Some(Operation::CherryPick)] {
            assert_eq!(git_side(op, Mine), "--ours", "{op:?}");
            assert_eq!(git_side(op, Theirs), "--theirs", "{op:?}");
        }
        assert_eq!(git_side(Some(Operation::Revert), Mine), "--ours");
        assert_eq!(git_side(Some(Operation::Rebase), Mine), "--theirs");
        assert_eq!(git_side(Some(Operation::Rebase), Theirs), "--ours");
    }

    #[test]
    fn rebase_use_mine_keeps_the_branch_being_rebased() {
        for (version, want) in [(Version::Mine, "main\n"), (Version::Theirs, "other\n")] {
            let (_tmp, _root, main) = conflicting();
            rebase(&main, "other").unwrap();
            let op = in_progress(&main).unwrap();
            assert_eq!(op, Some(Operation::Rebase));
            use_version(&main, op, "a.txt", version).unwrap();
            assert_eq!(read(&main, "a.txt"), want, "{version:?}");
            assert!(unmerged(&main).unwrap().is_empty());
        }
    }

    #[test]
    fn merge_use_mine_then_continue_makes_a_clean_merge_commit() {
        let (_tmp, _root, main) = conflicting();
        let before = head(&main, "HEAD");
        let out = merge(&main, "other", MergeMode::NoFf).unwrap();
        assert_eq!(out, MergeOutcome::Conflicts(vec!["a.txt".into()]));
        let op = in_progress(&main).unwrap();
        use_version(&main, op, "a.txt", Version::Mine).unwrap();
        assert_eq!(read(&main, "a.txt"), "main\n");
        assert_eq!(
            continue_op(&main, Operation::Merge).unwrap(),
            MergeOutcome::Done
        );
        assert_eq!(in_progress(&main).unwrap(), None);
        assert!(g(&main, &["status", "--porcelain"]).is_empty());
        let parents = g(&main, &["log", "-1", "--format=%P"]);
        assert_eq!(parents, format!("{before} {}", head(&main, "other")));
        assert_eq!(g(&main, &["show", "HEAD:a.txt"]), "main");
    }

    #[test]
    fn delete_modify_conflicts_rm_for_the_deleting_side() {
        for (version, gone) in [(Version::Mine, false), (Version::Theirs, true)] {
            let (_tmp, _root, main) = conflicting();
            g(&main, &["switch", "-q", "other"]);
            g(&main, &["rm", "-q", "a.txt"]);
            g(&main, &["commit", "-qm", "delete a"]);
            g(&main, &["switch", "-q", "main"]);
            let out = merge(&main, "other", MergeMode::NoFf).unwrap();
            assert_eq!(out, MergeOutcome::Conflicts(vec!["a.txt".into()]));
            use_version(&main, Some(Operation::Merge), "a.txt", version).unwrap();
            assert_eq!(!main.join("a.txt").exists(), gone, "{version:?}");
            assert!(unmerged(&main).unwrap().is_empty());
            continue_op(&main, Operation::Merge).unwrap();
            let tree = g(&main, &["ls-tree", "--name-only", "HEAD"]);
            assert_eq!(!tree.lines().any(|l| l == "a.txt"), gone, "{version:?}");
        }
    }

    #[test]
    fn save_resolution_writes_and_marks_resolved() {
        let (_tmp, _root, main) = conflicting();
        assert!(matches!(
            merge(&main, "other", MergeMode::NoFf).unwrap(),
            MergeOutcome::Conflicts(_)
        ));
        let original = std::fs::read_to_string(main.join("a.txt")).unwrap();
        assert!(save_resolution(&main, "a.txt", &original, "main\nother\n").unwrap());
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "main\nother\n"
        );
        assert!(unmerged(&main).unwrap().is_empty());
        assert_eq!(g(&main, &["diff", "--cached", "--name-only"]), "a.txt");
    }

    #[test]
    fn save_resolution_with_markers_saves_but_asks() {
        let (_tmp, _root, main) = conflicting();
        merge(&main, "other", MergeMode::NoFf).unwrap();
        let original = std::fs::read_to_string(main.join("a.txt")).unwrap();
        let kept = format!("top\n{original}");
        assert!(!save_resolution(&main, "a.txt", &original, &kept).unwrap());
        assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), kept);
        assert_eq!(unmerged(&main).unwrap(), ["a.txt"], "still conflicted");
    }

    #[test]
    fn save_resolution_refuses_a_file_changed_since() {
        let (_tmp, _root, main) = conflicting();
        merge(&main, "other", MergeMode::NoFf).unwrap();
        let original = std::fs::read_to_string(main.join("a.txt")).unwrap();
        std::fs::write(main.join("a.txt"), "edited elsewhere\n").unwrap();
        let err = save_resolution(&main, "a.txt", &original, "x\n").unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");
        let now = std::fs::read_to_string(main.join("a.txt")).unwrap();
        assert_eq!(now, "edited elsewhere\n", "not overwritten");
        assert_eq!(unmerged(&main).unwrap(), ["a.txt"]);
        // A symlink is never written through.
        std::fs::remove_file(main.join("a.txt")).unwrap();
        std::os::unix::fs::symlink("/tmp/x", main.join("a.txt")).unwrap();
        let err = save_resolution(&main, "a.txt", "", "x\n").unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn mark_resolved_refuses_markers_unless_asked_anyway() {
        let (_tmp, _root, main) = conflicting();
        merge(&main, "other", MergeMode::NoFf).unwrap();
        assert!(read(&main, "a.txt").contains("<<<<<<< HEAD"));
        assert!(!mark_resolved(&main, "a.txt", false).unwrap());
        assert_eq!(unmerged(&main).unwrap(), ["a.txt"], "unchanged");
        assert!(mark_resolved(&main, "a.txt", true).unwrap());
        assert!(unmerged(&main).unwrap().is_empty());
        g(&main, &["merge", "--abort"]);

        merge(&main, "other", MergeMode::NoFf).unwrap();
        std::fs::write(main.join("a.txt"), "Title\n=======\nboth\n").unwrap();
        assert!(
            mark_resolved(&main, "a.txt", false).unwrap(),
            "underline only"
        );
        assert!(use_version(&main, None, "a.txt", Version::Mine).is_err());
    }

    #[test]
    fn conflict_markers_need_the_angle_lines() {
        assert!(has_conflict_markers(
            "a\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n"
        ));
        assert!(has_conflict_markers(">>>>>>>\n"));
        assert!(!has_conflict_markers("Title\n=======\n"));
        assert!(!has_conflict_markers("<<<<<<<<< not one\n<<<<<<<x\n"));
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
        assert_eq!(in_progress(&main).unwrap(), Some(Operation::Merge));
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
