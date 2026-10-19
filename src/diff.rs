#![allow(dead_code)]

use crate::git;
use anyhow::Result;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
    pub no_newline: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub binary: bool,
    /// The diff was not valid UTF-8, so `text` fields are lossy and must not be written back.
    pub lossy: bool,
    pub hunks: Vec<Hunk>,
}

/// Parse `@@ -a[,b] +c[,d] @@ ...` into (old_start, old_len, new_start, new_len).
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((s, l)) => Some((s.parse().ok()?, l.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (os, ol) = range(old)?;
    let (ns, nl) = range(new)?;
    Some((os, ol, ns, nl))
}

/// Path from a `diff --git a/X b/Y` header (the `b/` side).
// ponytail: quoted paths (tabs, quotes in names) are not unescaped.
fn header_path(rest: &str) -> String {
    let half = rest.len() / 2;
    if rest.len() % 2 == 1 {
        if let (Some(a), Some(b)) = (rest.get(..half), rest.get(half + 1..)) {
            if let (Some(a), Some(b)) = (a.strip_prefix("a/"), b.strip_prefix("b/")) {
                if a == b {
                    return b.to_string();
                }
            }
        }
    }
    match rest.rfind(" b/") {
        Some(i) => rest[i + 3..].to_string(),
        None => rest.to_string(),
    }
}

pub fn parse(unified: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // Remaining old/new line counts of the open hunk; a hunk ends when both reach zero.
    let (mut rem_old, mut rem_new) = (0u32, 0u32);
    let (mut old_no, mut new_no) = (0u32, 0u32);
    for raw in unified.split('\n') {
        if let Some(file) = files.last_mut() {
            if raw.starts_with('\\') {
                if let Some(l) = file.hunks.last_mut().and_then(|h| h.lines.last_mut()) {
                    l.no_newline = true;
                }
                continue;
            }
            if rem_old > 0 || rem_new > 0 {
                let (kind, text) = match raw.as_bytes().first() {
                    Some(b'+') => (LineKind::Add, &raw[1..]),
                    Some(b'-') => (LineKind::Del, &raw[1..]),
                    Some(b' ') => (LineKind::Context, &raw[1..]),
                    _ => (LineKind::Context, raw), // tolerate a stripped blank context line
                };
                let (o, n) = match kind {
                    LineKind::Context => (Some(old_no), Some(new_no)),
                    LineKind::Del => (Some(old_no), None),
                    LineKind::Add => (None, Some(new_no)),
                };
                if o.is_some() {
                    old_no += 1;
                    rem_old = rem_old.saturating_sub(1);
                }
                if n.is_some() {
                    new_no += 1;
                    rem_new = rem_new.saturating_sub(1);
                }
                let hunk = file.hunks.last_mut().expect("open hunk");
                hunk.lines.push(Line {
                    kind,
                    old_no: o,
                    new_no: n,
                    text: text.to_string(),
                    no_newline: false,
                });
                continue;
            }
        }
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: header_path(rest),
                binary: false,
                lossy: false,
                hunks: Vec::new(),
            });
        } else if let Some(file) = files.last_mut() {
            if let Some((os, ol, ns, nl)) = parse_hunk_header(raw) {
                (old_no, new_no, rem_old, rem_new) = (os, ns, ol, nl);
                file.hunks.push(Hunk {
                    old_start: os,
                    old_len: ol,
                    new_start: ns,
                    new_len: nl,
                    header: raw.to_string(),
                    lines: Vec::new(),
                });
            } else if raw.starts_with("Binary files ") || raw == "GIT binary patch" {
                file.binary = true;
            } else if let Some(p) = raw.strip_prefix("rename to ") {
                file.path = p.to_string();
            }
        }
    }
    files
}

fn empty(path: &str) -> FileDiff {
    FileDiff {
        path: path.to_string(),
        binary: false,
        lossy: false,
        hunks: Vec::new(),
    }
}

pub fn file_diff(worktree: &Path, path: &str, staged: bool) -> Result<FileDiff> {
    let base = [
        "--literal-pathspecs",
        "-c",
        "core.quotepath=off",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-U3",
    ];
    let untracked = !staged
        && !git::run(
            worktree,
            &[
                "--literal-pathspecs",
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                path,
            ],
        )?
        .is_empty();
    let out = if untracked {
        // --no-index exits 1 when the files differ.
        let mut args = base.to_vec();
        args.extend(["--no-index", "--", "/dev/null", path]);
        git::run_bytes(worktree, &args, &[0, 1])?
    } else {
        let mut args = base.to_vec();
        if staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        git::run_bytes(worktree, &args, &[0])?
    };
    let lossy = std::str::from_utf8(&out).is_err();
    let text = String::from_utf8_lossy(&out);
    Ok(parse(&text)
        .into_iter()
        .next()
        .map(|mut f| {
            f.path = path.to_string();
            f.lossy = lossy;
            f
        })
        .unwrap_or_else(|| empty(path)))
}

pub fn commit_diff(worktree: &Path, sha: &str) -> Result<Vec<FileDiff>> {
    let out = git::run(
        worktree,
        &[
            "-c",
            "core.quotepath=off",
            "show",
            "--format=",
            // A merge shows `diff --cc` by default, which `parse` does not read.
            "--diff-merges=first-parent",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-U3",
            sha,
        ],
    )?;
    Ok(parse(&out))
}

