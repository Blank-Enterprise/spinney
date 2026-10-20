#![allow(dead_code)]

use crate::config::Space;
use crate::{creds, git};
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Worktree folder name for a branch: `/` becomes `-`.
pub fn worktree_dir_name(branch: &str) -> String {
    branch.replace('/', "-")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    WorktreeRoot,
    NormalClone,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub is_bare: bool,
    /// The repo's own working tree (listed first): git can't remove it.
    pub main: bool,
    /// `git worktree lock`ed: Spinney won't remove it.
    pub locked: bool,
    /// Its folder is gone or moved: git can't reach it until it is located or removed.
    pub prunable: bool,
}

pub fn detect(root: &Path) -> Result<Layout> {
    if !root.exists() {
        // Moved: opening it from the new place relinks its worktrees (`repair`).
        bail!(
            "{} no longer exists. If it moved, open it from its new folder",
            root.display()
        );
    }
    let git = root.join(".git");
    if root.join(".bare").is_dir() && git.is_file() {
        Ok(Layout::WorktreeRoot)
    } else if git.is_dir() {
        Ok(Layout::NormalClone)
    } else {
        bail!("{} is not a git project", root.display())
    }
}

/// Create `<parent>/<name>` for a new project. `name` must be one plain path segment (so
/// the cleanup's recursive delete can never reach outside it) and nothing may be there yet,
/// not even a dangling symlink. Only a root claimed here may be cleaned up.
fn claim_root(parent: &Path, name: &str) -> Result<PathBuf> {
    let mut parts = Path::new(name).components();
    if !matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) {
        bail!("{name:?} is not a valid folder name");
    }
    let root = parent.join(name);
    if std::fs::symlink_metadata(&root).is_ok() {
        bail!("{} already exists", root.display());
    }
    std::fs::create_dir(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

/// Clone `url` to `<parent>/<name>`: as `.bare` plus a worktree for the default branch, or
/// as a normal clone.
pub fn clone_into(
    url: &str,
    parent: &Path,
    name: &str,
    space: &Space,
    layout: Layout,
) -> Result<PathBuf> {
    let root = claim_root(parent, name)?;
    let result = match layout {
        Layout::WorktreeRoot => clone_steps(url, parent, &root, space),
        Layout::NormalClone => clone_normal(url, parent, &root, space),
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&root); // we created it, so clean it up
    }
    result.map(|()| root)
}

/// Plain `git clone` into the empty `root`, with the same credentials and identity as the
/// worktree layout.
fn clone_normal(url: &str, parent: &Path, root: &Path, space: &Space) -> Result<()> {
    let root_str = root.to_str().context("non-UTF-8 path")?;
    let mut args = creds::git_env_args(space)?;
    args.extend(["clone", url, root_str].map(String::from));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git::run(parent, &refs)?;
    creds::configure_repo(&root.join(".git"), space)
}

fn clone_steps(url: &str, parent: &Path, root: &Path, space: &Space) -> Result<()> {
    let bare = root.join(".bare");
    let bare_str = bare.to_str().context("non-UTF-8 path")?;
    let env = creds::git_env_args(space)?;
    let with_env = |rest: &[&str]| -> Vec<String> {
        env.iter()
            .cloned()
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    };
    let run_env = |dir: &Path, rest: &[&str]| -> Result<String> {
        let args = with_env(rest);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        git::run(dir, &refs)
    };

    run_env(parent, &["clone", "--bare", url, bare_str])?;
    std::fs::write(root.join(".git"), "gitdir: ./.bare\n")?;
    git::run(
        &bare,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )?;
    run_env(&bare, &["fetch", "origin"])?;
    let default = git::run(&bare, &["symbolic-ref", "--short", "HEAD"])?
        .trim()
        .to_string();
    // `clone --bare` copied every remote head into refs/heads/*: untracked branches that go
    // stale. Keep only the default; git's DWIM later creates tracking branches on demand.
    let keep = format!("refs/heads/{default}");
    let heads = git::run(
        &bare,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    )?;
    for r in heads.lines().filter(|r| *r != keep) {
        git::run(&bare, &["update-ref", "-d", r])?;
    }
    let dir = format!("./{}", worktree_dir_name(&default));
    git::run(root, &["worktree", "add", &dir, &default])?;
    git::run(
        root,
        &[
            "branch",
            &format!("--set-upstream-to=origin/{default}"),
            &default,
        ],
    )?;
    creds::configure_repo(&bare, space)
}

