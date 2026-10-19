mod changes;
mod dialogs;
mod diff_view;
mod graph_view;
mod sidebar;
mod spaces;

use crate::actions::{self, CheckoutOutcome};
use crate::config::{Config, Space};
use crate::diff::FileDiff;
use crate::graph::{LogStream, Row};
use crate::project::{self, Layout, Worktree};
use crate::refs::{self, Branch};
use crate::stage::Direction;
use crate::status::{FileStatus, Status};
use crate::{creds, git};
use dialogs::{CloneDialog, Dialogs, NewWorktreeDialog};
use eframe::egui;
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
}

pub struct ProjectTab {
    root: PathBuf,
    layout: Layout,
    worktrees: Vec<Worktree>,
    active_worktree: PathBuf,
    branches: Vec<Branch>,
    log: LogPager,
    rows: Vec<Row>,
    selected_commit: Option<usize>,
    /// Files changed by `selected_commit`.
    commit_files: Vec<FileDiff>,
    center: Center,
    status: Status,
    commit_msg: String,
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
            rows: vec![],
            selected_commit: None,
            commit_files: vec![],
            center: Center::Graph,
            status: Status::default(),
            commit_msg: String::new(),
        };
        tab.log.request(PAGE);
        tab.load_branches(errors);
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
    CommitGraph,
}

struct JobDone {
    kind: JobKind,
    /// The project the job is about (for a clone, the folder being created).
    root: PathBuf,
    space_id: String,
    result: Result<(), String>,
}

/// Background git work (network, clone, commit-graph). Workers wake the UI when done, so
/// nothing repaints while idle.
pub struct Jobs {
    tx: Sender<JobDone>,
    rx: Receiver<JobDone>,
    pending: usize,
    ctx: egui::Context,
}

