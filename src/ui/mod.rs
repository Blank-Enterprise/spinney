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

use crate::actions::{self, CheckoutOutcome, MergeOutcome, Operation};
use crate::config::{Config, Space};
use crate::diff::FileDiff;
use crate::graph::{LogStream, Row};
use crate::project::{self, Layout, Worktree};
use crate::refs::{self, Branch};
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
}

pub struct ProjectTab {
    root: PathBuf,
    layout: Layout,
    worktrees: Vec<Worktree>,
    active_worktree: PathBuf,
    branches: Vec<Branch>,
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
    /// The merge, rebase, cherry-pick or revert stopped in `active_worktree`, if any.
    operation: Option<Operation>,
    /// The commit HEAD is detached at in `active_worktree`, if it is.
    detached: Option<String>,
    /// The branch to go back to from a detached HEAD.
    previous_branch: Option<String>,
}

/// `<git common dir>` of a project (the `.bare` dir or a normal clone's `.git`).
fn common_dir(root: &Path) -> anyhow::Result<PathBuf> {
    let out = git::run(root, &["rev-parse", "--git-common-dir"])?;
    Ok(root.join(out.trim()))
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
        let layout = project::detect(root)?;
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
            layout,
            worktrees,
            log: LogPager::start(active_worktree.clone(), ctx.clone()),
            active_worktree,
            branches: vec![],
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
            operation: None,
            detached: None,
            previous_branch: None,
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

    /// Reload worktrees and branches (fast, local) and restart history paging.
    fn refresh(&mut self, ctx: &egui::Context, errors: &mut Vec<String>) {
        match project::list_worktrees(&self.root) {
            Ok(w) => self.worktrees = w.into_iter().filter(|w| !w.is_bare).collect(),
            Err(e) => errors.push(format!("worktrees: {e:#}")),
        }
        if !self
            .worktrees
            .iter()
            .any(|w| w.path == self.active_worktree)
        {
            if let Some(w) = self.worktrees.first() {
                self.active_worktree = w.path.clone();
            }
        }
        self.load_branches(errors);
        self.load_stashes(errors);
        self.log = LogPager::start(self.active_worktree.clone(), ctx.clone());
        self.log.request(PAGE);
        self.rows.clear();
        self.selected_commit = None;
        self.commit_files.clear();
        self.reload_changes(errors);
    }

    fn load_branches(&mut self, errors: &mut Vec<String>) {
        match refs::branches(&self.active_worktree) {
            Ok(b) => self.branches = b,
            Err(e) => errors.push(format!("branches: {e:#}")),
        }
        let wt = &self.active_worktree;
        self.detached = actions::detached_head(wt).unwrap_or_else(|e| {
            errors.push(format!("HEAD: {e:#}"));
            None
        });
        self.previous_branch = match &self.detached {
            None => None,
            Some(_) => actions::previous_branch(wt).unwrap_or_default(),
        };
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
            // A conflicted apply/pop exits non-zero; the Conflicted list shows the files.
            let new_conflicts = self
                .status
                .conflicted
                .iter()
                .any(|f| !before.contains(&f.path));
            if applies && new_conflicts {
                notice = Some("Stash applied with conflicts".to_string());
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
    CommitGraph,
    /// Normal clone to worktree layout; the project's tab is closed while it runs.
    Convert,
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

    /// Any job is in flight for `root`.
    fn busy(&self, root: &Path) -> bool {
        self.active.iter().any(|(_, r)| r == root)
    }

    /// A fetch, pull, push, remote branch delete or remote rename is in flight for `root`.
    fn network_busy(&self, root: &Path) -> bool {
        [
            JobKind::Fetch,
            JobKind::Pull,
            JobKind::Push,
            JobKind::DeleteRemote,
            JobKind::RenameRemote,
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
    Pull,
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
    /// New branch from the given base: a worktree in the worktree layout, else a switch.
    NewBranch(String),
    /// Open Terminal.app at the active worktree.
    OpenTerminal,
    /// Show the convert-to-worktree-layout confirm screen for a project.
    RequestConvert(PathBuf),
    /// "Not now" on the convert banner: hide it for that project for good.
    DismissConvert(PathBuf),
    ActivateWorktree(PathBuf),
    RemoveWorktree(PathBuf),
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
    /// "Create branch here…" on the detached-HEAD banner: a new branch at HEAD, switched to.
    AttachBranch,
    /// "Create branch here…" on a commit: the new-branch dialog with the commit as base,
    /// which creates the branch (worktree layout) or creates and switches (normal clone).
    BranchAt(String),
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
    /// Conversions in flight: (project root, worktree to activate afterwards).
    converting: Vec<(PathBuf, PathBuf)>,
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
            dialogs: Dialogs::default(),
            new_tab: false,
            recent_search: String::new(),
            ctx,
            focused: true,
            confirm_discard: None,
            converting: vec![],
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

    fn open_visible_tabs(&mut self) {
        self.tabs.clear();
        self.active_tab = 0;
        let Some(space) = self.space() else { return };
        for root in visible_projects(&self.config) {
            self.open_tab(&root, &space);
        }
    }

    fn open_tab(&mut self, root: &Path, space: &Space) {
        if self.jobs.running(JobKind::Convert, root) {
            // Its files are moving; git must not run on it until the job is done.
            self.notices.push(format!(
                "{} is being converted; it reopens when done",
                root.display()
            ));
            return;
        }
        match ProjectTab::open(root, space, &self.ctx, &mut self.jobs, &mut self.errors) {
            Ok(tab) => {
                self.tabs.push(tab);
                self.active_tab = self.tabs.len() - 1;
                self.new_tab = false;
            }
            Err(e) => self.errors.push(format!("{e:#}")),
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
        if kind == JobKind::Convert {
            return self.convert_done(root, result);
        }
        if let Err(e) = result {
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
                Err(e) => self.errors.push(format!("{kind:?} failed: {e:#}")),
            }
            return;
        }
        match kind {
            JobKind::Clone => {
                let normal = project::detect(&root).ok() == Some(Layout::NormalClone);
                let git_dir = root.join(if normal { ".git" } else { ".bare" });
                // Separate job: a commit-graph failure must not orphan the finished clone.
                self.jobs.commit_graph(root.clone(), git_dir);
                let mut changed = add_project(&mut self.config, &space_id, &root);
                // The user just chose a normal clone; don't offer to convert it straight away.
                if normal && !self.config.convert_dismissed.contains(&root) {
                    self.config.convert_dismissed.push(root.clone());
                    changed = true;
                }
                if changed {
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
            | JobKind::MergeTool => {
                if kind == JobKind::Fetch {
                    if let Ok(common) = common_dir(&root) {
                        self.jobs.commit_graph(root.clone(), common);
                    }
                }
                self.refresh_tab(&root);
            }
            JobKind::CommitGraph | JobKind::Convert => {}
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
                Ok(common) => Box::new(move || actions::fetch(&common)),
                Err(e) => return self.errors.push(format!("{e:#}")),
            },
            JobKind::Pull => Box::new(move || actions::pull(&wt)),
            _ => Box::new(move || actions::push(&wt)),
        };
        self.jobs.spawn(kind, root, String::new(), work);
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
            Action::Pull => {
                self.rejected_push = None;
                self.network(JobKind::Pull)
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
                    let work = move || actions::force_push(&lease);
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
            | Action::CloseDiff => {
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
            Action::AskRename(branch) => self.ask_rename(branch, &mut errors),
            Action::FastForward(branch) => self.fast_forward(branch, None, &mut errors),
            Action::FastForwardTo(branch, to) => self.fast_forward(branch, Some(to), &mut errors),
            Action::DropBranch(dragged, target, pos) => {
                self.drop_branch(dragged, target, pos, &mut errors)
            }
            Action::AskMerge(branch) => self.ask_merge(branch, false, &mut errors),
            Action::AskRebase(branch) => self.ask_merge(branch, true, &mut errors),
            Action::NewBranch(base) => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let mode = match tab.layout {
                        Layout::WorktreeRoot => BranchMode::Worktree,
                        Layout::NormalClone => BranchMode::Switch,
                    };
                    self.dialogs.new_worktree = Some(NewWorktreeDialog {
                        base,
                        branch: String::new(),
                        mode,
                    });
                }
            }
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
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let mode = match tab.layout {
                        Layout::WorktreeRoot => BranchMode::Create,
                        Layout::NormalClone => BranchMode::Switch,
                    };
                    self.dialogs.new_worktree = Some(NewWorktreeDialog {
                        base: sha,
                        branch: String::new(),
                        mode,
                    });
                }
            }
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
            Action::RequestConvert(root) => {
                let plan = crate::convert::plan(&root).map_err(|e| format!("{e:#}"));
                self.dialogs.convert = Some(dialogs::ConvertDialog { root, plan });
            }
            Action::DismissConvert(root) => {
                if !self.config.convert_dismissed.contains(&root) {
                    self.config.convert_dismissed.push(root);
                    self.save_config();
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
            Action::ActivateWorktree(path) => {
                self.confirm_discard = None; // it was asked about the other worktree's files
                self.confirm_op = None;
                self.confirm_resolve = None;
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.active_worktree = path;
                    tab.refresh(&ctx, &mut errors);
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
            Action::RemoveWorktree(path) => {
                let dirty = git::run(&path, &["status", "--porcelain"])
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if dirty {
                    self.dialogs.confirm_remove = Some(path);
                } else {
                    self.remove_worktree(&path, false, &mut errors);
                }
            }
        }
        self.errors.extend(errors);
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

    fn close_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.tabs.remove(i);
            self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
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

    /// Whether to show the "normal clone" banner for `tab`.
    fn offer_convert(&self, tab: &ProjectTab) -> bool {
        tab.layout == Layout::NormalClone && !self.config.convert_dismissed.contains(&tab.root)
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

    /// Drop the confirmations pending for `root`: they name files and worktrees that a
    /// conversion is about to move, so they must not come back when its tab reopens.
    fn forget_pending(&mut self, root: &Path) {
        if self
            .confirm_discard
            .as_ref()
            .is_some_and(|(r, _)| r == root)
        {
            self.confirm_discard = None;
        }
        if self.confirm_amend.as_deref() == Some(root) {
            self.confirm_amend = None;
        }
        if self.rejected_push.as_ref().is_some_and(|r| r.root == root) {
            self.rejected_push = None;
        }
        if self.confirm_op.as_ref().is_some_and(|p| p.root == root) {
            self.confirm_op = None;
        }
        if self
            .confirm_resolve
            .as_ref()
            .is_some_and(|(r, _)| r == root)
        {
            self.confirm_resolve = None;
        }
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
            Ok(MergeOutcome::Conflicts(files)) => self
                .notices
                .push(dialogs::conflict_message(&in_progress::title(op), &files)),
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
    fn push_bar(ui: &mut egui::Ui, r: &RejectedPush, busy: bool, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            let branch = &r.rejected.branch;
            let text = match &r.lease {
                None => format!(
                    "Push of {branch} was rejected: the remote has commits this branch doesn't."
                ),
                Some(l) => {
                    let lost = match l.lost {
                        0 => "Commits on the remote that aren't on this branch".to_string(),
                        1 => format!("1 commit on {} that isn't on this branch", l.tracking),
                        n => format!("{n} commits on {} that aren't on this branch", l.tracking),
                    };
                    format!(
                        "Force push {}? {lost} will be lost. If someone pushed since your \
                         last fetch, the push is refused.",
                        l.branch
                    )
                }
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
                        if ui.small_button("Pull").clicked() {
                            actions.push(Action::Pull);
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
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, e);
                if ui.small_button("Dismiss").clicked() {
                    dismiss = Some(i);
                }
            });
        }
        if let Some(i) = dismiss {
            self.errors.remove(i);
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
        Action::SelectCommit(i) => tab.select_commit(i, errors),
        Action::DeselectCommit => {
            tab.selected_commit = None;
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
        _ => {}
    }
}

/// Where leaving a detached HEAD goes once confirmed.
#[derive(Clone, Debug, PartialEq)]
pub enum LeaveTo {
    Branch(String),
    Commit(String),
}

/// Whether going to `to` moves the active worktree's HEAD. In the worktree layout a branch
/// checkout jumps to (or creates) that branch's own worktree, leaving this one alone.
pub fn leaves_head(layout: Layout, to: &LeaveTo) -> bool {
    match to {
        LeaveTo::Commit(_) => true,
        LeaveTo::Branch(_) => layout == Layout::NormalClone,
    }
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
    match actions::checkout(&tab.root, tab.layout, &tab.active_worktree, local) {
        Ok(CheckoutOutcome::Switched) => {}
        Ok(CheckoutOutcome::JumpTo(path)) => tab.active_worktree = path,
        Ok(CheckoutOutcome::NeedsWorktree) => {
            match project::add_worktree_existing(&tab.root, local) {
                Ok(path) => tab.active_worktree = path,
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
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
        if !leaves_head(tab.layout, &to) {
            return false;
        }
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

    /// Refuse to close while a conversion runs: its undo journal lives only in memory,
    /// so quitting would strand the repo half-converted.
    fn hold_close(&mut self, ctx: &egui::Context) {
        if !self.converting.is_empty() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            let wait = "Spinney can quit once the conversion finishes".to_string();
            if !self.notices.contains(&wait) {
                self.notices.push(wait);
            }
        }
    }
}

impl eframe::App for SpinneyApp {
    /// Runs every pass, even minimized, so a close request is always seen.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.hold_close(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        let focused = ui.input(|i| i.focused);
        if focused && !self.focused {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.reload_changes(&mut self.errors);
                tab.load_stashes(&mut self.errors);
            }
        }
        self.focused = focused;
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
                    toolbar::toolbar(ui, tab, projects, busy, running, &mut actions)
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
                if self.offer_convert(tab) {
                    let root = tab.root.clone();
                    egui::Panel::top("convert_offer").frame(bar).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("This project uses a normal clone");
                            ui.label(theme::caption(
                                "Convert it to the worktree layout to work on several \
                                 branches side by side.",
                            ));
                            if ui.add(theme::primary("Convert…")).clicked() {
                                actions.push(Action::RequestConvert(root.clone()));
                            }
                            if ui.button("Not now").clicked() {
                                actions.push(Action::DismissConvert(root));
                            }
                        });
                    });
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
                .show(ui, |ui| Self::push_bar(ui, r, busy, &mut actions));
        }
        if !self.errors.is_empty() || !self.notices.is_empty() {
            egui::Panel::top("errors").show(ui, |ui| self.error_banner(ui));
        }
        let shortcuts = !self.dialogs.any_open();
        let tab = if self.showing_new_tab() {
            None
        } else {
            self.tabs.get_mut(self.active_tab)
        };
        let merge_tool = self.config.merge_tool;
        if let Some(tab) = tab {
            egui::Panel::left("sidebar")
                .frame(theme::side_panel())
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| sidebar::show(ui, tab, &mut actions));
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
                let menu = graph_view::MenuContext {
                    layout: tab.layout,
                    dirty: graph_view::clean_blocked(&tab.status, tab.operation),
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

/// Projects whose tabs are visible: those of the active space.
pub fn visible_projects(cfg: &Config) -> Vec<PathBuf> {
    active_space(cfg).map_or_else(Vec::new, |s| s.projects.clone())
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
            convert_dismissed: vec![],
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
    fn normal_clone_skips_the_convert_offer() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let mut app = test_app(cfg(), &cfg_path);
        let normal = repo(1);
        let root = normal.path().to_path_buf();
        app.job_done(JobDone {
            kind: JobKind::Clone,
            root: root.clone(),
            space_id: "b".into(),
            result: Ok(()),
        });
        let saved = Config::load_from(&cfg_path).unwrap();
        assert!(saved.spaces[1].projects.contains(&root));
        assert_eq!(saved.convert_dismissed, [root]);
    }

    #[test]
    fn common_dir_for_both_layouts() {
        let normal = repo(0);
        let got = common_dir(normal.path()).unwrap();
        assert_eq!(
            got.canonicalize().unwrap(),
            normal.path().join(".git").canonicalize().unwrap()
        );
        assert!(needs_commit_graph(&got));

        let tmp = tempfile::tempdir().unwrap();
        crate::git::run(tmp.path(), &["init", "-q", "--bare", ".bare"]).unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: ./.bare\n").unwrap();
        let got = common_dir(tmp.path()).unwrap();
        assert_eq!(
            got.canonicalize().unwrap(),
            tmp.path().join(".bare").canonicalize().unwrap()
        );
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
            dialogs: Dialogs::default(),
            new_tab: false,
            recent_search: String::new(),
            ctx,
            focused: true,
            confirm_discard: None,
            converting: vec![],
            confirm_amend: None,
            rejected_push: None,
            confirm_op: None,
            confirm_resolve: None,
        }
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
    fn close_is_held_only_while_converting() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let ctx = egui::Context::default();
        let mut close = egui::RawInput::default();
        close
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        let held = |app: &mut SpinneyApp| {
            let out = ctx.run_logic(&close, |ctx| app.hold_close(ctx));
            out.viewport_commands
                .get(&egui::ViewportId::ROOT)
                .is_some_and(|c| c.contains(&egui::ViewportCommand::CancelClose))
        };
        assert!(!held(&mut app), "nothing running: close normally");
        app.converting
            .push((PathBuf::from("/p/root"), PathBuf::from("/p/root/main")));
        assert!(held(&mut app));
        assert!(app
            .notices
            .iter()
            .any(|n| n.contains("conversion finishes")));
    }

    #[test]
    fn converting_forgets_only_that_projects_confirmations() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(cfg(), &tmp.path().join("config.toml"));
        let (root, other) = (PathBuf::from("/p/root"), PathBuf::from("/p/other"));
        let rejected = |r: &Path| RejectedPush {
            root: r.to_path_buf(),
            rejected: actions::PushRejected {
                worktree: r.to_path_buf(),
                branch: "main".into(),
            },
            lease: None,
        };
        app.confirm_discard = Some((root.clone(), Discard::All));
        app.confirm_amend = Some(root.clone());
        app.rejected_push = Some(rejected(&root));
        app.forget_pending(&root);
        assert!(app.confirm_discard.is_none() && app.confirm_amend.is_none());
        assert!(app.rejected_push.is_none());

        app.confirm_discard = Some((other.clone(), Discard::All));
        app.confirm_amend = Some(other.clone());
        app.rejected_push = Some(rejected(&other));
        app.forget_pending(&root);
        assert!(app.confirm_discard.is_some() && app.confirm_amend.is_some());
        assert!(app.rejected_push.is_some());
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
    fn only_moving_this_worktrees_head_leaves_it() {
        let commit = LeaveTo::Commit("abc".into());
        let branch = LeaveTo::Branch("main".into());
        assert!(leaves_head(Layout::NormalClone, &commit));
        assert!(leaves_head(Layout::WorktreeRoot, &commit));
        assert!(leaves_head(Layout::NormalClone, &branch));
        assert!(
            !leaves_head(Layout::WorktreeRoot, &branch),
            "jumps to its worktree"
        );
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
