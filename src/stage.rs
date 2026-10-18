#![allow(dead_code)]

use crate::diff::{FileDiff, LineKind};
use crate::git;
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Stage,
    Unstage,
}

/// Build a patch from the selected `(hunk_idx, line_idx)` lines of `fd`.
/// Stage takes an index→worktree diff; Unstage takes a HEAD→index diff, applied reversed.
/// `None` when the file is binary or no Add/Del line is selected.
pub fn build_patch(
    fd: &FileDiff,
    selected: &HashSet<(usize, usize)>,
    dir: Direction,
) -> Option<String> {
    if fd.binary {
        return None;
    }
    let mut body = String::new();
    for (hi, h) in fd.hunks.iter().enumerate() {
        let picked = |li: usize| selected.contains(&(hi, li));
        if !h
            .lines
            .iter()
            .enumerate()
            .any(|(li, l)| l.kind != LineKind::Context && picked(li))
        {
            continue;
        }
        // (prefix, text, no_newline) per emitted line.
        let mut out: Vec<(char, &str, bool)> = Vec::new();
        for (li, l) in h.lines.iter().enumerate() {
            let prefix = match (l.kind, picked(li), dir) {
                (LineKind::Context, ..) => ' ',
                (LineKind::Add, true, _) => '+',
                (LineKind::Del, true, _) => '-',
                (LineKind::Add, false, Direction::Stage) => continue,
                (LineKind::Add, false, Direction::Unstage) => ' ',
                (LineKind::Del, false, Direction::Stage) => ' ',
                (LineKind::Del, false, Direction::Unstage) => continue,
            };
            out.push((prefix, &l.text, l.no_newline));
        }
        fix_no_newline(&mut out, dir);
        let old_len = out.iter().filter(|(p, ..)| *p != '+').count();
        let new_len = out.iter().filter(|(p, ..)| *p != '-').count();
        body.push_str(&format!(
            "@@ -{},{old_len} +{},{new_len} @@\n",
            h.old_start, h.new_start
        ));
        for (prefix, text, no_newline) in out {
            body.push(prefix);
            body.push_str(text);
            body.push('\n');
            if no_newline {
                body.push_str("\\ No newline at end of file\n");
            }
        }
    }
    if body.is_empty() {
        return None;
    }
    let p = &fd.path;
    Some(format!(
        "diff --git a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n{body}"
    ))
}

/// The side the patch matches against (old for Stage, new for Unstage) keeps the diff's
/// markers. The side being built (new for Stage, old for Unstage) may only carry the
/// marker on its last line: a marked line with more built-side lines after it gains a
/// newline, and a marked context line splits so only the matched side keeps the marker.
fn fix_no_newline(out: &mut Vec<(char, &str, bool)>, dir: Direction) {
    let (built, matched) = match dir {
        Direction::Stage => ('+', '-'),
        Direction::Unstage => ('-', '+'),
    };
    let Some(mut last) = out.iter().rposition(|(p, ..)| *p != matched) else {
        return;
    };
    let mut i = 0;
    while i < last {
        let (prefix, text, no_newline) = out[i];
        if no_newline && prefix != matched {
            if prefix == ' ' {
                // '-' before '+' either way; only the matched side keeps the marker.
                out[i] = ('-', text, matched == '-');
                out.insert(i + 1, ('+', text, matched == '+'));
                i += 1;
                last += 1;
            } else {
                debug_assert_eq!(prefix, built);
                out[i].2 = false;
            }
        }
        i += 1;
    }
}

pub fn apply(worktree: &Path, patch: &str, dir: Direction) -> Result<()> {
    let mut args = vec!["apply", "--cached", "--recount", "--whitespace=nowarn"];
    if dir == Direction::Unstage {
        args.push("--reverse");
    }
    args.push("-");
    git::run_stdin(worktree, &args, patch.as_bytes())?;
    Ok(())
}

