mod branch_drop;
mod changes;
mod conflict_view;
mod dialogs;
mod diff_view;
mod graph_view;
mod in_progress;
mod merge_view;
mod new_tab;
mod paths;
mod sidebar;
mod spaces;
mod theme;
mod toolbar;
mod worktrees_view;

use crate::actions::{self, CheckoutOutcome, MergeOutcome, Operation};
use crate::config::{Config, PullStrategy, Space};
use crate::diff::FileDiff;
use crate::graph::{Ancestry, LogStream, Row};
use crate::project::{self, Worktree};
use crate::refs::{self, Branch, Tag};
use crate::stage::Direction;
use crate::stash::{self, Stash};
use crate::status::{self, FileStatus, Status};
use crate::{conflict, creds, git, mergetool};
use dialogs::{
    BranchMode, CloneDialog, CreateDialog, DeleteBranchDialog, Dialogs, NewWorktreeDialog,
};
use eframe::egui::{self, Margin};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// Commits fetched per history page.
pub const PAGE: usize = 500;

/// Pages a `LogStream` on a worker thread. Dropping the pager closes the request channel,
/// the worker exits and the stream's Drop kills git.
pub struct LogPager {
    req: Sender<usize>,
    pages: Receiver<Result<Vec<Row>, String>>,
    asked: usize,
    pub pending: bool,
    pub done: bool,
}

impl LogPager {
    pub fn start(dir: PathBuf, ctx: egui::Context) -> LogPager {
        let (req, req_rx) = mpsc::channel::<usize>();
        let (tx, pages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut log = match LogStream::open(&dir) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Err(format!("{e:#}")));
                    ctx.request_repaint();
                    return;
                }
            };
            while let Ok(n) = req_rx.recv() {
                let page = log.next_page(n).map_err(|e| format!("{e:#}"));
                if tx.send(page).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        LogPager {
            req,
            pages,
            asked: 0,
            pending: false,
            done: false,
        }
    }

    /// Ask for `n` more rows unless a page is in flight or history is exhausted.
    pub fn request(&mut self, n: usize) {
        if self.pending || self.done {
            return;
        }
        if self.req.send(n).is_ok() {
            self.asked = n;
            self.pending = true;
        } else {
            self.done = true; // worker already exited (open failed)
        }
    }

    /// A finished page, if one has arrived.
    pub fn poll(&mut self) -> Option<Result<Vec<Row>, String>> {
        match self.pages.try_recv() {
            Ok(page) => {
                self.pending = false;
                self.done |= page.as_ref().map_or(true, |rows| rows.len() < self.asked);
                Some(page)
            }
            Err(TryRecvError::Disconnected) => {
                self.pending = false;
                self.done = true;
                None
            }
            Err(TryRecvError::Empty) => None,
        }
    }
}

/// What the centre panel shows.
pub enum Center {
    Graph,
    /// One side (unstaged or staged) of a working-copy file, with line selection.
    Diff(Box<diff_view::WorkDiff>),
    /// A file of the selected commit, read-only.
    CommitFile(Box<FileDiff>),
    /// A conflicted working-copy file with its conflict blocks, read-only.
    Conflict(Box<conflict_view::ConflictView>),
    /// The built-in merge tool on a conflicted file.
    Merge(Box<merge_view::MergeEditor>),
    /// A file a worktree changed since it branched, read-only, with where the change lives
    /// ("committed", "uncommitted"…).
    Review(Box<FileDiff>, String),
}

pub struct ProjectTab {
    root: PathBuf,
    worktrees: Vec<Worktree>,
    active_worktree: PathBuf,
    branches: Vec<Branch>,
    /// Newest first.
    tags: Vec<Tag>,
    stashes: Vec<Stash>,
    log: LogPager,
    rows: Vec<Row>,
    selected_commit: Option<usize>,
    /// Files changed by `selected_commit`.
    commit_files: Vec<FileDiff>,
    center: Center,
    status: Status,
    commit_summary: String,
    commit_desc: String,
    /// "Amend previous commit" is ticked: Commit replaces HEAD instead.
    amend: bool,
    /// Sidebar filter text.
    filter: String,
    /// Which sidebar sections are open and how tall each is.
    sections: sidebar::Sections,
    /// The merge, rebase, cherry-pick or revert stopped in `active_worktree`, if any.
    operation: Option<Operation>,
    /// The commit HEAD is detached at in `active_worktree`, if it is.
    detached: Option<String>,
    /// The branch to go back to from a detached HEAD.
    previous_branch: Option<String>,
    /// The commit HEAD points at, and which of `rows` are on its history (`on_head[i]`).
    head: Option<String>,
    /// `refs::stamp` of `active_worktree` when its branches were loaded, to tell on focus
    /// whether anything outside Spinney moved HEAD or a branch.
    refs_stamp: Option<String>,
    ancestry: Ancestry,
    on_head: Vec<bool>,
    /// Rows ⌘-clicked into a multi-selection (for cherry-picking several commits).
    picked: Vec<usize>,
    /// The pull strategy `active_worktree`'s git config asks for (`pull.rebase`, `pull.ff`).
    git_pull: PullStrategy,
    /// The commit graph, or the Worktrees view.
    view: worktrees_view::View,
    /// The Worktrees view's data, loaded while it shows.
    review: worktrees_view::Review,
}

/// `<git common dir>` of a project: its `.git`.
fn common_dir(root: &Path) -> anyhow::Result<PathBuf> {
    let out = git::run(root, &["rev-parse", "--git-common-dir"])?;
    Ok(root.join(out.trim()))
}

/// An error's first line for the bar, and the rest (git's own text) for Details.
fn error_lines(e: &str) -> (&str, Option<&str>) {
    match e.split_once('\n') {
        Some((line, rest)) if !rest.trim().is_empty() => (line, Some(rest.trim())),
        _ => (e.trim_end(), None),
    }
}

/// Whether what git says `now` differs from what a tab `loaded`. Git failing to answer
/// (`None`) counts as unchanged, so a hiccup doesn't restart history paging on every focus.
fn changed_outside<T: PartialEq>(loaded: &T, now: Option<&T>) -> bool {
    now.is_some_and(|now| now != loaded)
}

fn needs_commit_graph(common: &Path) -> bool {
    !common.join("objects/info/commit-graph").exists()
}

impl ProjectTab {
    /// Open `root`, configure it for `space` and start paging history. Non-fatal problems
    /// are pushed to `errors`; only a non-project folder fails.
    fn open(
        root: &Path,
        space: &Space,
        ctx: &egui::Context,
        jobs: &mut Jobs,
        errors: &mut Vec<String>,
    ) -> anyhow::Result<ProjectTab> {
        project::ensure_project(root)?;
        match common_dir(root) {
            Ok(common) => {
                if let Err(e) = creds::configure_repo(&common, space) {
                    errors.push(format!("configuring {}: {e:#}", root.display()));
                }
                if needs_commit_graph(&common) {
                    jobs.commit_graph(root.to_path_buf(), common);
                }
            }
            Err(e) => errors.push(format!("{}: {e:#}", root.display())),
        }
        // Relink worktrees an agent or the user moved, before listing them.
        if let Err(e) = project::repair(root) {
            errors.push(format!("repairing worktrees: {e:#}"));
        }
        let worktrees = match project::list_worktrees(root) {
            Ok(w) => w.into_iter().filter(|w| !w.is_bare).collect(),
            Err(e) => {
                errors.push(format!("worktrees: {e:#}"));
                vec![]
            }
        };
        let active_worktree = worktrees
            .first()
            .map_or_else(|| root.to_path_buf(), |w: &Worktree| w.path.clone());
        let mut tab = ProjectTab {
            root: root.to_path_buf(),
            worktrees,
            log: LogPager::start(active_worktree.clone(), ctx.clone()),
            active_worktree,
            branches: vec![],
            tags: vec![],
            stashes: vec![],
            rows: vec![],
            selected_commit: None,
            commit_files: vec![],
            center: Center::Graph,
            status: Status::default(),
            commit_summary: String::new(),
            commit_desc: String::new(),
            amend: false,
            filter: String::new(),
            sections: sidebar::Sections::default(),
            operation: None,
            detached: None,
            previous_branch: None,
            head: None,
            refs_stamp: None,
            ancestry: Ancestry::default(),
            on_head: vec![],
            picked: vec![],
            git_pull: PullStrategy::default(),
            view: worktrees_view::View::default(),
            review: worktrees_view::Review::default(),
        };
        tab.log.request(PAGE);
        tab.load_branches(errors);
        tab.load_stashes(errors);
        tab.reload_changes(errors);
        Ok(tab)
    }

    fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into(),
        )
    }

    /// The window is back: reload what may have changed meanwhile. A changed worktree list
    /// (an agent added, removed or moved one), or a HEAD or branch tip moved by a commit,
    /// checkout or reset outside Spinney, needs the full refresh; otherwise only the active
    /// worktree's changes and stashes, so history paging isn't restarted.
    fn refocus(&mut self, ctx: &egui::Context, errors: &mut Vec<String>) {
        let listed = project::list_worktrees(&self.root)
            .map(|w| w.into_iter().filter(|w| !w.is_bare).collect::<Vec<_>>());
        if changed_outside(&self.worktrees, listed.ok().as_ref())
            || changed_outside(
                &self.refs_stamp,
                refs::stamp(&self.active_worktree).ok().map(Some).as_ref(),
            )
        {
            return self.refresh(ctx, errors);
        }
        self.reload_changes(errors);
        self.load_stashes(errors);
    }

    /// Reload worktrees and branches (fast, local) and restart history paging.
    fn refresh(&mut self, ctx: &egui::Context, errors: &mut Vec<String>) {
        match project::list_worktrees(&self.root) {
            Ok(w) => self.worktrees = w.into_iter().filter(|w| !w.is_bare).collect(),
            Err(e) => errors.push(format!("worktrees: {e:#}")),
        }
        // Not listed, or its folder is gone (moved while Spinney ran): use the first that's
        // there, or every git call below fails.
        let here = |w: &Worktree| !w.prunable && w.path.is_dir();
        if !self
            .worktrees
            .iter()
            .any(|w| w.path == self.active_worktree && here(w))
        {
            if let Some(w) = self.worktrees.iter().find(|w| here(w)) {
                self.active_worktree = w.path.clone();
            }
        }
        self.load_branches(errors);
        self.load_stashes(errors);
        self.log = LogPager::start(self.active_worktree.clone(), ctx.clone());
        self.log.request(PAGE);
        self.rows.clear();
        self.picked.clear();
        self.selected_commit = None;
        self.commit_files.clear();
        self.reload_changes(errors);
        if self.view == worktrees_view::View::Worktrees {
            self.review.load(&self.root, &self.worktrees);
            // Re-read the open file, or close it if the worktree no longer changes it.
            if let Center::Review(fd, _) = &self.center {
                let path = fd.path.clone();
                self.center = Center::Graph;
                let at = self.review.listed().position(|p| p == path);
                if let Some(i) = at {
                    open_review_file(self, i, errors);
                }
            }
        } else if matches!(self.center, Center::Review(..)) {
            self.center = Center::Graph; // only the Worktrees view shows one
        }
    }

    /// Branches, tags and HEAD: what a checkout, fetch or commit can change.
    fn load_branches(&mut self, errors: &mut Vec<String>) {
        match refs::branches(&self.active_worktree) {
            Ok(b) => self.branches = b,
            Err(e) => errors.push(format!("branches: {e:#}")),
        }
        match refs::tags(&self.active_worktree) {
            Ok(t) => self.tags = t,
            Err(e) => errors.push(format!("tags: {e:#}")),
        }
        let wt = &self.active_worktree;
        self.refs_stamp = refs::stamp(wt).ok();
        self.head = actions::head_commit(wt).unwrap_or_else(|e| {
            errors.push(format!("HEAD: {e:#}"));
            None
        });
        self.detached = actions::detached_head(wt).unwrap_or_else(|e| {
            errors.push(format!("HEAD: {e:#}"));
            None
        });
        self.previous_branch = match &self.detached {
            None => None,
            Some(_) => actions::previous_branch(wt).unwrap_or_default(),
        };
        // Per branch (`branch.<name>.rebase`), so it is read again with the branches.
        self.git_pull = actions::git_pull_strategy(wt).unwrap_or_else(|e| {
            errors.push(format!("pull config: {e:#}"));
            PullStrategy::default()
        });
    }

    /// What Pull does without choosing: config.toml's `pull_strategy`, else git's config.
    fn pull_default(&self, config: &Config) -> PullStrategy {
        config.pull_strategy.unwrap_or(self.git_pull)
    }

    fn load_stashes(&mut self, errors: &mut Vec<String>) {
        match stash::list(&self.active_worktree) {
            Ok(s) => self.stashes = s,
            Err(e) => errors.push(format!("stashes: {e:#}")),
        }
    }

    /// Run a stash action in `wt`, refresh, and return a notice to show (if any).
    /// Apply/Pop/DropConfirmed name the stash by hash and re-resolve its current index.
    fn stash_action(
        &mut self,
        action: Action,
        wt: &Path,
        ctx: &egui::Context,
        errors: &mut Vec<String>,
    ) -> Option<String> {
        let conflicted = |wt: &Path| -> Vec<String> {
            status::status(wt)
                .map(|s| s.conflicted.into_iter().map(|f| f.path).collect())
                .unwrap_or_default()
        };
        // Only apply/pop can add conflicts; skip the status scan for the others.
        let applying = matches!(action, Action::ApplyStash(_) | Action::PopStash(_));
        let before = if applying { conflicted(wt) } else { vec![] };
        let mut notice = None;
        let mut applies = false;
        let (pop, drop) = (
            matches!(action, Action::PopStash(_)),
            matches!(action, Action::DropStashConfirmed(_)),
        );
        let res = match action {
            Action::Stash => stash::save(wt, None).map(|saved| {
                if !saved {
                    notice = Some("Nothing to stash".to_string());
                }
            }),
            Action::ApplyStash(h) | Action::PopStash(h) | Action::DropStashConfirmed(h) => {
                applies = !drop;
                match stash::index_of(wt, &h) {
                    Ok(Some(i)) if drop => stash::drop(wt, i),
                    Ok(Some(i)) if pop => stash::pop(wt, i),
                    Ok(Some(i)) => stash::apply(wt, i),
                    Ok(None) => {
                        self.refresh(ctx, errors);
                        return Some("Stash list changed — refreshed".to_string());
                    }
                    Err(e) => Err(e),
                }
            }
            _ => return None,
        };
        self.refresh(ctx, errors);
        if let Err(e) = res {
            // A conflicted apply/pop exits non-zero; name the files it conflicted.
            let new_conflicts: Vec<String> = self
                .status
                .conflicted
                .iter()
                .filter(|f| !before.contains(&f.path))
                .map(|f| f.path.clone())
                .collect();
            if applies && !new_conflicts.is_empty() {
                notice = Some(dialogs::stash_conflict_notice(&new_conflicts));
            } else {
                errors.push(format!("{e:#}"));
            }
        }
        notice
    }

    /// Append any page that arrived; returns an error message from the worker.
    fn poll_log(&mut self) -> Option<String> {
        match self.log.poll()? {
            Ok(rows) => {
                if self.rows.is_empty() {
                    self.ancestry = Ancestry::new(self.head.as_deref());
                    self.on_head.clear();
                }
                let anc = &mut self.ancestry;
                self.on_head
                    .extend(rows.iter().map(|r| anc.mark(&r.commit)));
                self.rows.extend(rows);
                None
            }
            Err(e) => Some(format!("history: {e}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum JobKind {
    Clone,
    Fetch,
    Pull,
    Push,
    /// `git push <remote> --delete`, run like a push.
    DeleteRemote,
    /// Renaming a branch on its remote after renaming it locally, run like a push.
    RenameRemote,
    /// `git push origin refs/tags/<name>`.
    PushTag,
    CommitGraph,
    /// An external merge tool, open until the user closes it.
    MergeTool,
}

struct JobDone {
    kind: JobKind,
    /// The project the job is about (for a clone, the folder being created).
    root: PathBuf,
    space_id: String,
    result: anyhow::Result<()>,
}

/// Background git work (network, clone, commit-graph). Workers wake the UI when done, so
/// nothing repaints while idle.
pub struct Jobs {
    tx: Sender<JobDone>,
    rx: Receiver<JobDone>,
    /// Kind and project of each job in flight.
    active: Vec<(JobKind, PathBuf)>,
    ctx: egui::Context,
}

impl Jobs {
    fn new(ctx: egui::Context) -> Jobs {
        let (tx, rx) = mpsc::channel();
        Jobs {
            tx,
            rx,
            active: vec![],
            ctx,
        }
    }

    fn spawn(
        &mut self,
        kind: JobKind,
        root: PathBuf,
        space_id: String,
        work: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.active.push((kind, root.clone()));
        std::thread::spawn(move || {
            let result = work();
            let _ = tx.send(JobDone {
                kind,
                root,
                space_id,
                result,
            });
            ctx.request_repaint();
        });
    }

    /// `git commit-graph write --reachable` keeps `--topo-order` fast on huge repos.
    fn commit_graph(&mut self, root: PathBuf, common: PathBuf) {
        self.spawn(JobKind::CommitGraph, root, String::new(), move || {
            git::run(&common, &["commit-graph", "write", "--reachable"]).map(|_| ())
        });
    }

    fn try_recv(&mut self) -> Option<JobDone> {
        let done = self.rx.try_recv().ok()?;
        if let Some(i) = self
            .active
            .iter()
            .position(|(k, r)| *k == done.kind && *r == done.root)
        {
            self.active.swap_remove(i);
        }
        Some(done)
    }

    fn any(&self, kind: JobKind) -> bool {
        self.active.iter().any(|(k, _)| *k == kind)
    }

    fn running(&self, kind: JobKind, root: &Path) -> bool {
        self.active.iter().any(|(k, r)| *k == kind && r == root)
    }

    /// A fetch, pull, push, remote branch or tag delete, remote rename or tag push is in
    /// flight for `root`.
    fn network_busy(&self, root: &Path) -> bool {
        [
            JobKind::Fetch,
            JobKind::Pull,
            JobKind::Push,
            JobKind::DeleteRemote,
            JobKind::RenameRemote,
            JobKind::PushTag,
        ]
        .into_iter()
        .any(|k| self.running(k, root))
    }
}

/// What the red confirmation bar asks to throw away. Staged changes always stay.
#[derive(Clone, Debug, PartialEq)]
pub enum Discard {
    /// Every unstaged change.
    All,
    /// One unstaged file: back to the index, or deleted when untracked.
    File(FileStatus),
    /// `n` selected lines (or a hunk) of `path`'s unstaged diff, as a `stage::discard_patch`.
    Lines {
        path: String,
        patch: String,
        n: usize,
    },
}

/// A conflict resolution that drops work, asked in the top bar before it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolve {
    /// Keep one version of the file, losing the other side and any edits to it.
    Use(FileStatus, actions::Version),
    /// Mark the file resolved although it still has conflict markers.
    Anyway(String),
}

/// Deferred UI intents, applied after rendering so closures need not borrow `self`.
pub enum Action {
    SwitchSpace(String),
    NewSpace,
    EditSpace,
    SelectTab(usize),
    CloseTab(usize),
    /// Show the New Tab page; `CloseNewTab` hides it again.
    NewTab,
    CloseNewTab,
    /// Pick a project folder with the native picker and open it.
    OpenFolder,
    CloneUrl,
    CreateRepo,
    /// Open a known project, or focus its tab.
    OpenProject(PathBuf),
    /// Drop a project from the space (closing its tab if open).
    RemoveRecent(PathBuf),
    Fetch,
    /// Pull the active worktree's upstream with this strategy.
    Pull(PullStrategy),
    Push,
    /// Stash all changes (untracked included).
    Stash,
    /// Stash commit hashes, so a stale list can never hit another stash.
    ApplyStash(String),
    PopStash(String),
    /// Ask to drop a stash (hash, label): opens the confirm dialog.
    RequestDropStash(String, String),
    /// Drop after confirmation; only the dialog sends this.
    DropStashConfirmed(String),
    Checkout(String),
    NewWorktreeFrom(String),
    /// Ask before deleting a local branch (refused when a worktree has it checked out).
    AskDeleteBranch(String),
    /// Ask before deleting `remote/branch` on its remote.
    AskDeleteRemote(String),
    /// Ask before deleting the branches shown in a sidebar folder (local, or on the remote).
    AskDeleteFolder {
        folder: String,
        branches: Vec<String>,
        remote: bool,
    },
    /// Ask for a new name for a local branch (refused when another worktree has it out).
    AskRename(String),
    /// Move a local branch checked out nowhere up to its upstream (fast-forward only).
    FastForward(String),
    /// Move the first branch (checked out nowhere) up to the second, fast-forward only.
    FastForwardTo(String, String),
    /// A branch was dropped onto another (dragged, target, where): opens the drop menu.
    DropBranch(String, String, egui::Pos2),
    /// Ask how to merge the branch (local or remote) into the checked-out one.
    AskMerge(String),
    /// Ask before rebasing the checked-out branch onto the branch (local or remote).
    AskRebase(String),
    /// New branch from the given base, switched to.
    NewBranch(String),
    /// Open Terminal.app at the active worktree.
    OpenTerminal,
    ActivateWorktree(PathBuf),
    RemoveWorktree(PathBuf),
    /// Point a missing worktree at its new folder (`git worktree repair`).
    LocateWorktree(PathBuf),
    /// Ask before removing a worktree and deleting its branch in one step.
    AskRemoveWithBranch(PathBuf),
    SelectCommit(usize),
    DeselectCommit,
    OpenDiff(FileStatus, bool),
    /// Show a conflicted file (by path) with its conflict blocks.
    OpenConflict(String),
    /// Built-in merge tool: open it on a conflicted file, pick (or un-pick) a conflict's
    /// side, `StartOverMerge` with no typing to lose, and Save. `AskMergeEditor` asks on
    /// the editor's bar before losing work; `ConfirmMergeEditor` goes ahead and
    /// `CancelMergeEditor` doesn't.
    OpenMerge(String),
    PickConflict(usize, conflict::Pick),
    StartOverMerge,
    SaveMerge,
    AskMergeEditor(merge_view::Ask),
    ConfirmMergeEditor,
    CancelMergeEditor,
    OpenCommitFile(usize),
    /// Stage the file, or unstage it when the flag is set.
    StageFile(FileStatus, bool),
    ApplyPatch(String, Direction),
    StageAll,
    UnstageAll,
    /// Show the confirmation bar; `ConfirmDiscard` (from that bar) does it, `CancelDiscard` hides it.
    AskDiscard(Discard),
    ConfirmDiscard,
    CancelDiscard,
    /// From the rejected-push bar: `AskForcePush` asks to confirm, `ForcePush` does it
    /// (with lease), `DismissPush` hides the bar.
    AskForcePush,
    ForcePush,
    DismissPush,
    /// Tick or untick "Amend previous commit".
    SetAmend(bool),
    /// Commit or amend; amending a pushed HEAD asks first (`ConfirmAmend` / `CancelAmend`).
    Commit,
    ConfirmAmend,
    CancelAmend,
    ToggleInline,
    CloseDiff,
    /// The toolbar's Git | Worktrees toggle.
    SetView(worktrees_view::View),
    /// List what a worktree changed since it branched.
    ReviewWorktree(PathBuf),
    /// Show one of the listed files: an index into `review.commit_files` when a commit is
    /// picked, else into `review.changes`.
    ReviewFile(usize),
    /// "Open in Git view": make a worktree active and show its graph.
    OpenInGit(PathBuf),
    /// The Worktrees view's Refresh: reload worktrees, standings and agents.
    RefreshWorktrees,
    /// Pick a commit in the Worktrees view's graph (an index into `review.commits`), or None
    /// for all changes since the branch point.
    ReviewCommit(Option<usize>),
    /// From the in-progress banner: `ContinueOp` goes on; `AskOpStep` asks before a Skip
    /// or Abort, which `ConfirmOpStep` runs and `CancelOpStep` drops.
    ContinueOp,
    AskOpStep(in_progress::Step),
    ConfirmOpStep,
    CancelOpStep,
    /// Open a worktree file (by path) in its default app.
    OpenFile(String),
    /// Mark a conflicted file resolved; with conflict markers left, asks first.
    MarkResolved(String),
    /// Merge a conflicted file in the configured external tool, as a job.
    OpenInMergeTool(String),
    /// Ask before a resolution that drops work; `ConfirmResolve` runs it, `CancelResolve` hides it.
    AskResolve(Resolve),
    ConfirmResolve,
    CancelResolve,
    /// Put text (a commit's SHA) on the clipboard.
    CopyText(String),
    /// Copy the full message of the commit with this SHA.
    CopyCommitMessage(String),
    /// Check out the commit with this SHA, detaching HEAD.
    CheckoutCommit(String),
    /// Push the tag to origin, as a job.
    PushTag(String),
    /// Ask before deleting the tag, locally and maybe on origin.
    AskDeleteTag(Tag),
    /// ⌘-click on a row: add it to the multi-selection, or take it out.
    TogglePick(usize),
    /// Cherry-pick these commits (SHAs, oldest first) onto the checked-out branch.
    CherryPick(Vec<String>),
    /// Revert the commit with this SHA; a merge commit first asks which parent to keep.
    Revert(String),
    /// Open the interactive rebase editor for the commits after the one with this SHA.
    AskInteractiveRebase(String),
    /// Ask before resetting the checked-out branch to the commit with this SHA.
    AskReset(String, actions::ResetMode),
    /// "Create branch here…" on the detached-HEAD banner: a new branch at HEAD, switched to.
    AttachBranch,
    /// "Create branch here…" on a commit: the new-branch dialog with the commit as base,
    /// which creates and switches.
    BranchAt(String),
    /// "Create tag here…" on a commit (its SHA) or a branch (its full ref): the tag dialog.
    TagAt(String),
}

/// A notice about the operation stopped in `worktree`, shown under its banner.
struct StoppedNotice {
    worktree: PathBuf,
    op: Operation,
    text: String,
}

/// Show `text` for the `op` stopped in `worktree`, in place of that worktree's earlier
/// one: a rebase stopping again on its next commit replaces the last commit's notice.
fn note_stopped(notices: &mut Vec<StoppedNotice>, worktree: &Path, op: Operation, text: String) {
    notices.retain(|n| n.worktree != worktree);
    notices.push(StoppedNotice {
        worktree: worktree.to_path_buf(),
        op,
        text,
    });
}

/// Drop the notices whose operation has ended, however it ended (continued, skipped past,
/// aborted, or finished outside Spinney). `shown` is each tab's worktree and the operation
/// in progress there; a worktree no tab shows keeps its notice, since nothing has looked.
fn prune_stopped(notices: &mut Vec<StoppedNotice>, shown: &[(&Path, Option<Operation>)]) {
    notices.retain(|n| {
        shown
            .iter()
            .filter(|(wt, _)| *wt == n.worktree)
            .all(|(_, op)| *op == Some(n.op))
    });
}

pub struct SpinneyApp {
    config: Config,
    /// None when the config file failed to load, so a broken file is never overwritten.
    config_path: Option<PathBuf>,
    tabs: Vec<ProjectTab>,
    active_tab: usize,
    jobs: Jobs,
    errors: Vec<String>,
    /// Non-error banner messages.
    notices: Vec<String>,
    /// "… stopped on conflicts" notices, at most one per worktree, until that operation ends.
    stopped: Vec<StoppedNotice>,
    dialogs: Dialogs,
    /// The New Tab page is showing over the open tabs (it always shows when there are none).
    new_tab: bool,
    /// New Tab page search text.
    recent_search: String,
    ctx: egui::Context,
    /// Window focus last frame; status reloads when it is regained.
    focused: bool,
    /// Root of the tab whose discard is awaiting confirmation in the top bar, and what it drops.
    confirm_discard: Option<(PathBuf, Discard)>,
    /// Root of the tab whose amend of a pushed HEAD is awaiting confirmation.
    confirm_amend: Option<PathBuf>,
    /// The last push rejected as non-fast-forward, offering Pull or Force push… in the top bar.
    rejected_push: Option<RejectedPush>,
    /// A Skip or Abort of the operation in progress, awaiting confirmation in its banner.
    confirm_op: Option<in_progress::Pending>,
    /// Root of the tab whose conflict resolution is awaiting confirmation, and what it is.
    confirm_resolve: Option<(PathBuf, Resolve)>,
}

struct RejectedPush {
    root: PathBuf,
    rejected: actions::PushRejected,
    /// Set by Force push…: the lease to confirm, snapshotted from the tracking ref then.
    lease: Option<actions::Lease>,
}

impl SpinneyApp {
    pub fn new(ctx: egui::Context) -> SpinneyApp {
        theme::apply(&ctx);
        let path = Config::path();
        let (config, config_path, errors) = match Config::load_from(&path) {
            Ok(c) => (c, Some(path), vec![]),
            Err(e) => (
                Config::default(),
                None,
                vec![format!("{e:#} (settings will not be saved)")],
            ),
        };
        let mut app = SpinneyApp {
            config,
            config_path,
            tabs: vec![],
            active_tab: 0,
            jobs: Jobs::new(ctx.clone()),
            errors,
            notices: vec![],
            stopped: vec![],
            dialogs: Dialogs::default(),
            new_tab: false,
            recent_search: String::new(),
            ctx,
            focused: true,
            confirm_discard: None,
            confirm_amend: None,
            rejected_push: None,
            confirm_op: None,
            confirm_resolve: None,
        };
        app.open_visible_tabs();
        app
    }

    fn save_config(&mut self) {
        if let Some(path) = &self.config_path {
            if let Err(e) = self.config.save_to(path) {
                self.errors.push(format!("saving settings: {e:#}"));
            }
        }
    }

    fn space(&self) -> Option<Space> {
        active_space(&self.config).cloned()
    }

    /// Reopen the active space's open tabs in order, with the saved one active.
    fn open_visible_tabs(&mut self) {
        self.tabs.clear();
        self.active_tab = 0;
        let Some(space) = self.space() else { return };
        for root in visible_projects(&self.config) {
            // Missing (deleted, or on an unmounted drive): no tab, no error, kept in `open`.
            if root.exists() {
                self.open_tab(&root, &space);
            }
        }
        if let Some(i) = space
            .active
            .and_then(|a| self.tabs.iter().position(|t| t.root == a))
        {
            self.active_tab = i;
        }
        self.remember_active();
    }

    /// Open a tab for `root`, make it active and add it to the space's open tabs.
    fn open_tab(&mut self, root: &Path, space: &Space) {
        match ProjectTab::open(root, space, &self.ctx, &mut self.jobs, &mut self.errors) {
            Ok(tab) => {
                self.tabs.push(tab);
                self.active_tab = self.tabs.len() - 1;
                self.new_tab = false;
                if note_open(&mut self.config, &space.id, root) {
                    self.save_config();
                }
            }
            Err(e) => self.errors.push(format!("{e:#}")),
        }
    }

    /// Save which tab is active, so the next launch restores it.
    fn remember_active(&mut self) {
        let Some(space) = self.space() else { return };
        let root = self.tabs.get(self.active_tab).map(|t| t.root.clone());
        if note_active(&mut self.config, &space.id, root.as_deref()) {
            self.save_config();
        }
    }

    fn tab_mut(&mut self, root: &Path) -> Option<&mut ProjectTab> {
        self.tabs.iter_mut().find(|t| t.root == root)
    }

    fn poll(&mut self) {
        for i in 0..self.tabs.len() {
            if let Some(e) = self.tabs[i].poll_log() {
                self.errors.push(e);
            }
        }
        while let Some(done) = self.jobs.try_recv() {
            self.job_done(done);
        }
    }

    fn job_done(&mut self, done: JobDone) {
        let JobDone {
            kind,
            root,
            space_id,
            result,
        } = done;
        if let Err(e) = result {
            // Left in progress: refresh so the banner and the Conflicted list show it.
            let e = match e.downcast::<actions::PullConflicts>() {
                Ok(c) => {
                    self.refresh_tab(&root);
                    let what = format!("Pull ({})", c.strategy.label().to_lowercase());
                    let text = dialogs::conflict_message(&what, &c.files);
                    return note_stopped(&mut self.stopped, &c.worktree, c.operation(), text);
                }
                Err(e) => e,
            };
            if kind == JobKind::MergeTool {
                // The tool may have resolved some of it; show what's left.
                self.refresh_tab(&root);
            }
            match e.downcast::<actions::PushRejected>() {
                Ok(rejected) => {
                    self.rejected_push = Some(RejectedPush {
                        root,
                        rejected,
                        lease: None,
                    })
                }
                // One line, with git's own text behind it for Details.
                Err(e) => match e.downcast_ref::<actions::SyncError>() {
                    Some(s) => self.errors.push(format!("{}\n{}", s.message, s.raw)),
                    None => self.errors.push(format!("{kind:?} failed: {e:#}")),
                },
            }
            return;
        }
        match kind {
            JobKind::Clone => {
                // Separate job: a commit-graph failure must not orphan the finished clone.
                self.jobs.commit_graph(root.clone(), root.join(".git"));
                if add_project(&mut self.config, &space_id, &root) {
                    self.save_config();
                }
                let space = self.space().filter(|s| s.id == space_id);
                if let Some(space) = space {
                    self.open_tab(&root, &space);
                }
            }
            JobKind::Fetch
            | JobKind::Pull
            | JobKind::Push
            | JobKind::DeleteRemote
            | JobKind::RenameRemote
            | JobKind::PushTag
            | JobKind::MergeTool => {
                if kind == JobKind::Fetch {
                    if let Ok(common) = common_dir(&root) {
                        self.jobs.commit_graph(root.clone(), common);
                    }
                }
                self.refresh_tab(&root);
            }
            JobKind::CommitGraph => {}
        }
    }

    fn refresh_tab(&mut self, root: &Path) {
        let (ctx, mut errors) = (self.ctx.clone(), vec![]);
        if let Some(tab) = self.tab_mut(root) {
            tab.refresh(&ctx, &mut errors);
        }
        self.errors.extend(errors);
    }

    /// Open `path` in the configured merge tool as a job; it ends when the tool closes.
    fn open_in_merge_tool(&mut self, path: String) {
        let tool = self.config.merge_tool;
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        if self.jobs.running(JobKind::MergeTool, &root) {
            return self
                .errors
                .push("A merge tool is already open for this project.".into());
        }
        if self.jobs.network_busy(&root) {
            return self
                .errors
                .push("Wait for the running fetch, pull or push".into());
        }
        let work = move || mergetool::run(&wt, tool, &path);
        self.jobs
            .spawn(JobKind::MergeTool, root, String::new(), work);
    }

    /// Start a network job for the active tab.
    fn network(&mut self, kind: JobKind) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        let work: Box<dyn FnOnce() -> anyhow::Result<()> + Send> = match kind {
            JobKind::Fetch => match common_dir(&root) {
                Ok(common) => Box::new(move || {
                    actions::fetch(&common).map_err(|e| actions::explain_sync(&common, e))
                }),
                Err(e) => return self.errors.push(format!("{e:#}")),
            },
            _ => Box::new(move || actions::push(&wt).map_err(|e| actions::explain_sync(&wt, e))),
        };
        self.jobs.spawn(kind, root, String::new(), work);
    }

    /// Pull the active tab's worktree as a job. Conflicts come back as `PullConflicts`.
    fn pull(&mut self, strategy: PullStrategy) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        self.jobs.spawn(
            JobKind::Pull,
            root,
            String::new(),
            move || match actions::pull(&wt, strategy) {
                Ok(MergeOutcome::Conflicts(files)) => Err(actions::PullConflicts {
                    worktree: wt,
                    strategy,
                    files,
                }
                .into()),
                Ok(_) => Ok(()),
                Err(e) => Err(actions::explain_sync(&wt, e)),
            },
        );
    }

    fn apply(&mut self, action: Action) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        match action {
            Action::SwitchSpace(id) => {
                if switch_space(&mut self.config, &id) {
                    self.save_config();
                    self.open_visible_tabs();
                }
            }
            Action::NewSpace => self.dialogs.space = Some(spaces::SpaceEdit::new_space()),
            Action::EditSpace => {
                self.dialogs.space = self.space().as_ref().map(spaces::SpaceEdit::open)
            }
            Action::SelectTab(i) => {
                self.active_tab = i;
                self.confirm_discard = None;
                self.confirm_amend = None;
                self.confirm_op = None;
                self.confirm_resolve = None;
                self.new_tab = false;
                if let Some(tab) = self.tabs.get_mut(i) {
                    tab.reload_changes(&mut errors);
                }
                self.remember_active();
            }
            Action::CloseTab(i) => self.close_tab(i), // the project stays in Recent
            Action::RemoveRecent(root) => {
                if let Some(space) = self.space() {
                    if remove_project(&mut self.config, &space.id, &root) {
                        self.save_config();
                    }
                }
                if let Some(i) = self.tabs.iter().position(|t| t.root == root) {
                    self.close_tab(i);
                }
            }
            Action::NewTab => {
                self.new_tab = true;
                self.recent_search.clear();
            }
            Action::CloseNewTab => self.new_tab = false,
            Action::OpenFolder => {
                let start = self.space().and_then(|s| s.default_clone_dir);
                if let Some(root) = paths::pick_folder("Open a project folder", start.as_deref()) {
                    self.open_folder(root);
                    ctx.request_repaint(); // show the opened tab now, not on the next input
                }
            }
            Action::OpenProject(root) => self.open_folder(root),
            Action::CloneUrl => self.dialogs.clone = self.space().as_ref().map(CloneDialog::new),
            Action::CreateRepo => {
                self.dialogs.create = self.space().as_ref().map(CreateDialog::new)
            }
            Action::Fetch => self.network(JobKind::Fetch),
            Action::Pull(strategy) => {
                self.rejected_push = None;
                self.pull(strategy)
            }
            Action::Push => {
                self.rejected_push = None;
                self.network(JobKind::Push)
            }
            Action::AskForcePush => {
                if let Some(r) = &mut self.rejected_push {
                    match actions::lease(&r.rejected.worktree) {
                        Ok(lease) if lease.branch == r.rejected.branch => r.lease = Some(lease),
                        // Switched branch since the push: never force push a different one.
                        Ok(_) => {
                            self.rejected_push = None;
                            errors.push("force push: the branch changed since the push".into());
                        }
                        Err(e) => errors.push(format!("force push: {e:#}")),
                    }
                }
            }
            Action::ForcePush => {
                if let Some(RejectedPush {
                    root,
                    lease: Some(lease),
                    ..
                }) = self.rejected_push.take()
                {
                    let work = move || {
                        actions::force_push(&lease)
                            .map_err(|e| actions::explain_sync(&lease.worktree, e))
                    };
                    self.jobs.spawn(JobKind::Push, root, String::new(), work);
                }
            }
            Action::DismissPush => self.rejected_push = None,
            Action::Stash | Action::ApplyStash(_) | Action::PopStash(_) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    // A pull or push may be rewriting the same files (the toolbar disables it).
                    if self.jobs.network_busy(&tab.root) {
                        errors.push("stash: wait for the running fetch, pull or push".into());
                    } else {
                        let wt = tab.active_worktree.clone();
                        let notice = tab.stash_action(action, &wt, &ctx, &mut errors);
                        self.notices.extend(notice);
                    }
                }
            }
            Action::DropStashConfirmed(_) => {} // sent by the confirm dialog only
            Action::RequestDropStash(hash, label) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    self.dialogs.confirm_drop_stash = Some(dialogs::DropStash {
                        root: tab.root.clone(),
                        worktree: tab.active_worktree.clone(),
                        hash,
                        label,
                    });
                }
            }
            Action::ToggleInline => {
                self.config.diff_inline = !self.config.diff_inline;
                self.save_config();
            }
            Action::AskDiscard(what) => {
                self.confirm_discard = self
                    .tabs
                    .get(self.active_tab)
                    .map(|t| (t.root.clone(), what))
            }
            Action::CancelDiscard => self.confirm_discard = None,
            Action::ConfirmDiscard => {
                // Only for the tab that asked; the bar is hidden on other tabs anyway.
                let what = self.confirm_discard.as_ref().map(|(_, w)| w.clone());
                if let (Some(what), Some(tab)) = (what, self.discard_tab()) {
                    tab.discard(&what, &mut errors);
                }
                self.confirm_discard = None;
            }
            Action::Commit => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    // Amending pushed history asks first; so does a failed check.
                    let pushed = tab.amend
                        && actions::head_pushed(&tab.active_worktree).unwrap_or_else(|e| {
                            errors.push(format!("upstream: {e:#}"));
                            true
                        });
                    if pushed {
                        self.confirm_amend = Some(tab.root.clone());
                    } else {
                        tab.commit(&ctx, &mut errors);
                    }
                }
            }
            Action::CancelAmend => self.confirm_amend = None,
            Action::ContinueOp => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    if let Some(op) = tab.operation {
                        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
                        self.op_step(&root, &wt, op, None, &mut errors);
                    }
                }
            }
            Action::AskOpStep(step) => {
                let tab = self.tabs.get(self.active_tab);
                self.confirm_op = tab.and_then(|t| {
                    Some(in_progress::Pending {
                        root: t.root.clone(),
                        worktree: t.active_worktree.clone(),
                        op: t.operation?,
                        step,
                    })
                });
            }
            Action::CancelOpStep => self.confirm_op = None,
            Action::OpenFile(path) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.open_file(&path, &mut errors);
                }
            }
            Action::OpenInMergeTool(path) => self.open_in_merge_tool(path),
            Action::MarkResolved(path) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    if tab.mark_resolved(&path, false, &mut errors) {
                        self.confirm_resolve = Some((tab.root.clone(), Resolve::Anyway(path)));
                    }
                }
            }
            Action::AskResolve(what) => {
                self.confirm_resolve = self
                    .tabs
                    .get(self.active_tab)
                    .map(|t| (t.root.clone(), what))
            }
            Action::CancelResolve => self.confirm_resolve = None,
            Action::ConfirmResolve => {
                if let Some((root, what)) = self.confirm_resolve.take() {
                    if let Some(tab) = self.asking_tab(Some(root)) {
                        tab.resolve(&what, &mut errors);
                    }
                }
            }
            Action::ConfirmOpStep => {
                if let Some(p) = self.confirm_op.take() {
                    if self.asking_tab(Some(p.root.clone())).is_some() {
                        self.op_step(&p.root, &p.worktree, p.op, Some(p.step), &mut errors);
                    }
                }
            }
            Action::ConfirmAmend => {
                if let Some(tab) = self.asking_tab(self.confirm_amend.clone()) {
                    tab.commit(&ctx, &mut errors);
                }
                self.confirm_amend = None;
            }
            Action::SaveMerge => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    if let Some(path) = tab.save_merge(&mut errors) {
                        self.confirm_resolve = Some((tab.root.clone(), Resolve::Anyway(path)));
                    }
                }
            }
            Action::SelectCommit(_)
            | Action::TogglePick(_)
            | Action::DeselectCommit
            | Action::OpenDiff(..)
            | Action::OpenConflict(_)
            | Action::OpenMerge(_)
            | Action::PickConflict(..)
            | Action::StartOverMerge
            | Action::AskMergeEditor(_)
            | Action::ConfirmMergeEditor
            | Action::CancelMergeEditor
            | Action::OpenCommitFile(_)
            | Action::StageFile(..)
            | Action::ApplyPatch(..)
            | Action::StageAll
            | Action::UnstageAll
            | Action::SetAmend(_)
            | Action::CloseDiff
            | Action::SetView(_)
            | Action::ReviewWorktree(_)
            | Action::ReviewFile(_)
            | Action::ReviewCommit(_) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab_action(tab, action, &mut errors);
                }
            }
            Action::NewWorktreeFrom(base) => {
                self.dialogs.new_worktree = Some(NewWorktreeDialog {
                    base,
                    branch: String::new(),
                    mode: BranchMode::Worktree,
                });
            }
            Action::AskDeleteBranch(branch) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    // Refuse now rather than after the confirmation.
                    match actions::ensure_not_checked_out(&tab.active_worktree, &branch) {
                        Ok(()) => {
                            let remote = format!("origin/{branch}");
                            let has_remote =
                                tab.branches.iter().any(|b| b.remote && b.name == remote);
                            self.dialogs.delete_branch = Some(DeleteBranchDialog {
                                branch,
                                remote: has_remote.then_some(remote),
                                also_remote: false,
                                not_merged: false,
                            });
                        }
                        Err(e) => errors.push(format!("{e:#}")),
                    }
                }
            }
            Action::AskDeleteRemote(name) => self.dialogs.delete_remote = Some(name),
            Action::AskDeleteFolder {
                folder,
                branches,
                remote,
            } => self.ask_delete_folder(folder, branches, remote, &mut errors),
            Action::PushTag(name) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
                    if self.jobs.network_busy(&root) {
                        errors.push("Wait for the running fetch, pull or push".into());
                    } else {
                        let work = move || actions::push_tag(&wt, "origin", &name);
                        self.jobs.spawn(JobKind::PushTag, root, String::new(), work);
                    }
                }
            }
            Action::AskDeleteTag(tag) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    self.dialogs.delete_tag = Some(dialogs::DeleteTagDialog {
                        root: tab.root.clone(),
                        worktree: tab.active_worktree.clone(),
                        tag,
                        also_remote: false,
                    });
                }
            }
            Action::AskRename(branch) => self.ask_rename(branch, &mut errors),
            Action::FastForward(branch) => self.fast_forward(branch, None, &mut errors),
            Action::FastForwardTo(branch, to) => self.fast_forward(branch, Some(to), &mut errors),
            Action::DropBranch(dragged, target, pos) => {
                self.drop_branch(dragged, target, pos, &mut errors)
            }
            Action::AskMerge(branch) => self.ask_merge(branch, false, &mut errors),
            Action::AskRebase(branch) => self.ask_merge(branch, true, &mut errors),
            Action::NewBranch(base) => {
                self.dialogs.new_worktree = Some(NewWorktreeDialog {
                    base,
                    branch: String::new(),
                    mode: BranchMode::Switch,
                });
            }
            Action::CherryPick(shas) => self.cherry_pick(&shas, &mut errors),
            Action::Revert(sha) => self.ask_revert(sha, &mut errors),
            Action::AskInteractiveRebase(sha) => self.ask_interactive(&sha, &mut errors),
            Action::AskReset(sha, mode) => self.ask_reset(&sha, mode, &mut errors),
            Action::AttachBranch => {
                let head = self
                    .tabs
                    .get(self.active_tab)
                    .and_then(|t| t.detached.clone());
                if let Some(head) = head {
                    self.dialogs.new_worktree = Some(NewWorktreeDialog {
                        base: head,
                        branch: String::new(),
                        mode: BranchMode::Switch,
                    });
                }
            }
            Action::BranchAt(sha) => {
                self.dialogs.new_worktree = Some(NewWorktreeDialog {
                    base: sha,
                    branch: String::new(),
                    mode: BranchMode::Switch,
                });
            }
            Action::TagAt(target) => self.ask_tag(target, &mut errors),
            Action::CopyText(text) => ctx.copy_text(text),
            Action::CopyCommitMessage(sha) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    match actions::commit_message(&tab.active_worktree, &sha) {
                        Ok(m) => ctx.copy_text(m),
                        Err(e) => errors.push(format!("{e:#}")),
                    }
                }
            }
            Action::CheckoutCommit(sha) => {
                if self.ask_leave(LeaveTo::Commit(sha.clone()), &mut errors) {
                    // Asked first: commits only this detached HEAD has would be left behind.
                } else if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    // A pull or push may be rewriting the same files (the toolbar disables it).
                    if self.jobs.network_busy(&tab.root) {
                        errors.push("Wait for the running fetch, pull or push".into());
                    } else {
                        if let Err(e) = actions::checkout_detached(&tab.active_worktree, &sha) {
                            errors.push(format!("{e:#}"));
                        }
                        tab.refresh(&ctx, &mut errors);
                    }
                }
            }
            Action::OpenTerminal => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    if let Err(e) = actions::open_terminal(&tab.active_worktree) {
                        errors.push(format!("{e:#}"));
                    }
                }
            }
            Action::Checkout(name) => {
                if !self.ask_leave(LeaveTo::Branch(name.clone()), &mut errors) {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        checkout(tab, &name, &mut errors);
                        tab.refresh(&ctx, &mut errors);
                    }
                }
            }
            Action::RefreshWorktrees => {
                if let Some(root) = self.tabs.get(self.active_tab).map(|t| t.root.clone()) {
                    self.refresh_tab(&root);
                }
            }
            Action::OpenInGit(path) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.view = worktrees_view::View::Git;
                    tab.center = Center::Graph; // a review diff belongs to the Worktrees view
                }
                self.apply(Action::ActivateWorktree(path));
            }
            Action::ActivateWorktree(path) => {
                self.confirm_discard = None; // it was asked about the other worktree's files
                self.confirm_op = None;
                self.confirm_resolve = None;
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    if !path.is_dir() {
                        errors.push(format!(
                            "{} is missing: locate or remove it",
                            path.display()
                        ));
                    }
                    tab.active_worktree = path;
                    tab.refresh(&ctx, &mut errors); // falls back if it's missing
                }
            }
            Action::AskRemoveWithBranch(path) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let root = tab.root.clone();
                    // Refuse now (busy, detached, out elsewhere) rather than after asking.
                    let plan = if self.jobs.network_busy(&root) {
                        Err(anyhow::anyhow!("Wait for the running fetch, pull or push"))
                    } else {
                        actions::plan_remove_with_branch(&root, &path)
                    };
                    match plan {
                        Ok(plan) => {
                            self.dialogs.remove_with_branch =
                                Some(dialogs::RemoveWithBranchDialog { root, plan })
                        }
                        Err(e) => errors.push(format!("{e:#}")),
                    }
                }
            }
            Action::LocateWorktree(path) => {
                let start = path.parent().map(Path::to_path_buf);
                let picked = paths::pick_folder("Where is this worktree now?", start.as_deref());
                if let (Some(new), Some(tab)) = (picked, self.tabs.get_mut(self.active_tab)) {
                    if let Err(e) = project::relink(&tab.root, &[new]) {
                        errors.push(format!("{e:#}"));
                    }
                    tab.refresh(&ctx, &mut errors);
                }
                ctx.request_repaint();
            }
            Action::RemoveWorktree(path) => {
                // Missing: it may have moved, and removing it unlinks it there (#166).
                let ask = !path.is_dir()
                    || git::run(&path, &["status", "--porcelain"])
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                if ask {
                    // Named in the confirmation; a missing one has none to lose.
                    let files = match path.is_dir() {
                        true => status::status(&path)
                            .map(|s| s.changed_paths())
                            .unwrap_or_default(),
                        false => vec![],
                    };
                    self.dialogs.confirm_remove = Some((path, files));
                } else {
                    self.remove_worktree(&path, false, &mut errors);
                }
            }
        }
        self.errors.extend(errors);
        self.prune_stopped();
    }

    /// Drop the stopped-on-conflicts notices of operations the tabs no longer show.
    fn prune_stopped(&mut self) {
        let shown: Vec<_> = self
            .tabs
            .iter()
            .map(|t| (t.active_worktree.as_path(), t.operation))
            .collect();
        prune_stopped(&mut self.stopped, &shown);
    }

    /// Fast-forward `branch` to `to`, or to its upstream when `to` is None.
    fn fast_forward(&mut self, branch: String, to: Option<String>, errors: &mut Vec<String>) {
        let ctx = self.ctx.clone();
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        // A fetch may be moving the upstream it goes to.
        if self.jobs.network_busy(&tab.root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let wt = tab.active_worktree.clone();
        let res = match &to {
            Some(to) => actions::fast_forward_to(&wt, &branch, to),
            None => actions::fast_forward(&wt, &branch),
        };
        tab.refresh(&ctx, errors);
        match res {
            Ok(MergeOutcome::UpToDate) => {
                self.notices.push(format!("{branch} is already up to date"))
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
    }

    /// Open the drop menu for `dragged` dropped onto `target` in the active tab.
    fn drop_branch(
        &mut self,
        dragged: String,
        target: String,
        pos: egui::Pos2,
        errors: &mut Vec<String>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let wt = &tab.active_worktree;
        let current = match refs::current_branch(wt) {
            Ok(c) => c,
            Err(e) => return errors.push(format!("{e:#}")),
        };
        let remote = |name: &str| tab.branches.iter().any(|b| b.remote && b.name == name);
        let target_remote = remote(&target);
        let checked_out = tab
            .worktrees
            .iter()
            .any(|w| w.branch.as_deref() == Some(&target));
        let fast_forward = !target_remote
            && !checked_out
            && actions::can_fast_forward(wt, &target, &dragged).unwrap_or(false);
        let facts = branch_drop::DropFacts {
            current: current.as_deref(),
            dragged_remote: remote(&dragged),
            target_remote,
            fast_forward,
        };
        let choices = branch_drop::drop_choices(&dragged, &target, facts);
        self.dialogs.drop_menu = Some(branch_drop::DropMenu {
            root: tab.root.clone(),
            dragged,
            target,
            choices,
            pos,
        });
    }

    /// Close tab `i`; its project leaves the open tabs but stays in Recent.
    fn close_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            let tab = self.tabs.remove(i);
            self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
            if let Some(space) = self.space() {
                if note_closed(&mut self.config, &space.id, &tab.root) {
                    self.save_config();
                }
            }
            self.remember_active();
        }
    }

    fn remove_worktree(&mut self, path: &Path, force: bool, errors: &mut Vec<String>) {
        let ctx = self.ctx.clone();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Err(e) = project::remove_worktree(&tab.root, path, force) {
                errors.push(format!("{e:#}"));
            }
            tab.refresh(&ctx, errors);
        }
    }

    /// Save the space editor; re-configure every project of an edited space.
    fn save_space(&mut self, edit: spaces::SpaceEdit) {
        let id = match edit.apply(&mut self.config) {
            Ok(id) => id,
            Err(e) => {
                self.errors.push(e);
                self.dialogs.space = Some(edit);
                return;
            }
        };
        match edit.token_save() {
            spaces::TokenSave::Keep => {}
            spaces::TokenSave::Store(token) => {
                if let Err(e) = creds::set_token(&id, token) {
                    self.errors.push(format!("saving token: {e}"));
                }
            }
            spaces::TokenSave::Delete => {
                if let Err(e) = creds::delete_token(&id) {
                    self.errors
                        .push(format!("removing the old host's token: {e}"));
                }
            }
        }
        self.save_config();
        if let Some(space) = self.config.spaces.iter().find(|s| s.id == id).cloned() {
            for root in &space.projects {
                let res = common_dir(root).and_then(|c| creds::configure_repo(&c, &space));
                if let Err(e) = res {
                    self.errors
                        .push(format!("configuring {}: {e:#}", root.display()));
                }
            }
        }
        if edit.id.is_none() {
            self.open_visible_tabs(); // the new space is now active
        }
    }

    /// Whether the centre shows the New Tab page instead of the active project.
    fn showing_new_tab(&self) -> bool {
        self.new_tab || self.tabs.is_empty()
    }

    /// The active tab, when it is the one with a pending discard.
    fn discard_tab(&mut self) -> Option<&mut ProjectTab> {
        let root = self.confirm_discard.as_ref().map(|(root, _)| root.clone());
        self.asking_tab(root)
    }

    /// The active tab, when it is `asking` (the root of a pending confirmation).
    fn asking_tab(&mut self, asking: Option<PathBuf>) -> Option<&mut ProjectTab> {
        let root = asking?;
        if self.showing_new_tab() {
            return None;
        }
        self.tabs
            .get_mut(self.active_tab)
            .filter(|t| t.root == root)
    }

    /// Continue the operation stopped in `worktree`, or with `step` skip or abort it, then
    /// refresh. Stopping again on the next commit's conflicts is a notice, not an error.
    fn op_step(
        &mut self,
        root: &Path,
        worktree: &Path,
        op: Operation,
        step: Option<in_progress::Step>,
        errors: &mut Vec<String>,
    ) {
        // A pull or push may be moving the same branch (the banner disables it too).
        if self.jobs.network_busy(root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let ctx = self.ctx.clone();
        let tab = self.tabs.iter_mut().find(|t| t.root == root);
        let Some(tab) = tab.filter(|t| t.active_worktree == worktree) else {
            return;
        };
        let res = match step {
            None => actions::continue_op(worktree, op),
            Some(in_progress::Step::Skip) => actions::skip_op(worktree, op),
            Some(in_progress::Step::Abort) => {
                actions::abort_op(worktree, op).map(|()| MergeOutcome::Done)
            }
        };
        tab.refresh(&ctx, errors);
        match res {
            Ok(MergeOutcome::Conflicts(files)) => {
                let text = dialogs::conflict_message(&in_progress::title(op), &files);
                note_stopped(&mut self.stopped, worktree, op, text)
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{op}: {e:#}")),
        }
    }

    /// "The last commit is already pushed" with Amend / Cancel.
    fn amend_bar(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("⚠").color(theme::WARN));
            ui.label(
                egui::RichText::new(
                    "The last commit is already pushed. Amending rewrites history.",
                )
                .color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Cancel").clicked() {
                    actions.push(Action::CancelAmend);
                }
                let amend = egui::Button::new(
                    egui::RichText::new("Amend")
                        .color(theme::ON_ACCENT)
                        .strong(),
                )
                .fill(theme::WARN)
                .small();
                if ui.add(amend).clicked() {
                    actions.push(Action::ConfirmAmend);
                }
            });
        });
    }

    /// The discard question (e.g. "Discard all N changes?") with Discard / Cancel.
    fn discard_bar(ui: &mut egui::Ui, question: &str, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🗑").color(theme::DELETED));
            ui.label(
                egui::RichText::new(format!("{question} This can't be undone.")).color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Cancel").clicked() {
                    actions.push(Action::CancelDiscard);
                }
                let discard = egui::Button::new(
                    egui::RichText::new("Discard")
                        .color(theme::ON_ACCENT)
                        .strong(),
                )
                .fill(theme::DELETED)
                .small();
                if ui.add(discard).clicked() {
                    actions.push(Action::ConfirmDiscard);
                }
            });
        });
    }

    /// The resolve question (e.g. "Use your version of a.txt?") with its button / Cancel.
    fn resolve_bar(ui: &mut egui::Ui, question: &str, button: &str, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("⚠").color(theme::DELETED));
            ui.label(egui::RichText::new(question).color(theme::TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Cancel").clicked() {
                    actions.push(Action::CancelResolve);
                }
                let go =
                    egui::Button::new(egui::RichText::new(button).color(theme::ON_ACCENT).strong())
                        .fill(theme::DELETED)
                        .small();
                if ui.add(go).clicked() {
                    actions.push(Action::ConfirmResolve);
                }
            });
        });
    }

    /// The rejected push, when it was for the active tab's active worktree.
    fn push_tab(&self) -> Option<&RejectedPush> {
        let r = self.rejected_push.as_ref()?;
        if self.showing_new_tab() {
            return None;
        }
        let tab = self.tabs.get(self.active_tab)?;
        (tab.root == r.root && tab.active_worktree == r.rejected.worktree).then_some(r)
    }

    /// "Push rejected" with Pull / Force push…, then the force push confirmation.
    fn push_bar(
        ui: &mut egui::Ui,
        r: &RejectedPush,
        busy: bool,
        pull: PullStrategy,
        actions: &mut Vec<Action>,
    ) {
        ui.horizontal(|ui| {
            let branch = &r.rejected.branch;
            let text = match &r.lease {
                None => format!(
                    "Push of {branch} was rejected: the remote has commits this branch doesn't."
                ),
                Some(l) => force_push_question(l),
            };
            let icon = if r.lease.is_some() { "⚠" } else { "↑" };
            ui.label(egui::RichText::new(icon).color(theme::WARN));
            ui.label(egui::RichText::new(text).color(theme::TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Cancel").clicked() {
                    actions.push(Action::DismissPush);
                }
                ui.add_enabled_ui(!busy, |ui| {
                    if r.lease.is_some() {
                        let force = egui::Button::new(
                            egui::RichText::new("Force push")
                                .color(theme::ON_ACCENT)
                                .strong(),
                        )
                        .fill(theme::DELETED)
                        .small();
                        if ui.add(force).clicked() {
                            actions.push(Action::ForcePush);
                        }
                    } else {
                        if ui.small_button("Force push…").clicked() {
                            actions.push(Action::AskForcePush);
                        }
                        // The branches have diverged, so offer merge and rebase right here.
                        ui.menu_button("▾", |ui| toolbar::pull_menu(ui, pull, actions))
                            .response
                            .on_hover_text("Pull with…");
                        if ui
                            .small_button("Pull")
                            .on_hover_text(pull.label())
                            .clicked()
                        {
                            actions.push(Action::Pull(pull));
                        }
                    }
                })
                .response
                .on_disabled_hover_text("Waiting for the running job");
            });
        });
    }

    fn error_banner(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = None;
        for (i, e) in self.errors.iter().enumerate() {
            let (line, details) = error_lines(e);
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, line);
                if let Some(details) = details {
                    ui.weak("Details").on_hover_text(details);
                }
                if ui.small_button("Dismiss").clicked() {
                    dismiss = Some(i);
                }
            });
        }
        if let Some(i) = dismiss {
            self.errors.remove(i);
        }
        let mut dismiss = None;
        for (i, n) in self.stopped.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.colored_label(theme::ACCENT_TEXT, &n.text);
                if ui.small_button("Dismiss").clicked() {
                    dismiss = Some(i);
                }
            });
        }
        if let Some(i) = dismiss {
            self.stopped.remove(i);
        }
        let mut dismiss = None;
        for (i, n) in self.notices.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.colored_label(theme::ACCENT_TEXT, n);
                if ui.small_button("Dismiss").clicked() {
                    dismiss = Some(i);
                }
            });
        }
        if let Some(i) = dismiss {
            self.notices.remove(i);
        }
    }
}

