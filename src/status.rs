#![allow(dead_code)]

use crate::git;
use anyhow::Result;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Modified,
    Added,
    Deleted,
    Renamed { from: String },
    Untracked,
    Conflicted(Conflict),
}

/// How each side left a conflicted file, in git's terms: `ours` is HEAD (index stage 2)
/// and `theirs` the commit coming in (stage 3). During a rebase git's ours is the branch
/// being rebased onto, so "mine" is not always ours: see `actions::git_side`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Conflict {
    pub ours: Side,
    pub theirs: Side,
}

/// One side of a conflict.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Modified,
    Added,
    Deleted,
}

impl Conflict {
    /// From porcelain's `XY` (e.g. `UU`, `UD`, `AA`); None for anything else.
    pub fn parse(xy: &str) -> Option<Conflict> {
        let side = |c| match c {
            'U' => Some(Side::Modified),
            'A' => Some(Side::Added),
            'D' => Some(Side::Deleted),
            _ => None,
        };
        let mut c = xy.chars();
        let (ours, theirs) = (side(c.next()?)?, side(c.next()?)?);
        Some(Conflict { ours, theirs })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileStatus {
    pub path: String,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub staged: Vec<FileStatus>,
    pub unstaged: Vec<FileStatus>,
    pub conflicted: Vec<FileStatus>,
}

impl Status {
    /// Distinct paths with any change (a file changed on both sides counts once).
    pub fn changed_files(&self) -> usize {
        let all = self
            .staged
            .iter()
            .chain(&self.unstaged)
            .chain(&self.conflicted);
        all.map(|f| f.path.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }
}

fn change_of(code: char, from: Option<&str>) -> Change {
    match code {
        'A' => Change::Added,
        'D' => Change::Deleted,
        'R' => Change::Renamed {
            from: from.unwrap_or_default().to_string(),
        },
        _ => Change::Modified, // M, T, C
    }
}

pub fn status(worktree: &Path) -> Result<Status> {
    let out = git::run(
        worktree,
        &[
            "-c",
            "core.quotepath=off",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    let mut st = Status::default();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    while let Some(e) = entries.next() {
        let (kind, rest) = e.split_at(1);
        let fs = |path: &str, change| FileStatus {
            path: path.to_string(),
            change,
        };
        match kind {
            "1" | "2" => {
                // 1 XY sub mH mI mW hH hI path / 2 XY sub mH mI mW hH hI score path, then origPath
                let n = if kind == "1" { 8 } else { 9 };
                let f: Vec<&str> = rest.trim_start().splitn(n, ' ').collect();
                let from = if kind == "2" { entries.next() } else { None };
                let xy: Vec<char> = f[0].chars().collect();
                let path = f[n - 1];
                if xy[0] != '.' {
                    st.staged.push(fs(path, change_of(xy[0], from)));
                }
                if xy[1] != '.' {
                    st.unstaged.push(fs(path, change_of(xy[1], from)));
                }
            }
            "u" => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let f: Vec<&str> = rest.trim_start().splitn(10, ' ').collect();
                let conflict = Conflict::parse(f[0]).unwrap_or(Conflict {
                    ours: Side::Modified,
                    theirs: Side::Modified,
                });
                let path = f.last().copied().unwrap_or_default();
                st.conflicted.push(fs(path, Change::Conflicted(conflict)));
            }
            "?" => st.unstaged.push(fs(rest.trim_start(), Change::Untracked)),
            _ => {}
        }
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn g(dir: &Path, args: &[&str]) {
        git::run(dir, args).unwrap();
    }

    fn fs_(path: &str, change: Change) -> FileStatus {
        FileStatus {
            path: path.into(),
            change,
        }
    }

    #[test]
    fn changed_files_counts_each_path_once() {
        let mut st = Status::default();
        assert_eq!(st.changed_files(), 0);
        st.staged.push(fs_("a", Change::Modified));
        st.unstaged.push(fs_("a", Change::Modified)); // both sides: one file
        st.unstaged.push(fs_("b", Change::Untracked));
        st.conflicted.push(fs_("c", Change::Conflicted(BOTH)));
        assert_eq!(st.changed_files(), 3);
    }

    #[test]
    fn status_reports_all_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        g(d, &["init", "-q"]);
        for f in ["mod.txt", "del.txt", "old.txt"] {
            fs::write(d.join(f), "one\ntwo\nthree\n").unwrap();
        }
        fs::write(d.join("tracked.bin"), b"\x00one").unwrap();
        g(d, &["add", "."]);
        g(
            d,
            &[
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        fs::write(d.join("mod.txt"), "one\nTWO\nthree\n").unwrap();
        fs::remove_file(d.join("del.txt")).unwrap();
        g(d, &["mv", "old.txt", "new.txt"]);
        fs::write(d.join("added.txt"), "a\n").unwrap();
        g(d, &["add", "added.txt"]);
        fs::write(d.join("untracked.txt"), "u\n").unwrap();
        fs::write(d.join("bin.dat"), b"\x00\x01\x02binary").unwrap();
        fs::write(d.join("tracked.bin"), b"\x00two").unwrap();

        let s = status(d).unwrap();
        assert_eq!(
            s.staged,
            vec![
                fs_("added.txt", Change::Added),
                fs_(
                    "new.txt",
                    Change::Renamed {
                        from: "old.txt".into()
                    }
                ),
            ]
        );
        assert_eq!(
            s.unstaged,
            vec![
                fs_("del.txt", Change::Deleted),
                fs_("mod.txt", Change::Modified),
                fs_("tracked.bin", Change::Modified),
                fs_("bin.dat", Change::Untracked),
                fs_("untracked.txt", Change::Untracked),
            ]
        );
        assert!(s.conflicted.is_empty());

        let bin = crate::diff::file_diff(d, "bin.dat", false).unwrap();
        assert!(bin.binary && bin.hunks.is_empty());
        let tb = crate::diff::file_diff(d, "tracked.bin", false).unwrap();
        assert!(tb.binary && tb.hunks.is_empty());
        let un = crate::diff::file_diff(d, "untracked.txt", false).unwrap();
        assert_eq!(un.hunks[0].lines[0].text, "u");
        let m = crate::diff::file_diff(d, "mod.txt", false).unwrap();
        assert_eq!(m.hunks[0].lines.len(), 4);
    }

    #[test]
    fn status_reports_conflicts_and_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let c = |args: &[&str]| {
            let mut a = vec![
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
            ];
            a.extend_from_slice(args);
            git::run(d, &a).unwrap();
        };
        c(&["init", "-q", "-b", "main"]);
        fs::write(d.join("f"), "base\n").unwrap();
        c(&["add", "."]);
        c(&["commit", "-q", "-m", "base"]);
        c(&["checkout", "-q", "-b", "other"]);
        fs::write(d.join("f"), "other\n").unwrap();
        c(&["commit", "-q", "-am", "other"]);
        c(&["checkout", "-q", "main"]);
        fs::write(d.join("f"), "main\n").unwrap();
        c(&["commit", "-q", "-am", "main"]);
        let _ = git::run(d, &["merge", "other"]);
        let s = status(d).unwrap();
        assert_eq!(s.conflicted, vec![fs_("f", Change::Conflicted(BOTH))]);
        assert!(s.staged.is_empty() && s.unstaged.is_empty());

        // Deleted on `gone`, changed on main: deleted by them.
        c(&["merge", "--abort"]);
        c(&["checkout", "-q", "-b", "gone", "main~1"]);
        c(&["rm", "-q", "f"]);
        c(&["commit", "-q", "-m", "gone"]);
        c(&["checkout", "-q", "main"]);
        let _ = git::run(d, &["merge", "gone"]);
        let s = status(d).unwrap();
        let ud = Conflict {
            ours: Side::Modified,
            theirs: Side::Deleted,
        };
        assert_eq!(s.conflicted, vec![fs_("f", Change::Conflicted(ud))]);
    }

    const BOTH: Conflict = Conflict {
        ours: Side::Modified,
        theirs: Side::Modified,
    };

    #[test]
    fn conflict_codes_parse_per_side() {
        let c = |xy| Conflict::parse(xy).map(|c| (c.ours, c.theirs));
        assert_eq!(c("UU"), Some((Side::Modified, Side::Modified)));
        assert_eq!(c("DU"), Some((Side::Deleted, Side::Modified)));
        assert_eq!(c("AA"), Some((Side::Added, Side::Added)));
        assert_eq!(c("DD"), Some((Side::Deleted, Side::Deleted)));
        assert_eq!(c("U."), None);
        assert_eq!(c("U"), None);
    }
}
