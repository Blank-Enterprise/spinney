//! Convert a normal clone (`foo/.git/`) to the worktree layout:
//! `foo/.bare` (the old `.git`), `foo/.git` = `gitdir: ./.bare`, and `foo/<branch>/` holding
//! everything that was in `foo/`. Every change is journaled and undone on any failure.

use crate::config::Space;
use crate::project::{self, Layout};
use crate::{creds, git};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Canonical project folder.
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub target_dir_name: String,
    /// Name under `.bare/worktrees/`: the target name, suffixed if a linked worktree has it.
    pub worktree_id: String,
    /// Top-level entry names in `root` except `.git`, sorted.
    pub items: Vec<PathBuf>,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
}

/// Held in the git dir while a conversion runs (it moves with `.git` to `.bare`).
const LOCK: &str = "spinney-convert.lock";

/// Something is at `p` (a dangling symlink counts; so does a case-insensitive match on APFS).
fn taken(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn config(root: &Path, key: &str) -> Result<Option<String>> {
    let v = git::run_ok_codes(root, &["config", "--get", key], &[0, 1])?;
    Ok(Some(v.trim().to_string()).filter(|v| !v.is_empty()))
}

/// Read-only checks; refuses anything the conversion cannot carry over exactly.
pub fn plan(root: &Path) -> Result<Plan> {
    let root = root
        .canonicalize()
        .with_context(|| format!("opening {}", root.display()))?;
    if root.to_str().is_none() {
        bail!("{} is not a UTF-8 path", root.display());
    }
    let git_dir = root.join(".git");
    if !fs::symlink_metadata(&git_dir).is_ok_and(|m| m.is_dir()) {
        bail!(
            "{}/.git is not a folder: this is already a worktree layout, a linked worktree \
             or a submodule",
            root.display()
        );
    }
    if taken(&git_dir.join(LOCK)) {
        bail!(
            "another conversion is running ({}/.git/{LOCK} exists; remove it if none is)",
            root.display()
        );
    }
    if taken(&root.join(".bare")) {
        bail!(
            "{}/.bare already exists; move it away first",
            root.display()
        );
    }
    let is_true = |key| -> Result<bool> { Ok(config(&root, key)?.as_deref() == Some("true")) };
    if is_true("core.bare")? {
        bail!("core.bare is already true in this repository");
    }
    if config(&root, "core.worktree")?.is_some() {
        bail!("core.worktree is set; repositories with a separate work tree are not supported");
    }
    if is_true("extensions.worktreeConfig")? {
        bail!("extensions.worktreeConfig is enabled; per-worktree config is not supported");
    }
    if is_true("core.sparseCheckout")? {
        bail!("core.sparseCheckout is enabled; sparse checkouts are not supported");
    }
    if config(&root, "extensions.refStorage")?.is_some_and(|v| v != "files") {
        bail!("extensions.refStorage is not \"files\"; reftable repositories are not supported");
    }
    let checks = [
        ("MERGE_HEAD", "a merge is in progress"),
        ("rebase-merge", "a rebase is in progress"),
        ("rebase-apply", "a rebase or am is in progress"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
        (
            "sequencer",
            "a cherry-pick or revert sequence is in progress",
        ),
        ("BISECT_LOG", "a bisect is in progress"),
        (
            "index.lock",
            "another git process is running (index.lock exists)",
        ),
        (
            "info/sparse-checkout",
            "sparse checkout is set up (info/sparse-checkout)",
        ),
        ("refs/worktree", "per-worktree refs exist (refs/worktree)"),
    ];
    // One git process for all of them: rev-parse prints one path per --git-path, in order.
    let mut args = vec!["rev-parse"];
    for (name, _) in &checks {
        args.extend(["--git-path", name]);
    }
    let paths = git::run(&root, &args)?;
    let paths: Vec<&str> = paths.lines().collect();
    if paths.len() != checks.len() {
        bail!("unexpected `git rev-parse --git-path` output");
    }
    for ((_, why), rel) in checks.iter().zip(paths) {
        if taken(&root.join(rel)) {
            bail!("{why}; finish or remove it first");
        }
    }
    // Its files would move into the new worktree and git would lose track of it.
    if let Ok(entries) = fs::read_dir(git_dir.join("worktrees")) {
        for e in entries {
            let gitdir = e?.path().join("gitdir");
            let Ok(link) = fs::read_to_string(&gitdir) else {
                continue;
            };
            let link = linked_git_file(&gitdir, &link);
            let wt = link.parent().unwrap_or(Path::new(""));
            if wt.starts_with(&root) {
                bail!(
                    "the linked worktree {} is inside the project folder; move it out first \
                     (`git worktree move`)",
                    wt.display()
                );
            }
        }
    }
    let split_index = fs::read_dir(&git_dir)?
        .any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with("sharedindex.")));
    if split_index {
        bail!("a split index is in use; run `git update-index --no-split-index` first");
    }
    if taken(&root.join(".gitmodules"))
        || !git::run(&root, &["submodule", "status"])?.trim().is_empty()
    {
        bail!("the repository has submodules; submodules are not supported");
    }
    let branch = git::run_ok_codes(&root, &["symbolic-ref", "--short", "-q", "HEAD"], &[0, 1])?
        .trim()
        .to_string();
    let branch = Some(branch).filter(|b| !b.is_empty());
    let target_dir_name = match &branch {
        Some(b) => project::worktree_dir_name(b),
        None => {
            let sha = git::run(&root, &["rev-parse", "--verify", "HEAD"])?;
            format!("detached-{}", sha.get(..7).context("short HEAD sha")?)
        }
    };
    if taken(&root.join(&target_dir_name)) {
        bail!(
            "{}/{target_dir_name} already exists; rename it first",
            root.display()
        );
    }
    let mut worktree_id = target_dir_name.clone();
    for n in 1.. {
        if !taken(&git_dir.join("worktrees").join(&worktree_id)) {
            break;
        }
        worktree_id = format!("{target_dir_name}{n}");
    }
    let mut items = vec![];
    for e in fs::read_dir(&root)? {
        let name = e?.file_name();
        if name != ".git" {
            items.push(PathBuf::from(name));
        }
    }
    items.sort();
    Ok(Plan {
        root,
        git_dir,
        target_dir_name,
        worktree_id,
        items,
        branch,
    })
}

pub fn convert(plan: &Plan, space: &Space) -> Result<PathBuf> {
    convert_with(plan, space, &|_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    RenameGitDir,
    MoveItems,
    LinkWorktree,
    Configure,
    Verify,
}

/// Where the test hook is called: after `n` items moved, or after a whole step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    /// Locked and snapshotted, nothing changed yet.
    Start,
    Moved(usize),
    After(Step),
}

