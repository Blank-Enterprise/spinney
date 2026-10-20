//! Parse a conflicted file's `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` blocks.

/// A run of the file: lines both sides agree on, or one conflict.
#[derive(Clone, Debug, PartialEq)]
pub enum Chunk {
    Same(Vec<String>),
    Conflict(Block),
}

/// One conflict. Labels are what git wrote after the markers: `HEAD`, a branch name, or
/// for a rebase a commit and its subject.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Block {
    pub ours_label: String,
    pub ours: Vec<String>,
    /// The common ancestor's lines, with `merge.conflictStyle` diff3 or zdiff3.
    pub base: Option<(String, Vec<String>)>,
    pub theirs_label: String,
    pub theirs: Vec<String>,
}

/// The label after a seven-character marker, or None when `line` isn't that marker.
fn marker<'a>(line: &'a str, m: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(m)?;
    match rest.strip_prefix(' ') {
        Some(label) => Some(label),
        None => rest.is_empty().then_some(""),
    }
}

enum In {
    Ours,
    Base,
    Theirs,
}

/// Split `text` into chunks. A block that never ends (or a lone `=======`, e.g. a heading
/// underline) is not a conflict and stays plain text.
pub fn parse(text: &str) -> Vec<Chunk> {
    let mut chunks = vec![];
    let mut same: Vec<String> = vec![];
    // The open block, where in it we are, and its raw lines in case it never closes.
    let mut open: Option<(Block, In, Vec<String>)> = None;
    for line in text.lines() {
        let Some((block, at, raw)) = &mut open else {
            match marker(line, "<<<<<<<") {
                Some(label) => {
                    let block = Block {
                        ours_label: label.to_string(),
                        ..Default::default()
                    };
                    open = Some((block, In::Ours, vec![line.to_string()]));
                }
                None => same.push(line.to_string()),
            }
            continue;
        };
        raw.push(line.to_string());
        match at {
            In::Ours | In::Base if line == "=======" => *at = In::Theirs,
            In::Ours => match marker(line, "|||||||") {
                Some(label) => {
                    block.base = Some((label.to_string(), vec![]));
                    *at = In::Base;
                }
                None => block.ours.push(line.to_string()),
            },
            In::Base => {
                if let Some((_, base)) = &mut block.base {
                    base.push(line.to_string());
                }
            }
            In::Theirs => match marker(line, ">>>>>>>") {
                Some(label) => {
                    block.theirs_label = label.to_string();
                    let (block, _, _) = open.take().expect("open");
                    if !same.is_empty() {
                        chunks.push(Chunk::Same(std::mem::take(&mut same)));
                    }
                    chunks.push(Chunk::Conflict(block));
                }
                None => block.theirs.push(line.to_string()),
            },
        }
    }
    if let Some((_, _, raw)) = open {
        same.extend(raw);
    }
    if !same.is_empty() {
        chunks.push(Chunk::Same(same));
    }
    chunks
}

/// Number of conflicts in `chunks`.
pub fn count(chunks: &[Chunk]) -> usize {
    chunks
        .iter()
        .filter(|c| matches!(c, Chunk::Conflict(_)))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn splits_plain_text_and_conflicts() {
        let text = "a\n<<<<<<< HEAD\nmine\n=======\ntheirs\nmore\n>>>>>>> topic\nb\n";
        let chunks = parse(text);
        assert_eq!(
            chunks,
            vec![
                Chunk::Same(v(&["a"])),
                Chunk::Conflict(Block {
                    ours_label: "HEAD".into(),
                    ours: v(&["mine"]),
                    base: None,
                    theirs_label: "topic".into(),
                    theirs: v(&["theirs", "more"]),
                }),
                Chunk::Same(v(&["b"])),
            ]
        );
        assert_eq!(count(&chunks), 1);
    }

    #[test]
    fn reads_the_base_of_diff3_and_empty_sides() {
        let text = "<<<<<<< HEAD\n||||||| merged common ancestors\nbase\n=======\nt\n>>>>>>>\n";
        let [Chunk::Conflict(b)] = &parse(text)[..] else {
            panic!("{:?}", parse(text))
        };
        assert!(b.ours.is_empty());
        assert_eq!(
            b.base,
            Some(("merged common ancestors".into(), v(&["base"])))
        );
        assert_eq!((b.theirs_label.as_str(), &b.theirs), ("", &v(&["t"])));
    }

    #[test]
    fn rebase_labels_keep_the_commit_subject() {
        let text = "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> 1a2b3c4 (fix: the thing)\n";
        let [Chunk::Conflict(b)] = &parse(text)[..] else {
            panic!()
        };
        assert_eq!(b.theirs_label, "1a2b3c4 (fix: the thing)");
    }

    #[test]
    fn lone_separators_and_unclosed_blocks_stay_text() {
        let text = "Title\n=======\n";
        assert_eq!(parse(text), vec![Chunk::Same(v(&["Title", "======="]))]);
        let text = "a\n<<<<<<< HEAD\nx\n=======\ny\n";
        assert_eq!(
            parse(text),
            vec![Chunk::Same(v(&["a", "<<<<<<< HEAD", "x", "=======", "y"]))]
        );
        // Eight angle brackets, or no space before the label, is not a marker.
        let text = "<<<<<<<< x\n<<<<<<<y\n";
        assert_eq!(count(&parse(text)), 0);
        assert_eq!(parse(""), vec![]);
    }

    #[test]
    fn parses_what_git_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let g = |args: &[&str]| {
            let mut a = vec!["-c", "user.name=T", "-c", "user.email=t@example.com"];
            a.extend_from_slice(args);
            let _ = crate::git::run(d, &a);
        };
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(d.join("f"), "top\nbase\nend\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "base"]);
        g(&["switch", "-qc", "topic"]);
        std::fs::write(d.join("f"), "top\ntopic\nend\n").unwrap();
        g(&["commit", "-qam", "topic"]);
        g(&["switch", "-q", "main"]);
        std::fs::write(d.join("f"), "top\nmain\nend\n").unwrap();
        g(&["commit", "-qam", "main"]);
        g(&["-c", "merge.conflictStyle=diff3", "merge", "topic"]);
        let text = std::fs::read_to_string(d.join("f")).unwrap();
        let chunks = parse(&text);
        let [Chunk::Same(top), Chunk::Conflict(b), Chunk::Same(end)] = &chunks[..] else {
            panic!("{text}")
        };
        assert_eq!((top, end), (&v(&["top"]), &v(&["end"])));
        assert_eq!((b.ours_label.as_str(), &b.ours), ("HEAD", &v(&["main"])));
        assert_eq!(b.base.as_ref().map(|(_, l)| l), Some(&v(&["base"])));
        assert_eq!(
            (b.theirs_label.as_str(), &b.theirs),
            ("topic", &v(&["topic"]))
        );
    }
}