/// Changes-panel and diff-view actions on the active tab.
/// Show the selected worktree's `i`th listed file in the diff view: of the picked commit, or
/// of all its changes.
fn open_review_file(tab: &mut ProjectTab, i: usize, errors: &mut Vec<String>) {
    let Some((wt, base)) = tab.review.base() else {
        return;
    };
    let name = tab
        .worktrees
        .iter()
        .find(|w| w.path == wt)
        .map(|w| sidebar::worktree_label(w).0)
        .unwrap_or_default();
    if let Some(c) = tab.review.commit.and_then(|k| tab.review.commits.get(k)) {
        if let Some(fd) = tab.review.commit_files.get(i) {
            let label = worktrees_view::review_label(&c.sha[..7.min(c.sha.len())], &name);
            tab.center = Center::Review(Box::new(fd.clone()), label);
        }
        return;
    }
    let Some(change) = tab.review.changes.get(i).cloned() else {
        return;
    };
    match crate::review::file_since(wt, base, &change) {
        Ok(fd) => {
            let label = worktrees_view::review_label(change.origin.label(), &name);
            tab.center = Center::Review(Box::new(fd), label);
        }
        Err(e) => errors.push(format!("{e:#}")),
    }
}

fn tab_action(tab: &mut ProjectTab, action: Action, errors: &mut Vec<String>) {
    // Anything that would replace the merge editor asks first when it holds work.
    let replaces = matches!(
        action,
        Action::OpenDiff(..)
            | Action::OpenConflict(_)
            | Action::OpenMerge(_)
            | Action::OpenCommitFile(_)
            | Action::SelectCommit(_)
            | Action::CloseDiff
    );
    if let Center::Merge(e) = &mut tab.center {
        if replaces && e.dirty() {
            e.ask = Some(merge_view::Ask::Close);
            return;
        }
    }
    match action {
        Action::SelectCommit(i) => {
            tab.picked.clear();
            tab.select_commit(i, errors)
        }
        Action::TogglePick(i) => tab.picked = toggle_pick(&tab.picked, tab.selected_commit, i),
        Action::DeselectCommit => {
            tab.selected_commit = None;
            tab.picked.clear();
            tab.commit_files.clear();
        }
        Action::OpenDiff(f, staged) => tab.open_diff(f, staged, errors),
        Action::OpenConflict(path) => {
            let view = conflict_view::ConflictView::load(&tab.active_worktree, &path);
            tab.center = Center::Conflict(Box::new(view));
        }
        Action::OpenMerge(path) => match merge_view::MergeEditor::load(&tab.active_worktree, &path)
        {
            Ok(e) => tab.center = Center::Merge(Box::new(e)),
            Err(e) => errors.push(format!("{e:#}")),
        },
        Action::PickConflict(i, pick) => {
            if let Center::Merge(e) = &mut tab.center {
                e.pick(i, pick);
            }
        }
        Action::StartOverMerge => {
            if let Center::Merge(e) = &mut tab.center {
                e.start_over();
            }
        }
        Action::AskMergeEditor(ask) => {
            if let Center::Merge(e) = &mut tab.center {
                e.ask = Some(ask);
            }
        }
        Action::CancelMergeEditor => {
            if let Center::Merge(e) = &mut tab.center {
                e.ask = None;
            }
        }
        Action::ConfirmMergeEditor => {
            if let Center::Merge(e) = &mut tab.center {
                match e.ask.take() {
                    Some(merge_view::Ask::Close) => tab.center = Center::Graph,
                    Some(merge_view::Ask::StartOver) => e.start_over(),
                    None => {}
                }
            }
        }
        Action::OpenCommitFile(k) => tab.open_commit_file(k),
        Action::StageFile(f, unstage) => tab.stage_file(&f, unstage, errors),
        Action::ApplyPatch(p, dir) => tab.apply_patch(&p, dir, errors),
        Action::StageAll => tab.stage_all(false, errors),
        Action::UnstageAll => tab.stage_all(true, errors),
        Action::SetAmend(on) => tab.set_amend(on, errors),
        Action::CloseDiff => tab.center = Center::Graph,
        Action::SetView(view) => {
            tab.view = view;
            tab.center = Center::Graph;
            if view == worktrees_view::View::Worktrees {
                tab.review.load(&tab.root, &tab.worktrees);
            }
        }
        Action::ReviewWorktree(path) => {
            tab.center = Center::Graph;
            tab.review.select(path);
            // Straight to the first file, as a commit's files would be read.
            if !tab.review.changes.is_empty() {
                open_review_file(tab, 0, errors);
            }
        }
        Action::ReviewFile(i) => open_review_file(tab, i, errors),
        Action::ReviewCommit(i) => {
            tab.center = Center::Graph;
            if let Err(e) = tab.review.pick_commit(i) {
                errors.push(format!("{e:#}"));
            }
            open_review_file(tab, 0, errors);
        }
        _ => {}
    }
}

