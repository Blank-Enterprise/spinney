#![allow(dead_code)]

use crate::git;
use anyhow::Result;
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
    pub lanes_in: Vec<Option<String>>,
    pub lanes_out: Vec<Option<String>>,
    pub commit: Commit,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lanes {
    active: Vec<Option<String>>,
}

fn first_free(active: &mut Vec<Option<String>>, sha: String) {
    match active.iter().position(Option::is_none) {
        Some(i) => active[i] = Some(sha),
        None => active.push(Some(sha)),
    }
}

impl Lanes {
    pub fn push(&mut self, c: Commit) -> Row {
        let lanes_in = self.active.clone();
        let want = Some(c.sha.clone());
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
        self.active[lane] = c.parents.first().cloned();
        for p in c.parents.iter().skip(1) {
            if !self.active.contains(&Some(p.clone())) {
                first_free(&mut self.active, p.clone());
            }
        }
        while self.active.last().is_some_and(Option::is_none) {
            self.active.pop();
        }
        Row {
            lane,
            lanes_in,
            lanes_out: self.active.clone(),
            commit: c,
        }
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

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
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
        assert_eq!(m.lane, 0);
        assert_eq!(m.lanes_out, vec![some("a"), some("f")]);
        assert_eq!(f.lane, 1);
        assert_eq!(f.lanes_out, vec![some("a"), some("a")]);
        assert_eq!(a.lane, 0);
        assert_eq!(a.lanes_in, vec![some("a"), some("a")]);
        assert!(a.lanes_out.is_empty());
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
    fn log_stream_empty_repo() {
        let tmp = repo();
        let mut s = LogStream::open(tmp.path()).unwrap();
        assert!(s.next_page(500).unwrap().is_empty());
    }
}
