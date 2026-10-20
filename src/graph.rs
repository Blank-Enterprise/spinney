#![allow(dead_code)]

use crate::git;
use anyhow::Result;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdout};

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time: i64,
    pub refs: String,
    pub subject: String,
}

/// Parse one `\x1f`-separated log record: sha, parents, author, time, refs, subject.
pub fn parse_record(rec: &str) -> Option<Commit> {
    let mut f = rec.splitn(6, '\x1f');
    let sha = f.next().filter(|s| !s.is_empty())?;
    let parents = f.next()?.split_whitespace().map(String::from).collect();
    let author = f.next()?;
    let time = f.next()?.parse().ok()?;
    let refs = f.next()?;
    let subject = f.next()?;
    Some(Commit {
        sha: sha.into(),
        parents,
        author: author.into(),
        time,
        refs: refs.into(),
        subject: subject.into(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Slot of the commit dot; its first-parent line continues in the same slot of `lanes_out`.
    pub lane: usize,
    /// Compact id of `commit.sha`; lane slots hold the id of the commit they lead to.
    pub id: u32,
    /// Ids of `commit.parents`, in order.
    pub parent_ids: Vec<u32>,
    pub lanes_in: Vec<Option<u32>>,
    pub lanes_out: Vec<Option<u32>>,
    pub commit: Commit,
}

/// Lane state across rows. Shas are interned to `u32` ids so each row's lane snapshots stay
/// small; an id leaves the map once its commit is pushed (topo order shows children first).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lanes {
    active: Vec<Option<u32>>,
    ids: HashMap<String, u32>,
    next: u32,
}

fn first_free(active: &mut Vec<Option<u32>>, id: u32) {
    match active.iter().position(Option::is_none) {
        Some(i) => active[i] = Some(id),
        None => active.push(Some(id)),
    }
}

impl Lanes {
    fn intern(&mut self, sha: &str) -> u32 {
        if let Some(&id) = self.ids.get(sha) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.ids.insert(sha.to_string(), id);
        id
    }

    pub fn push(&mut self, c: Commit) -> Row {
        let id = self.intern(&c.sha);
        self.ids.remove(&c.sha);
        let parent_ids: Vec<u32> = c.parents.iter().map(|p| self.intern(p)).collect();
        let lanes_in = self.active.clone();
        let want = Some(id);
        let lane = match self.active.iter().position(|l| *l == want) {
            Some(i) => i,
            None => match self.active.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    self.active.push(None);
                    self.active.len() - 1
                }
            },
        };
        for (i, slot) in self.active.iter_mut().enumerate() {
            if i != lane && *slot == want {
                *slot = None; // branch lines merging into this commit
            }
        }
        self.active[lane] = parent_ids.first().copied();
        for &p in parent_ids.iter().skip(1) {
            if !self.active.contains(&Some(p)) {
                first_free(&mut self.active, p);
            }
        }
        while self.active.last().is_some_and(Option::is_none) {
            self.active.pop();
        }
        Row {
            lane,
            id,
            parent_ids,
            lanes_in,
            lanes_out: self.active.clone(),
            commit: c,
        }
    }
}

/// Which listed commits are on HEAD's history, decided row by row as pages arrive: in
/// topo order every child comes before its parents, so one forward pass is enough.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ancestry {
    /// Ancestors of HEAD not listed yet.
    wanted: std::collections::HashSet<String>,
}

impl Ancestry {
    /// Start from `head` (None: no commits, or nothing is on its history).
    pub fn new(head: Option<&str>) -> Ancestry {
        Ancestry {
            wanted: head.into_iter().map(String::from).collect(),
        }
    }

    /// Whether `c`, the next commit in topo order, is HEAD or one of its ancestors.
    pub fn mark(&mut self, c: &Commit) -> bool {
        if !self.wanted.remove(&c.sha) {
            return false;
        }
        self.wanted.extend(c.parents.iter().cloned());
        true
    }
}

/// One long-running `git log`, read in pages. Dropping it kills git.
pub struct LogStream {
    child: Child,
    out: BufReader<ChildStdout>,
    lanes: Lanes,
}

impl LogStream {
    pub fn open(dir: &Path) -> Result<LogStream> {
        let mut child = git::spawn_stream(
            dir,
            &[
                "log",
                "--all",
                "--topo-order",
                "-z",
                "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s",
            ],
        )?;
        let out = BufReader::new(child.stdout.take().expect("stdout piped"));
        Ok(LogStream {
            child,
            out,
            lanes: Lanes::default(),
        })
    }

    /// Up to `n` rows; fewer at end of history. An empty repo yields no rows, whatever git's exit status.
    pub fn next_page(&mut self, n: usize) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(n);
        let mut buf = Vec::new();
        while rows.len() < n {
            buf.clear();
            if self.out.read_until(0, &mut buf)? == 0 {
                break;
            }
            if buf.last() == Some(&0) {
                buf.pop();
            }
            let text = String::from_utf8_lossy(&buf);
            let text = text.strip_prefix('\n').unwrap_or(&text);
            if let Some(c) = parse_record(text) {
                rows.push(self.lanes.push(c));
            }
        }
        Ok(rows)
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> Commit {
        Commit {
            sha: sha.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            author: "Test User".into(),
            time: 0,
            refs: String::new(),
            subject: String::new(),
        }
    }