/// What a user can observe, taken before and compared after (re-rooted) and after rollback.
#[derive(Debug, PartialEq)]
struct Snapshot {
    status: Vec<u8>,
    head: String,
    refs: String,
    /// Every path except the top-level `.git`: kind, mode, size, symlink target.
    files: Vec<(PathBuf, String)>,
}

impl Snapshot {
    fn take(dir: &Path) -> Result<Snapshot> {
        Snapshot::read(dir).with_context(|| format!("recording the state of {}", dir.display()))
    }

    fn read(dir: &Path) -> Result<Snapshot> {
        let status = git::run_bytes(
            dir,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
            &[0],
        )?;
        let head = git::run_ok_codes(dir, &["rev-parse", "--verify", "-q", "HEAD"], &[0, 1])?
            + &git::run_ok_codes(dir, &["symbolic-ref", "-q", "HEAD"], &[0, 1])?;
        let refs = git::run(
            dir,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
                "refs/remotes",
                "refs/stash",
            ],
        )?;
        let mut files = vec![];
        list(dir, Path::new(""), &mut files)?;
        Ok(Snapshot {
            status,
            head,
            refs,
            files,
        })
    }

    /// First difference, for the error message.
    fn diff(&self, other: &Snapshot) -> String {
        if self.status != other.status {
            return "git status differs".into();
        }
        if self.head != other.head {
            return "HEAD differs".into();
        }
        if self.refs != other.refs {
            return "refs differ".into();
        }
        let (a, b) = (&self.files, &other.files);
        match a.iter().zip(b).find(|(x, y)| x != y) {
            Some((x, y)) => format!(
                "{} ({}) became {} ({})",
                x.0.display(),
                x.1,
                y.0.display(),
                y.1
            ),
            None => format!("{} files before, {} after", a.len(), b.len()),
        }
    }
}

fn list(base: &Path, rel: &Path, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let dir = base.join(rel);
    let mut names: Vec<std::ffi::OsString> = fs::read_dir(&dir)
        .and_then(|d| d.map(|e| e.map(|e| e.file_name())).collect())
        .with_context(|| format!("listing {}", dir.display()))?;
    names.sort();
    for name in names {
        if rel.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let r: PathBuf = rel.join(&name);
        let p = base.join(&r);
        let m = fs::symlink_metadata(&p).with_context(|| format!("reading {}", p.display()))?;
        let mode = m.permissions().mode();
        if m.file_type().is_symlink() {
            let to = fs::read_link(&p).with_context(|| format!("reading {}", p.display()))?;
            out.push((r, format!("symlink -> {}", to.display())));
        } else if m.is_dir() {
            out.push((r.clone(), format!("dir {mode:o}")));
            list(base, &r, out)?;
        } else {
            out.push((r, format!("file {mode:o} {} bytes", m.len())));
        }
    }
    Ok(())
}

/// One change made so far, undone in reverse order on failure.
enum Undo {
    Renamed { from: PathBuf, to: PathBuf },
    CreatedFile(PathBuf),
    CreatedDir(PathBuf),
}

#[derive(Default)]
struct Journal(Vec<Undo>);

impl Journal {
    /// Never replaces anything at `to` (`rename` would silently overwrite a file).
    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        if taken(to) {
            bail!("{} already exists", to.display());
        }
        fs::rename(from, to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        self.0.push(Undo::Renamed {
            from: from.into(),
            to: to.into(),
        });
        Ok(())
    }

    fn mkdir(&mut self, p: &Path) -> Result<()> {
        fs::create_dir(p).with_context(|| format!("creating {}", p.display()))?;
        self.0.push(Undo::CreatedDir(p.into()));
        Ok(())
    }

    /// `create_new`, so a journaled file is provably one this conversion made.
    fn write_new(&mut self, p: &Path, bytes: &[u8]) -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
            .with_context(|| format!("creating {}", p.display()))?;
        self.0.push(Undo::CreatedFile(p.into()));
        f.write_all(bytes)
            .with_context(|| format!("writing {}", p.display()))
    }
}

/// Bytes of git admin files that later steps rewrite (config; the other linked worktrees'
/// `.git` files and their `gitdir` entries, which `worktree repair` updates), restored on
/// rollback, plus the other worktrees to check after conversion.
struct Saved {
    files: Vec<(PathBuf, Vec<u8>)>,
    others: Vec<PathBuf>,
}

/// The linked worktree's `.git` file named by its admin dir's `gitdir` file, which git
/// 2.48+ may write relative to that admin dir (`extensions.relativeWorktrees`).
fn linked_git_file(gitdir: &Path, contents: &str) -> PathBuf {
    let link = Path::new(contents.trim());
    let link = match gitdir.parent() {
        Some(admin) if link.is_relative() => admin.join(link),
        _ => link.to_path_buf(),
    };
    link.canonicalize().unwrap_or(link)
}

impl Saved {
    fn record(git_dir: &Path) -> Result<Saved> {
        let mut files = vec![];
        let mut others = vec![];
        let config = git_dir.join("config");
        files.push((config.clone(), fs::read(&config)?));
        if let Ok(entries) = fs::read_dir(git_dir.join("worktrees")) {
            for e in entries {
                let gitdir = e?.path().join("gitdir");
                let Ok(bytes) = fs::read(&gitdir) else {
                    continue;
                };
                let link = linked_git_file(&gitdir, &String::from_utf8_lossy(&bytes));
                files.push((gitdir, bytes));
                if let Ok(b) = fs::read(&link) {
                    others.extend(link.parent().map(Path::to_path_buf));
                    files.push((link, b));
                }
            }
        }
        Ok(Saved { files, others })
    }
}

/// Replace `path`'s bytes via a sibling temp file and rename.
fn restore_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".spinney-restore");
    let tmp = PathBuf::from(tmp);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?
        .write_all(bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

const FETCH_SPEC: &str = "+refs/heads/*:refs/remotes/origin/*";

fn convert_with(plan: &Plan, space: &Space, hook: &dyn Fn(Point) -> Result<()>) -> Result<PathBuf> {
    // The confirm screen may have been open a while: re-check everything.
    if self::plan(&plan.root)? != *plan {
        bail!("the project changed since the preview; review it again");
    }
    let lock = plan.git_dir.join(LOCK);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow!("another conversion is running"),
            _ => anyhow!(e).context(format!("creating {}", lock.display())),
        })?;
    let result = locked(plan, space, hook);
    // Wherever `.git` ended up; the lock is ours, nothing else can have created it.
    for dir in [".bare", ".git"] {
        let _ = fs::remove_file(plan.root.join(dir).join(LOCK));
    }
    result
}

fn locked(plan: &Plan, space: &Space, hook: &dyn Fn(Point) -> Result<()>) -> Result<PathBuf> {
    let before = Snapshot::take(&plan.root)?;
    let saved = Saved::record(&plan.git_dir)?;
    let mut journal = Journal::default();
    steps(plan, space, &before, &saved, &mut journal, hook)
        .map_err(|cause| rollback(plan, &before, &saved, journal, cause))
}