/// Check `branch` with `git check-ref-format --branch`.
pub fn check_branch_name(branch: &str) -> Result<()> {
    // Outside any repo, so `@{-1}`-style shorthands cannot resolve to a real branch.
    git::run(
        &std::env::temp_dir(),
        &["check-ref-format", "--branch", branch],
    )
    .map(|_| ())
}

/// New empty project at `<parent>/<name>`: a normal repo on `branch`. No remote and no
/// commit.
pub fn create_new(parent: &Path, name: &str, branch: &str, space: &Space) -> Result<PathBuf> {
    let root = claim_root(parent, name)?;
    let result = create_steps(parent, &root, branch, space);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&root); // we created it, so clean it up
    }
    result.map(|()| root)
}

fn create_steps(parent: &Path, root: &Path, branch: &str, space: &Space) -> Result<()> {
    let root_str = root.to_str().context("non-UTF-8 path")?;
    git::run(parent, &["init", "-q", "-b", branch, root_str])?;
    creds::configure_repo(&root.join(".git"), space)
}

/// The project a folder belongs to: the main repo of any of its worktrees, wherever that
/// worktree lives (Spinney's, an agent's, an IDE's). A `.git` or `.bare` common dir means
/// its parent; any other bare repo is the project itself.
pub fn project_root(dir: &Path) -> Result<PathBuf> {
    let out = git::run(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .with_context(|| format!("{} is not a git project", dir.display()))?;
    let common = PathBuf::from(out.trim());
    let name = common.file_name().unwrap_or_default();
    Ok(match common.parent() {
        Some(parent) if name == ".git" || name == ".bare" => parent.to_path_buf(),
        _ => common,
    })
}

/// Same folder, through symlinks such as macOS's `/tmp` (git lists canonical paths).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || a.canonicalize().ok() == b.canonicalize().ok()
}

/// Relink worktrees that moved, or whose project moved: every listed worktree folder that
/// still exists (git still knows where they are) plus every folder in `worktrees_dir` (finds
/// ones renamed there). `git worktree repair` leaves healthy links alone.
pub fn repair(root: &Path) -> Result<()> {
    let mut paths: Vec<PathBuf> = list_worktrees(root)?
        .into_iter()
        .filter(|w| !w.is_bare && w.path.is_dir())
        .map(|w| w.path)
        .collect();
    let dir = worktrees_dir(root);
    if dir != root {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let renamed: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join(".git").is_file() && !paths.contains(p))
                .collect();
            paths.extend(renamed);
        }
    }
    relink(root, &paths)
}

/// `git worktree repair` for `paths`, e.g. a missing worktree's new location.
pub fn relink(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut args = vec!["worktree".to_string(), "repair".to_string()];
    for p in paths {
        args.push(p.to_str().context("non-UTF-8 path")?.to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git::run(root, &refs).map(|_| ())
}

pub fn list_worktrees(root: &Path) -> Result<Vec<Worktree>> {
    let out = git::run(root, &["worktree", "list", "--porcelain"])?;
    Ok(out
        .split("\n\n")
        .enumerate()
        .filter_map(|(i, block)| {
            let mut wt = Worktree {
                path: PathBuf::new(),
                branch: None,
                head: String::new(),
                is_bare: false,
                main: i == 0,
                locked: false,
                prunable: false,
            };
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    wt.path = PathBuf::from(p);
                } else if let Some(h) = line.strip_prefix("HEAD ") {
                    wt.head = h.to_string();
                } else if let Some(b) = line.strip_prefix("branch ") {
                    wt.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
                } else if line == "bare" {
                    wt.is_bare = true;
                } else if line == "locked" || line.starts_with("locked ") {
                    wt.locked = true;
                } else if line == "prunable" || line.starts_with("prunable ") {
                    wt.prunable = true;
                }
            }
            wt.main &= !wt.is_bare;
            (!wt.path.as_os_str().is_empty()).then_some(wt)
        })
        .collect())
}