/// The multi-selection after ⌘-clicking row `i`: the selected commit joins it first, so
/// ⌘-clicking a second row selects both.
pub fn toggle_pick(picked: &[usize], selected: Option<usize>, i: usize) -> Vec<usize> {
    let mut out = picked.to_vec();
    if out.is_empty() {
        out.extend(selected.filter(|&s| s != i));
    }
    match out.iter().position(|&k| k == i) {
        Some(at) => {
            out.remove(at);
        }
        None => out.push(i),
    }
    out
}

/// Where leaving a detached HEAD goes once confirmed.
#[derive(Clone, Debug, PartialEq)]
pub enum LeaveTo {
    Branch(String),
    Commit(String),
}

/// The force push confirmation, naming the commits it drops (newest first, the rest
/// counted). Subjects can hold commas, so they're separated by semicolons.
pub fn force_push_question(l: &actions::Lease) -> String {
    let lost = match l.lost {
        0 => "Commits on the remote that aren't on this branch".to_string(),
        1 => format!("1 commit on {} that isn't on this branch", l.tracking),
        n => format!("{n} commits on {} that aren't on this branch", l.tracking),
    };
    let mut named = l.lost_commits.join("; ");
    if !named.is_empty() {
        named = format!(": {named}");
        if l.lost > l.lost_commits.len() {
            named += &format!(" and {} more", l.lost - l.lost_commits.len());
        }
    }
    format!(
        "Force push {}? {lost} will be lost{named}. If someone pushed since your last fetch, \
         the push is refused.",
        l.branch
    )
}

