//! The Worktrees view's data: for any worktree (Spinney's, an agent's), what it changed since
//! it branched from the default branch, committed or not.

use crate::diff::{self, FileDiff};
use crate::git;
use anyhow::Result;
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

/// Where a changed file's change lives, relative to the worktree's HEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Only in commits since the branch point.
    Committed,
    /// Only in the working tree or index.
    Uncommitted,
    /// Committed, then edited again.
    Both,
    /// A new file git doesn't track yet.
    Untracked,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Committed => "committed",
            Origin::Uncommitted => "uncommitted",
            Origin::Both => "committed + uncommitted",
            Origin::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub path: String,
    pub origin: Origin,
    /// Lines added and removed; None for a binary file.
    pub lines: Option<(u32, u32)>,
}

/// The default branch to compare against: `origin/HEAD` when set, else local `main`, else
/// `master`.
pub fn default_branch(dir: &Path) -> Option<String> {
    if let Ok(r) = git::run(
        dir,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Some(r.trim().to_string());
    }
    ["main", "master"]
        .into_iter()
        .find(|b| verify(dir, &format!("refs/heads/{b}")))
        .map(str::to_string)
}

fn verify(dir: &Path, rev: &str) -> bool {
    git::run(dir, &["rev-parse", "-q", "--verify", rev]).is_ok()
}

/// What `branch` (None when detached) is compared against: the default branch, or for the
/// default branch itself and detached HEADs, the upstream when there is one, else HEAD (so
/// only uncommitted changes show).
pub fn base_for(worktree: &Path, branch: Option<&str>, default: Option<&str>) -> String {
    let is_default = match (branch, default) {
        (Some(b), Some(d)) => d == b || d.split_once('/').is_some_and(|(_, d)| d == b),
        _ => true,
    };
    match default {
        Some(d) if !is_default => d.to_string(),
        _ if verify(worktree, "@{upstream}") => "@{upstream}".to_string(),
        _ => "HEAD".to_string(),
    }
}

/// Commits HEAD has that `base` lacks, and the reverse.
pub fn ahead_behind(worktree: &Path, base: &str) -> Result<(usize, usize)> {
    let range = format!("HEAD...{base}");
    let out = git::run(worktree, &["rev-list", "--left-right", "--count", &range])?;
    let mut n = out.split_whitespace().map(|n| n.parse().unwrap_or(0));
    Ok((n.next().unwrap_or(0), n.next().unwrap_or(0)))
}

/// Where HEAD branched from `base`.
pub fn merge_base(worktree: &Path, base: &str) -> Result<String> {
    Ok(git::run(worktree, &["merge-base", base, "HEAD"])?
        .trim()
        .to_string())
}

/// `git diff -z --name-only` output as paths.
fn names(worktree: &Path, args: &[&str]) -> Result<Vec<String>> {
    let mut all = vec!["-c", "core.quotepath=off", "diff", "--no-renames", "-z"];
    all.extend_from_slice(args);
    Ok(git::run(worktree, &all)?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

/// `<added>\t<removed>\t<path>` per file (`-` for binary), NUL-terminated.
fn parse_numstat(out: &str) -> Vec<(String, Option<(u32, u32)>)> {
    out.split('\0')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').splitn(3, '\t');
            let (a, r, p) = (f.next()?, f.next()?, f.next()?);
            let lines = a.parse().ok().zip(r.parse().ok());
            Some((p.to_string(), lines))
        })
        .collect()
}