/// Where new worktrees go: `<parent>/<name>.worktrees`, beside the project and outside its
/// working tree (so IDEs, search and builds don't see a copy). Old `.bare` projects keep
/// theirs inside the root.
pub fn worktrees_dir(root: &Path) -> PathBuf {
    if detect(root).ok() == Some(Layout::WorktreeRoot) {
        return root.to_path_buf();
    }
    let name = root.file_name().unwrap_or_default().to_string_lossy();
    root.with_file_name(format!("{name}.worktrees"))
}

/// Target folder for `branch`, refusing before git runs if it already exists.
fn free_target(root: &Path, branch: &str) -> Result<(PathBuf, String)> {
    let target = worktrees_dir(root).join(worktree_dir_name(branch));
    if target.exists() {
        bail!("{} already exists", target.display());
    }
    let arg = target.to_str().context("non-UTF-8 path")?.to_string();
    Ok((target, arg))
}

/// Create worktree + new branch `branch` from `base`.
pub fn add_worktree(root: &Path, branch: &str, base: &str) -> Result<PathBuf> {
    let (target, rel) = free_target(root, branch)?;
    git::run(root, &["worktree", "add", "-b", branch, &rel, base])?;
    Ok(target)
}

/// Create a worktree for an existing branch (local, or origin-only via git's DWIM).
pub fn add_worktree_existing(root: &Path, branch: &str) -> Result<PathBuf> {
    let (target, rel) = free_target(root, branch)?;
    git::run(root, &["worktree", "add", &rel, branch])?;
    Ok(target)
}

