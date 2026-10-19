#![allow(dead_code)]

use crate::diff::{FileDiff, Line, LineKind};
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
/// `None` when the file is binary, its diff is not valid UTF-8, or no Add/Del line is selected.
/// Only for Modified files: added, deleted, untracked and mode-changed files are staged
/// at file level (`stage_file` / `unstage_file`), since the patch carries no mode headers.
pub fn build_patch(
    fd: &FileDiff,
    selected: &HashSet<(usize, usize)>,
    dir: Direction,
) -> Option<String> {
    if fd.binary || fd.lossy {
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
        for li in pair_order(&h.lines) {
            let l = &h.lines[li];
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

/// Line indices of `lines` with each change run (all Dels, then all Adds) interleaved as
/// Del i, Add i, then the longer side's leftovers. Each side keeps its own order, and an
/// unselected line turned into context stays beside its partner instead of reordering
/// the result.
fn pair_order(lines: &[Line]) -> Vec<usize> {
    let mut order = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            order.push(i);
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && lines[i].kind != LineKind::Context {
            i += 1;
        }
        let (dels, adds): (Vec<usize>, Vec<usize>) =
            (start..i).partition(|&k| lines[k].kind == LineKind::Del);
        for k in 0..dels.len().max(adds.len()) {
            order.extend(dels.get(k));
            order.extend(adds.get(k));
        }
    }
    order
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

/// Stage exactly `paths` (new, modified or deleted) in one call; nothing when empty.
pub fn stage_paths(worktree: &Path, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(()); // a bare `add -A` would stage everything, conflicts included
    }
    let mut args = vec!["--literal-pathspecs", "add", "-A", "--"];
    args.extend_from_slice(paths);
    git::run(worktree, &args)?;
    Ok(())
}

pub fn unstage_file(worktree: &Path, path: &str) -> Result<()> {
    unstage_files(worktree, &[path])
}

/// Unstage `paths` in one call; nothing when empty.
pub fn unstage_files(worktree: &Path, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    // `restore --staged` needs HEAD; before the first commit `reset` drops the paths from the
    // index (`rm --cached` would refuse once a file was edited after staging).
    let unborn = git::run(worktree, &["rev-parse", "--verify", "-q", "HEAD"]).is_err();
    let mut args = if unborn {
        vec!["--literal-pathspecs", "reset", "-q", "--"]
    } else {
        vec!["--literal-pathspecs", "restore", "--staged", "--"]
    };
    args.extend_from_slice(paths);
    git::run(worktree, &args)?;
    Ok(())
}

/// Throw away unstaged work: `tracked` paths go back to their index content and `untracked`
/// files are deleted. Staged changes stay; nothing happens for an empty slice.
pub fn discard_files(worktree: &Path, tracked: &[&str], untracked: &[&str]) -> Result<()> {
    // Restores from the index, so it also works before the first commit.
    for (cmd, paths) in [
        (["restore", "--worktree"], tracked),
        (["clean", "-fq"], untracked),
    ] {
        if paths.is_empty() {
            continue;
        }
        let mut args = vec!["--literal-pathspecs"];
        args.extend_from_slice(&cmd);
        args.push("--");
        args.extend_from_slice(paths);
        git::run(worktree, &args)?;
    }
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
        // `-e` pairs with `+e`, so the restored `e` lands beside it, after `D`.
        assert_eq!(index(d, "f.txt"), "a\nB\nc\nD\ne\ne\nf");
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

    const PAIR_BASE: &str = "c\nd\ne\nz\n";
    const PAIR_WORK: &str = "c\nD\nE\nz\n";

    /// Apply `want` from the two-pair block `-d -e +D +E` and return the index.
    fn pair_case(want: &[(LineKind, &str)], dir: Direction) -> String {
        let tmp = repo("p.txt", PAIR_BASE, PAIR_WORK);
        let d = tmp.path();
        if dir == Direction::Unstage {
            g(d, &["add", "p.txt"]);
        }
        let fd = file_diff(d, "p.txt", dir == Direction::Unstage).unwrap();
        stage(d, "p.txt", &pick(&fd, want), dir);
        index(d, "p.txt")
    }

    #[test]
    fn stage_first_pair() {
        let got = pair_case(&[(Del, "d"), (Add, "D")], Direction::Stage);
        assert_eq!(got, "c\nD\ne\nz\n");
    }

    #[test]
    fn stage_second_pair() {
        let got = pair_case(&[(Del, "e"), (Add, "E")], Direction::Stage);
        assert_eq!(got, "c\nd\nE\nz\n");
    }

    #[test]
    fn unstage_first_pair() {
        let got = pair_case(&[(Del, "d"), (Add, "D")], Direction::Unstage);
        assert_eq!(got, "c\nd\nE\nz\n");
    }

    #[test]
    fn unstage_second_pair() {
        let got = pair_case(&[(Del, "e"), (Add, "E")], Direction::Unstage);
        assert_eq!(got, "c\nD\ne\nz\n");
    }

    #[test]
    fn stage_add_only() {
        let got = pair_case(&[(Add, "D")], Direction::Stage);
        assert_eq!(got, "c\nd\nD\ne\nz\n");
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
    fn non_utf8_refuses_line_staging_but_file_stage_is_exact() {
        let tmp = repo("f.txt", "a\nb\nc\n", "a\nb\nc\n");
        let d = tmp.path();
        let work: &[u8] = b"a\nb\ncaf\xe9\nc\n";
        fs::write(d.join("f.txt"), work).unwrap();
        let fd = file_diff(d, "f.txt", false).unwrap();
        assert!(fd.lossy);
        assert_eq!(
            build_patch(&fd, &whole_hunk(&fd, 0), Direction::Stage),
            None
        );
        stage_file(d, "f.txt").unwrap();
        assert_eq!(git::run_bytes(d, &["show", ":f.txt"], &[0]).unwrap(), work);
        let utf8 = repo("g.txt", BASE, WORK);
        assert!(!file_diff(utf8.path(), "g.txt", false).unwrap().lossy);
    }

    #[test]
    fn unstage_before_first_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        g(d, &["init", "-q"]);
        fs::write(d.join("new.txt"), "x\n").unwrap();
        stage_file(d, "new.txt").unwrap();
        assert_eq!(g(d, &["ls-files"]), "new.txt\n");
        // Edited after staging: `rm --cached` would refuse (index differs from file and HEAD).
        fs::write(d.join("new.txt"), "x\ny\n").unwrap();
        unstage_file(d, "new.txt").unwrap();
        assert!(g(d, &["ls-files"]).is_empty());
        assert_eq!(
            fs::read_to_string(d.join("new.txt")).unwrap(),
            "x\ny\n",
            "worktree kept"
        );
    }

    fn commit(d: &Path, msg: &str) {
        let c = [
            "-c",
            "user.name=Test User",
            "-c",
            "user.email=test@example.com",
        ];
        let mut args = c.to_vec();
        args.extend(["-c", "commit.gpgsign=false", "commit", "-q", "-am", msg]);
        g(d, &args);
    }

    #[test]
    fn stage_paths_leaves_conflicts_unmerged() {
        let tmp = repo("f.txt", BASE, BASE);
        let d = tmp.path();
        fs::write(d.join("c.txt"), "base\n").unwrap();
        g(d, &["add", "c.txt"]);
        commit(d, "c");
        g(d, &["checkout", "-q", "-b", "other"]);
        fs::write(d.join("c.txt"), "theirs\n").unwrap();
        commit(d, "theirs");
        g(d, &["checkout", "-q", "-"]);
        fs::write(d.join("c.txt"), "ours\n").unwrap();
        commit(d, "ours");
        assert!(git::run(d, &["merge", "-q", "other"]).is_err(), "conflict");
        fs::write(d.join("f.txt"), WORK).unwrap();
        fs::write(d.join("*.txt"), "glob\n").unwrap();

        let st = crate::status::status(d).unwrap();
        assert_eq!(st.conflicted.len(), 1);
        let paths: Vec<&str> = st.unstaged.iter().map(|f| f.path.as_str()).collect();
        stage_paths(d, &paths).unwrap();
        let st = crate::status::status(d).unwrap();
        assert!(st.unstaged.is_empty(), "{st:?}");
        assert_eq!(st.conflicted.len(), 1, "conflict still unmerged: {st:?}");
        assert_eq!(index(d, "f.txt"), WORK);
        stage_paths(d, &[]).unwrap(); // no paths: no-op, not `add -A` of everything
        assert_eq!(crate::status::status(d).unwrap().conflicted.len(), 1);
    }

    #[test]
    fn stage_paths_stages_deletions_and_new_files() {
        let tmp = repo("f.txt", BASE, BASE);
        let d = tmp.path();
        fs::remove_file(d.join("f.txt")).unwrap();
        fs::write(d.join("new.txt"), "n\n").unwrap();
        stage_paths(d, &["f.txt", "new.txt"]).unwrap();
        let st = crate::status::status(d).unwrap();
        assert!(st.unstaged.is_empty(), "{st:?}");
        assert_eq!(st.staged.len(), 2);
    }

    #[test]
    fn unstage_files_in_one_call() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        fs::write(d.join("a.txt"), "a\n").unwrap();
        g(d, &["add", "a.txt"]);
        commit(d, "a");
        fs::write(d.join("f.txt"), "changed\n").unwrap(); // `commit -a` took WORK
        g(d, &["mv", "a.txt", "b.txt"]);
        stage_file(d, "f.txt").unwrap();
        let st = crate::status::status(d).unwrap();
        assert_eq!(st.staged.len(), 2, "{st:?}");
        unstage_files(d, &["f.txt", "b.txt", "a.txt"]).unwrap();
        assert!(crate::status::status(d).unwrap().staged.is_empty());
        assert_eq!(
            fs::read_to_string(d.join("f.txt")).unwrap(),
            "changed\n",
            "worktree kept"
        );

        let fresh = tempfile::tempdir().unwrap();
        let d = fresh.path();
        g(d, &["init", "-q"]);
        for f in ["x.txt", "y.txt"] {
            fs::write(d.join(f), "x\n").unwrap();
            stage_file(d, f).unwrap();
        }
        fs::write(d.join("x.txt"), "edited\n").unwrap();
        unstage_files(d, &["x.txt", "y.txt"]).unwrap();
        assert!(g(d, &["ls-files"]).is_empty(), "unborn HEAD");
        unstage_files(d, &[]).unwrap();
    }

    #[test]
    fn discard_files_reverts_unstaged_and_keeps_staged() {
        let tmp = repo("f.txt", BASE, WORK);
        let d = tmp.path();
        fs::write(d.join("gone.txt"), "g\n").unwrap();
        fs::write(d.join("kept.txt"), "k\n").unwrap();
        g(d, &["add", "gone.txt", "kept.txt"]);
        commit(d, "more");
        fs::write(d.join("f.txt"), "staged\n").unwrap();
        stage_file(d, "f.txt").unwrap();
        fs::write(d.join("f.txt"), "staged\nthen edited\n").unwrap();
        fs::remove_file(d.join("gone.txt")).unwrap();
        fs::write(d.join("new.txt"), "n\n").unwrap();
        fs::write(d.join("*.txt"), "glob\n").unwrap();

        discard_files(d, &["f.txt", "gone.txt"], &["*.txt"]).unwrap();
        assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "staged\n");
        assert_eq!(index(d, "f.txt"), "staged\n", "staged change kept");
        assert_eq!(fs::read_to_string(d.join("gone.txt")).unwrap(), "g\n");
        assert!(!d.join("*.txt").exists());
        assert!(d.join("new.txt").exists(), "literal name, not a glob");
        assert_eq!(fs::read_to_string(d.join("kept.txt")).unwrap(), "k\n");
        discard_files(d, &[], &[]).unwrap();
        assert!(d.join("new.txt").exists(), "no paths: no-op");
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