pub fn stage_file(worktree: &Path, path: &str) -> Result<()> {
    git::run(worktree, &["--literal-pathspecs", "add", "--", path])?;
    Ok(())
}

pub fn unstage_file(worktree: &Path, path: &str) -> Result<()> {
    git::run(
        worktree,
        &["--literal-pathspecs", "restore", "--staged", "--", path],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::file_diff;
    use std::fs;

    const BASE: &str = "a\nb\nc\nd\ne";
    const WORK: &str = "a\nB\nc\nD\ne\nf";

    fn g(d: &Path, args: &[&str]) -> String {
        git::run(d, args).unwrap()
    }

    /// Temp repo with `name` committed as `base` and the worktree holding `work`.
    fn repo(name: &str, base: &str, work: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        g(d, &["init", "-q"]);
        fs::write(d.join(name), base).unwrap();
        g(d, &["add", "."]);
        g(
            d,
            &[
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        fs::write(d.join(name), work).unwrap();
        tmp
    }

    /// Indices of the lines in `fd` matching each (kind, text).
    fn pick(fd: &FileDiff, want: &[(LineKind, &str)]) -> HashSet<(usize, usize)> {
        let mut sel = HashSet::new();
        for (hi, h) in fd.hunks.iter().enumerate() {
            for (li, l) in h.lines.iter().enumerate() {
                if want.contains(&(l.kind, l.text.as_str())) {
                    sel.insert((hi, li));
                }
            }
        }
        assert_eq!(sel.len(), want.len(), "{fd:?}");
        sel
    }

    fn whole_hunk(fd: &FileDiff, hi: usize) -> HashSet<(usize, usize)> {
        (0..fd.hunks[hi].lines.len()).map(|li| (hi, li)).collect()
    }

    fn stage(d: &Path, path: &str, sel: &HashSet<(usize, usize)>, dir: Direction) {
        let fd = file_diff(d, path, dir == Direction::Unstage).unwrap();
        let patch = build_patch(&fd, sel, dir).expect("patch");
        apply(d, &patch, dir).unwrap();
    }

    fn index(d: &Path, path: &str) -> String {
        g(d, &["show", &format!(":{path}")])
    }

    use LineKind::{Add, Del};

    #[test]
    fn stage_one_line() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        let fd = file_diff(d, "f.txt", false).unwrap();
        let sel = pick(&fd, &[(Del, "b"), (Add, "B")]);
        stage(d, "f.txt", &sel, Direction::Stage);
        assert_eq!(index(d, "f.txt"), "a\nB\nc\nd\ne");
        let rest = g(d, &["diff"]);
        assert!(rest.contains("-d\n") && rest.contains("+D\n"), "{rest}");
        assert!(rest.contains("+f\n"), "{rest}");
        assert!(!rest.contains("-b\n"), "{rest}");
    }

    #[test]
    fn unstage_one_line() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        g(d, &["add", "f.txt"]);
        let fd = file_diff(d, "f.txt", true).unwrap();
        let sel = pick(&fd, &[(Del, "d"), (Add, "D")]);
        stage(d, "f.txt", &sel, Direction::Unstage);
        assert_eq!(index(d, "f.txt"), "a\nB\nc\nd\ne\nf");
    }

    #[test]
    fn stage_last_line_without_newline() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        let fd = file_diff(d, "f.txt", false).unwrap();
        let sel = pick(&fd, &[(Add, "f")]);
        stage(d, "f.txt", &sel, Direction::Stage);
        assert_eq!(index(d, "f.txt"), "a\nb\nc\nd\ne\nf");
        let rest = g(d, &["diff"]);
        assert!(rest.contains("-b\n+B\n"), "{rest}");
        assert!(rest.contains("-d\n+D\n"), "{rest}");
        assert!(!rest.contains("+f"), "{rest}");
    }

    #[test]
    fn unstage_last_line_without_newline() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        g(d, &["add", "f.txt"]);
        let fd = file_diff(d, "f.txt", true).unwrap();
        let sel = pick(&fd, &[(Add, "f")]);
        stage(d, "f.txt", &sel, Direction::Unstage);
        // The `e` newline change stays staged; only `f` leaves the index.
        assert_eq!(index(d, "f.txt"), "a\nB\nc\nD\ne\n");
    }

    #[test]
    fn unstage_marked_del_before_more_lines() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        g(d, &["add", "f.txt"]);
        let fd = file_diff(d, "f.txt", true).unwrap();
        // `-e` carries the marker but reverse-applying it puts lines after it.
        let sel = pick(&fd, &[(Del, "e")]);
        stage(d, "f.txt", &sel, Direction::Unstage);
        assert_eq!(index(d, "f.txt"), "a\nB\nc\ne\nD\ne\nf");
    }

    #[test]
    fn fix_no_newline_checks_lines_after_a_split() {
        let mut out = vec![(' ', "e", true), ('+', "x", true), ('+', "y", false)];
        fix_no_newline(&mut out, Direction::Stage);
        assert_eq!(
            out,
            vec![
                ('-', "e", true),
                ('+', "e", false),
                ('+', "x", false),
                ('+', "y", false)
            ]
        );
    }

    const LONG_BASE: &str =
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
    const LONG_WORK: &str =
        "1\nTWO\nnew\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\nEIGHTEEN\n19\n20\n";

    #[test]
    fn stage_hunk() {
        let tmp = repo("g.txt", LONG_BASE, LONG_WORK);
        let d = tmp.path();
        let fd = file_diff(d, "g.txt", false).unwrap();
        assert_eq!(fd.hunks.len(), 2);
        stage(d, "g.txt", &whole_hunk(&fd, 0), Direction::Stage);
        assert_eq!(
            index(d, "g.txt"),
            LONG_BASE.replacen("2\n", "TWO\nnew\n", 1)
        );
    }

    #[test]
    fn stage_second_hunk_only() {
        let tmp = repo("g.txt", LONG_BASE, LONG_WORK);
        let d = tmp.path();
        let fd = file_diff(d, "g.txt", false).unwrap();
        stage(d, "g.txt", &whole_hunk(&fd, 1), Direction::Stage);
        assert_eq!(index(d, "g.txt"), LONG_BASE.replace("18\n", "EIGHTEEN\n"));
    }

    #[test]
    fn unstage_second_hunk_only() {
        let tmp = repo("g.txt", LONG_BASE, LONG_WORK);
        let d = tmp.path();
        g(d, &["add", "g.txt"]);
        let fd = file_diff(d, "g.txt", true).unwrap();
        stage(d, "g.txt", &whole_hunk(&fd, 1), Direction::Unstage);
        assert_eq!(index(d, "g.txt"), LONG_WORK.replace("EIGHTEEN\n", "18\n"));
    }

    #[test]
    fn nothing_selected_none() {
        let tmp = repo("f.txt", BASE, WORK);
        let fd = file_diff(tmp.path(), "f.txt", false).unwrap();
        assert_eq!(build_patch(&fd, &HashSet::new(), Direction::Stage), None);
        // Context-only and out-of-range selections are not applicable either.
        let sel = HashSet::from([(0, 0), (5, 5), (0, 99)]);
        assert_eq!(build_patch(&fd, &sel, Direction::Stage), None);
        let bin = FileDiff {
            binary: true,
            ..fd.clone()
        };
        assert_eq!(
            build_patch(&bin, &whole_hunk(&fd, 0), Direction::Stage),
            None
        );
    }

    #[test]
    fn file_level_stage_and_unstage() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        fs::write(d.join("*.txt"), "glob\n").unwrap();
        stage_file(d, "f.txt").unwrap();
        assert_eq!(index(d, "f.txt"), WORK);
        assert!(g(d, &["ls-files", "--", ":(literal)*.txt"]).is_empty());
        unstage_file(d, "f.txt").unwrap();
        assert_eq!(index(d, "f.txt"), BASE);
    }
}