pub fn remove_worktree(root: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = path.to_str().context("non-UTF-8 path")?;
    // A missing folder has no changes to lose; git drops its record.
    if !force
        && path.is_dir()
        && !git::run(path, &["status", "--porcelain"])?
            .trim()
            .is_empty()
    {
        bail!("worktree has uncommitted changes");
    }
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);
    git::run(root, &args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn fixture() -> (tempfile::TempDir, String, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        let g = |args: &[&str]| git::run(&remote, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
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

    fn cloned() -> (tempfile::TempDir, PathBuf) {
        let (tmp, url, parent) = fixture();
        let root = clone_into(&url, &parent, "proj", &space(), Layout::WorktreeRoot).unwrap();
        (tmp, root)
    }

    /// Normal clone of the fixture; returns (tmp, root).
    fn normal() -> (tempfile::TempDir, PathBuf) {
        let (tmp, url, parent) = fixture();
        let root = clone_into(&url, &parent, "proj", &space(), Layout::NormalClone).unwrap();
        (tmp, root)
    }

    /// `git worktree add` run directly, as an agent or another tool would.
    fn external(root: &Path, path: &Path, args: &[&str]) {
        let p = path.to_str().unwrap();
        let mut all = vec!["worktree", "add", "-q"];
        all.extend_from_slice(args);
        all.push(p);
        git::run(root, &all).unwrap();
    }

    fn listed(root: &Path, path: &Path) -> Worktree {
        list_worktrees(root)
            .unwrap()
            .into_iter()
            .find(|w| same_dir(&w.path, path))
            .unwrap_or_else(|| panic!("{} not listed", path.display()))
    }

    #[test]
    fn add_worktree_goes_beside_the_project() {
        let (_tmp, root) = normal();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        assert_eq!(p, root.with_file_name("proj.worktrees").join("feature-new"));
        assert_eq!(listed(&root, &p).branch.as_deref(), Some("feature/new"));
        let main = listed(&root, &root);
        assert!(main.main && !main.is_bare);
        assert!(!listed(&root, &p).main);
    }

    #[test]
    fn worktrees_made_outside_spinney_are_listed_resolved_and_removable() {
        let (tmp, root) = normal();
        let nested = root.join(".claude/worktrees/agent-1");
        let sibling = root.with_file_name("proj.feature-x");
        let elsewhere = tmp.path().join("elsewhere/wt");
        let detached = tmp.path().join("detached");
        let locked = tmp.path().join("locked");
        external(&root, &nested, &["-b", "agent-1"]);
        external(&root, &sibling, &[]);
        git::run(&root, &["branch", "other", "main"]).unwrap();
        // Path before the branch: `git worktree add <path> <branch>`.
        let e = elsewhere.to_str().unwrap();
        git::run(&root, &["worktree", "add", "-q", e, "other"]).unwrap();
        external(&root, &detached, &["--detach"]);
        external(&root, &locked, &["--lock", "-b", "held"]);
        // The sibling was added with no branch argument: git named one after the folder.
        for wt in [&nested, &sibling, &elsewhere, &detached, &locked] {
            let got = project_root(wt).unwrap();
            assert!(
                same_dir(&got, &root),
                "{} -> {}",
                wt.display(),
                got.display()
            );
            assert!(!listed(&root, wt).prunable);
        }
        assert_eq!(listed(&root, &detached).branch, None);
        assert!(listed(&root, &locked).locked);
        assert!(!listed(&root, &nested).locked);
        for wt in [&nested, &sibling, &elsewhere, &detached] {
            remove_worktree(&root, wt, false).unwrap();
            assert!(!wt.exists());
        }
        assert!(remove_worktree(&root, &locked, false).is_err(), "locked");
    }

    #[test]
    fn project_root_of_old_layout() {
        let (_tmp, root) = cloned();
        assert!(same_dir(&project_root(&root.join("main")).unwrap(), &root));
        assert!(same_dir(&project_root(&root).unwrap(), &root));
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        assert_eq!(p, root.join("feature-new"), "inside the root, as before");
    }

    #[test]
    fn repair_finds_a_worktree_renamed_beside_the_project() {
        let (_tmp, root) = normal();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        let renamed = p.with_file_name("renamed");
        std::fs::rename(&p, &renamed).unwrap();
        assert!(listed(&root, &p).prunable);
        repair(&root).unwrap();
        let wt = listed(&root, &renamed);
        assert!(!wt.prunable);
        assert_eq!(wt.branch.as_deref(), Some("feature/new"));
        git::run(&renamed, &["status"]).unwrap();
    }

    #[test]
    fn repair_after_the_project_folder_moves() {
        let (_tmp, root) = normal();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        let moved = root.with_file_name("moved");
        std::fs::rename(&root, &moved).unwrap();
        assert!(git::run(&p, &["status"]).is_err(), "link broken");
        repair(&moved).unwrap();
        git::run(&p, &["status"]).unwrap();
        assert!(same_dir(&project_root(&p).unwrap(), &moved));
    }

    #[test]
    fn a_worktree_moved_elsewhere_is_missing_until_located() {
        let (tmp, root) = normal();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        let away = tmp.path().join("away");
        std::fs::rename(&p, &away).unwrap();
        repair(&root).unwrap();
        assert!(
            listed(&root, &p).prunable,
            "repair can't know where it went"
        );
        relink(&root, std::slice::from_ref(&away)).unwrap();
        assert!(!listed(&root, &away).prunable);
        git::run(&away, &["status"]).unwrap();
    }

    #[test]
    fn remove_a_missing_worktree() {
        let (tmp, root) = normal();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        std::fs::rename(&p, tmp.path().join("gone")).unwrap();
        remove_worktree(&root, &p, false).unwrap();
        assert!(list_worktrees(&root)
            .unwrap()
            .iter()
            .all(|w| !same_dir(&w.path, &p)));
    }

    #[test]
    fn dir_name_flattens() {
        assert_eq!(worktree_dir_name("feature/login-fix"), "feature-login-fix");
    }

    #[test]
    fn clone_creates_layout() {
        let (_tmp, root) = cloned();
        assert_eq!(
            std::fs::read_to_string(root.join(".git")).unwrap(),
            "gitdir: ./.bare\n"
        );
        assert!(root.join("main").is_dir());
        let main = root.join("main");
        let up = git::run(&main, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap();
        assert_eq!(up.trim(), "origin/main");
        assert_eq!(detect(&root).unwrap(), Layout::WorktreeRoot);
        let email = git::run(&main, &["config", "user.email"]).unwrap();
        assert_eq!(email.trim(), space().author_email);
        let heads = git::run(
            &root,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
        );
        assert_eq!(heads.unwrap(), "refs/heads/main\n");
    }

    #[test]
    fn normal_clone_has_upstream_identity_and_helper() {
        let (_tmp, url, parent) = fixture();
        let root = clone_into(&url, &parent, "proj", &space(), Layout::NormalClone).unwrap();
        assert_eq!(detect(&root).unwrap(), Layout::NormalClone);
        assert!(!root.join(".bare").exists());
        let up = git::run(&root, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap();
        assert_eq!(up.trim(), "origin/main");
        let email = git::run(&root, &["config", "--local", "user.email"]).unwrap();
        assert_eq!(email.trim(), space().author_email);
        let helpers = git::run(
            &root,
            &["config", "--local", "--get-all", "credential.helper"],
        );
        assert_eq!(
            helpers.unwrap().lines().next(),
            Some(""),
            "resets inherited helpers"
        );
    }

    #[test]
    fn normal_clone_failure_cleans_up() {
        let (tmp, _url, parent) = fixture();
        let bad = format!("file://{}", tmp.path().join("missing").display());
        assert!(clone_into(&bad, &parent, "proj", &space(), Layout::NormalClone).is_err());
        assert!(!parent.join("proj").exists());
    }

    #[test]
    fn clone_refuses_existing_dir() {
        let (_tmp, url, parent) = fixture();
        std::fs::create_dir(parent.join("proj")).unwrap();
        assert!(clone_into(&url, &parent, "proj", &space(), Layout::WorktreeRoot).is_err());
    }

    #[test]
    fn clone_failure_cleans_up() {
        let (tmp, _url, parent) = fixture();
        let bad = format!("file://{}", tmp.path().join("missing").display());
        assert!(clone_into(&bad, &parent, "proj", &space(), Layout::WorktreeRoot).is_err());
        assert!(!parent.join("proj").exists());
    }

    #[test]
    fn add_worktree_and_list() {
        let (_tmp, root) = cloned();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        assert_eq!(p, root.join("feature-new"));
        let list = list_worktrees(&root).unwrap();
        let want = p.canonicalize().unwrap();
        let wt = list.iter().find(|w| w.path == want).expect("listed");
        assert_eq!(wt.branch.as_deref(), Some("feature/new"));
        assert!(list.iter().any(|w| w.is_bare));
    }

    #[test]
    fn add_worktree_at_commit() {
        let (_tmp, root) = cloned();
        let main = root.join("main");
        let sha = git::run(&main, &["rev-parse", "origin/feature/x"]).unwrap();
        let sha = sha.trim();
        let p = add_worktree(&root, "topic", sha).unwrap();
        assert_eq!(git::run(&p, &["rev-parse", "HEAD"]).unwrap().trim(), sha);
        let branch = git::run(&p, &["symbolic-ref", "--short", "HEAD"]).unwrap();
        assert_eq!(branch.trim(), "topic");
        assert!(p.join("b.txt").exists());
    }

    #[test]
    fn add_existing_tracks_origin_only_branch() {
        let (tmp, root) = cloned();
        let remote = tmp.path().join("remote");
        let g = |args: &[&str]| git::run(&remote, args).unwrap();
        g(&["checkout", "-q", "feature/x"]);
        std::fs::write(remote.join("c.txt"), "c\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "advance"]);
        let tip = g(&["rev-parse", "HEAD"]);
        git::run(&root, &["fetch", "-q", "--all", "--prune"]).unwrap();

        let p = add_worktree_existing(&root, "feature/x").unwrap();
        let up = git::run(&p, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap();
        assert_eq!(up.trim(), "origin/feature/x");
        assert_eq!(git::run(&p, &["rev-parse", "HEAD"]).unwrap(), tip);
        assert!(p.join("c.txt").exists());
    }

    #[test]
    fn add_worktree_collision_refuses() {
        let (_tmp, root) = cloned();
        let dir = root.join("feature-a-b");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "keep").unwrap();
        assert!(add_worktree(&root, "feature/a-b", "main").is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("keep.txt")).unwrap(),
            "keep"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn remove_dirty_refuses_without_force() {
        let (_tmp, root) = cloned();
        let p = add_worktree(&root, "feature/new", "main").unwrap();
        std::fs::write(p.join("dirty.txt"), "x").unwrap();
        let err = remove_worktree(&root, &p, false).unwrap_err();
        assert!(err.to_string().contains("uncommitted"), "{err}");
        assert!(p.exists());
        remove_worktree(&root, &p, true).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn create_new_makes_empty_normal_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = create_new(tmp.path(), "fresh", "feature/x", &space()).unwrap();
        assert_eq!(root, tmp.path().join("fresh"));
        assert!(root.join(".git").is_dir());
        let wt = root.clone();
        let head = git::run(&wt, &["symbolic-ref", "--short", "HEAD"]).unwrap();
        assert_eq!(head.trim(), "feature/x");
        assert_eq!(detect(&root).unwrap(), Layout::NormalClone);
        let st = crate::status::status(&wt).unwrap();
        assert_eq!(st, crate::status::Status::default());
        let email = git::run(&wt, &["config", "user.email"]).unwrap();
        assert_eq!(email.trim(), space().author_email);
        assert!(
            git::run(&root, &["remote"]).unwrap().is_empty(),
            "no remote"
        );
        // What opening the tab runs next must cope with an unborn branch.
        git::run(
            &root.join(".git"),
            &["commit-graph", "write", "--reachable"],
        )
        .unwrap();
        let mut log = crate::graph::LogStream::open(&wt).unwrap();
        assert!(log.next_page(10).unwrap().is_empty());
        crate::refs::branches(&wt).unwrap();
    }

    #[test]
    fn create_new_refuses_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("fresh")).unwrap();
        std::fs::write(tmp.path().join("fresh/keep.txt"), "keep").unwrap();
        assert!(create_new(tmp.path(), "fresh", "main", &space()).is_err());
        assert_eq!(
            std::fs::read_dir(tmp.path().join("fresh")).unwrap().count(),
            1
        );
    }

    #[test]
    fn create_new_bad_branch_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(create_new(tmp.path(), "fresh", "a..b", &space()).is_err());
        assert!(!tmp.path().join("fresh").exists());
    }

    #[test]
    fn branch_names_checked_by_git() {
        assert!(check_branch_name("main").is_ok());
        assert!(check_branch_name("feature/x").is_ok());
        for bad in ["", "a..b", "-x", "a b", "x.lock"] {
            assert!(check_branch_name(bad).is_err(), "{bad:?}");
        }
    }

    /// Parent with a marker file; every bad name must leave both untouched.
    fn guarded_parent() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("marker.txt"), "keep").unwrap();
        (tmp, parent)
    }

    fn assert_intact(parent: &Path) {
        assert_eq!(
            std::fs::read_to_string(parent.join("marker.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn create_new_rejects_escaping_names() {
        let (tmp, parent) = guarded_parent();
        let abs = tmp.path().join("abs").display().to_string();
        for bad in ["a/..", "..", ".", "", "a/b", abs.as_str()] {
            for branch in ["main", "a..b"] {
                assert!(
                    create_new(&parent, bad, branch, &space()).is_err(),
                    "{bad:?}"
                );
                assert_intact(&parent);
            }
        }
        assert!(!parent.join("a").exists());
        assert!(!tmp.path().join("abs").exists());
    }

    #[test]
    fn clone_rejects_escaping_names() {
        let (tmp, url, parent) = fixture();
        std::fs::write(parent.join("marker.txt"), "keep").unwrap();
        let bad_url = format!("file://{}", tmp.path().join("missing").display());
        let abs = tmp.path().join("abs").display().to_string();
        for bad in ["a/..", "..", abs.as_str()] {
            assert!(
                clone_into(&bad_url, &parent, bad, &space(), Layout::WorktreeRoot).is_err(),
                "{bad:?}"
            );
            assert!(
                clone_into(&url, &parent, bad, &space(), Layout::WorktreeRoot).is_err(),
                "{bad:?}"
            );
            assert_intact(&parent);
        }
        assert!(!tmp.path().join("abs").exists());
    }

    #[test]
    fn dangling_symlink_counts_as_existing() {
        let (tmp, url, parent) = fixture();
        let link = parent.join("proj");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();
        assert!(create_new(&parent, "proj", "main", &space()).is_err());
        assert!(clone_into(&url, &parent, "proj", &space(), Layout::WorktreeRoot).is_err());
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "symlink untouched");
        assert!(!tmp.path().join("nowhere").exists());
    }
}