/// "Detached at 1a2b3c4".
pub fn detached_text(head: &str) -> String {
    format!("Detached at {}", &head[..head.len().min(7)])
}

/// "Detached at …" with Create branch here… and Checkout <previous branch>.
fn detached_bar(ui: &mut egui::Ui, head: &str, previous: Option<&str>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("⚠").color(theme::WARN));
        ui.label(egui::RichText::new(detached_text(head)).color(theme::TEXT));
        ui.label(theme::caption(
            "Commits made here belong to no branch until you create one.",
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(b) = previous {
                if ui.small_button(format!("Checkout {b}")).clicked() {
                    actions.push(Action::Checkout(b.to_string()));
                }
            }
            if ui.small_button("Create branch here…").clicked() {
                actions.push(Action::AttachBranch);
            }
        });
    });
}

/// Check out `name` (local, or `remote/branch`) in the tab, jumping to or creating its worktree.
fn checkout(tab: &mut ProjectTab, name: &str, errors: &mut Vec<String>) {
    let remote = tab.branches.iter().any(|b| b.remote && b.name == name);
    let local = if remote {
        local_branch_name(name)
    } else {
        name
    };
    match actions::checkout(&tab.root, &tab.active_worktree, local) {
        Ok(CheckoutOutcome::Switched) => {}
        Ok(CheckoutOutcome::JumpTo(path)) => tab.active_worktree = path,
        Err(e) => errors.push(format!("{e:#}")),
    }
}

impl SpinneyApp {
    /// Before `to` moves a detached HEAD away from commits no branch, tag or remote has,
    /// open the confirmation and return true. A failed check also stops it (true).
    fn ask_leave(&mut self, to: LeaveTo, errors: &mut Vec<String>) -> bool {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return false;
        };
        // From git, not `tab.detached`: commits made outside Spinney since the last refresh
        // count too, and the confirmation is pinned to the HEAD they were counted at.
        let wt = &tab.active_worktree;
        let head = match actions::detached_head(wt) {
            Ok(Some(h)) => h,
            Ok(None) => return false,
            Err(e) => {
                errors.push(format!("detached HEAD: {e:#}"));
                return true;
            }
        };
        match actions::detached_orphans(wt) {
            Ok(0) => false,
            Ok(lost) => {
                self.dialogs.leave_detached = Some(dialogs::LeaveDetached {
                    root: tab.root.clone(),
                    worktree: tab.active_worktree.clone(),
                    head,
                    lost,
                    to,
                });
                true
            }
            Err(e) => {
                errors.push(format!("detached HEAD: {e:#}"));
                true
            }
        }
    }

    /// The in-progress banner for the active tab, or its Skip/Abort confirmation. A
    /// confirmation for that tab whose operation finished or changed meanwhile is dropped.
    fn op_banner(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.showing_new_tab() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let pending = self.confirm_op.as_ref().filter(|p| p.root == tab.root);
        let asking = pending
            .filter(|p| p.worktree == tab.active_worktree && Some(p.op) == tab.operation)
            .map(|p| p.step);
        if pending.is_some() && asking.is_none() {
            self.confirm_op = None;
        }
        let Some(op) = tab.operation else { return };
        let (conflicts, busy) = (
            tab.status.conflicted.len(),
            self.jobs.network_busy(&tab.root),
        );
        let fill = if asking.is_some() {
            theme::DIFF_DEL
        } else {
            theme::WARN.gamma_multiply(0.12)
        };
        let bar = egui::Frame::new()
            .fill(fill)
            .inner_margin(Margin::symmetric(14, 6));
        egui::Panel::top("in_progress").frame(bar).show(ui, |ui| {
            in_progress::bar(ui, op, conflicts, busy, asking, actions)
        });
    }
}