/// Entries left in `root` beside the layout after converting to `worktree` (created there
/// while files were moving), to point out to the user.
pub fn strays(root: &Path, worktree: &Path) -> Result<Vec<PathBuf>> {
    let keep = [Path::new(".bare"), Path::new(".git"), worktree];
    let mut out = vec![];
    for e in fs::read_dir(root)? {
        let name = PathBuf::from(e?.file_name());
        if !keep.iter().any(|k| k.file_name() == Some(name.as_os_str())) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Top-level files of `bare` that git keeps per worktree (`--git-path` resolves them inside
/// the admin dir `admin`, relative to it): they belong to the new worktree. HEAD is copied
/// instead, and the lock stays.
fn per_worktree_files(bare: &Path, admin: &Path) -> Result<Vec<String>> {
    let mut names = vec![];
    for e in fs::read_dir(bare)? {
        let e = e?;
        let name = e.file_name();
        if !e.file_type()?.is_file() || name == "HEAD" || name == LOCK {
            continue;
        }
        match name.to_str() {
            Some(n) if !n.contains('\n') => names.push(n.to_string()),
            _ => bail!("unexpected file name in the git dir: {:?}", name),
        }
    }
    names.sort();
    if names.is_empty() {
        return Ok(names);
    }
    let mut args = vec!["--git-dir=.", "rev-parse"];
    for n in &names {
        args.extend(["--git-path", n.as_str()]);
    }
    let out = git::run(admin, &args)?;
    let paths: Vec<&str> = out.lines().collect();
    if paths.len() != names.len() {
        bail!(
            "git rev-parse --git-path returned {} paths for {}",
            paths.len(),
            names.len()
        );
    }
    Ok(names
        .into_iter()
        .zip(paths)
        .filter(|(_, p)| Path::new(p).is_relative())
        .map(|(n, _)| n)
        .collect())
}

fn steps(
    plan: &Plan,
    space: &Space,
    before: &Snapshot,
    saved: &Saved,
    j: &mut Journal,
    hook: &dyn Fn(Point) -> Result<()>,
) -> Result<PathBuf> {
    let root = &plan.root;
    let bare = root.join(".bare");
    let wt = root.join(&plan.target_dir_name);
    let admin = bare.join("worktrees").join(&plan.worktree_id);

    hook(Point::Start)?;

    // 2. Until step 4 writes root/.git, git run from root would find a repo above it, so
    // address the repository explicitly.
    j.rename(&plan.git_dir, &bare)?;
    git::run(&bare, &["--git-dir=.", "config", "core.bare", "true"])?;
    hook(Point::After(Step::RenameGitDir))?;

    // 3. Same-folder renames only: nothing is copied.
    j.mkdir(&wt)?;
    for (i, name) in plan.items.iter().enumerate() {
        hook(Point::Moved(i))?;
        j.rename(&root.join(name), &wt.join(name))?;
    }
    hook(Point::After(Step::MoveItems))?;

    // 4. The layout `git worktree add` creates: worktrees/<id>/{HEAD,commondir,gitdir} plus
    // every per-worktree file (index, logs/HEAD, ORIG_HEAD, MERGE_MSG, AUTO_MERGE, …, as
    // `git rev-parse --git-path` classifies them). The bare HEAD keeps naming the branch,
    // as in a fresh clone_into.
    let worktrees = bare.join("worktrees");
    if !taken(&worktrees) {
        j.mkdir(&worktrees)?;
    }
    j.mkdir(&admin)?;
    j.write_new(&admin.join("HEAD"), &fs::read(bare.join("HEAD"))?)?;
    j.write_new(&admin.join("commondir"), b"../..\n")?;
    let wt_git = wt.join(".git");
    j.write_new(
        &admin.join("gitdir"),
        format!("{}\n", wt_git.display()).as_bytes(),
    )?;
    for f in per_worktree_files(&bare, &admin)? {
        j.rename(&bare.join(&f), &admin.join(&f))?;
    }
    if taken(&bare.join("logs/HEAD")) {
        j.mkdir(&admin.join("logs"))?;
        j.rename(&bare.join("logs/HEAD"), &admin.join("logs/HEAD"))?;
    }
    j.write_new(&wt_git, format!("gitdir: {}\n", admin.display()).as_bytes())?;
    j.write_new(&root.join(".git"), b"gitdir: ./.bare\n")?;
    // Point the other linked worktrees at .bare. Only those: run bare, `repair` would also
    // rewrite root/.git to an absolute path. It exits 0 even when it fails; step 6 checks.
    if !saved.others.is_empty() {
        let mut args = vec!["worktree", "repair"];
        for o in &saved.others {
            args.push(o.to_str().context("non-UTF-8 worktree path")?);
        }
        git::run(root, &args)?;
    }
    hook(Point::After(Step::LinkWorktree))?;

    // 5.
    let remotes = git::run(root, &["remote"])?;
    if remotes.lines().any(|r| r == "origin") {
        let specs = git::run_ok_codes(
            root,
            &["config", "--get-all", "remote.origin.fetch"],
            &[0, 1],
        )?;
        // A --single-branch clone's narrower refspec is the user's choice; keep it.
        if specs.trim().is_empty() {
            git::run(
                root,
                &["config", "--add", "remote.origin.fetch", FETCH_SPEC],
            )?;
        }
    }
    creds::configure_repo(&bare, space)?;
    hook(Point::After(Step::Configure))?;

    // 6.
    let after = Snapshot::take(&wt)?;
    if after != *before {
        bail!("verification failed: {}", before.diff(&after));
    }
    if project::detect(root)? != Layout::WorktreeRoot {
        bail!("verification failed: not detected as a worktree layout");
    }
    let listed = project::list_worktrees(root)?
        .into_iter()
        .any(|w| w.path == wt && w.branch == plan.branch);
    if !listed {
        bail!("verification failed: git does not list {}", wt.display());
    }
    for other in &saved.others {
        git::run(other, &["status", "--porcelain"])
            .with_context(|| format!("verification failed: worktree {}", other.display()))?;
    }
    hook(Point::After(Step::Verify))?;
    Ok(wt)
}

/// Undo the journal in reverse. Deletes only what the conversion created; anything it
/// cannot put back is left in place and named in the error.
fn rollback(
    plan: &Plan,
    before: &Snapshot,
    saved: &Saved,
    journal: Journal,
    cause: anyhow::Error,
) -> anyhow::Error {
    if journal.0.is_empty() {
        return anyhow!("conversion failed: {cause:#}. Nothing was changed.");
    }
    let mut stuck = vec![];
    for undo in journal.0.into_iter().rev() {
        let (r, what) = match &undo {
            Undo::Renamed { from, to } => (
                if taken(from) {
                    Err(anyhow!("something else is now at {}", from.display()))
                } else {
                    fs::rename(to, from).map_err(Into::into)
                },
                format!("{} is still at {}", from.display(), to.display()),
            ),
            Undo::CreatedFile(p) => (
                fs::remove_file(p).map_err(Into::into),
                format!("{} (created by the conversion) is still there", p.display()),
            ),
            Undo::CreatedDir(p) => (
                fs::remove_dir(p).map_err(Into::into),
                format!(
                    "folder {} (created by the conversion) is still there",
                    p.display()
                ),
            ),
        };
        if let Err(e) = r {
            stuck.push(format!("{what}: {e:#}"));
        }
    }
    // Restore git's own files even when some undo above got stuck: otherwise core.bare stays
    // true and the other worktrees point into .bare. Moving .git back was journaled first,
    // so it was undone last; all saved paths are valid once it is a folder again.
    let root = plan.root.display();
    let fix = format!(
        "move .bare back to .git if needed, then run `git config core.bare false` and \
         `git worktree repair` in {root}"
    );
    if fs::symlink_metadata(&plan.git_dir).is_ok_and(|m| m.is_dir()) {
        for (path, bytes) in &saved.files {
            if let Err(e) = restore_bytes(path, bytes) {
                if *path == plan.git_dir.join("config") {
                    stuck.push(format!(
                        "could not restore {} ({e:#}); it may still say core.bare=true: {fix}",
                        path.display()
                    ));
                } else {
                    stuck.push(format!("could not restore {}: {e:#}", path.display()));
                }
            }
        }
    } else {
        stuck.push(format!(
            "{root}/.git is not back (the repository is in {root}/.bare, with core.bare=true); \
             the other linked worktrees were not repointed: {fix}"
        ));
    }
    if stuck.is_empty() {
        let check = Snapshot::take(&plan.root).and_then(|now| {
            if now != *before {
                bail!("{}", before.diff(&now));
            }
            if project::detect(&plan.root)? != Layout::NormalClone {
                bail!("not a normal clone");
            }
            Ok(())
        });
        match check {
            Ok(()) => {
                return anyhow!(
                    "conversion failed: {cause:#}. Everything was rolled back: the project \
                     is a normal clone again, unchanged."
                )
            }
            Err(e) if project::detect(&plan.root).ok() == Some(Layout::NormalClone) => {
                // Every change was undone; the difference is the user's own edits.
                return anyhow!(
                    "conversion failed: {cause:#}. Files changed while converting; \
                     everything was put back and the project is a normal clone again. \
                     Changed: {e:#}"
                );
            }
            Err(e) => stuck.push(format!("rolled back, but the project differs: {e:#}")),
        }
    }
    anyhow!(
        "conversion failed: {cause:#}. Rollback incomplete; nothing was deleted. \
         Needs attention:\n- {}",
        stuck.join("\n- ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions;
    use std::os::unix::fs::symlink;

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

    fn g(dir: &Path, args: &[&str]) -> String {
        git::run(dir, args).unwrap()
    }

    fn set_user(dir: &Path) {
        g(dir, &["config", "user.name", "Test User"]);
        g(dir, &["config", "user.email", "test@example.com"]);
    }

    /// `<tmp>/seed` (the "remote", one commit on `main`) and a normal clone at `<tmp>/proj`.
    fn normal_clone() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let seed = base.join("seed");
        fs::create_dir(&seed).unwrap();
        g(&seed, &["init", "-q", "-b", "main"]);
        set_user(&seed);
        for f in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            fs::write(seed.join(f), format!("{f}\n")).unwrap();
        }
        fs::write(seed.join(".gitignore"), ".env\nnode_modules/\ntarget/\n").unwrap();
        g(&seed, &["add", "."]);
        g(&seed, &["commit", "-q", "-m", "init"]);
        let url = format!("file://{}", seed.display());
        g(&base, &["clone", "-q", &url, "proj"]);
        let root = base.join("proj");
        set_user(&root);
        (tmp, root)
    }

    /// Everything a user could observe: files (kind, mode, bytes, symlink target, excluding
    /// the top-level `.git`), status, index, diffs, refs and HEAD.
    #[derive(Debug, PartialEq)]
    struct State {
        files: Vec<(PathBuf, String)>,
        status: String,
        stage: String,
        cached: String,
        unstaged: String,
        refs: String,
        head: String,
    }

    fn walk(base: &Path, rel: &Path, skip_git: bool, out: &mut Vec<(PathBuf, String)>) {
        let mut names: Vec<_> = fs::read_dir(base.join(rel))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        for name in names {
            if skip_git && rel.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let r = rel.join(&name);
            let p = base.join(&r);
            let m = fs::symlink_metadata(&p).unwrap();
            let mode = m.permissions().mode();
            if m.file_type().is_symlink() {
                out.push((r, format!("link {:?}", fs::read_link(&p).unwrap())));
            } else if m.is_dir() {
                out.push((r.clone(), format!("dir {mode:o}")));
                walk(base, &r, false, out);
            } else {
                out.push((r, format!("file {mode:o} {:?}", fs::read(&p).unwrap())));
            }
        }
    }

    fn files(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut out = vec![];
        walk(dir, Path::new(""), true, &mut out);
        out
    }

    fn state(dir: &Path) -> State {
        let ok = |args: &[&str]| git::run_ok_codes(dir, args, &[0, 1]).unwrap();
        State {
            files: files(dir),
            status: g(
                dir,
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                ],
            ),
            stage: g(dir, &["ls-files", "-s"]),
            cached: g(dir, &["diff", "--cached", "--binary"]),
            unstaged: g(dir, &["diff", "--binary"]),
            refs: g(dir, &["for-each-ref"]),
            head: ok(&["rev-parse", "--verify", "-q", "HEAD"])
                + &ok(&["symbolic-ref", "-q", "HEAD"]),
        }
    }

    /// Convert and assert the new worktree shows exactly what the clone showed.
    fn convert_ok(root: &Path) -> PathBuf {
        let before = state(root);
        let p = plan(root).unwrap();
        let wt = convert(&p, &space()).unwrap();
        assert_eq!(wt, root.join(&p.target_dir_name));
        assert_eq!(state(&wt), before);
        assert!(!taken(&root.join(".bare").join(LOCK)), "lock removed");
        assert_eq!(project::detect(root).unwrap(), Layout::WorktreeRoot);
        assert_eq!(
            fs::read_to_string(root.join(".git")).unwrap(),
            "gitdir: ./.bare\n"
        );
        assert_eq!(
            g(&root.join(".bare"), &["config", "--bool", "core.bare"]).trim(),
            "true"
        );
        let mut names: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut want = vec![
            ".bare".to_string(),
            ".git".into(),
            p.target_dir_name.clone(),
        ];
        want.sort();
        assert_eq!(names, want, "only the layout is left at the root");
        wt
    }

    // 1
    #[test]
    fn index_preserved_exactly() {
        let (_tmp, root) = normal_clone();
        fs::write(root.join("a.txt"), "staged\n").unwrap();
        g(&root, &["add", "a.txt"]);
        fs::write(root.join("a.txt"), "staged\nunstaged\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        g(&root, &["add", "new.txt"]);
        g(&root, &["rm", "-q", "b.txt"]);
        g(&root, &["mv", "c.txt", "renamed.txt"]);
        let cached = g(&root, &["diff", "--cached", "-M", "--name-status"]);
        assert!(cached.contains("R100\tc.txt\trenamed.txt"), "{cached}");
        let wt = convert_ok(&root);
        assert_eq!(g(&wt, &["diff", "--cached", "-M", "--name-status"]), cached);
        assert_eq!(
            fs::read_to_string(wt.join("a.txt")).unwrap(),
            "staged\nunstaged\n"
        );
    }

    // 2
    #[test]
    fn untracked_ignored_and_special_files_move() {
        let (_tmp, root) = normal_clone();
        fs::create_dir_all(root.join("u/nested/deeper")).unwrap();
        fs::write(root.join("u/nested/deeper/f.txt"), "deep\n").unwrap();
        fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        for i in 0..300 {
            let d = root.join(format!("node_modules/pkg{i}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("index.js"), format!("module.exports = {i};\n")).unwrap();
        }
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/app"), [0u8, 1, 2, 255]).unwrap();
        fs::write(root.join(".hidden"), "dot\n").unwrap();
        fs::write(root.join("with space.txt"), "s\n").unwrap();
        fs::write(root.join("ünïcødé ✓.txt"), "u\n").unwrap();
        let exe = root.join("run.sh");
        fs::write(&exe, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("a.txt", root.join("link")).unwrap();
        symlink("../nowhere", root.join("dangling")).unwrap();
        fs::create_dir(root.join("empty")).unwrap();

        let wt = convert_ok(&root);
        let m = fs::metadata(wt.join("run.sh")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read_link(wt.join("link")).unwrap(), Path::new("a.txt"));
        assert_eq!(
            fs::read_link(wt.join("dangling")).unwrap(),
            Path::new("../nowhere")
        );
        assert!(fs::read_dir(wt.join("empty")).unwrap().next().is_none());
        assert_eq!(fs::read_dir(wt.join("node_modules")).unwrap().count(), 300);
    }

    // 3
    #[test]
    fn stashes_branches_and_reflog_survive() {
        let (_tmp, root) = normal_clone();
        g(&root, &["branch", "topic"]);
        fs::write(root.join("a.txt"), "stashed\n").unwrap();
        g(&root, &["stash", "push", "-q", "-m", "saved work"]);
        fs::write(root.join("d.txt"), "next\n").unwrap();
        g(&root, &["commit", "-q", "-am", "second"]);
        let stashes = g(&root, &["stash", "list"]);
        let reflog = g(&root, &["reflog", "-1"]);
        assert!(stashes.contains("saved work"), "{stashes}");
        let wt = convert_ok(&root);
        assert_eq!(g(&wt, &["stash", "list"]), stashes);
        assert_eq!(g(&wt, &["reflog", "-1"]), reflog);
        g(&wt, &["rev-parse", "--verify", "topic"]);
        g(&wt, &["stash", "pop", "-q"]);
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "stashed\n");
    }

    // 4
    #[test]
    fn detached_head_gets_sha_folder() {
        let (_tmp, root) = normal_clone();
        g(&root, &["checkout", "-q", "--detach"]);
        let sha = g(&root, &["rev-parse", "HEAD"]);
        let p = plan(&root).unwrap();
        assert_eq!(p.branch, None);
        assert_eq!(p.target_dir_name, format!("detached-{}", &sha[..7]));
        let wt = convert_ok(&root);
        assert_eq!(g(&wt, &["rev-parse", "HEAD"]), sha);
    }

    #[test]
    fn unborn_repo_uses_branch_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("fresh");
        fs::create_dir(&root).unwrap();
        g(&root, &["init", "-q", "-b", "trunk"]);
        fs::write(root.join("first.txt"), "1\n").unwrap();
        g(&root, &["add", "first.txt"]);
        let wt = convert_ok(&root);
        assert_eq!(wt, root.join("trunk"));
        assert_eq!(g(&wt, &["status", "--porcelain"]), "A  first.txt\n");
        set_user(&wt);
        actions::commit(&wt, "first").unwrap();
        g(&wt, &["rev-parse", "--verify", "trunk"]);
    }

    #[test]
    fn slashed_branch_is_flattened() {
        let (_tmp, root) = normal_clone();
        g(&root, &["switch", "-q", "-c", "feature/login"]);
        let wt = convert_ok(&root);
        assert_eq!(wt, root.join("feature-login"));
        let list = project::list_worktrees(&root).unwrap();
        assert!(list
            .iter()
            .any(|w| w.path == wt && w.branch.as_deref() == Some("feature/login")));
    }

    // 5
    #[test]
    fn existing_linked_worktree_keeps_working() {
        let (tmp, root) = normal_clone();
        let other = tmp.path().canonicalize().unwrap().join("other");
        g(&root, &["worktree", "add", "-q", "../other", "-b", "other"]);
        fs::write(other.join("o.txt"), "wip\n").unwrap();
        let other_status = g(&other, &["status", "--porcelain"]);
        convert_ok(&root);
        assert_eq!(g(&other, &["status", "--porcelain"]), other_status);
        let list = project::list_worktrees(&root).unwrap();
        assert!(list
            .iter()
            .any(|w| w.path == other && w.branch.as_deref() == Some("other")));
        let link = fs::read_to_string(other.join(".git")).unwrap();
        assert!(link.contains(".bare/worktrees/other"), "{link}");
    }

    #[test]
    fn worktree_id_clash_gets_suffix() {
        let (tmp, root) = normal_clone();
        let other = tmp.path().canonicalize().unwrap().join("x/main");
        g(&root, &["worktree", "add", "-q", "../x/main", "-b", "side"]);
        let p = plan(&root).unwrap();
        assert_eq!(p.target_dir_name, "main");
        assert_eq!(p.worktree_id, "main1");
        let wt = convert_ok(&root);
        let list = project::list_worktrees(&root).unwrap();
        assert!(list.iter().any(|w| w.path == wt));
        assert!(list.iter().any(|w| w.path == other));
        g(&other, &["status"]);
    }

    // 6
    fn assert_refused(root: &Path, needle: &str) {
        let before = files(root);
        let err = plan(root).unwrap_err().to_string();
        assert!(err.contains(needle), "{needle:?} not in {err:?}");
        assert_eq!(files(root), before, "refusal changed nothing");
    }

    /// `main` and `side` both change `a.txt`.
    fn diverged() -> (tempfile::TempDir, PathBuf) {
        let (tmp, root) = normal_clone();
        g(&root, &["switch", "-q", "-c", "side"]);
        fs::write(root.join("a.txt"), "side\n").unwrap();
        g(&root, &["commit", "-q", "-am", "side"]);
        g(&root, &["switch", "-q", "main"]);
        fs::write(root.join("a.txt"), "main\n").unwrap();
        g(&root, &["commit", "-q", "-am", "main"]);
        (tmp, root)
    }

    #[test]
    fn refuses_merge_in_progress() {
        let (_tmp, root) = diverged();
        assert!(git::run(&root, &["merge", "side"]).is_err());
        assert_refused(&root, "merge");
    }

    #[test]
    fn refuses_rebase_in_progress() {
        let (_tmp, root) = diverged();
        assert!(git::run(&root, &["rebase", "side"]).is_err());
        assert_refused(&root, "rebase");
    }

    #[test]
    fn refuses_submodules() {
        let (tmp, root) = normal_clone();
        let url = format!(
            "file://{}/seed",
            tmp.path().canonicalize().unwrap().display()
        );
        g(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &url,
                "sub",
            ],
        );
        assert_refused(&root, "submodule");
    }

    #[test]
    fn refuses_existing_bare_or_target() {
        let (tmp, root) = normal_clone();
        fs::create_dir(root.join(".bare")).unwrap();
        assert_refused(&root, ".bare");
        fs::remove_dir(root.join(".bare")).unwrap();
        fs::create_dir(root.join("main")).unwrap();
        assert_refused(&root, "main");
        fs::remove_dir(root.join("main")).unwrap();
        symlink(tmp.path().join("nowhere"), root.join("main")).unwrap();
        assert_refused(&root, "main");
    }

    #[test]
    fn refuses_git_file() {
        let (tmp, root) = normal_clone();
        g(&root, &["worktree", "add", "-q", "../linked"]);
        let linked = tmp.path().canonicalize().unwrap().join("linked");
        assert_refused(&linked, "not a folder");
    }

    #[test]
    fn refuses_unsupported_config() {
        let (_tmp, root) = normal_clone();
        let r = root.to_str().unwrap();
        for (key, value) in [
            ("core.worktree", r),
            ("core.bare", "true"),
            ("extensions.worktreeConfig", "true"),
            ("core.sparseCheckout", "true"),
        ] {
            g(&root, &["config", key, value]);
            assert_refused(&root, key);
            g(
                &root.join(".git"),
                &["--git-dir=.", "config", "--unset", key],
            );
            plan(&root).unwrap();
        }
    }

    // 6 + 9
    #[test]
    fn already_converted_is_refused_not_moved_again() {
        let (_tmp, root) = normal_clone();
        let p = plan(&root).unwrap();
        let wt = convert_ok(&root);
        let after = files(&root);
        assert_refused(&root, "not a folder");
        assert!(convert(&p, &space()).is_err());
        assert_eq!(files(&root), after);
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "a.txt\n");
    }

    #[test]
    fn stale_plan_is_refused() {
        let (_tmp, root) = normal_clone();
        let p = plan(&root).unwrap();
        fs::write(root.join("late.txt"), "late\n").unwrap();
        let before = files(&root);
        let err = convert(&p, &space()).unwrap_err().to_string();
        assert!(err.contains("changed"), "{err}");
        assert_eq!(files(&root), before);
        assert_eq!(project::detect(&root).unwrap(), Layout::NormalClone);
    }

    // 7
    /// Normal clone with staged/unstaged/untracked/ignored files, a stash and a linked
    /// worktree elsewhere; returns (tmp, root, other).
    fn rich_clone() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, root) = normal_clone();
        fs::write(root.join("a.txt"), "stash me\n").unwrap();
        g(&root, &["stash", "push", "-q"]);
        fs::write(root.join("a.txt"), "staged\n").unwrap();
        g(&root, &["add", "a.txt"]);
        fs::write(root.join("b.txt"), "unstaged\n").unwrap();
        fs::write(root.join("untracked.txt"), "u\n").unwrap();
        fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/x.js"), "x\n").unwrap();
        symlink("a.txt", root.join("link")).unwrap();
        g(&root, &["worktree", "add", "-q", "../other", "-b", "other"]);
        let other = tmp.path().canonicalize().unwrap().join("other");
        (tmp, root, other)
    }

    /// `State` plus every byte under `.git` except the index (status refreshes it), plus the
    /// other worktree's `.git` file.
    fn full_state(root: &Path, other: &Path) -> (State, Vec<(PathBuf, String)>, String) {
        let st = state(root);
        let mut admin = vec![];
        walk(&root.join(".git"), Path::new(""), false, &mut admin);
        admin.retain(|(p, _)| p != Path::new("index"));
        let link = fs::read_to_string(other.join(".git")).unwrap();
        (st, admin, link)
    }

    fn fail_at(at: Point) -> impl Fn(Point) -> Result<()> {
        move |p| {
            if p == at {
                bail!("injected failure at {p:?}")
            }
            Ok(())
        }
    }

    fn assert_rolled_back(at: Point) {
        let (_tmp, root, other) = rich_clone();
        let before = full_state(&root, &other);
        let p = plan(&root).unwrap();
        let err = convert_with(&p, &space(), &fail_at(at))
            .unwrap_err()
            .to_string();
        assert!(err.contains("injected failure"), "{err}");
        assert!(err.contains("rolled back"), "{err}");
        assert_eq!(full_state(&root, &other), before, "{at:?}");
        assert_eq!(project::detect(&root).unwrap(), Layout::NormalClone);
        g(&other, &["status"]);
    }

    #[test]
    fn rollback_after_each_step() {
        for step in [
            Step::RenameGitDir,
            Step::MoveItems,
            Step::LinkWorktree,
            Step::Configure,
            Step::Verify,
        ] {
            assert_rolled_back(Point::After(step));
        }
    }

    #[test]
    fn rollback_part_way_through_moving() {
        assert_rolled_back(Point::Moved(0));
        assert_rolled_back(Point::Moved(3));
    }

    #[test]
    fn blocked_rollback_keeps_both_copies_and_says_where() {
        let (_tmp, root, other) = rich_clone();
        let p = plan(&root).unwrap();
        let wt = root.join(&p.target_dir_name);
        let intruder = root.join("a.txt");
        let hook = |pt| {
            if pt == Point::After(Step::MoveItems) {
                fs::write(root.join("a.txt"), "intruder\n").unwrap();
                bail!("injected failure");
            }
            Ok(())
        };
        let err = convert_with(&p, &space(), &hook).unwrap_err().to_string();
        assert!(err.contains("incomplete"), "{err}");
        assert!(
            err.contains(&wt.join("a.txt").display().to_string()),
            "{err}"
        );
        assert_eq!(fs::read_to_string(&intruder).unwrap(), "intruder\n");
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "staged\n");
        assert!(root.join(".git").is_dir(), "other undo steps still ran");
        assert!(root.join("b.txt").exists());
        assert_git_healthy(&root, &other);
    }

    /// After any rollback, complete or not, git works in the project and the other worktree.
    fn assert_git_healthy(root: &Path, other: &Path) {
        g(root, &["status", "--porcelain"]);
        let bare = git::run_ok_codes(root, &["config", "--bool", "core.bare"], &[0, 1]).unwrap();
        assert_ne!(bare.trim(), "true", "core.bare restored");
        g(other, &["status", "--porcelain"]);
        assert_eq!(project::detect(root).unwrap(), Layout::NormalClone);
        assert!(!taken(&root.join(".git").join(LOCK)), "lock removed");
    }

    #[test]
    fn blocked_rollback_after_linking_restores_git() {
        let (_tmp, root, other) = rich_clone();
        let p = plan(&root).unwrap();
        let hook = |pt| {
            if pt == Point::After(Step::LinkWorktree) {
                fs::write(root.join("a.txt"), "intruder\n").unwrap();
                bail!("injected failure");
            }
            Ok(())
        };
        let err = convert_with(&p, &space(), &hook).unwrap_err().to_string();
        assert!(err.contains("incomplete"), "{err}");
        assert_git_healthy(&root, &other);
        assert!(
            fs::read_to_string(other.join(".git"))
                .unwrap()
                .contains("/.git/worktrees/other"),
            "other worktree points at .git again"
        );
    }

    #[test]
    fn foreign_file_in_admin_dir_still_restores_git() {
        let (_tmp, root, other) = rich_clone();
        let p = plan(&root).unwrap();
        let admin = root.join(".bare/worktrees").join(&p.worktree_id);
        let hook = |pt| {
            if pt == Point::After(Step::LinkWorktree) {
                fs::write(admin.join("locked"), "ide").unwrap(); // say, another tool
            }
            if pt == Point::After(Step::Configure) {
                bail!("injected failure");
            }
            Ok(())
        };
        let err = convert_with(&p, &space(), &hook).unwrap_err().to_string();
        assert!(err.contains("incomplete"), "{err}");
        assert!(err.contains(&p.worktree_id), "{err}");
        assert_git_healthy(&root, &other);
    }

    /// No injected failure: a read-only folder cannot be moved (EACCES), and Finder rewrites
    /// .DS_Store in the folder while it is being emptied, so that undo is blocked too.
    #[test]
    fn organic_failure_with_finder_still_restores_git() {
        let (_tmp, root, other) = rich_clone();
        fs::write(root.join(".DS_Store"), "finder").unwrap();
        fs::create_dir(root.join("rodir")).unwrap();
        fs::write(root.join("rodir/f"), "f").unwrap();
        fs::set_permissions(root.join("rodir"), fs::Permissions::from_mode(0o555)).unwrap();
        let p = plan(&root).unwrap();
        let idx = p
            .items
            .iter()
            .position(|i| i == Path::new("rodir"))
            .unwrap();
        let hook = |pt| {
            if pt == Point::Moved(idx) {
                fs::write(root.join(".DS_Store"), "finder again").unwrap();
            }
            Ok(())
        };
        let err = convert_with(&p, &space(), &hook).unwrap_err().to_string();
        fs::set_permissions(root.join("rodir"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            err.contains("incomplete") && err.contains(".DS_Store"),
            "{err}"
        );
        assert_git_healthy(&root, &other);
        assert_eq!(fs::read_to_string(root.join("rodir/f")).unwrap(), "f");
    }

    #[test]
    fn edits_during_conversion_are_reported_as_put_back() {
        let (_tmp, root, other) = rich_clone();
        let p = plan(&root).unwrap();
        let wt = root.join(&p.target_dir_name);
        let hook = |pt| {
            if pt == Point::After(Step::Configure) {
                fs::write(wt.join("untracked.txt"), "edited in the IDE\n").unwrap();
            }
            Ok(())
        };
        let err = convert_with(&p, &space(), &hook).unwrap_err().to_string();
        assert!(err.contains("Files changed while converting"), "{err}");
        assert!(err.contains("everything was put back"), "{err}");
        assert!(!err.contains("incomplete"), "{err}");
        assert_eq!(
            fs::read_to_string(root.join("untracked.txt")).unwrap(),
            "edited in the IDE\n"
        );
        assert_git_healthy(&root, &other);
    }

    #[test]
    fn per_worktree_git_files_move_with_the_worktree() {
        let (_tmp, root) = diverged();
        g(&root, &["switch", "-q", "side"]);
        g(&root, &["reset", "-q", "--hard", "HEAD~1"]);
        fs::write(root.join("side.txt"), "side\n").unwrap();
        g(&root, &["add", "side.txt"]);
        g(&root, &["commit", "-q", "-m", "side only"]);
        g(&root, &["switch", "-q", "main"]);
        g(&root, &["merge", "-q", "--squash", "side"]);
        fs::write(root.join(".git/COMMIT_EDITMSG"), "msg\n").unwrap();
        fs::write(root.join(".git/gitk.cache"), "tool state\n").unwrap();
        assert!(root.join(".git/SQUASH_MSG").exists());
        let mut top = vec![];
        for e in fs::read_dir(root.join(".git")).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_file() {
                let bytes = fs::read(e.path()).unwrap();
                top.push((e.file_name().into_string().unwrap(), bytes));
            }
        }
        let p = plan(&root).unwrap();
        let wt = convert_ok(&root);
        let admin = root.join(".bare/worktrees").join(&p.worktree_id);
        let mut per_wt = 0;
        for (name, bytes) in top {
            let gp = g(&wt, &["rev-parse", "--git-path", &name]);
            let resolved = wt.join(gp.trim());
            if resolved.starts_with(&admin) && name != "index" {
                per_wt += 1;
                let now = fs::read(&resolved).unwrap_or_else(|e| panic!("{name} lost: {e}"));
                if name != "HEAD" {
                    assert_eq!(now, bytes, "{name}");
                    assert!(!root.join(".bare").join(&name).exists(), "{name} moved");
                }
            }
        }
        assert!(per_wt >= 4, "{per_wt}");
        let msg = g(&wt, &["rev-parse", "--git-path", "SQUASH_MSG"]);
        assert!(wt.join(msg.trim()).exists());
    }

    #[test]
    fn conflicted_stash_apply_keeps_auto_merge() {
        let (_tmp, root) = normal_clone();
        fs::write(root.join("a.txt"), "stash\n").unwrap();
        g(&root, &["stash", "push", "-q"]);
        fs::write(root.join("a.txt"), "commit\n").unwrap();
        g(&root, &["commit", "-q", "-am", "c"]);
        assert!(git::run(&root, &["stash", "apply"]).is_err());
        let auto = fs::read(root.join(".git/AUTO_MERGE")).unwrap();
        let wt = convert_ok(&root);
        let gp = g(&wt, &["rev-parse", "--git-path", "AUTO_MERGE"]);
        assert_eq!(fs::read(wt.join(gp.trim())).unwrap(), auto);
    }

    #[test]
    fn files_created_during_the_move_are_reported() {
        let (_tmp, root) = normal_clone();
        let p = plan(&root).unwrap();
        let hook = |pt| {
            if pt == Point::Moved(1) {
                fs::write(root.join("late.txt"), "late\n").unwrap();
            }
            Ok(())
        };
        let wt = convert_with(&p, &space(), &hook).unwrap();
        assert_eq!(strays(&root, &wt).unwrap(), [PathBuf::from("late.txt")]);
        assert_eq!(fs::read_to_string(root.join("late.txt")).unwrap(), "late\n");
        let (_tmp2, clean) = normal_clone();
        let wt = convert_ok(&clean);
        assert!(strays(&clean, &wt).unwrap().is_empty());
    }

    #[test]
    fn refuses_linked_worktree_inside_root() {
        let (_tmp, root) = normal_clone();
        g(
            &root,
            &["worktree", "add", "-q", ".worktrees/feat", "-b", "feat"],
        );
        assert_refused(&root, "inside the project folder");
    }

    /// `git worktree add --relative-paths` (git 2.48+); false on an older git.
    fn add_relative_worktree(root: &Path, path: &str, branch: &str) -> bool {
        let args = [
            "worktree",
            "add",
            "-q",
            "--relative-paths",
            path,
            "-b",
            branch,
        ];
        git::run(root, &args).is_ok()
    }

    #[test]
    fn refuses_relative_linked_worktree_inside_root() {
        let (_tmp, root) = normal_clone();
        if !add_relative_worktree(&root, ".worktrees/feat", "feat") {
            return; // git too old for relative worktree paths
        }
        assert_refused(&root, "inside the project folder");
    }

    #[test]
    fn relative_linked_worktree_keeps_working() {
        let (tmp, root) = normal_clone();
        let other = tmp.path().canonicalize().unwrap().join("other");
        if !add_relative_worktree(&root, "../other", "other") {
            return; // git too old for relative worktree paths
        }
        fs::write(other.join("o.txt"), "wip\n").unwrap();
        let other_status = g(&other, &["status", "--porcelain"]);
        convert_ok(&root);
        assert_eq!(g(&other, &["status", "--porcelain"]), other_status);
        let list = project::list_worktrees(&root).unwrap();
        assert!(list
            .iter()
            .any(|w| w.path == other && w.branch.as_deref() == Some("other")));
    }

    #[test]
    fn single_branch_refspec_is_kept() {
        let (tmp, _root) = normal_clone();
        let base = tmp.path().canonicalize().unwrap();
        let url = format!("file://{}", base.join("seed").display());
        g(
            &base,
            &["clone", "-q", "--single-branch", "-b", "main", &url, "sb"],
        );
        let root = base.join("sb");
        let spec = g(&root, &["config", "--get-all", "remote.origin.fetch"]);
        assert_eq!(spec, "+refs/heads/main:refs/remotes/origin/main\n");
        let wt = convert_ok(&root);
        assert_eq!(
            g(&wt, &["config", "--get-all", "remote.origin.fetch"]),
            spec
        );
    }

    #[test]
    fn lock_refuses_a_second_conversion() {
        let (_tmp, root) = normal_clone();
        let p = plan(&root).unwrap();
        fs::write(root.join(".git").join(LOCK), "").unwrap();
        assert_refused(&root, "another conversion is running");
        let err = convert(&p, &space()).unwrap_err().to_string();
        assert!(err.contains("another conversion is running"), "{err}");
        assert!(
            taken(&root.join(".git").join(LOCK)),
            "someone else's lock kept"
        );
        assert_eq!(project::detect(&root).unwrap(), Layout::NormalClone);
    }

    #[test]
    fn failure_before_any_change_touches_nothing() {
        let (_tmp, root, other) = rich_clone();
        let p = plan(&root).unwrap();
        let link = other.join(".git");
        // Another tool rewrites the other worktree's link after it was saved: an empty
        // rollback must not "restore" the saved bytes over it.
        let hook = |pt| {
            if pt == Point::Start {
                assert!(taken(&root.join(".git").join(LOCK)), "locked while running");
                let mut s = fs::read_to_string(&link).unwrap();
                s.push('\n');
                fs::write(&link, s).unwrap();
                bail!("injected failure");
            }
            Ok(())
        };
        let before = fs::read_to_string(&link).unwrap();
        let err = convert_with(&p, &space(), &hook).unwrap_err().to_string();
        assert!(err.contains("Nothing was changed"), "{err}");
        assert_eq!(fs::read_to_string(&link).unwrap(), format!("{before}\n"));
        assert_git_healthy(&root, &other);
    }

    // 8
    #[test]
    fn converted_project_is_usable() {
        let (tmp, root) = normal_clone();
        let seed = tmp.path().canonicalize().unwrap().join("seed");
        let wt = convert_ok(&root);

        fs::write(wt.join("mine.txt"), "m\n").unwrap();
        g(&wt, &["add", "mine.txt"]);
        actions::commit(&wt, "mine").unwrap();
        assert_eq!(g(&wt, &["log", "-1", "--format=%s"]), "mine\n");

        let second = project::add_worktree(&root, "feature/new", "main").unwrap();
        assert_eq!(second, root.join("feature-new"));
        g(&second, &["status"]);

        // `main` now has a local commit, so pull fast-forwards a branch without one.
        fs::write(seed.join("remote.txt"), "r\n").unwrap();
        g(&seed, &["add", "remote.txt"]);
        g(&seed, &["commit", "-q", "-m", "remote"]);
        actions::fetch(&wt).unwrap();
        g(&wt, &["rev-parse", "--verify", "origin/main"]);
        g(&second, &["branch", "-q", "--set-upstream-to=origin/main"]);
        g(&second, &["reset", "-q", "--hard", "main~1"]);
        actions::pull(&second).unwrap();
        assert!(second.join("remote.txt").exists());

        let spec = g(&wt, &["config", "--get-all", "remote.origin.fetch"]);
        assert!(
            spec.contains("+refs/heads/*:refs/remotes/origin/*"),
            "{spec}"
        );
        let helpers = g(&wt, &["config", "--get-all", "credential.helper"]);
        assert!(helpers.contains("credential test-space"), "{helpers}");
        assert_eq!(g(&wt, &["config", "user.email"]).trim(), "test@example.com");
    }
}