/// Row pairs of indices into `h.lines`; a run of Dels then Adds is paired row by row.
pub fn side_by_side(h: &Hunk) -> Vec<(Option<usize>, Option<usize>)> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < h.lines.len() {
        if h.lines[i].kind == LineKind::Context {
            rows.push((Some(i), Some(i)));
            i += 1;
            continue;
        }
        let start = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Del {
            i += 1;
        }
        let mid = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Add {
            i += 1;
        }
        for k in 0..(mid - start).max(i - mid) {
            rows.push((
                (start + k < mid).then_some(start + k),
                (mid + k < i).then_some(mid + k),
            ));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = "diff --git a/f.txt b/f.txt\nindex 1..2 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -2,4 +2,5 @@ fn main() {\n a\n-b\n+B\n+B2\n c\n d\n";

    #[test]
    fn parse_basic_hunk() {
        let d = parse(BASIC);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "f.txt");
        assert!(!d[0].binary);
        let h = &d[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (2, 4, 2, 5)
        );
        assert_eq!(h.header, "@@ -2,4 +2,5 @@ fn main() {");
        let nos: Vec<_> = h
            .lines
            .iter()
            .map(|l| (l.kind, l.old_no, l.new_no))
            .collect();
        use LineKind::*;
        assert_eq!(
            nos,
            vec![
                (Context, Some(2), Some(2)),
                (Del, Some(3), None),
                (Add, None, Some(3)),
                (Add, None, Some(4)),
                (Context, Some(4), Some(5)),
                (Context, Some(5), Some(6)),
            ]
        );
        assert_eq!(h.lines[1].text, "b");
    }

    #[test]
    fn parse_no_newline_marker() {
        let s = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let h = &parse(s)[0].hunks[0];
        assert_eq!((h.old_len, h.new_len), (1, 1));
        assert!(h.lines[0].no_newline);
        assert!(!h.lines[1].no_newline);
        assert_eq!(h.lines.len(), 2);
    }

    #[test]
    fn parse_non_ascii_rename_header_does_not_panic() {
        let s = "diff --git a/xx é b/y\nsimilarity index 100%\nrename from xx é\nrename to y\n";
        let d = parse(s);
        assert_eq!(d[0].path, "y");
    }

    #[test]
    fn file_diff_ignores_user_diff_config_and_globs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::run(d, &["init", "-q"]).unwrap();
        git::run(d, &["config", "diff.noprefix", "true"]).unwrap();
        for f in ["[id].tsx", "i.tsx"] {
            std::fs::write(d.join(f), "a\n").unwrap();
        }
        git::run(d, &["add", "."]).unwrap();
        git::run(
            d,
            &[
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "i",
            ],
        )
        .unwrap();
        for f in ["[id].tsx", "i.tsx"] {
            std::fs::write(d.join(f), "b\n").unwrap();
        }
        let fd = file_diff(d, "[id].tsx", false).unwrap();
        assert_eq!(fd.path, "[id].tsx");
        assert_eq!(fd.hunks.len(), 1);
        assert_eq!(fd.hunks[0].lines.len(), 2);
        let all = parse(&git::run(d, &["diff", "--src-prefix=a/", "--dst-prefix=b/"]).unwrap());
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn commit_diff_shows_merge_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let g = |args: &[&str]| git::run(d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        g(&["checkout", "-q", "-b", "side"]);
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "side"]);
        g(&["checkout", "-q", "main"]);
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        g(&["commit", "-q", "-am", "main"]);
        g(&["merge", "-q", "--no-edit", "side"]);
        let files = commit_diff(d, "HEAD").unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["b.txt"]);
        assert_eq!(files[0].hunks.len(), 1);
    }

    #[test]
    fn parse_binary() {
        let s = "diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\ndiff --git a/g b/g\n--- a/g\n+++ b/g\n@@ -1 +1 @@\n-x\n+y\n";
        let d = parse(s);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, "img.png");
        assert!(d[0].binary && d[0].hunks.is_empty());
        assert_eq!(d[1].hunks.len(), 1);
    }

    #[test]
    fn parse_deleted_line_that_looks_like_header() {
        let s = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1 @@\n--- x\n keep\n";
        let h = &parse(s)[0].hunks[0];
        assert_eq!(h.lines[0].text, "-- x");
        assert_eq!(h.lines[0].kind, LineKind::Del);
    }

    #[test]
    fn side_by_side_pairs_runs() {
        let l = |kind| Line {
            kind,
            old_no: None,
            new_no: None,
            text: String::new(),
            no_newline: false,
        };
        use LineKind::*;
        let h = Hunk {
            old_start: 1,
            old_len: 4,
            new_start: 1,
            new_len: 3,
            header: String::new(),
            lines: vec![l(Context), l(Del), l(Del), l(Add), l(Context)],
        };
        assert_eq!(
            side_by_side(&h),
            vec![
                (Some(0), Some(0)),
                (Some(1), Some(3)),
                (Some(2), None),
                (Some(4), Some(4))
            ]
        );
    }
}