impl eframe::App for SpinneyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        let focused = ui.input(|i| i.focused);
        if focused && !self.focused {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.refocus(&ctx, &mut self.errors);
            }
        }
        self.focused = focused;
        // A refresh or a finished job may have seen an operation end.
        self.prune_stopped();
        let mut actions = vec![];
        let strip = egui::Frame::new().fill(theme::TABS).inner_margin(Margin {
            left: 10,
            right: 12,
            top: 6,
            bottom: 0,
        });
        egui::Panel::top("tabs")
            .frame(strip)
            .show_separator_line(false)
            .show(ui, |ui| self.tab_strip(ui, &mut actions));
        if !self.showing_new_tab() {
            if let Some(tab) = self.tabs.get(self.active_tab) {
                let projects = active_space(&self.config).map_or(&[][..], |s| &s.projects[..]);
                let jobs = &self.jobs;
                let bar = egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(14, 6));
                egui::Panel::top("toolbar").frame(bar).show(ui, |ui| {
                    let busy = jobs.network_busy(&tab.root);
                    let running = |k| jobs.running(k, &tab.root);
                    let pull = tab.pull_default(&self.config);
                    toolbar::toolbar(ui, tab, projects, busy, running, pull, &mut actions)
                });
                // A rebase detaches HEAD too; the in-progress banner covers it.
                if let (Some(head), None) = (&tab.detached, tab.operation) {
                    let warn = egui::Frame::new()
                        .fill(theme::WARN.gamma_multiply(0.12))
                        .inner_margin(Margin::symmetric(14, 6));
                    let previous = tab.previous_branch.as_deref();
                    egui::Panel::top("detached")
                        .frame(warn)
                        .show(ui, |ui| detached_bar(ui, head, previous, &mut actions));
                }
            }
        }
        let question = self.confirm_discard.clone().and_then(|(_, what)| {
            let tab = self.discard_tab()?;
            Some(changes::discard_question(&what, &tab.status, &tab.center))
        });
        match question {
            // Nothing left to discard (e.g. cleaned outside, or the diff was closed).
            Some(None) => self.confirm_discard = None,
            Some(Some(q)) => {
                let bar = egui::Frame::new()
                    .fill(theme::DIFF_DEL)
                    .inner_margin(Margin::symmetric(14, 6));
                egui::Panel::top("discard_confirm")
                    .frame(bar)
                    .show(ui, |ui| Self::discard_bar(ui, &q, &mut actions));
            }
            None => {}
        }
        let resolve = self.confirm_resolve.clone().and_then(|(root, what)| {
            let tab = self.asking_tab(Some(root))?;
            Some(changes::resolve_question(&what, &tab.status, tab.operation))
        });
        match resolve {
            // The file is no longer conflicted.
            Some(None) => self.confirm_resolve = None,
            Some(Some((q, button))) => {
                let bar = egui::Frame::new()
                    .fill(theme::DIFF_DEL)
                    .inner_margin(Margin::symmetric(14, 6));
                egui::Panel::top("resolve_confirm")
                    .frame(bar)
                    .show(ui, |ui| Self::resolve_bar(ui, &q, button, &mut actions));
            }
            None => {}
        }
        let amend = self.confirm_amend.clone();
        if let Some(on) = self.asking_tab(amend).map(|t| t.amend) {
            if !on {
                self.confirm_amend = None; // unticked while asking
            } else {
                let bar = egui::Frame::new()
                    .fill(theme::WARN.gamma_multiply(0.12))
                    .inner_margin(Margin::symmetric(14, 6));
                egui::Panel::top("amend_confirm")
                    .frame(bar)
                    .show(ui, |ui| Self::amend_bar(ui, &mut actions));
            }
        }
        self.op_banner(ui, &mut actions);
        if let Some(r) = self.push_tab() {
            let busy = self.jobs.network_busy(&r.root);
            let pull = self.tabs[self.active_tab].pull_default(&self.config);
            let fill = if r.lease.is_some() {
                theme::DIFF_DEL
            } else {
                theme::PANEL
            };
            let bar = egui::Frame::new()
                .fill(fill)
                .inner_margin(Margin::symmetric(14, 6));
            egui::Panel::top("push_rejected")
                .frame(bar)
                .show(ui, |ui| Self::push_bar(ui, r, busy, pull, &mut actions));
        }
        if !self.errors.is_empty() || !self.notices.is_empty() || !self.stopped.is_empty() {
            egui::Panel::top("errors").show(ui, |ui| self.error_banner(ui));
        }
        let shortcuts = !self.dialogs.any_open();
        let tab = if self.showing_new_tab() {
            None
        } else {
            self.tabs.get_mut(self.active_tab)
        };
        let merge_tool = self.config.merge_tool;
        let tips = branch_drop::row_tips_shown(
            branch_drop::dragging(ui.ctx()),
            self.dialogs.drop_menu.is_some(),
        );
        if let Some(tab) = tab {
            egui::Panel::left("sidebar")
                .frame(theme::side_panel())
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| sidebar::show(ui, tab, tips, &mut actions));
            egui::Panel::right("changes")
                .frame(theme::side_panel())
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| {
                    changes::show(ui, tab, shortcuts, merge_tool, &mut actions)
                });
            let inline = self.config.diff_inline;
            let center = egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(Margin::symmetric(12, 6));
            egui::CentralPanel::default().frame(center).show(ui, |ui| {
                if let Center::Conflict(view) = &tab.center {
                    return conflict_view::show(ui, view, &mut actions);
                }
                if let Center::Merge(e) = &mut tab.center {
                    return merge_view::show(ui, e, &mut actions);
                }
                if !matches!(tab.center, Center::Graph) {
                    return diff_view::show(ui, &mut tab.center, inline, &mut actions);
                }
                if tab.view == worktrees_view::View::Worktrees {
                    return worktrees_view::center_hint(ui, &tab.worktrees, &tab.review);
                }
                let wip = Some(tab.status.changed_files()).filter(|&n| n > 0);
                let remotes = tab
                    .branches
                    .iter()
                    .filter(|b| b.remote)
                    .map(|b| b.name.as_str())
                    .collect();
                let branches = tab.branches.iter().map(|b| b.name.as_str()).collect();
                let names = graph_view::Names {
                    remotes: &remotes,
                    branches: &branches,
                };
                let branch = tab.branches.iter().find(|b| b.head && !b.remote);
                let menu = graph_view::MenuContext {
                    dirty: graph_view::clean_blocked(&tab.status, tab.operation),
                    branch: branch.map(|b| b.name.clone()),
                    operation: tab.operation,
                    on_head: &tab.on_head,
                    picked: &tab.picked,
                };
                let out = graph_view::show(
                    ui,
                    &tab.rows,
                    tab.selected_commit,
                    wip,
                    names,
                    &menu,
                    &mut actions,
                );
                actions.extend(out.drops);
                if let Some(i) = out.toggled {
                    actions.push(Action::TogglePick(i));
                }
                if let Some(i) = out.clicked {
                    actions.push(Action::SelectCommit(i));
                }
                if out.wip_clicked {
                    actions.push(Action::DeselectCommit);
                }
                if out.near_end {
                    tab.log.request(PAGE);
                }
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| match active_space(&self.config) {
                Some(space) => new_tab::show(ui, space, &mut self.recent_search, &mut actions),
                None => {
                    ui.label("Create a space to get started.");
                    if ui.add(theme::primary("New space…")).clicked() {
                        actions.push(Action::NewSpace);
                    }
                }
            });
        }
        branch_drop::preview(&ctx);
        self.dialogs(&ctx);
        for a in actions {
            self.apply(a);
        }
    }
}

/// The active space, falling back to the first one when `active_space` is unset or stale.
pub fn active_space(cfg: &Config) -> Option<&Space> {
    cfg.active_space
        .as_ref()
        .and_then(|id| cfg.spaces.iter().find(|s| &s.id == id))
        .or(cfg.spaces.first())
}

/// Projects whose tabs are visible: the active space's open tabs, not all of Recent.
pub fn visible_projects(cfg: &Config) -> Vec<PathBuf> {
    active_space(cfg).map_or_else(Vec::new, |s| s.open.clone())
}

/// Record that `path` has a tab open in a space (at the end); false if nothing changed.
pub fn note_open(cfg: &mut Config, space_id: &str, path: &Path) -> bool {
    match cfg.spaces.iter_mut().find(|s| s.id == space_id) {
        Some(s) if !s.open.iter().any(|p| p == path) => {
            s.open.push(path.to_path_buf());
            true
        }
        _ => false,
    }
}

/// Record that `path`'s tab closed; it stays in Recent. False if nothing changed.
pub fn note_closed(cfg: &mut Config, space_id: &str, path: &Path) -> bool {
    let Some(s) = cfg.spaces.iter_mut().find(|s| s.id == space_id) else {
        return false;
    };
    let before = s.open.len();
    s.open.retain(|p| p != path);
    let was_active = s.active.as_deref() == Some(path);
    if was_active {
        s.active = None;
    }
    s.open.len() != before || was_active
}

/// Record the active tab's project; false if nothing changed.
pub fn note_active(cfg: &mut Config, space_id: &str, path: Option<&Path>) -> bool {
    match cfg.spaces.iter_mut().find(|s| s.id == space_id) {
        Some(s) if s.active.as_deref() != path => {
            s.active = path.map(Path::to_path_buf);
            true
        }
        _ => false,
    }
}

/// Make `id` the active space; false when no such space exists.
pub fn switch_space(cfg: &mut Config, id: &str) -> bool {
    let found = cfg.spaces.iter().any(|s| s.id == id);
    if found {
        cfg.active_space = Some(id.to_string());
    }
    found
}

/// Add `path` to a space's projects; false if the space is missing or already has it.
pub fn add_project(cfg: &mut Config, space_id: &str, path: &Path) -> bool {
    match cfg.spaces.iter_mut().find(|s| s.id == space_id) {
        Some(s) if !s.projects.iter().any(|p| p == path) => {
            s.projects.push(path.to_path_buf());
            true
        }
        _ => false,
    }
}

/// Remove `path` from a space's projects; false if it was not there.
pub fn remove_project(cfg: &mut Config, space_id: &str, path: &Path) -> bool {
    let Some(s) = cfg.spaces.iter_mut().find(|s| s.id == space_id) else {
        return false;
    };
    let before = s.projects.len();
    s.projects.retain(|p| p != path);
    s.open.retain(|p| p != path);
    if s.active.as_deref() == Some(path) {
        s.active = None;
    }
    s.projects.len() != before
}

