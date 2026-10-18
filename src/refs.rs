#![allow(dead_code)]

use crate::git;
use anyhow::Result;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    pub name: String,
    pub remote: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub head: bool,
}

/// Parse `ahead N`, `behind M`, `ahead N, behind M`, `gone` or empty into (ahead, behind).
fn parse_track(track: &str) -> (u32, u32) {
    let (mut ahead, mut behind) = (0, 0);
    for part in track.split(',') {
        let mut words = part.split_whitespace();
        match (words.next(), words.next().and_then(|n| n.parse().ok())) {
            (Some("ahead"), Some(n)) => ahead = n,
            (Some("behind"), Some(n)) => behind = n,
            _ => {}
        }
    }
    (ahead, behind)
}

pub fn branches(dir: &Path) -> Result<Vec<Branch>> {
    let out = git::run(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let (refname, upstream, track, head) = (f.next()?, f.next()?, f.next()?, f.next()?);
            let (name, remote) = match refname.strip_prefix("refs/heads/") {
                Some(n) => (n, false),
                None => (refname.strip_prefix("refs/remotes/")?, true),
            };
            if remote && name.ends_with("/HEAD") {
                return None;
            }
            let (ahead, behind) = parse_track(track);
            Some(Branch {
                name: name.to_string(),
                remote,
                upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
                ahead,
                behind,
                head: head == "*",
            })
        })
        .collect())
}

/// Short name of the checked-out branch, or `None` when HEAD is detached.
pub fn current_branch(worktree: &Path) -> Result<Option<String>> {
    let out = git::run_ok_codes(
        worktree,
        &["symbolic-ref", "--short", "-q", "HEAD"],
        &[0, 1],
    )?;
    let name = out.trim();
    Ok((!name.is_empty()).then(|| name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::clone_into;
    use std::path::PathBuf;

    fn space() -> crate::config::Space {
        crate::config::Space {
            id: "test-space".into(),
            name: "Test".into(),
            host: "github.com".into(),
            author_name: "Test User".into(),
            author_email: "test@example.com".into(),
            default_clone_dir: None,
            projects: vec![],
        }
    }

    fn cloned() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        let g = |args: &[&str]| git::run(&remote, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "--allow-empty", "-q", "-m", "init"]);
        let url = format!("file://{}", remote.display());
        let parent = tmp.path().join("clones");
        std::fs::create_dir(&parent).unwrap();
        let root = clone_into(&url, &parent, "proj", &space()).unwrap();
        (tmp, root)
    }

    #[test]
    fn track_parsing() {
        assert_eq!(parse_track(""), (0, 0));
        assert_eq!(parse_track("gone"), (0, 0));
        assert_eq!(parse_track("ahead 2"), (2, 0));
        assert_eq!(parse_track("behind 3"), (0, 3));
        assert_eq!(parse_track("ahead 2, behind 3"), (2, 3));
    }

    #[test]
    fn branches_ahead_behind() {
        let (_tmp, root) = cloned();
        let main = root.join("main");
        git::run(&main, &["commit", "--allow-empty", "-q", "-m", "local"]).unwrap();
        let list = branches(&main).unwrap();
        let b = list.iter().find(|b| b.name == "main" && !b.remote).unwrap();
        assert_eq!((b.ahead, b.behind, b.head), (1, 0, true));
        assert_eq!(b.upstream.as_deref(), Some("origin/main"));
        let r = list.iter().find(|b| b.name == "origin/main").unwrap();
        assert!(r.remote && !r.head && r.upstream.is_none());
        assert!(list.iter().all(|b| !b.name.ends_with("/HEAD")));
    }

    #[test]
    fn current_branch_detached_none() {
        let (_tmp, root) = cloned();
        let main = root.join("main");
        assert_eq!(current_branch(&main).unwrap().as_deref(), Some("main"));
        git::run(&main, &["checkout", "-q", "--detach"]).unwrap();
        assert_eq!(current_branch(&main).unwrap(), None);
    }

    #[test]
    fn empty_repo_has_no_branches() {
        let tmp = tempfile::tempdir().unwrap();
        git::run(tmp.path(), &["init", "-q", "-b", "main"]).unwrap();
        assert!(branches(tmp.path()).unwrap().is_empty());
    }
}