    #[test]
    fn lanes_linear() {
        let mut l = Lanes::default();
        let rows = [
            l.push(commit("c", &["b"])),
            l.push(commit("b", &["a"])),
            l.push(commit("a", &[])),
        ];
        assert!(rows.iter().all(|r| r.lane == 0));
        assert!(rows[2].lanes_out.is_empty());
    }

    #[test]
    fn lanes_merge() {
        let mut l = Lanes::default();
        let m = l.push(commit("m", &["a", "f"]));
        let f = l.push(commit("f", &["a"]));
        let a = l.push(commit("a", &[]));
        assert_eq!(m.parent_ids, vec![a.id, f.id]);
        assert_eq!(f.parent_ids, vec![a.id]);
        assert_ne!(a.id, f.id);
        let (a_, f_) = (Some(a.id), Some(f.id));
        assert_eq!(m.lane, 0);
        assert_eq!(m.lanes_out, vec![a_, f_]);
        assert_eq!(f.lane, 1);
        assert_eq!(f.lanes_in, vec![a_, f_]);
        assert_eq!(f.lanes_out, vec![a_, a_]);
        assert_eq!(a.lane, 0);
        assert_eq!(a.lanes_in, vec![a_, a_]);
        assert!(a.lanes_out.is_empty());
        assert!(l.ids.is_empty(), "pushed commits leave the intern map");
    }

    #[test]
    fn ancestry_follows_head_through_merges_only() {
        // m merges f into a; x is on another branch; HEAD is m.
        let rows = [
            commit("x", &["a"]),
            commit("m", &["a", "f"]),
            commit("f", &["a"]),
            commit("a", &["r"]),
            commit("r", &[]),
        ];
        let mut anc = Ancestry::new(Some("m"));
        let on: Vec<bool> = rows.iter().map(|c| anc.mark(c)).collect();
        assert_eq!(on, [false, true, true, true, true]);
        let mut from_f = Ancestry::new(Some("f"));
        let on: Vec<bool> = rows.iter().map(|c| from_f.mark(c)).collect();
        assert_eq!(on, [false, false, true, true, true]);
        let mut none = Ancestry::new(None);
        assert!(rows.iter().all(|c| !none.mark(c)));
    }

    #[test]
    fn parse_record_fields() {
        let c =
            parse_record("abc\x1fp1 p2\x1fTest User\x1f1700000000\x1fHEAD -> main\x1fhello world")
                .unwrap();
        assert_eq!(c.sha, "abc");
        assert_eq!(c.parents, vec!["p1", "p2"]);
        assert_eq!((c.author.as_str(), c.time), ("Test User", 1700000000));
        assert_eq!(
            (c.refs.as_str(), c.subject.as_str()),
            ("HEAD -> main", "hello world")
        );
        let root = parse_record("abc\x1f\x1fA\x1f1\x1f\x1fs").unwrap();
        assert!(root.parents.is_empty());
        assert!(parse_record("garbage").is_none());
        assert!(parse_record("abc\x1f\x1fA\x1fnotatime\x1f\x1fs").is_none());
    }

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git::run(tmp.path(), &["init", "-q", "-b", "main"]).unwrap();
        for (k, v) in [
            ("user.name", "Test User"),
            ("user.email", "test@example.com"),
        ] {
            git::run(tmp.path(), &["config", k, v]).unwrap();
        }
        tmp
    }

    #[test]
    fn log_stream_pages() {
        let tmp = repo();
        let stream: String = (0..1200)
            .map(|i| {
                format!(
                    "commit refs/heads/main\ncommitter Test User <test@example.com> {} +0000\ndata 2\nn\n\n",
                    1_700_000_000 + i
                )
            })
            .collect();
        use std::io::Write;
        let mut child = std::process::Command::new("git")
            .args(["fast-import", "--quiet"])
            .current_dir(tmp.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());

        let mut s = LogStream::open(tmp.path()).unwrap();
        let lens: Vec<usize> = (0..4).map(|_| s.next_page(500).unwrap().len()).collect();
        assert_eq!(lens, vec![500, 500, 200, 0]);
    }

    #[test]
    fn log_stream_shows_commits_only_a_detached_head_has() {
        let tmp = repo();
        let g = |args: &[&str]| git::run(tmp.path(), args).unwrap();
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        g(&["switch", "-q", "--detach"]);
        g(&["commit", "-q", "--allow-empty", "-m", "loose"]);
        let mut s = LogStream::open(tmp.path()).unwrap();
        let rows = s.next_page(10).unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.commit.subject.as_str(), r.commit.refs.as_str()))
            .collect();
        assert_eq!(got, [("loose", "HEAD"), ("init", "main")]);
    }

    #[test]
    fn log_stream_empty_repo() {
        let tmp = repo();
        let mut s = LogStream::open(tmp.path()).unwrap();
        assert!(s.next_page(500).unwrap().is_empty());
    }
}