/// Default folder name for a clone URL: last path segment without `.git`.
pub fn repo_name_from_url(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("");
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// `origin/feature/x` -> `feature/x`.
pub fn local_branch_name(remote_branch: &str) -> &str {
    remote_branch
        .split_once('/')
        .map_or(remote_branch, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(id: &str, projects: &[&str]) -> Space {
        Space {
            id: id.into(),
            name: id.into(),
            host: "github.com".into(),
            projects: projects.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    pub(super) fn cfg() -> Config {
        Config {
            spaces: vec![
                space("a", &["/tmp/p1", "/tmp/p2"]),
                space("b", &["/tmp/p3"]),
            ],
            active_space: Some("a".into()),
            diff_inline: false,
            ..Default::default()
        }
    }

    #[test]
    fn active_space_falls_back_to_first() {
        let mut c = cfg();
        assert_eq!(active_space(&c).unwrap().id, "a");
        c.active_space = Some("gone".into());
        assert_eq!(active_space(&c).unwrap().id, "a");
        c.active_space = None;
        assert_eq!(active_space(&c).unwrap().id, "a");
        assert!(active_space(&Config::default()).is_none());
    }

    #[test]
    fn switching_space_swaps_visible_projects() {
        let mut c = cfg();
        for s in &mut c.spaces {
            s.open = s.projects.clone();
        }
        assert_eq!(
            visible_projects(&c),
            [PathBuf::from("/tmp/p1"), PathBuf::from("/tmp/p2")]
        );
        assert!(switch_space(&mut c, "b"));
        assert_eq!(c.active_space.as_deref(), Some("b"));
        assert_eq!(visible_projects(&c), [PathBuf::from("/tmp/p3")]);
        assert!(!switch_space(&mut c, "nope"));
        assert_eq!(c.active_space.as_deref(), Some("b"));
        assert!(visible_projects(&Config::default()).is_empty());
    }

    #[test]
    fn add_and_remove_projects_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut c = cfg();
        assert!(add_project(&mut c, "b", Path::new("/tmp/p4")));
        assert!(!add_project(&mut c, "b", Path::new("/tmp/p4")), "dedupe");
        assert!(!add_project(&mut c, "nope", Path::new("/tmp/p4")));
        assert!(remove_project(&mut c, "a", Path::new("/tmp/p1")));
        assert!(!remove_project(&mut c, "a", Path::new("/tmp/p1")));
        c.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert_eq!(back.spaces[0].projects, [PathBuf::from("/tmp/p2")]);
        assert_eq!(
            back.spaces[1].projects,
            [PathBuf::from("/tmp/p3"), PathBuf::from("/tmp/p4")]
        );
    }

    #[test]
    fn open_tabs_are_tracked_apart_from_recent() {
        let mut c = cfg();
        let (p1, p2) = (Path::new("/tmp/p1"), Path::new("/tmp/p2"));
        // An older config has no open tabs: nothing reopens, Recent is untouched.
        assert!(visible_projects(&c).is_empty());
        assert!(note_open(&mut c, "a", p2));
        assert!(note_open(&mut c, "a", p1));
        assert!(!note_open(&mut c, "a", p1), "dedupe");
        assert!(!note_open(&mut c, "nope", p1));
        assert_eq!(visible_projects(&c), [p2, p1], "in the order opened");
        assert!(note_active(&mut c, "a", Some(p1)));
        assert!(!note_active(&mut c, "a", Some(p1)));

        assert!(note_closed(&mut c, "a", p1));
        assert!(!note_closed(&mut c, "a", p1));
        assert_eq!(visible_projects(&c), [p2]);
        assert_eq!(
            c.spaces[0].active, None,
            "a closed tab isn't the active one"
        );
        assert_eq!(c.spaces[0].projects, [p1, p2], "still in Recent");

        // Removing from Recent also forgets the open tab.
        assert!(note_active(&mut c, "a", Some(p2)));
        assert!(remove_project(&mut c, "a", p2));
        assert!(c.spaces[0].open.is_empty());
        assert_eq!(c.spaces[0].active, None);
    }

    #[test]
    fn repo_name_from_urls() {
        assert_eq!(repo_name_from_url("https://github.com/org/foo.git"), "foo");
        assert_eq!(repo_name_from_url("https://github.com/org/foo"), "foo");
        assert_eq!(repo_name_from_url("https://github.com/org/foo/"), "foo");
        assert_eq!(repo_name_from_url("git@github.com:org/foo.git"), "foo");
        assert_eq!(repo_name_from_url("git@github.com:foo.git"), "foo");
        assert_eq!(repo_name_from_url(""), "");
    }

    fn wait(p: &mut LogPager) -> Result<Vec<Row>, String> {
        for _ in 0..500 {
            if let Some(r) = p.poll() {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("pager never answered");
    }

    fn repo(commits: usize) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| crate::git::run(tmp.path(), args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        for i in 0..commits {
            g(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
        }
        tmp
    }

    #[test]
    fn pager_pages_until_done() {
        let tmp = repo(3);
        let mut p = LogPager::start(tmp.path().to_path_buf(), egui::Context::default());
        p.request(2);
        assert!(p.pending);
        p.request(2); // ignored while pending
        let rows = wait(&mut p).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].commit.subject, "c2");
        assert!(!p.pending && !p.done);
        p.request(2);
        assert_eq!(wait(&mut p).unwrap().len(), 1);
        assert!(p.done, "short page means end of history");
        p.request(2);
        assert!(!p.pending, "no requests after done");
    }

    #[test]
    fn pager_empty_repo_is_done() {
        let tmp = repo(0);
        let mut p = LogPager::start(tmp.path().to_path_buf(), egui::Context::default());
        p.request(PAGE);
        assert!(wait(&mut p).unwrap().is_empty());
        assert!(p.done);
    }

    #[test]
    fn common_dir_of_a_normal_clone() {
        let normal = repo(0);
        let got = common_dir(normal.path()).unwrap();
        assert_eq!(
            got.canonicalize().unwrap(),
            normal.path().join(".git").canonicalize().unwrap()
        );
        assert!(needs_commit_graph(&got));
    }

    fn texts(n: &[StoppedNotice]) -> Vec<&str> {
        n.iter().map(|n| n.text.as_str()).collect()
    }

    #[test]
    fn a_worktree_shows_only_its_latest_stop() {
        let (a, b) = (Path::new("/p/proj"), Path::new("/p/proj.worktrees/x"));
        let mut n = vec![];
        note_stopped(&mut n, a, Operation::Rebase, "chapter1".into());
        note_stopped(&mut n, b, Operation::Merge, "other".into());
        note_stopped(&mut n, a, Operation::Rebase, "chapter2".into());
        assert_eq!(texts(&n), ["other", "chapter2"]);
    }

    #[test]
    fn ending_the_operation_clears_its_notice() {
        let (a, b) = (Path::new("/p/proj"), Path::new("/p/proj.worktrees/x"));
        let mut n = vec![];
        note_stopped(&mut n, a, Operation::Merge, "merge".into());
        note_stopped(&mut n, b, Operation::Rebase, "rebase".into());

        // Still stopped, or in no tab: kept.
        prune_stopped(&mut n, &[(a, Some(Operation::Merge))]);
        assert_eq!(texts(&n), ["merge", "rebase"]);
        // Continued, skipped past or aborted: gone, as is one replaced by another kind.
        prune_stopped(&mut n, &[(a, None), (b, Some(Operation::Rebase))]);
        assert_eq!(texts(&n), ["rebase"]);
        prune_stopped(&mut n, &[(b, Some(Operation::CherryPick))]);
        assert!(n.is_empty());
    }

    pub(super) fn test_app(config: Config, cfg_path: &Path) -> SpinneyApp {
        let ctx = egui::Context::default();
        SpinneyApp {
            config,
            config_path: Some(cfg_path.to_path_buf()),
            tabs: vec![],
            active_tab: 0,
            jobs: Jobs::new(ctx.clone()),
            errors: vec![],
            notices: vec![],
            stopped: vec![],
            dialogs: Dialogs::default(),
            new_tab: false,
            recent_search: String::new(),
            ctx,
            focused: true,
            confirm_discard: None,
            confirm_amend: None,
            rejected_push: None,
            confirm_op: None,
            confirm_resolve: None,
        }
    }

    #[test]
    fn closed_tabs_stay_closed_and_open_ones_come_back_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let (a, b, c) = (repo(1), repo(1), repo(1));
        let roots: Vec<PathBuf> = [&a, &b, &c]
            .iter()
            .map(|r| r.path().canonicalize().unwrap())
            .collect();
        let mut app = test_app(cfg(), &cfg_path);
        for root in &roots {
            app.apply(Action::OpenProject(root.clone()));
        }
        app.apply(Action::SelectTab(0));
        app.apply(Action::CloseTab(1));
        assert!(app.errors.is_empty(), "{:?}", app.errors);

        let reload = || {
            let mut app = test_app(Config::load_from(&cfg_path).unwrap(), &cfg_path);
            app.open_visible_tabs();
            assert!(app.errors.is_empty(), "{:?}", app.errors);
            app
        };
        let app = reload();
        let open: Vec<_> = app.tabs.iter().map(|t| t.root.clone()).collect();
        assert_eq!(open, [roots[0].clone(), roots[2].clone()]);
        assert_eq!(app.active_tab, 0, "the active tab is restored");
        assert!(
            app.config.spaces[0].projects.contains(&roots[1]),
            "in Recent"
        );

        // A project deleted outside Spinney opens no tab and shows no error.
        drop(a);
        let app = reload();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].root, roots[2]);
    }

    #[test]
    fn opening_a_worktree_folder_opens_its_project_with_it_active() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let proj = repo(1);
        let root = proj.path().canonicalize().unwrap();
        let agent = root.join(".claude/worktrees/agent-1");
        let agent_str = agent.to_str().unwrap();
        crate::git::run(&root, &["worktree", "add", "-q", "-b", "a", agent_str]).unwrap();
        let mut app = test_app(cfg(), &cfg_path);
        app.apply(Action::OpenProject(agent.clone()));
        assert_eq!(app.tabs.len(), 1, "{:?}", app.errors);
        assert_eq!(app.tabs[0].root, root);
        assert_eq!(app.tabs[0].active_worktree, agent);
        assert!(app.config.spaces[0].projects.contains(&root));
        app.apply(Action::OpenProject(root.clone()));
        assert_eq!(app.tabs.len(), 1, "same tab");
    }

    #[test]
    fn a_worktree_gone_while_open_falls_back_to_one_that_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = repo(1);
        let root = proj.path().canonicalize().unwrap();
        let wt = tmp.path().canonicalize().unwrap().join("wt");
        let wt_str = wt.to_str().unwrap();
        crate::git::run(&root, &["worktree", "add", "-q", "-b", "a", wt_str]).unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        app.apply(Action::OpenProject(root.clone()));
        std::fs::rename(&wt, tmp.path().join("moved")).unwrap();
        app.errors.clear();
        app.apply(Action::ActivateWorktree(wt.clone()));
        assert_eq!(app.tabs[0].active_worktree, root);
        assert_eq!(app.errors.len(), 1, "{:?}", app.errors);
        assert!(app.errors[0].contains("is missing"));
        assert!(app.tabs[0]
            .worktrees
            .iter()
            .any(|w| w.path == wt && w.prunable));
    }

    #[test]
    fn removing_a_missing_worktree_asks_first() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = repo(1);
        let root = proj.path().canonicalize().unwrap();
        let wt = tmp.path().canonicalize().unwrap().join("wt");
        let wt_str = wt.to_str().unwrap();
        crate::git::run(&root, &["worktree", "add", "-q", "-b", "a", wt_str]).unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        app.apply(Action::OpenProject(root.clone()));
        std::fs::rename(&wt, tmp.path().join("moved")).unwrap();
        app.apply(Action::RemoveWorktree(wt.clone()));
        assert_eq!(
            app.dialogs.confirm_remove.as_ref(),
            Some(&(wt.clone(), vec![])),
            "asks"
        );
        let listed = |app: &SpinneyApp| {
            project::list_worktrees(&app.tabs[0].root)
                .unwrap()
                .iter()
                .any(|w| w.path == wt)
        };
        assert!(listed(&app), "not removed yet");
        let mut errors = vec![];
        app.remove_worktree(&wt, true, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!listed(&app), "removed once confirmed");
        let q = dialogs::remove_question(&wt, true, &[]);
        assert!(q.contains("is missing") && q.contains("Locate"), "{q}");
    }

    #[test]
    fn worktrees_view_lists_reviews_and_opens_a_worktree() {
        use worktrees_view::View;
        let tmp = tempfile::tempdir().unwrap();
        let proj = repo(1);
        let root = proj.path().canonicalize().unwrap();
        let agent = root.join(".claude/worktrees/agent-1");
        let agent_str = agent.to_str().unwrap();
        let g = |dir: &Path, args: &[&str]| crate::git::run(dir, args).unwrap();
        g(
            &root,
            &["worktree", "add", "-q", "-b", "agent-1", agent_str],
        );
        std::fs::write(agent.join("done.txt"), "a\nb\n").unwrap();
        g(&agent, &["add", "."]);
        g(&agent, &["commit", "-q", "-m", "agent work"]);
        std::fs::write(agent.join("wip.txt"), "w\n").unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        app.apply(Action::OpenProject(root.clone()));
        assert!(
            app.tabs[0].review.standings.is_empty(),
            "not loaded in the Git view"
        );

        app.apply(Action::SetView(View::Worktrees));
        let tab = &app.tabs[0];
        assert_eq!(tab.view, View::Worktrees);
        let s = tab
            .review
            .standings
            .iter()
            .find(|s| s.path == agent)
            .unwrap();
        assert_eq!((s.base.as_str(), s.counts), ("main", Some((1, 0))));

        app.apply(Action::ReviewWorktree(agent.clone()));
        let paths: Vec<_> = app.tabs[0]
            .review
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.origin.label()))
            .collect();
        assert_eq!(paths, [("done.txt", "committed"), ("wip.txt", "untracked")]);

        let Center::Review(fd, _) = &app.tabs[0].center else {
            panic!("selecting a worktree opens its first file")
        };
        assert_eq!(fd.path, "done.txt");
        app.apply(Action::ReviewFile(1));
        std::fs::write(agent.join("wip.txt"), "w\nmore\n").unwrap();
        app.apply(Action::RefreshWorktrees);
        let Center::Review(fd, _) = &app.tabs[0].center else {
            panic!("the open file survives a refresh")
        };
        assert_eq!((fd.path.as_str(), fd.hunks[0].lines.len()), ("wip.txt", 2));
        app.apply(Action::ReviewFile(0));
        let Center::Review(fd, origin) = &app.tabs[0].center else {
            panic!("no review diff")
        };
        assert_eq!(
            (fd.path.as_str(), origin.as_str()),
            ("done.txt", "committed · in agent-1")
        );
        // The graph: picking the commit lists only its files and opens the first.
        let sha = app.tabs[0].review.commits[0].sha.clone();
        assert_eq!(app.tabs[0].review.commits.len(), 1);
        app.apply(Action::ReviewCommit(Some(0)));
        let listed: Vec<_> = app.tabs[0].review.listed().map(str::to_string).collect();
        assert_eq!(listed, ["done.txt"], "not the untracked wip.txt");
        let Center::Review(fd, origin) = &app.tabs[0].center else {
            panic!("a commit opens its first file")
        };
        let header = format!("{} · in agent-1", &sha[..7]);
        assert_eq!((fd.path.as_str(), origin.as_str()), ("done.txt", &*header));
        app.apply(Action::RefreshWorktrees);
        assert_eq!(
            app.tabs[0].review.commit,
            Some(0),
            "a refresh keeps the commit"
        );
        app.apply(Action::ReviewCommit(None));
        assert_eq!(app.tabs[0].review.listed().count(), 2, "all changes again");

        app.apply(Action::CloseDiff);
        assert!(matches!(app.tabs[0].center, Center::Graph));
        assert_eq!(app.tabs[0].view, View::Worktrees, "back to the list");

        // Open in Git view with a review file open: the graph, not that file (#179).
        app.apply(Action::ReviewFile(0));
        assert!(matches!(app.tabs[0].center, Center::Review(..)));
        app.apply(Action::OpenInGit(agent.clone()));
        assert_eq!(app.tabs[0].view, View::Git);
        assert_eq!(app.tabs[0].active_worktree, agent);
        assert!(
            matches!(app.tabs[0].center, Center::Graph),
            "graph, not review"
        );

        // Nor does switching to the Git view, or worktree once there.
        app.apply(Action::SetView(View::Worktrees));
        app.apply(Action::ReviewWorktree(agent.clone()));
        app.apply(Action::SetView(View::Git));
        assert!(matches!(app.tabs[0].center, Center::Graph));
        app.apply(Action::SetView(View::Worktrees));
        app.apply(Action::ReviewWorktree(agent.clone()));
        app.tabs[0].view = View::Git; // however it got there
        app.apply(Action::ActivateWorktree(root.clone()));
        assert!(matches!(app.tabs[0].center, Center::Graph));
        assert!(app.errors.is_empty(), "{:?}", app.errors);
    }

    #[test]
    fn refocus_picks_up_worktrees_changed_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = repo(1);
        let root = proj.path().canonicalize().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        app.apply(Action::OpenProject(root.clone()));
        let ctx = app.ctx.clone();
        let mut errors = vec![];
        let tab = &mut app.tabs[0];
        tab.selected_commit = Some(0);
        tab.refocus(&ctx, &mut errors);
        assert_eq!(tab.selected_commit, Some(0), "unchanged: no full refresh");

        // An agent adds a worktree while Spinney is in the background.
        let wt = tmp.path().canonicalize().unwrap().join("agent");
        let wt_str = wt.to_str().unwrap();
        crate::git::run(&root, &["worktree", "add", "-q", "-b", "a", wt_str]).unwrap();
        tab.refocus(&ctx, &mut errors);
        assert!(tab.worktrees.iter().any(|w| w.path == wt && !w.prunable));
        assert_eq!(tab.selected_commit, None, "changed: full refresh");

        // Then moves it away.
        std::fs::rename(&wt, tmp.path().join("moved")).unwrap();
        tab.refocus(&ctx, &mut errors);
        assert!(tab.worktrees.iter().any(|w| w.path == wt && w.prunable));
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn refocus_picks_up_head_moved_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = repo(1);
        let root = proj.path().canonicalize().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        app.apply(Action::OpenProject(root.clone()));
        let ctx = app.ctx.clone();
        let mut errors = vec![];
        let tab = &mut app.tabs[0];
        let g = |args: &[&str]| crate::git::run(&root, args).unwrap();
        let sha = |rev: &str| g(&["rev-parse", rev]).trim().to_string();

        // A commit made in the terminal.
        g(&["commit", "-q", "--allow-empty", "-m", "outside"]);
        tab.selected_commit = Some(0);
        tab.refocus(&ctx, &mut errors);
        assert_eq!(tab.selected_commit, None, "changed: full refresh");
        assert_eq!(tab.head, Some(sha("HEAD")));
        assert_eq!(wait(&mut tab.log).unwrap()[0].commit.subject, "outside");

        // A detached checkout at the same commit: no sha moves, but HEAD leaves main.
        g(&["checkout", "-q", "--detach"]);
        tab.selected_commit = Some(0);
        tab.refocus(&ctx, &mut errors);
        assert_eq!(tab.selected_commit, None, "detached: full refresh");
        assert_eq!(tab.detached, Some(sha("HEAD")));
        assert!(tab.branches.iter().all(|b| !b.head));

        // Then a commit on the detached HEAD: the banner names the new one.
        g(&["commit", "-q", "--allow-empty", "-m", "detached"]);
        tab.refocus(&ctx, &mut errors);
        assert_eq!(tab.detached, Some(sha("HEAD")));
        assert_eq!(wait(&mut tab.log).unwrap()[0].commit.subject, "detached");

        tab.selected_commit = Some(0);
        tab.refocus(&ctx, &mut errors);
        assert_eq!(tab.selected_commit, Some(0), "unchanged: no full refresh");
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn changed_outside_ignores_git_failing() {
        assert!(!changed_outside(&1, Some(&1)));
        assert!(changed_outside(&1, Some(&2)));
        assert!(!changed_outside(&1, None), "unreadable counts as unchanged");
    }

    #[test]
    fn close_tab_keeps_recent_and_remove_drops_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let proj = repo(1);
        let root = proj.path().to_path_buf();
        let mut c = cfg();
        c.spaces[0].projects.push(root.clone());
        let mut app = test_app(c, &cfg_path);
        let space = app.space().unwrap();
        app.open_tab(&root, &space);
        assert_eq!(app.tabs.len(), 1, "{:?}", app.errors);

        app.apply(Action::CloseTab(0));
        assert!(app.tabs.is_empty());
        assert!(
            app.config.spaces[0].projects.contains(&root),
            "still recent"
        );

        app.apply(Action::OpenProject(root.clone()));
        assert_eq!(app.tabs.len(), 1);
        app.apply(Action::RemoveRecent(root.clone()));
        assert!(app.tabs.is_empty(), "open tab closed");
        assert!(!app.config.spaces[0].projects.contains(&root));
        let saved = Config::load_from(&cfg_path).unwrap();
        assert!(!saved.spaces[0].projects.contains(&root), "saved");
        assert_eq!(
            saved.spaces[0].projects,
            [PathBuf::from("/tmp/p1"), PathBuf::from("/tmp/p2")]
        );
    }

    #[test]
    fn clone_success_adds_project_even_if_commit_graph_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let mut app = test_app(cfg(), &cfg_path);
        // Not a repo, so the follow-up commit-graph job is bound to fail.
        let root = tmp.path().join("cloned");
        app.job_done(JobDone {
            kind: JobKind::Clone,
            root: root.clone(),
            space_id: "b".into(),
            result: Ok(()),
        });
        assert_eq!(app.jobs.active.len(), 1, "commit-graph runs as its own job");
        let saved = Config::load_from(&cfg_path).unwrap();
        assert!(saved.spaces[1].projects.contains(&root));
        let mut done = None;
        for _ in 0..500 {
            done = app.jobs.try_recv();
            if done.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let done = done.expect("commit-graph job finished");
        assert_eq!(done.kind, JobKind::CommitGraph);
        assert!(done.result.is_err());
        app.job_done(done);
        assert!(app
            .errors
            .iter()
            .any(|e| e.starts_with("CommitGraph failed")));
        assert!(app.config.spaces[1].projects.contains(&root), "still added");
    }

    #[test]
    fn sync_error_is_one_line_with_details() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let raw = "git pull failed: There is no tracking information for the current branch.\n\
                   Please specify which branch you want to merge with.";
        let e = actions::SyncError {
            message: "x has no upstream yet: Push it first, or pull from a branch".into(),
            raw: raw.into(),
        };
        app.job_done(JobDone {
            kind: JobKind::Pull,
            root: tmp.path().to_path_buf(),
            space_id: String::new(),
            result: Err(e.clone().into()),
        });
        let (line, details) = error_lines(&app.errors[0]);
        assert_eq!(line, e.message, "no \"Pull failed:\" in front");
        assert_eq!(details, Some(raw));
    }

    #[test]
    fn error_lines_split_off_details() {
        assert_eq!(error_lines("one line"), ("one line", None));
        assert_eq!(error_lines("one line\n"), ("one line", None));
        assert_eq!(
            error_lines("short\nraw\ntext\n"),
            ("short", Some("raw\ntext"))
        );
    }

    #[test]
    fn rejected_push_shows_bar_instead_of_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let proj = repo(1);
        let rejected = actions::PushRejected {
            worktree: proj.path().to_path_buf(),
            branch: "main".into(),
        };
        app.job_done(JobDone {
            kind: JobKind::Push,
            root: proj.path().to_path_buf(),
            space_id: String::new(),
            result: Err(rejected.clone().into()),
        });
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.rejected_push.as_ref().unwrap().rejected, rejected);

        // No remote-tracking ref to lease against: say so, and do not ask to confirm.
        app.apply(Action::AskForcePush);
        assert!(app.errors[0].contains("fetch first"), "{:?}", app.errors);
        assert!(app.rejected_push.as_ref().unwrap().lease.is_none());
        app.apply(Action::ForcePush);
        assert!(
            app.jobs.active.is_empty(),
            "nothing confirmed, nothing pushed"
        );
        app.apply(Action::DismissPush);
        assert!(app.rejected_push.is_none());
    }

    #[test]
    fn force_push_question_names_the_lost_commits() {
        let mut l = actions::Lease {
            worktree: "/tmp/x".into(),
            branch: "feature/x".into(),
            remote: "origin".into(),
            remote_ref: "refs/heads/feature/x".into(),
            tracking: "origin/feature/x".into(),
            expect: "1a2b3c4d".into(),
            lost: 1,
            lost_commits: vec!["1a2b3c4 Fix the parser, again".into()],
        };
        assert_eq!(
            force_push_question(&l),
            "Force push feature/x? 1 commit on origin/feature/x that isn't on this branch \
             will be lost: 1a2b3c4 Fix the parser, again. If someone pushed since your last \
             fetch, the push is refused."
        );
        l.lost = 7;
        l.lost_commits = (0..5).map(|i| format!("abc000{i} c{i}")).collect();
        assert!(
            force_push_question(&l).contains(
                "7 commits on origin/feature/x that aren't on this branch will be lost: \
                 abc0000 c0; abc0001 c1; abc0002 c2; abc0003 c3; abc0004 c4 and 2 more. If"
            ),
            "{}",
            force_push_question(&l)
        );
        l.lost = 0;
        l.lost_commits.clear();
        assert!(force_push_question(&l).contains("branch will be lost. If"));
    }

    #[test]
    fn jobs_track_what_is_running() {
        let mut jobs = Jobs::new(egui::Context::default());
        let (a, b) = (Path::new("/tmp/a"), Path::new("/tmp/b"));
        let (go, wait) = mpsc::channel::<()>();
        jobs.spawn(JobKind::Pull, a.into(), String::new(), move || {
            let _ = wait.recv();
            Ok(())
        });
        assert!(jobs.running(JobKind::Pull, a) && jobs.any(JobKind::Pull));
        assert!(!jobs.running(JobKind::Push, a) && !jobs.any(JobKind::Clone));
        assert!(jobs.network_busy(a) && !jobs.network_busy(b));
        go.send(()).unwrap();
        let mut done = None;
        for _ in 0..500 {
            done = jobs.try_recv();
            if done.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(done.expect("job finished").kind, JobKind::Pull);
        assert!(!jobs.network_busy(a) && jobs.active.is_empty());
    }

    #[test]
    fn cmd_click_builds_a_selection_from_the_selected_commit() {
        assert_eq!(toggle_pick(&[], Some(2), 5), [2, 5]);
        assert_eq!(toggle_pick(&[], None, 5), [5]);
        assert_eq!(toggle_pick(&[], Some(5), 5), [5]);
        assert_eq!(toggle_pick(&[2, 5], Some(2), 5), [2], "toggled off");
        assert_eq!(toggle_pick(&[2, 5], Some(2), 7), [2, 5, 7]);
    }

    #[test]
    fn detached_text_shortens_the_sha() {
        let sha = "83174b7246c76c9b6426a95147a715f0d26bf6c6";
        assert_eq!(detached_text(sha), "Detached at 83174b7");
    }

    #[test]
    fn local_name_strips_remote() {
        assert_eq!(local_branch_name("origin/feature/x"), "feature/x");
        assert_eq!(local_branch_name("origin/main"), "main");
        assert_eq!(local_branch_name("main"), "main");
    }

    #[test]
    fn discard_asks_first_and_keeps_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let proj = repo(0);
        let d = proj.path();
        let g = |args: &[&str]| crate::git::run(d, args).unwrap();
        let read = |name: &str| std::fs::read_to_string(d.join(name)).unwrap();
        std::fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        std::fs::write(d.join("f.txt"), "a\nB\nc\n").unwrap();
        g(&["add", "f.txt"]);
        std::fs::write(d.join("f.txt"), "a\nB\nc\nd\n").unwrap();
        std::fs::write(d.join("added.txt"), "x\n").unwrap();
        g(&["add", "added.txt"]);
        std::fs::write(d.join("added.txt"), "x\ny\n").unwrap();
        std::fs::write(d.join("new.txt"), "n\n").unwrap();

        let mut app = test_app(cfg(), &cfg_path);
        let space = app.space().unwrap();
        app.open_tab(d, &space);
        let file = |app: &SpinneyApp, path: &str| {
            let st = &app.tabs[0].status;
            st.unstaged.iter().find(|f| f.path == path).unwrap().clone()
        };
        let question = |app: &SpinneyApp| {
            let (_, what) = app.confirm_discard.as_ref()?;
            changes::discard_question(what, &app.tabs[0].status, &app.tabs[0].center)
        };

        // One unstaged line of a file that also has a staged change.
        app.apply(Action::OpenDiff(file(&app, "f.txt"), false));
        let Center::Diff(w) = &app.tabs[0].center else {
            panic!("diff open: {:?}", app.errors);
        };
        let ask = diff_view::ask_discard(&w.fd, &diff_view::hunk_lines(&w.fd, 0)).unwrap();
        app.apply(ask);
        assert_eq!(question(&app).as_deref(), Some("Discard 1 line in f.txt?"));
        assert_eq!(read("f.txt"), "a\nB\nc\nd\n", "nothing until confirmed");
        app.apply(Action::ConfirmDiscard);
        assert!(app.confirm_discard.is_none() && app.errors.is_empty());
        assert_eq!(read("f.txt"), "a\nB\nc\n");
        assert_eq!(g(&["show", ":f.txt"]), "a\nB\nc\n", "index untouched");

        // New in the index: refused, even past the disabled menu item.
        app.apply(Action::AskDiscard(Discard::File(file(&app, "added.txt"))));
        app.apply(Action::ConfirmDiscard);
        assert!(app.errors.iter().any(|e| e.contains("Unstage it first")));
        assert_eq!(read("added.txt"), "x\ny\n");

        let untracked = Discard::File(file(&app, "new.txt"));
        app.apply(Action::AskDiscard(untracked.clone()));
        assert_eq!(question(&app).as_deref(), Some("Delete untracked new.txt?"));
        app.apply(Action::CancelDiscard);
        assert!(d.join("new.txt").exists(), "cancelled");
        app.apply(Action::AskDiscard(untracked));
        app.apply(Action::ConfirmDiscard);
        assert!(!d.join("new.txt").exists());
    }

    #[test]
    fn merge_editor_saves_picks_and_asks_about_markers_left() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        let g = |args: &[&str]| git::run(d, args);
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test User"],
            &["config", "user.email", "test@example.com"],
        ] {
            g(args).unwrap();
        }
        let write = |text: &str| {
            for f in ["a.txt", "b.txt"] {
                std::fs::write(d.join(f), format!("{text}\n")).unwrap();
            }
        };
        write("base");
        g(&["add", "."]).unwrap();
        g(&["commit", "-qm", "init"]).unwrap();
        g(&["switch", "-qc", "topic"]).unwrap();
        write("topic");
        g(&["commit", "-qam", "topic"]).unwrap();
        g(&["switch", "-q", "main"]).unwrap();
        write("main");
        g(&["commit", "-qam", "main"]).unwrap();
        assert!(g(&["merge", "topic"]).is_err(), "conflicts");
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        // Both sides, theirs first: saved and resolved, and the view closes.
        app.apply(Action::OpenConflict("a.txt".into()));
        app.apply(Action::OpenMerge("a.txt".into()));
        assert!(matches!(app.tabs[0].center, Center::Merge(_)));
        app.apply(Action::PickConflict(0, conflict::Pick::TheirsThenOurs));
        app.tabs[0].reload_changes(&mut app.errors);
        let Center::Merge(e) = &app.tabs[0].center else {
            panic!("a reload keeps the picks")
        };
        assert_eq!(e.result, "topic\nmain\n");
        app.apply(Action::SaveMerge);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "topic\nmain\n"
        );
        assert!(
            matches!(app.tabs[0].center, Center::Graph),
            "resolved: closed"
        );
        let conflicted = |app: &SpinneyApp| app.tabs[0].status.conflicted.len();
        assert_eq!(conflicted(&app), 1);
        assert!(app.confirm_resolve.is_none());

        // Nothing picked: saved as is, then the usual "mark resolved anyway?" bar.
        app.apply(Action::OpenMerge("b.txt".into()));
        app.apply(Action::SaveMerge);
        assert!(matches!(
            &app.confirm_resolve,
            Some((_, Resolve::Anyway(p))) if p == "b.txt"
        ));
        app.apply(Action::ConfirmResolve);
        assert_eq!(conflicted(&app), 0);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
    }

    #[test]
    fn merge_editor_asks_before_losing_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        git::run(d, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(d.join("f"), "<<<<<<< HEAD\nm\n=======\nt\n>>>>>>> x\n").unwrap();
        let space = app.space().unwrap();
        app.open_tab(d, &space);
        app.apply(Action::OpenMerge("f".into()));
        app.apply(Action::PickConflict(0, conflict::Pick::Ours));
        app.apply(Action::AskMergeEditor(merge_view::Ask::Close));
        app.apply(Action::CancelMergeEditor);
        let Center::Merge(e) = &app.tabs[0].center else {
            panic!("still open")
        };
        assert_eq!((e.result.as_str(), e.ask), ("m\n", None));
        // Opening something else in the centre asks too.
        app.apply(Action::OpenConflict("f".into()));
        let Center::Merge(e) = &app.tabs[0].center else {
            panic!("still open")
        };
        assert_eq!(e.ask, Some(merge_view::Ask::Close));
        app.apply(Action::ConfirmMergeEditor);
        assert!(matches!(app.tabs[0].center, Center::Graph));
    }

    #[test]
    fn conflict_view_opens_reloads_and_closes_when_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        let g = |args: &[&str]| git::run(d, args);
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test User"],
            &["config", "user.email", "test@example.com"],
        ] {
            g(args).unwrap();
        }
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]).unwrap();
        g(&["commit", "-qm", "init"]).unwrap();
        g(&["switch", "-qc", "topic"]).unwrap();
        std::fs::write(d.join("a.txt"), "topic\n").unwrap();
        g(&["commit", "-qam", "topic"]).unwrap();
        g(&["switch", "-q", "main"]).unwrap();
        std::fs::write(d.join("a.txt"), "main\n").unwrap();
        g(&["commit", "-qam", "main"]).unwrap();
        assert!(g(&["merge", "topic"]).is_err(), "conflicts");
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        app.apply(Action::OpenConflict("a.txt".into()));
        let Center::Conflict(v) = &app.tabs[0].center else {
            panic!("conflict view open")
        };
        assert_eq!(v.head.as_deref(), Some("main"));
        let conflicts = |app: &SpinneyApp| match &app.tabs[0].center {
            Center::Conflict(v) => match &v.content {
                conflict_view::Content::Text(c) => Some(crate::conflict::count(c)),
                _ => None,
            },
            _ => None,
        };
        assert_eq!(conflicts(&app), Some(1));

        // Edited outside: re-read on the next reload.
        std::fs::write(d.join("a.txt"), "both\n").unwrap();
        app.tabs[0].reload_changes(&mut app.errors);
        assert_eq!(conflicts(&app), Some(0));

        let f = app.tabs[0].status.conflicted[0].clone();
        app.apply(Action::StageFile(f, false));
        assert!(
            matches!(app.tabs[0].center, Center::Graph),
            "resolved: closed"
        );
        assert!(app.errors.is_empty(), "{:?}", app.errors);
    }
}