/// Everything the worktree changed since it branched from `base`: committed work, edits not
/// yet committed and untracked files, with line counts.
pub fn changes_since(worktree: &Path, base: &str) -> Result<Vec<Change>> {
    let mb = merge_base(worktree, base)?;
    let committed = names(worktree, &["--name-only", &mb, "HEAD"])?;
    let uncommitted = names(worktree, &["--name-only", "HEAD"])?;
    let stat = git::run(
        worktree,
        &[
            "-c",
            "core.quotepath=off",
            "diff",
            "--no-renames",
            "--numstat",
            "-z",
            &mb,
        ],
    )?;
    let mut changes: Vec<Change> = parse_numstat(&stat)
        .into_iter()
        .map(|(path, lines)| {
            let origin = match (committed.contains(&path), uncommitted.contains(&path)) {
                (true, true) => Origin::Both,
                (false, true) => Origin::Uncommitted,
                _ => Origin::Committed,
            };
            Change {
                path,
                origin,
                lines,
            }
        })
        .collect();
    let untracked = git::run(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    // A trailing `/` is an embedded repo or worktree (an agent's `.claude/worktrees/…`):
    // its own work, not this one's.
    let files = untracked
        .split('\0')
        .filter(|p| !p.is_empty() && !p.ends_with('/'));
    let files: Vec<&str> = files.collect();
    let no_diff = no_diff_attr(worktree, &files)?;
    for path in files {
        let lines = if no_diff.contains(path) {
            None
        } else {
            untracked_lines(worktree, path)
        };
        changes.push(Change {
            path: path.to_string(),
            origin: Origin::Untracked,
            lines,
        });
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// The `paths` whose `diff` attribute is unset (`-diff`, `binary`), which git shows as binary
/// whatever they hold. One `check-attr` for all of them.
fn no_diff_attr(worktree: &Path, paths: &[&str]) -> Result<HashSet<String>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut stdin = Vec::new();
    for p in paths {
        stdin.extend_from_slice(p.as_bytes());
        stdin.push(0);
    }
    let out = git::run_stdin(worktree, &["check-attr", "-z", "--stdin", "diff"], &stdin)?;
    // `<path>\0diff\0<value>\0` per path.
    let fields: Vec<&str> = out.split('\0').collect();
    let (records, _) = fields.as_chunks::<3>();
    Ok(records
        .iter()
        .filter(|[_, _, value]| *value == "unset")
        .map(|[path, _, _]| path.to_string())
        .collect())
}

/// How many bytes git looks at for a NUL to call a file binary.
const BINARY_SNIFF: usize = 8000;
/// Files bigger than this are binary to git (`core.bigFileThreshold`'s default).
const BIG_FILE: u64 = 512 << 20;

/// An untracked file's diff, counted without running git: every line is added. Matches
/// `git diff --no-index /dev/null <path>`: a symlink is one line (its target, never
/// followed), a NUL in the first 8000 bytes or a huge file is binary (None), and a last
/// line without a newline still counts. Only a regular file inside the worktree is read;
/// anything else, or a file that can't be read, has no count.
fn untracked_lines(worktree: &Path, path: &str) -> Option<(u32, u32)> {
    let full = worktree.join(path);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if meta.is_symlink() {
        return Some((1, 0));
    }
    if !meta.is_file() || meta.len() > BIG_FILE || !inside(worktree, &full) {
        return None;
    }
    count_lines(std::fs::File::open(&full).ok()?).map(|n| (n, 0))
}

/// Whether `full`'s folder resolves inside `worktree`, so no symlinked folder leads out.
fn inside(worktree: &Path, full: &Path) -> bool {
    let (Some(dir), Ok(root)) = (full.parent(), worktree.canonicalize()) else {
        return false;
    };
    dir.canonicalize().is_ok_and(|d| d.starts_with(root))
}

/// Lines in `r` as git counts them, or None when it's binary.
fn count_lines(mut r: impl Read) -> Option<u32> {
    let mut buf = [0u8; 64 * 1024];
    let (mut seen, mut lines, mut last) = (0usize, 0u32, b'\n');
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        let chunk = &buf[..n];
        if seen < BINARY_SNIFF && chunk[..n.min(BINARY_SNIFF - seen)].contains(&0) {
            return None;
        }
        seen += n;
        lines += chunk.iter().filter(|&&b| b == b'\n').count() as u32;
        last = chunk[n - 1];
    }
    Some(lines + u32::from(last != b'\n'))
}

/// A commit on a worktree's branch since it branched.
#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    /// "2 hours ago".
    pub when: String,
}

/// The most commits listed for a worktree.
pub const MAX_COMMITS: usize = 200;

/// The commits HEAD made since it branched from `base`, newest first. First parent only, so a
/// merge into the branch is one commit and the list stays a single line.
pub fn commits_since(worktree: &Path, base: &str) -> Result<Vec<Commit>> {
    let range = format!("{}..HEAD", merge_base(worktree, base)?);
    let max = format!("--max-count={MAX_COMMITS}");
    let out = git::run(
        worktree,
        &[
            "log",
            "--first-parent",
            "--no-color",
            &max,
            "--format=%H%x1f%s%x1f%an%x1f%cr%x1e",
            &range,
        ],
    )?;
    Ok(parse_log(&out))
}

