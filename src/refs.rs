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
            branch(refname, upstream, track, head)
        })
        .collect())
}

/// A local or remote branch from `for-each-ref` fields; `None` for other refs and `<remote>/HEAD`.
fn branch(refname: &str, upstream: &str, track: &str, head: &str) -> Option<Branch> {
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub name: String,
    /// The commit it names: an annotated tag is peeled to its commit.
    pub target: String,
    pub annotated: bool,
}

/// Parse `for-each-ref` lines of `refname NUL objectname NUL *objectname NUL objecttype`.
fn parse_tags(out: &str) -> Vec<Tag> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            tag(f.next()?, f.next()?, f.next()?, f.next()?)
        })
        .collect()
}

/// A tag from `for-each-ref` fields; `None` for any other ref.
fn tag(refname: &str, object: &str, peeled: &str, kind: &str) -> Option<Tag> {
    // Not `refname:short`: it turns into `tags/x` when a branch is also called x.
    let name = refname.strip_prefix("refs/tags/")?;
    let target = if peeled.is_empty() { object } else { peeled };
    Some(Tag {
        name: name.to_string(),
        target: target.to_string(),
        annotated: kind == "tag",
    })
}

/// Tags, newest first: by tagger date, or the commit date for a lightweight tag.
pub fn tags(dir: &Path) -> Result<Vec<Tag>> {
    let out = git::run(
        dir,
        &[
            "for-each-ref",
            "--sort=-creatordate",
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(objecttype)",
            "refs/tags",
        ],
    )?;
    Ok(parse_tags(&out))
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

/// Every branch and tag tip, which branch the worktree has checked out (`*`) and the commit
/// HEAD is at: what a commit, checkout, reset or fetch made outside Spinney changes. One
/// cheap git call on a branch, so it can be compared on every focus.
pub fn stamp(worktree: &Path) -> Result<String> {
    let tips = git::run(
        worktree,
        &[
            "for-each-ref",
            "--format=%(HEAD)%(refname) %(objectname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    // The checked-out branch's tip is HEAD.
    let on_branch = tips
        .lines()
        .find_map(|l| l.strip_prefix('*')?.rsplit_once(' '))
        .map(|(_, sha)| sha.to_string());
    let head = match on_branch {
        Some(sha) => Some(sha),
        None => off_branch_head(worktree)?.1,
    };
    Ok(stamp_text(head.as_deref(), &tips))
}

/// `stamp` from HEAD's commit and `for-each-ref` lines of `%(HEAD)%(refname) %(objectname)`.
fn stamp_text(head: Option<&str>, tips: &str) -> String {
    format!("HEAD {}\n{tips}", head.unwrap_or(""))
}

/// Where HEAD is when `for-each-ref` marks no branch as checked out: (branch, commit). An
/// unborn branch has a name but no commit, a detached HEAD a commit but no branch.
fn off_branch_head(worktree: &Path) -> Result<(Option<String>, Option<String>)> {
    if let Some(branch) = current_branch(worktree)? {
        return Ok((Some(branch), None));
    }
    let out = git::run_ok_codes(worktree, &["rev-parse", "--verify", "-q", "HEAD"], &[0, 1])?;
    let sha = out.trim();
    Ok((None, (!sha.is_empty()).then(|| sha.to_string())))
}

/// What a tab shows of a worktree's refs, read together.
#[derive(Debug, PartialEq)]
pub struct Snapshot {
    pub branches: Vec<Branch>,
    /// Newest first, as `tags` returns them.
    pub tags: Vec<Tag>,
    /// As `stamp` returns it.
    pub stamp: String,
    /// The commit HEAD is at; `None` on an unborn branch.
    pub head: Option<String>,
    /// The checked-out branch; `None` when HEAD is detached.
    pub current: Option<String>,
}

/// `branches`, `tags`, `stamp`, HEAD and the current branch from one `for-each-ref` (and one
/// or two more calls only when no branch is checked out), instead of six calls. Runs on
/// every refresh: after each commit, checkout, fetch and stash.
pub fn snapshot(worktree: &Path) -> Result<Snapshot> {
    let out = git::run(
        worktree,
        &[
            "for-each-ref",
            "--format=%(HEAD)%00%(refname)%00%(objectname)%00%(*objectname)%00%(objecttype)\
             %00%(upstream:short)%00%(upstream:track,nobracket)%00%(creatordate:unix)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    let rows: Vec<Vec<&str>> = out
        .lines()
        .map(|l| l.split('\0').collect::<Vec<_>>())
        .filter(|f| f.len() == 8)
        .collect();
    let checked_out = rows.iter().find(|f| f[0] == "*");
    let (current, head) = match checked_out {
        Some(f) => (
            f[1].strip_prefix("refs/heads/").map(str::to_string),
            Some(f[2].to_string()),
        ),
        None => off_branch_head(worktree)?,
    };
    let tips: String = rows
        .iter()
        .map(|f| format!("{}{} {}\n", f[0], f[1], f[2]))
        .collect();
    let branches = rows
        .iter()
        .filter_map(|f| branch(f[1], f[5], f[6], f[0]))
        .collect();
    let mut tags: Vec<(i64, Tag)> = rows
        .iter()
        .filter_map(|f| Some((f[7].parse().unwrap_or(0), tag(f[1], f[2], f[3], f[4])?)))
        .collect();
    // Stable, so equal dates keep refname order, as `--sort=-creatordate` does.
    tags.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
    Ok(Snapshot {
        branches,
        tags: tags.into_iter().map(|(_, t)| t).collect(),
        stamp: stamp_text(head.as_deref(), &tips),
        head,
        current,
    })
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
            ssh_key: None,
            ..Default::default()
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

    /// Each of `snapshot`'s fields is what the separate calls return.
    fn assert_snapshot_matches(dir: &Path) {
        let s = snapshot(dir).unwrap();
        assert_eq!(s.branches, branches(dir).unwrap());
        assert_eq!(s.tags, tags(dir).unwrap());
        assert_eq!(s.stamp, stamp(dir).unwrap());
        assert_eq!(s.current, current_branch(dir).unwrap());
        let head = git::run_ok_codes(dir, &["rev-parse", "--verify", "-q", "HEAD"], &[0, 1]);
        let head = head.unwrap().trim().to_string();
        assert_eq!(s.head, (!head.is_empty()).then_some(head.clone()));
        // The stamp as it was built before: tips plus `rev-parse HEAD`.
        let tips = git::run(
            dir,
            &[
                "for-each-ref",
                "--format=%(HEAD)%(refname) %(objectname)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ],
        )
        .unwrap();
        assert_eq!(s.stamp, format!("HEAD {head}\n{tips}"));
    }

    #[test]
    fn snapshot_matches_the_separate_calls() {
        let (_tmp, root) = cloned();
        let g = |args: &[&str]| git::run(&root, args).unwrap();
        // Tags with distinct dates, and two on one commit (equal dates).
        let dated = |date: &str, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&root)
                .env("GIT_COMMITTER_DATE", date)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        dated(
            "2026-01-01T00:00:00Z",
            &["commit", "--allow-empty", "-q", "-m", "a"],
        );
        dated(
            "2026-01-02T00:00:00Z",
            &["tag", "-a", "-m", "annotated", "v2"],
        );
        g(&["tag", "v1"]);
        g(&["tag", "also-v1"]);
        dated(
            "2026-01-03T00:00:00Z",
            &["commit", "--allow-empty", "-q", "-m", "b"],
        );
        g(&["tag", "v3"]);
        g(&["branch", "feature/x", "HEAD~1"]);
        g(&["branch", "--set-upstream-to=origin/main", "feature/x"]);
        assert_snapshot_matches(&root);
        assert_eq!(snapshot(&root).unwrap().current.as_deref(), Some("main"));

        g(&["checkout", "-q", "--detach", "HEAD~1"]);
        assert_snapshot_matches(&root);
        assert_eq!(snapshot(&root).unwrap().current, None);

        g(&["checkout", "-q", "--orphan", "unborn"]);
        assert_snapshot_matches(&root);
        let s = snapshot(&root).unwrap();
        assert_eq!((s.current.as_deref(), s.head), (Some("unborn"), None));
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
        let main = root.clone();
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
        let main = root.clone();
        assert_eq!(current_branch(&main).unwrap().as_deref(), Some("main"));
        git::run(&main, &["checkout", "-q", "--detach"]).unwrap();
        assert_eq!(current_branch(&main).unwrap(), None);
    }

    #[test]
    fn tag_lines_parse() {
        let out = "refs/tags/v2\0aaa\0ccc\0tag\nrefs/tags/release/v1\0bbb\0\0commit\n";
        assert_eq!(
            parse_tags(out),
            [
                Tag {
                    name: "v2".into(),
                    target: "ccc".into(),
                    annotated: true
                },
                Tag {
                    name: "release/v1".into(),
                    target: "bbb".into(),
                    annotated: false
                },
            ]
        );
        assert!(parse_tags("").is_empty());
    }

    #[test]
    fn tags_newest_first_and_peeled() {
        let (_tmp, root) = cloned();
        let main = root.clone();
        let at = |date: &str, args: &[&str]| {
            git::run_env(&main, args, &[("GIT_COMMITTER_DATE", date)]).unwrap();
        };
        let head = || {
            git::run(&main, &["rev-parse", "HEAD"])
                .unwrap()
                .trim()
                .to_string()
        };
        at(
            "2020-01-01T00:00:00Z",
            &["commit", "--allow-empty", "-q", "-m", "a"],
        );
        let a = head();
        at("2020-01-01T00:00:00Z", &["tag", "old"]);
        at(
            "2023-01-01T00:00:00Z",
            &["commit", "--allow-empty", "-q", "-m", "b"],
        );
        let b = head();
        at("2023-01-01T00:00:00Z", &["tag", "new"]);
        // Annotated: dated by the tagger (2021), not by its newer commit.
        at(
            "2021-01-01T00:00:00Z",
            &["tag", "-a", "-m", "mid", "mid", &b],
        );
        let list = tags(&main).unwrap();
        let names: Vec<_> = list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["new", "mid", "old"]);
        assert_eq!(
            (list[1].target.as_str(), list[1].annotated),
            (b.as_str(), true)
        );
        assert_eq!(
            (list[2].target.as_str(), list[2].annotated),
            (a.as_str(), false)
        );
    }

    #[test]
    fn empty_repo_has_no_branches() {
        let tmp = tempfile::tempdir().unwrap();
        git::run(tmp.path(), &["init", "-q", "-b", "main"]).unwrap();
        assert!(branches(tmp.path()).unwrap().is_empty());
        assert!(tags(tmp.path()).unwrap().is_empty());
    }
}
