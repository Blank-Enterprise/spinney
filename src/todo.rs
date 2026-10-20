//! Interactive rebase: the commits after a base, edited as a todo list (pick, reword,
//! squash, fixup, drop, reorder) and run with `git rebase -i` without any editor.
//!
//! The todo git would open is replaced by ours through `GIT_SEQUENCE_EDITOR`. New messages
//! for reword and squash are applied by `exec` lines that amend the commit just built, so
//! they also survive a stop for conflicts and a Continue from the in-progress banner.

use crate::actions::{self, MergeOutcome};
use crate::{git, refs};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TodoAction {
    Pick,
    /// Pick with a new message.
    Reword,
    /// Fold into the commit above, combining the messages.
    Squash,
    /// Fold into the commit above, keeping its message.
    Fixup,
    Drop,
}

impl TodoAction {
    pub const ALL: [TodoAction; 5] = [
        TodoAction::Pick,
        TodoAction::Reword,
        TodoAction::Squash,
        TodoAction::Fixup,
        TodoAction::Drop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TodoAction::Pick => "pick",
            TodoAction::Reword => "reword",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
        }
    }

    /// Starts a commit of its own (squash and fixup fold into one, drop makes none).
    fn starts_commit(self) -> bool {
        matches!(self, TodoAction::Pick | TodoAction::Reword)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TodoItem {
    pub sha: String,
    pub subject: String,
    /// The commit's message as it is now.
    pub original: String,
    /// The message to give it: used for a reword, or for a commit that squashes others in.
    pub message: String,
    pub action: TodoAction,
}

/// The commits after `base` on `branch` (at `head`), oldest first, as listed when asked.
#[derive(Clone, Debug, PartialEq)]
pub struct TodoPlan {
    pub branch: String,
    pub base: String,
    pub head: String,
    pub items: Vec<TodoItem>,
}

/// List the commits after `base` on the checked-out branch, all `pick`. Refuses a range
/// with merge commits: `rebase -i` would flatten them.
pub fn plan(worktree: &Path, base: &str) -> Result<TodoPlan> {
    let Some(branch) = refs::current_branch(worktree)? else {
        bail!("Check out a branch first");
    };
    let head = actions::rev_parse(worktree, "HEAD")?;
    let base = actions::rev_parse(worktree, &format!("{base}^{{commit}}"))?;
    if !actions::is_ancestor(worktree, &base, "HEAD")? {
        bail!("{} isn't on {branch}'s history", &base[..8]);
    }
    let range = format!("{base}..{head}");
    let format = "--format=%H%x1f%P%x1f%B%x1e";
    let out = git::run(
        worktree,
        &["log", "--reverse", "--topo-order", format, &range, "--"],
    )?;
    let mut items = vec![];
    for rec in out.split('\x1e').map(|r| r.trim_start_matches('\n')) {
        let mut f = rec.splitn(3, '\x1f');
        let (Some(sha), Some(parents), Some(message)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if parents.split_whitespace().count() > 1 {
            bail!(
                "{} is a merge commit; interactive rebase over merges isn't supported",
                &sha[..8]
            );
        }
        let message = message.trim_end().to_string();
        items.push(TodoItem {
            sha: sha.to_string(),
            subject: message.lines().next().unwrap_or_default().to_string(),
            original: message.clone(),
            message,
            action: TodoAction::Pick,
        });
    }
    if items.is_empty() {
        bail!("There are no commits after this one on {branch}");
    }
    Ok(TodoPlan {
        branch,
        base,
        head,
        items,
    })
}

/// Index of the commit item `k` folds into: the nearest earlier pick or reword.
fn group_head(items: &[TodoItem], k: usize) -> Option<usize> {
    (0..k).rev().find(|&j| items[j].action.starts_commit())
}

/// Set item `k`'s action. Squashing appends its message to the commit it folds into, so
/// the combined message starts out like git's.
pub fn set_action(items: &mut [TodoItem], k: usize, action: TodoAction) {
    items[k].action = action;
    if action != TodoAction::Squash {
        return;
    }
    if let Some(h) = group_head(items, k) {
        let add = items[k].original.clone();
        if !items[h].message.contains(&add) {
            items[h].message = format!("{}\n\n{add}", items[h].message.trim_end());
        }
    }
}

/// Item `k` gets a message editor: it's reworded, or other commits squash into it.
pub fn edits_message(items: &[TodoItem], k: usize) -> bool {
    let item = &items[k];
    match item.action {
        TodoAction::Reword => true,
        TodoAction::Pick => items[k + 1..]
            .iter()
            .take_while(|i| !i.action.starts_commit())
            .any(|i| i.action == TodoAction::Squash),
        _ => false,
    }
}

/// Why the list can't run as it is, if it can't.
pub fn problem(items: &[TodoItem]) -> Option<String> {
    let first = items.iter().find(|i| i.action != TodoAction::Drop);
    if first.is_some_and(|i| !i.action.starts_commit()) {
        return Some(
            "The first commit can't be squashed or fixed up: nothing comes before it".into(),
        );
    }
    let empty =
        (0..items.len()).find(|&k| edits_message(items, k) && items[k].message.trim().is_empty());
    empty.map(|k| format!("Write a message for \"{}\"", items[k].subject))
}

/// `s` as one single-quoted shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A file the todo's `exec` lines read, relative to the rebase's working folder.
#[derive(Debug, PartialEq)]
pub struct TodoFile {
    pub name: String,
    pub contents: Vec<u8>,
}

/// What a pick keeps of a commit, so an amend script can tell it is on the commit it was
/// written for: author, author time and subject (a resolved conflict can rewrap the body).
const IDENTITY: &str = "--format=%an%n%ae%n%at%n%s";

/// The todo for `items` and the files it needs in `dir`. Each commit that needs a new
/// message is followed by `exec sh amend-N.sh`, which amends it with `msg-N` only while
/// HEAD is still that commit (its `IDENTITY` in `who-N`): if the user skipped it after a
/// conflict, the commit before it is left alone.
pub fn todo(items: &[TodoItem], identities: &[Vec<u8>], dir: &Path) -> (String, Vec<TodoFile>) {
    let mut lines = vec![];
    let mut files = vec![];
    let mut amend = |lines: &mut Vec<String>, head: usize| {
        let n = files.len() / 3;
        let path = |name: &str| sh_quote(&dir.join(name).display().to_string());
        let (msg, who, script) = (
            format!("msg-{n}"),
            format!("who-{n}"),
            format!("amend-{n}.sh"),
        );
        let body = format!(
            "git log -1 '{IDENTITY}' HEAD | cmp -s - {who} || exit 0\n\
             exec git commit --amend --no-verify --allow-empty --quiet --cleanup=whitespace \
             -F {msg}\n",
            who = path(&who),
            msg = path(&msg),
        );
        files.push(TodoFile {
            name: msg,
            contents: format!("{}\n", items[head].message.trim_end()).into_bytes(),
        });
        files.push(TodoFile {
            name: who,
            contents: identities[head].clone(),
        });
        files.push(TodoFile {
            name: script.clone(),
            contents: body.into_bytes(),
        });
        lines.push(format!("exec sh {}", path(&script)));
    };
    let mut open: Option<usize> = None;
    for (k, item) in items.iter().enumerate() {
        if item.action.starts_commit() {
            if let Some(h) = open.take() {
                if edits_message(items, h) {
                    amend(&mut lines, h);
                }
            }
            open = Some(k);
        }
        let verb = match item.action {
            TodoAction::Pick | TodoAction::Reword => "pick",
            TodoAction::Squash | TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
        };
        lines.push(format!("{verb} {}", item.sha));
    }
    if let Some(h) = open.filter(|&h| edits_message(items, h)) {
        amend(&mut lines, h);
    }
    let mut todo = lines.join("\n");
    todo.push('\n');
    (todo, files)
}

/// `sha`'s `IDENTITY`, byte for byte as the amend script will see it.
fn identity(worktree: &Path, sha: &str) -> Result<Vec<u8>> {
    git::run_bytes(worktree, &["log", "-1", IDENTITY, sha, "--"], &[0])
}

/// Where the todo and its files go: a fresh folder in this worktree's git dir.
fn work_dir(worktree: &Path) -> Result<PathBuf> {
    let rel = git::run(worktree, &["rev-parse", "--git-path", "spinney-rebase"])?;
    let dir = worktree.join(rel.trim());
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir.canonicalize()?)
}

/// Run `plan` with the user's `items`, refusing if the branch moved or the list no longer
/// holds exactly the commits that were listed. Conflicts are left to resolve under the
/// in-progress banner; any other stop is aborted, back to where it started.
pub fn start(worktree: &Path, plan: &TodoPlan, items: &[TodoItem]) -> Result<MergeOutcome> {
    let branch = refs::current_branch(worktree)?;
    if branch.as_deref() != Some(&plan.branch) || actions::rev_parse(worktree, "HEAD")? != plan.head
    {
        bail!(
            "{} moved since the list was made; open it again",
            plan.branch
        );
    }
    actions::ensure_clean(worktree)?;
    let mut want: Vec<&str> = plan.items.iter().map(|i| i.sha.as_str()).collect();
    let mut got: Vec<&str> = items.iter().map(|i| i.sha.as_str()).collect();
    want.sort_unstable();
    got.sort_unstable();
    if want != got {
        bail!("The list doesn't match the commits after the base; open it again");
    }
    if let Some(p) = problem(items) {
        bail!("{p}");
    }
    let identities = items
        .iter()
        .map(|i| identity(worktree, &i.sha))
        .collect::<Result<Vec<_>>>()?;
    let dir = work_dir(worktree)?;
    let (list, files) = todo(items, &identities, &dir);
    for f in &files {
        std::fs::write(dir.join(&f.name), &f.contents)?;
    }
    let todo_path = dir.join("todo");
    std::fs::write(&todo_path, list)?;
    let editor = format!("cp {}", sh_quote(&todo_path.display().to_string()));
    let env = [("GIT_SEQUENCE_EDITOR", editor.as_str())];
    // A commit the edits leave empty is kept rather than stopping the whole rebase.
    let args = [
        "rebase",
        "-i",
        "--no-autosquash",
        "--empty=keep",
        &plan.base,
    ];
    if let Err(e) = git::run_env(worktree, &args, &env) {
        return actions::stopped(worktree, e, false, &plan.head);
    }
    Ok(MergeOutcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{abort_op, in_progress, Operation};

    fn item(sha: &str, action: TodoAction) -> TodoItem {
        TodoItem {
            sha: sha.into(),
            subject: format!("{sha} subject"),
            original: format!("{sha} subject"),
            message: format!("{sha} subject"),
            action,
        }
    }

    #[test]
    fn squash_folds_into_the_commit_above_and_combines_messages() {
        use TodoAction::*;
        let mut items = vec![item("a", Pick), item("b", Pick), item("c", Pick)];
        set_action(&mut items, 2, Squash);
        assert_eq!(items[1].message, "b subject\n\nc subject");
        assert!(edits_message(&items, 1) && !edits_message(&items, 0));
        set_action(&mut items, 2, Squash);
        assert_eq!(items[1].message, "b subject\n\nc subject", "added once");
        set_action(&mut items, 1, Drop);
        set_action(&mut items, 2, Squash);
        assert_eq!(
            items[0].message, "a subject\n\nc subject",
            "past the dropped one"
        );
        set_action(&mut items, 0, Reword);
        assert!(edits_message(&items, 0));
    }

    #[test]
    fn problems_stop_the_list_running() {
        use TodoAction::*;
        let ok = [item("a", Pick), item("b", Fixup)];
        assert_eq!(problem(&ok), None);
        let first = [item("a", Drop), item("b", Squash)];
        assert!(problem(&first)
            .unwrap()
            .starts_with("The first commit can't"));
        let mut empty = vec![item("a", Reword)];
        empty[0].message = " \n".into();
        assert_eq!(
            problem(&empty).as_deref(),
            Some("Write a message for \"a subject\"")
        );
        assert_eq!(
            problem(&[item("a", Drop)]),
            None,
            "dropping everything is allowed"
        );
    }

    #[test]
    fn todo_turns_rewords_and_squashes_into_guarded_amends() {
        use TodoAction::*;
        let dir = Path::new("/tmp/x y");
        let mut items = vec![
            item("a", Reword),
            item("b", Pick),
            item("c", Squash),
            item("d", Fixup),
            item("e", Drop),
            item("f", Pick),
        ];
        items[0].message = "new a".into();
        items[1].message = "b and c".into();
        let ids: Vec<Vec<u8>> = ["A", "B", "C", "D", "E", "F"].map(|a| a.into()).into();
        let (list, files) = todo(&items, &ids, dir);
        assert_eq!(
            list,
            "pick a\nexec sh '/tmp/x y/amend-0.sh'\npick b\nfixup c\nfixup d\ndrop e\n\
             exec sh '/tmp/x y/amend-1.sh'\npick f\n"
        );
        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "msg-0",
                "who-0",
                "amend-0.sh",
                "msg-1",
                "who-1",
                "amend-1.sh"
            ]
        );
        assert_eq!(files[0].contents, b"new a\n");
        assert_eq!(files[1].contents, b"A");
        assert_eq!(files[3].contents, b"b and c\n");
        assert_eq!(files[4].contents, b"B");
        let script = String::from_utf8_lossy(&files[2].contents);
        assert!(script.contains("cmp -s - '/tmp/x y/who-0'"), "{script}");
        assert!(script.contains("-F '/tmp/x y/msg-0'"), "{script}");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    /// A repo on `main` with `base`, then commits a, b, c each adding a file; returns
    /// (tmp, dir, base sha).
    fn repo() -> (tempfile::TempDir, PathBuf, String) {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("it's here");
        std::fs::create_dir(&d).unwrap();
        let g = |args: &[&str]| git::run(&d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "-q", "--allow-empty", "-m", "base"]);
        let base = g(&["rev-parse", "HEAD"]).trim().to_string();
        for name in ["a", "b", "c"] {
            std::fs::write(d.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            g(&["add", "."]);
            g(&["commit", "-q", "-m", name]);
        }
        (tmp, d, base)
    }

    fn log(d: &Path) -> String {
        git::run(d, &["log", "--format=%s", "-10"])
            .unwrap()
            .trim()
            .to_string()
    }

    fn run(d: &Path, base: &str, edit: impl FnOnce(&mut Vec<TodoItem>)) -> MergeOutcome {
        let p = plan(d, base).unwrap();
        let mut items = p.items.clone();
        edit(&mut items);
        start(d, &p, &items).unwrap()
    }

    #[test]
    fn plan_lists_commits_after_the_base_oldest_first() {
        let (_tmp, d, base) = repo();
        let p = plan(&d, &base).unwrap();
        let subjects: Vec<_> = p.items.iter().map(|i| i.subject.as_str()).collect();
        assert_eq!(subjects, ["a", "b", "c"]);
        assert!(p.items.iter().all(|i| i.action == TodoAction::Pick));
        let head = git::run(&d, &["rev-parse", "HEAD"]).unwrap();
        let err = plan(&d, head.trim()).unwrap_err();
        assert!(err.to_string().contains("no commits after"), "{err}");
    }

    #[test]
    fn reorder_runs_without_an_editor() {
        let (_tmp, d, base) = repo();
        let out = run(&d, &base, |items| items.swap(0, 2));
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(log(&d), "a\nb\nc\nbase");
        assert_eq!(refs::current_branch(&d).unwrap().as_deref(), Some("main"));
        assert_eq!(in_progress(&d).unwrap(), None);
    }

    #[test]
    fn squash_and_fixup_combine_commits_with_the_given_message() {
        let (_tmp, d, base) = repo();
        let out = run(&d, &base, |items| {
            set_action(items, 1, TodoAction::Squash);
            items[0].message = "a and b".into();
            set_action(items, 2, TodoAction::Fixup);
        });
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(log(&d), "a and b\nbase");
        let files = git::run(&d, &["show", "--format=", "--name-only", "HEAD"]).unwrap();
        assert_eq!(files.trim(), "a.txt\nb.txt\nc.txt");
    }

    #[test]
    fn reword_and_drop() {
        let (_tmp, d, base) = repo();
        let out = run(&d, &base, |items| {
            items[0].action = TodoAction::Reword;
            items[0].message = "a, reworded\n\nwith a body".into();
            items[1].action = TodoAction::Drop;
        });
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(log(&d), "c\na, reworded\nbase");
        let msg = actions::commit_message(&d, "HEAD~1").unwrap();
        assert_eq!(msg, "a, reworded\n\nwith a body");
        assert!(!d.join("b.txt").exists());
    }

    #[test]
    fn refuses_a_stale_or_broken_list() {
        let (_tmp, d, base) = repo();
        let p = plan(&d, &base).unwrap();
        let mut items = p.items.clone();
        items.pop();
        let err = start(&d, &p, &items).unwrap_err();
        assert!(err.to_string().contains("doesn't match"), "{err}");
        let mut items = p.items.clone();
        items[0].action = TodoAction::Fixup;
        let err = start(&d, &p, &items).unwrap_err();
        assert!(err.to_string().contains("first commit"), "{err}");
        git::run(&d, &["commit", "-q", "--allow-empty", "-m", "d"]).unwrap();
        let err = start(&d, &p, &p.items).unwrap_err();
        assert!(err.to_string().contains("moved since"), "{err}");
        assert_eq!(in_progress(&d).unwrap(), None);
    }

    #[test]
    fn conflict_stops_in_progress_and_abort_restores_head() {
        let (_tmp, d, base) = repo();
        // b edits a.txt, so putting b before a conflicts.
        let g = |args: &[&str]| git::run(&d, args).unwrap();
        g(&["reset", "-q", "--hard", &base]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "a"]);
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        g(&["commit", "-qam", "edit a"]);
        let before = g(&["rev-parse", "HEAD"]);
        let out = run(&d, &base, |items| {
            items.swap(0, 1);
            items[0].action = TodoAction::Reword;
            items[0].message = "edited first".into();
        });
        assert!(matches!(out, MergeOutcome::Conflicts(_)), "{out:?}");
        assert_eq!(in_progress(&d).unwrap(), Some(Operation::Rebase));
        abort_op(&d, Operation::Rebase).unwrap();
        assert_eq!(g(&["rev-parse", "HEAD"]), before);
        assert_eq!(refs::current_branch(&d).unwrap().as_deref(), Some("main"));
        assert_eq!(log(&d), "edit a\na\nbase");
    }

    #[test]
    fn reword_survives_a_conflict_and_continue() {
        let (_tmp, d, base) = repo();
        let g = |args: &[&str]| git::run(&d, args).unwrap();
        g(&["reset", "-q", "--hard", &base]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "a"]);
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        g(&["commit", "-qam", "edit a"]);
        let out = run(&d, &base, |items| {
            items.swap(0, 1);
            items[0].action = TodoAction::Reword;
            items[0].message = "edited first".into();
        });
        assert!(matches!(out, MergeOutcome::Conflicts(_)), "{out:?}");
        // Resolve and Continue (as the banner does); then "a" conflicts too.
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        g(&["add", "a.txt"]);
        let out = actions::continue_op(&d, Operation::Rebase).unwrap();
        assert!(matches!(out, MergeOutcome::Conflicts(_)), "{out:?}");
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "a.txt"]);
        let out = actions::continue_op(&d, Operation::Rebase).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(in_progress(&d).unwrap(), None);
        assert_eq!(log(&d), "a\nedited first\nbase");
    }

    #[test]
    fn skipping_a_reworded_commit_leaves_the_one_before_alone() {
        let (_tmp, d, base) = repo();
        let g = |args: &[&str]| git::run(&d, args).unwrap();
        g(&["reset", "-q", "--hard", &base]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "a"]);
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        g(&["commit", "-qam", "edit a"]);
        let out = run(&d, &base, |items| {
            items.swap(0, 1);
            items[0].action = TodoAction::Reword;
            items[0].message = "edited first".into();
        });
        assert!(matches!(out, MergeOutcome::Conflicts(_)), "{out:?}");
        // Skip "edit a": its amend must not rename "base", which HEAD is now.
        let out = actions::skip_op(&d, Operation::Rebase).unwrap();
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(log(&d), "a\nbase");
    }

    #[test]
    fn message_lines_starting_with_a_hash_are_kept() {
        let (_tmp, d, base) = repo();
        run(&d, &base, |items| {
            items[2].action = TodoAction::Reword;
            items[2].message = "c\n\n#123 stays".into();
        });
        assert_eq!(
            actions::commit_message(&d, "HEAD").unwrap(),
            "c\n\n#123 stays"
        );
    }

    #[test]
    fn a_commit_left_empty_is_kept() {
        let (_tmp, d, base) = repo();
        let g = |args: &[&str]| git::run(&d, args).unwrap();
        g(&["reset", "-q", "--hard", &base]);
        for (v, msg) in [("1", "one"), ("2", "two"), ("1", "back to one")] {
            std::fs::write(d.join("f.txt"), format!("{v}\n")).unwrap();
            g(&["add", "."]);
            g(&["commit", "-q", "-m", msg]);
        }
        let out = run(&d, &base, |items| items[1].action = TodoAction::Drop);
        assert_eq!(out, MergeOutcome::Done);
        assert_eq!(log(&d), "back to one\none\nbase");
        assert_eq!(in_progress(&d).unwrap(), None);
    }

    #[test]
    fn refuses_merges_in_the_range() {
        let (_tmp, d, base) = repo();
        let g = |args: &[&str]| git::run(&d, args).unwrap();
        g(&["switch", "-q", "-c", "side", &base]);
        g(&["commit", "-q", "--allow-empty", "-m", "side"]);
        g(&["switch", "-q", "main"]);
        g(&["merge", "-q", "--no-ff", "--no-edit", "side"]);
        let err = plan(&d, &base).unwrap_err();
        assert!(err.to_string().contains("merge commit"), "{err}");
    }
}