/// `%H %s %an %cr` fields split by 0x1f, records ended by 0x1e.
fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').split('\x1f');
            Some(Commit {
                sha: f.next().filter(|s| !s.is_empty())?.to_string(),
                subject: f.next()?.to_string(),
                author: f.next()?.to_string(),
                when: f.next()?.to_string(),
            })
        })
        .collect()
}

/// Lines a file's diff adds and removes; None for a binary file.
pub fn line_counts(fd: &FileDiff) -> Option<(u32, u32)> {
    if fd.binary {
        return None;
    }
    let lines = fd.hunks.iter().flat_map(|h| &h.lines);
    Some(lines.fold((0, 0), |(a, r), l| match l.kind {
        diff::LineKind::Add => (a + 1, r),
        diff::LineKind::Del => (a, r + 1),
        _ => (a, r),
    }))
}

/// One file's change since `base`'s branch point, working tree included.
pub fn file_since(worktree: &Path, base: &str, change: &Change) -> Result<FileDiff> {
    if change.origin == Origin::Untracked {
        return diff::file_diff(worktree, &change.path, false);
    }
    let mb = merge_base(worktree, base)?;
    diff::file_diff_from(worktree, &mb, &change.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn g(dir: &Path, args: &[&str]) -> String {
        git::run(dir, args).unwrap()
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
    }

    /// A clone of a remote on `main` with one commit; returns (tmp, clone).
    fn clone() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        g(&remote, &["init", "-q", "-b", "main"]);
        write(&remote, "a.txt", "one\ntwo\n");
        write(&remote, "b.txt", "b\n");
        g(&remote, &["add", "."]);
        g(&remote, &["commit", "-q", "-m", "init"]);
        let url = format!("file://{}", remote.display());
        g(tmp.path(), &["clone", "-q", &url, "proj"]);
        let root = tmp.path().join("proj");
        (tmp, root)
    }

    fn worktree(root: &Path, path: &Path, args: &[&str]) {
        let p = path.to_str().unwrap();
        let mut all = vec!["worktree", "add", "-q"];
        all.extend_from_slice(args);
        all.push(p);
        g(root, &all);
    }

    #[test]
    fn default_branch_prefers_origin_head_then_main_then_master() {
        let (tmp, root) = clone();
        assert_eq!(default_branch(&root).as_deref(), Some("origin/main"));
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        g(&plain, &["init", "-q", "-b", "master"]);
        g(&plain, &["commit", "-q", "--allow-empty", "-m", "x"]);
        assert_eq!(default_branch(&plain).as_deref(), Some("master"));
        g(&plain, &["branch", "main"]);
        assert_eq!(default_branch(&plain).as_deref(), Some("main"));
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        g(&empty, &["init", "-q", "-b", "trunk"]);
        assert_eq!(default_branch(&empty), None);
    }

    #[test]
    fn base_is_the_default_unless_on_it_or_detached() {
        let (_tmp, root) = clone();
        let d = Some("origin/main");
        assert_eq!(base_for(&root, Some("topic"), d), "origin/main");
        assert_eq!(base_for(&root, Some("main"), d), "@{upstream}");
        g(&root, &["switch", "-q", "--detach"]);
        assert_eq!(base_for(&root, None, d), "HEAD");
        assert_eq!(base_for(&root, Some("topic"), None), "HEAD");
    }

    #[test]
    fn ahead_and_behind_per_worktree() {
        let (tmp, root) = clone();
        let agent = root.join(".claude/worktrees/agent-1");
        worktree(&root, &agent, &["-b", "agent-1"]);
        g(&agent, &["commit", "-q", "--allow-empty", "-m", "a1"]);
        g(&agent, &["commit", "-q", "--allow-empty", "-m", "a2"]);
        let side = tmp.path().join("proj.side");
        worktree(&root, &side, &["-b", "side"]);
        // The default branch moves on after `side` branched.
        g(&root, &["commit", "-q", "--allow-empty", "-m", "m1"]);
        g(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        assert_eq!(ahead_behind(&agent, "origin/main").unwrap(), (2, 1));
        assert_eq!(ahead_behind(&side, "origin/main").unwrap(), (0, 1));
        assert_eq!(ahead_behind(&root, "origin/main").unwrap(), (0, 0));
    }

    #[test]
    fn changes_since_marks_committed_uncommitted_and_untracked() {
        let (tmp, root) = clone();
        let wt = tmp.path().join("proj.worktrees/topic");
        worktree(&root, &wt, &["-b", "topic"]);
        write(&wt, "a.txt", "one\nTWO\nthree\n");
        write(&wt, "c.txt", "c\n");
        g(&wt, &["add", "."]);
        g(&wt, &["commit", "-q", "-m", "work"]);
        write(&wt, "a.txt", "one\nTWO\nthree\nfour\n"); // edited again
        write(&wt, "b.txt", "b\nmore\n"); // edited, not committed
        write(&wt, "new.txt", "n1\nn2\n"); // untracked
        let got = changes_since(&wt, "origin/main").unwrap();
        let want = [
            ("a.txt", Origin::Both, Some((3, 1))),
            ("b.txt", Origin::Uncommitted, Some((1, 0))),
            ("c.txt", Origin::Committed, Some((1, 0))),
            ("new.txt", Origin::Untracked, Some((2, 0))),
        ];
        let want: Vec<Change> = want
            .into_iter()
            .map(|(p, origin, lines)| Change {
                path: p.into(),
                origin,
                lines,
            })
            .collect();
        assert_eq!(got, want);

        let fd = file_since(&wt, "origin/main", &got[0]).unwrap();
        assert_eq!(fd.path, "a.txt");
        let added: Vec<&str> = fd.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == diff::LineKind::Add)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(added, ["TWO", "three", "four"]);
        let fd = file_since(&wt, "origin/main", &got[3]).unwrap();
        assert_eq!(fd.hunks[0].lines.len(), 2, "untracked: all added");
    }

    #[test]
    fn a_detached_worktree_shows_only_uncommitted_changes() {
        let (tmp, root) = clone();
        let wt = tmp.path().join("detached");
        worktree(&root, &wt, &["--detach"]);
        write(&wt, "b.txt", "b\nx\n");
        let base = base_for(&wt, None, Some("origin/main"));
        let got = changes_since(&wt, &base).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].origin, Origin::Uncommitted);
    }

    #[test]
    fn a_nested_agent_worktree_is_not_a_change_of_the_main_one() {
        let (_tmp, root) = clone();
        let agent = root.join(".claude/worktrees/agent-1");
        worktree(&root, &agent, &["-b", "agent-1"]);
        write(&agent, "x.txt", "x\n");
        write(&root, "real.txt", "r\n");
        let got = changes_since(&root, "HEAD").unwrap();
        let paths: Vec<&str> = got.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["real.txt"]);
    }

    #[test]
    fn binary_files_have_no_line_counts() {
        let (_tmp, root) = clone();
        std::fs::write(root.join("bin"), [0u8, 1, 2, 0]).unwrap();
        g(&root, &["add", "bin"]);
        let got = changes_since(&root, "HEAD").unwrap();
        assert_eq!(got[0].path, "bin");
        assert_eq!(got[0].lines, None);
    }

    /// What git itself says for an untracked file, the way `changes_since` used to ask.
    fn git_lines(wt: &Path, path: &str) -> Option<(u32, u32)> {
        let args = [
            "diff",
            "--numstat",
            "-z",
            "--no-index",
            "--",
            "/dev/null",
            path,
        ];
        let out = git::run_ok_codes(wt, &args, &[0, 1]).unwrap();
        parse_numstat(&out).into_iter().next().and_then(|(_, l)| l)
    }

    #[test]
    fn untracked_counts_match_git() {
        let (tmp, root) = clone();
        let outside = tmp.path().join("outside.bin");
        std::fs::write(&outside, [0u8; 4]).unwrap();
        let mut late_nul = "a".repeat(9000).into_bytes();
        late_nul.extend_from_slice(b"\n\0\n");
        type File<'a> = (&'a str, &'a [u8], Option<(u32, u32)>);
        let files: [File; 8] = [
            ("text.txt", b"one\ntwo\n", Some((2, 0))),
            ("empty.txt", b"", Some((0, 0))),
            ("no-newline.txt", b"one\ntwo", Some((2, 0))),
            ("blank-lines.txt", b"\n\n\n", Some((3, 0))),
            ("crlf.txt", b"a\r\nb\r\n", Some((2, 0))),
            ("bin", b"x\0y", None),
            ("late-nul.txt", &late_nul, Some((2, 0))),
            ("marked.txt", b"text, but marked binary\n", None),
        ];
        for (name, bytes, _) in &files {
            std::fs::write(root.join(name), bytes).unwrap();
        }
        write(&root, ".gitattributes", "marked.txt binary\n");
        std::os::unix::fs::symlink("text.txt", root.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out-link")).unwrap();
        std::os::unix::fs::symlink("missing", root.join("dangling")).unwrap();
        let links = [("link", Some((1, 0))), ("out-link", Some((1, 0)))];
        let links = links.into_iter().chain([("dangling", Some((1, 0)))]);

        let got = changes_since(&root, "HEAD").unwrap();
        let want = files.iter().map(|(n, _, l)| (*n, *l)).chain(links);
        for (name, lines) in want {
            let c = got.iter().find(|c| c.path == name).expect(name);
            assert_eq!(c.origin, Origin::Untracked, "{name}");
            assert_eq!(c.lines, lines, "{name}");
            assert_eq!(c.lines, git_lines(&root, name), "{name}: same as git");
        }
    }

    #[test]
    fn untracked_files_are_only_read_inside_the_worktree() {
        let (tmp, root) = clone();
        let away = tmp.path().join("away");
        std::fs::create_dir(&away).unwrap();
        write(&away, "f.txt", "a\nb\nc\n");
        // A path whose folder is a symlink out of the worktree is never read.
        std::os::unix::fs::symlink(&away, root.join("dir")).unwrap();
        assert_eq!(untracked_lines(&root, "dir/f.txt"), None);
        assert_eq!(untracked_lines(&root, "missing.txt"), None);
        std::fs::create_dir(root.join("sub")).unwrap();
        write(&root.join("sub"), "f.txt", "a\n");
        assert_eq!(untracked_lines(&root, "sub/f.txt"), Some((1, 0)));
    }

    #[test]
    fn lines_count_like_git() {
        assert_eq!(count_lines(&b""[..]), Some(0));
        assert_eq!(count_lines(&b"a"[..]), Some(1));
        assert_eq!(count_lines(&b"a\n\nb"[..]), Some(3));
        assert_eq!(count_lines(&b"\0"[..]), None);
        let mut nul_at_edge = vec![b'a'; BINARY_SNIFF - 1];
        nul_at_edge.push(0);
        assert_eq!(count_lines(&nul_at_edge[..]), None);
        let mut nul_past = vec![b'a'; BINARY_SNIFF];
        nul_past.push(0);
        assert_eq!(count_lines(&nul_past[..]), Some(1));
    }

    /// `cargo test changes_since_timing -- --ignored --nocapture`: how long
    /// `changes_since` takes with 300 untracked files.
    #[test]
    #[ignore]
    fn changes_since_timing() {
        let (_tmp, root) = clone();
        for i in 0..300 {
            write(&root, &format!("new{i}.txt"), "one\ntwo\nthree\n");
        }
        let start = std::time::Instant::now();
        let got = changes_since(&root, "HEAD").unwrap();
        eprintln!("changes_since, 300 untracked files: {:?}", start.elapsed());
        assert_eq!(got.len(), 300);
    }

    #[test]
    fn commits_since_lists_the_branch_newest_first() {
        let (tmp, root) = clone();
        let wt = tmp.path().join("proj.worktrees/topic");
        worktree(&root, &wt, &["-b", "topic"]);
        write(&wt, "a.txt", "one\nTWO\n");
        g(&wt, &["commit", "-q", "-am", "first"]);
        write(&wt, "c.txt", "c\nd\n");
        g(&wt, &["add", "."]);
        g(&wt, &["commit", "-q", "-m", "second: with a | and a\ttab"]);
        let got = commits_since(&wt, "origin/main").unwrap();
        let subjects: Vec<&str> = got.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["second: with a | and a\ttab", "first"]);
        assert_eq!(got[0].sha.len(), 40);
        assert!(!got[0].author.is_empty() && !got[0].when.is_empty());
        assert_eq!(
            commits_since(&root, "HEAD").unwrap(),
            [],
            "nothing since HEAD"
        );

        let files = diff::commit_diff(&wt, &got[0].sha).unwrap();
        assert_eq!(files[0].path, "c.txt");
        assert_eq!(line_counts(&files[0]), Some((2, 0)));
        let files = diff::commit_diff(&wt, &got[1].sha).unwrap();
        assert_eq!(line_counts(&files[0]), Some((1, 1)));
    }
}
