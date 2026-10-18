#![allow(dead_code)]

use crate::config::Space;
use crate::{creds, git};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

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
}

pub fn detect(root: &Path) -> Result<Layout> {
    let git = root.join(".git");
    if root.join(".bare").is_dir() && git.is_file() {
        Ok(Layout::WorktreeRoot)
    } else if git.is_dir() {
        Ok(Layout::NormalClone)
    } else {
        bail!("{} is not a git project", root.display())
    }
}

/// Clone `url` as `<parent>/<name>/.bare` plus a worktree for the default branch.
pub fn clone_into(url: &str, parent: &Path, name: &str, space: &Space) -> Result<PathBuf> {
    let root = parent.join(name);
    if root.exists() {
        bail!("{} already exists", root.display());
    }
    let result = clone_steps(url, parent, &root, space);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&root); // we created it, so clean it up
    }
    result.map(|()| root)
}

fn clone_steps(url: &str, parent: &Path, root: &Path, space: &Space) -> Result<()> {
    let bare = root.join(".bare");
    let bare_str = bare.to_str().context("non-UTF-8 path")?;
    let env = creds::git_env_args(space)?;
    let with_env = |rest: &[&str]| -> Vec<String> {
        env.iter().cloned().chain(rest.iter().map(|s| s.to_string())).collect()
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
        &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"],
    )?;
    run_env(&bare, &["fetch", "origin"])?;
    let default = git::run(&bare, &["symbolic-ref", "--short", "HEAD"])?
        .trim()
        .to_string();
    let dir = format!("./{}", worktree_dir_name(&default));
    git::run(root, &["worktree", "add", &dir, &default])?;
    git::run(
        root,
        &["branch", &format!("--set-upstream-to=origin/{default}"), &default],
    )?;
    creds::configure_repo(&bare, space)
}

pub fn list_worktrees(root: &Path) -> Result<Vec<Worktree>> {
    let out = git::run(root, &["worktree", "list", "--porcelain"])?;
    Ok(out
        .split("\n\n")
        .filter_map(|block| {
            let mut wt = Worktree {
                path: PathBuf::new(),
                branch: None,
                head: String::new(),
                is_bare: false,
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
                }
            }
            (!wt.path.as_os_str().is_empty()).then_some(wt)
        })
        .collect())
}

/// Target folder for `branch`, refusing before git runs if it already exists.
fn free_target(root: &Path, branch: &str) -> Result<(PathBuf, String)> {
    let dir = worktree_dir_name(branch);
    let target = root.join(&dir);
    if target.exists() {
        bail!("{} already exists", target.display());
    }
    Ok((target, format!("./{dir}")))
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
    if !force && !git::run(path, &["status", "--porcelain"])?.trim().is_empty() {
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
        let root = clone_into(&url, &parent, "proj", &space()).unwrap();
        (tmp, root)
    }

    #[test]
    fn dir_name_flattens() {
        assert_eq!(worktree_dir_name("feature/login-fix"), "feature-login-fix");
    }

    #[test]
    fn clone_creates_layout() {
        let (_tmp, root) = cloned();
        assert_eq!(std::fs::read_to_string(root.join(".git")).unwrap(), "gitdir: ./.bare\n");
        assert!(root.join("main").is_dir());
        let main = root.join("main");
        let up = git::run(&main, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap();
        assert_eq!(up.trim(), "origin/main");
        assert_eq!(detect(&root).unwrap(), Layout::WorktreeRoot);
        let email = git::run(&main, &["config", "user.email"]).unwrap();
        assert_eq!(email.trim(), space().author_email);
    }

    #[test]
    fn clone_refuses_existing_dir() {
        let (_tmp, url, parent) = fixture();
        std::fs::create_dir(parent.join("proj")).unwrap();
        assert!(clone_into(&url, &parent, "proj", &space()).is_err());
    }

    #[test]
    fn clone_failure_cleans_up() {
        let (tmp, _url, parent) = fixture();
        let bad = format!("file://{}", tmp.path().join("missing").display());
        assert!(clone_into(&bad, &parent, "proj", &space()).is_err());
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
    fn add_existing_tracks_origin_only_branch() {
        let (_tmp, root) = cloned();
        let p = add_worktree_existing(&root, "feature/x").unwrap();
        assert!(p.join("b.txt").exists());
    }

    #[test]
    fn add_worktree_collision_refuses() {
        let (_tmp, root) = cloned();
        let dir = root.join("feature-a-b");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "keep").unwrap();
        assert!(add_worktree(&root, "feature/a-b", "main").is_err());
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "keep");
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
}