impl Jobs {
    fn new(ctx: egui::Context) -> Jobs {
        let (tx, rx) = mpsc::channel();
        Jobs {
            tx,
            rx,
            pending: 0,
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
        self.pending += 1;
        std::thread::spawn(move || {
            let result = work().map_err(|e| format!("{e:#}"));
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
        self.pending -= 1;
        Some(done)
    }
}

/// Deferred UI intents, applied after rendering so closures need not borrow `self`.
pub enum Action {
    SwitchSpace(String),
    NewSpace,
    EditSpace,
    SelectTab(usize),
    CloseTab(usize),
    OpenFolder,
    CloneUrl,
    Fetch,
    Pull,
    Push,
    Checkout(String),
    NewWorktreeFrom(String),
    ActivateWorktree(PathBuf),
    RemoveWorktree(PathBuf),
    SelectCommit(usize),
    DeselectCommit,
    OpenDiff(FileStatus, bool),
    OpenCommitFile(usize),
    /// Stage the file, or unstage it when the flag is set.
    StageFile(FileStatus, bool),
    ApplyPatch(String, Direction),
    Commit,
    ToggleInline,
    CloseDiff,
}

pub struct SpinneyApp {
    config: Config,
    /// None when the config file failed to load, so a broken file is never overwritten.
    config_path: Option<PathBuf>,
    tabs: Vec<ProjectTab>,
    active_tab: usize,
    jobs: Jobs,
    errors: Vec<String>,
    dialogs: Dialogs,
    ctx: egui::Context,
    /// Window focus last frame; status reloads when it is regained.
    focused: bool,
}

impl SpinneyApp {
    pub fn new(ctx: egui::Context) -> SpinneyApp {
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
            dialogs: Dialogs::default(),
            ctx,
            focused: true,
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
        match ProjectTab::open(root, space, &self.ctx, &mut self.jobs, &mut self.errors) {
            Ok(tab) => {
                self.tabs.push(tab);
                self.active_tab = self.tabs.len() - 1;
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
        if let Err(e) = result {
            self.errors.push(format!("{kind:?} failed: {e}"));
            return;
        }
        match kind {
            JobKind::Clone => {
                // Separate job: a commit-graph failure must not orphan the finished clone.
                self.jobs.commit_graph(root.clone(), root.join(".bare"));
                if add_project(&mut self.config, &space_id, &root) {
                    self.save_config();
                }
                let space = self.space().filter(|s| s.id == space_id);
                if let Some(space) = space {
                    self.open_tab(&root, &space);
                }
            }
            JobKind::Fetch | JobKind::Pull | JobKind::Push => {
                if kind == JobKind::Fetch {
                    if let Ok(common) = common_dir(&root) {
                        self.jobs.commit_graph(root.clone(), common);
                    }
                }
                let (ctx, mut errors) = (self.ctx.clone(), vec![]);
                if let Some(tab) = self.tab_mut(&root) {
                    tab.refresh(&ctx, &mut errors);
                }
                self.errors.extend(errors);
            }
            JobKind::CommitGraph => {}
        }
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
                self.dialogs.space = self.space().as_ref().map(spaces::SpaceEdit::from_space)
            }
            Action::SelectTab(i) => {
                self.active_tab = i;
                if let Some(tab) = self.tabs.get_mut(i) {
                    tab.reload_changes(&mut errors);
                }
            }
            Action::CloseTab(i) => {
                if i < self.tabs.len() {
                    let tab = self.tabs.remove(i);
                    if let Some(space) = self.space() {
                        if remove_project(&mut self.config, &space.id, &tab.root) {
                            self.save_config();
                        }
                    }
                    self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
                }
            }
            Action::OpenFolder => self.dialogs.open_folder = Some(String::new()),
            Action::CloneUrl => {
                let dest = self
                    .space()
                    .and_then(|s| s.default_clone_dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.dialogs.clone = Some(CloneDialog {
                    url: String::new(),
                    dest,
                    name: String::new(),
                    name_edited: false,
                });
            }
            Action::Fetch => self.network(JobKind::Fetch),
            Action::Pull => self.network(JobKind::Pull),
            Action::Push => self.network(JobKind::Push),
            Action::ToggleInline => {
                self.config.diff_inline = !self.config.diff_inline;
                self.save_config();
            }
            Action::SelectCommit(_)
            | Action::DeselectCommit
            | Action::OpenDiff(..)
            | Action::OpenCommitFile(_)
            | Action::StageFile(..)
            | Action::ApplyPatch(..)
            | Action::Commit
            | Action::CloseDiff => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab_action(tab, action, &ctx, &mut errors);
                }
            }
            Action::NewWorktreeFrom(base) => {
                self.dialogs.new_worktree = Some(NewWorktreeDialog {
                    base,
                    branch: String::new(),
                });
            }
            Action::Checkout(name) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    checkout(tab, &name, &mut errors);
                    tab.refresh(&ctx, &mut errors);
                }
            }
            Action::ActivateWorktree(path) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.active_worktree = path;
                    tab.refresh(&ctx, &mut errors);
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
        if let Some(token) = edit.token() {
            if let Err(e) = creds::set_token(&id, token) {
                self.errors.push(format!("saving token: {e}"));
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

    fn top_bar(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            spaces::switcher(ui, &self.config, actions);
            ui.separator();
            for (i, tab) in self.tabs.iter().enumerate() {
                if ui
                    .selectable_label(i == self.active_tab, tab.name())
                    .clicked()
                {
                    actions.push(Action::SelectTab(i));
                }
                if ui.small_button("×").on_hover_text("Close tab").clicked() {
                    actions.push(Action::CloseTab(i));
                }
            }
            let has_space = active_space(&self.config).is_some();
            ui.add_enabled_ui(has_space, |ui| {
                ui.menu_button("+", |ui| {
                    if ui.button("Open folder…").clicked() {
                        actions.push(Action::OpenFolder);
                        ui.close();
                    }
                    if ui.button("Clone URL…").clicked() {
                        actions.push(Action::CloneUrl);
                        ui.close();
                    }
                });
            });
            ui.separator();
            ui.add_enabled_ui(!self.tabs.is_empty(), |ui| {
                for (label, action) in [
                    ("Fetch", Action::Fetch),
                    ("Pull", Action::Pull),
                    ("Push", Action::Push),
                ] {
                    if ui.button(label).clicked() {
                        actions.push(action);
                    }
                }
            });
            if self.jobs.pending > 0 {
                ui.add(egui::Spinner::new());
            }
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
    }
}

/// Changes-panel and diff-view actions on the active tab.
fn tab_action(tab: &mut ProjectTab, action: Action, ctx: &egui::Context, errors: &mut Vec<String>) {
    match action {
        Action::SelectCommit(i) => tab.select_commit(i, errors),
        Action::DeselectCommit => {
            tab.selected_commit = None;
            tab.commit_files.clear();
        }
        Action::OpenDiff(f, staged) => tab.open_diff(f, staged, errors),
        Action::OpenCommitFile(k) => tab.open_commit_file(k),
        Action::StageFile(f, unstage) => tab.stage_file(&f, unstage, errors),
        Action::ApplyPatch(p, dir) => tab.apply_patch(&p, dir, errors),
        Action::Commit => tab.commit(ctx, errors),
        Action::CloseDiff => tab.center = Center::Graph,
        _ => {}
    }
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

impl eframe::App for SpinneyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        let focused = ui.input(|i| i.focused);
        if focused && !self.focused {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.reload_changes(&mut self.errors);
            }
        }
        self.focused = focused;
        let mut actions = vec![];
        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui, &mut actions));
        if !self.errors.is_empty() {
            egui::Panel::top("errors").show(ui, |ui| self.error_banner(ui));
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| sidebar::show(ui, tab, &mut actions));
            egui::Panel::right("changes")
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| changes::show(ui, tab, &mut actions));
            let inline = self.config.diff_inline;
            egui::CentralPanel::default().show(ui, |ui| {
                if !matches!(tab.center, Center::Graph) {
                    return diff_view::show(ui, &mut tab.center, inline, &mut actions);
                }
                let out = graph_view::show(ui, &tab.rows, tab.selected_commit);
                if let Some(i) = out.clicked {
                    actions.push(Action::SelectCommit(i));
                }
                if out.near_end {
                    tab.log.request(PAGE);
                }
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                if active_space(&self.config).is_none() {
                    ui.label("Create a space to get started.");
                    if ui.button("New space…").clicked() {
                        actions.push(Action::NewSpace);
                    }
                } else {
                    ui.label("Open a folder or clone a URL with the + menu.");
                }
            });
        }
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

    fn cfg() -> Config {
        Config {
            spaces: vec![
                space("a", &["/tmp/p1", "/tmp/p2"]),
                space("b", &["/tmp/p3"]),
            ],
            active_space: Some("a".into()),
            diff_inline: false,
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

    #[test]
    fn clone_success_adds_project_even_if_commit_graph_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let ctx = egui::Context::default();
        let mut app = SpinneyApp {
            config: cfg(),
            config_path: Some(cfg_path.clone()),
            tabs: vec![],
            active_tab: 0,
            jobs: Jobs::new(ctx.clone()),
            errors: vec![],
            dialogs: Dialogs::default(),
            ctx,
            focused: true,
        };
        // Not a repo, so the follow-up commit-graph job is bound to fail.
        let root = tmp.path().join("cloned");
        app.job_done(JobDone {
            kind: JobKind::Clone,
            root: root.clone(),
            space_id: "b".into(),
            result: Ok(()),
        });
        assert_eq!(app.jobs.pending, 1, "commit-graph runs as its own job");
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
    fn local_name_strips_remote() {
        assert_eq!(local_branch_name("origin/feature/x"), "feature/x");
        assert_eq!(local_branch_name("origin/main"), "main");
        assert_eq!(local_branch_name("main"), "main");
    }
}
