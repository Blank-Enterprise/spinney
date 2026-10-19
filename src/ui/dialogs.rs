//! Modal dialogs: clone, create, new worktree, confirm worktree removal and branch deletion,
//! convert.

use super::{
    actions, add_project, checkout, graph_view, paths, project, repo_name_from_url, spaces, theme,
    toolbar::folder_name, visible_projects, JobKind, SpinneyApp,
};
use crate::convert::{self, Plan};
use crate::{creds, github};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Vec2};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Where the Clone dialog takes its repository from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Source {
    Url,
    GitHub,
}

/// Outcome of listing the space's repositories.
pub(super) enum Listing {
    NoToken,
    Failed(String),
    Repos(github::RepoList),
}

/// Repository list for the dialog session; fetched once, refetched on ↻.
pub(super) enum Browse {
    NotLoaded,
    Loading(Receiver<Listing>),
    Done(Listing),
}

pub(super) struct CloneDialog {
    source: Source,
    /// "Where to clone to", as typed or picked.
    where_dir: String,
    url: String,
    name: String,
    /// Once the user types a name, the URL (or picked repo) stops overwriting it.
    name_edited: bool,
    /// The space's default clone folder: prefill and Browse fallback.
    default_dir: Option<PathBuf>,
    /// Label for the space's GitHub source.
    github: String,
    space_id: String,
    api_base: String,
    browse: Browse,
    query: String,
    /// Focus the search field the next time it shows.
    focus_search: bool,
    /// "Edit space" was clicked: close and open the space editor.
    edit_space: bool,
}

impl CloneDialog {
    pub(super) fn new(space: &crate::config::Space) -> Self {
        let default_dir = space.default_clone_dir.clone();
        CloneDialog {
            source: Source::Url,
            where_dir: default_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            url: String::new(),
            name: String::new(),
            name_edited: false,
            default_dir,
            github: github_label(&space.host),
            space_id: space.id.clone(),
            api_base: space.api_base(),
            browse: Browse::NotLoaded,
            query: String::new(),
            focus_search: false,
            edit_space: false,
        }
    }

    /// Canonical parent folder and trimmed name, or why the clone cannot start yet.
    fn target(&self, home: Option<&Path>) -> Result<(PathBuf, String), String> {
        let parent = paths::existing_dir(&self.where_dir, home)?;
        if self.url.trim().is_empty() {
            return Err(match self.source {
                Source::Url => "Enter a URL".into(),
                Source::GitHub => "Select a repository".into(),
            });
        }
        paths::check_name(&self.name)?;
        let name = self.name.trim().to_string();
        let root = parent.join(&name);
        if root.exists() {
            return Err(format!("{} already exists", root.display()));
        }
        Ok((parent, name))
    }

    /// Switch source; the URL (and a name that followed it) belong to the old source.
    fn set_source(&mut self, source: Source) {
        if source == self.source {
            return;
        }
        self.source = source;
        self.url.clear();
        if !self.name_edited {
            self.name.clear();
        }
        self.focus_search = source == Source::GitHub;
    }

    fn select_repo(&mut self, repo: &github::Repo) {
        self.url = repo.clone_url.clone();
        if !self.name_edited {
            self.name = repo.name.clone();
        }
    }

    /// List the space's repos on a worker thread; it wakes the UI once, when done.
    fn load(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let (id, base, ctx) = (self.space_id.clone(), self.api_base.clone(), ctx.clone());
        std::thread::spawn(move || {
            let listing = match creds::get_token(&id) {
                Ok(Some(token)) if !token.trim().is_empty() => {
                    match github::list_repos(&base, &token) {
                        Ok(list) => Listing::Repos(list),
                        Err(e) => Listing::Failed(format!("{e:#}")),
                    }
                }
                Ok(_) => Listing::NoToken,
                Err(e) => Listing::Failed(format!("reading the token: {e:#}")),
            };
            let _ = tx.send(listing);
            ctx.request_repaint();
        });
        self.browse = Browse::Loading(rx);
    }

    /// Take a finished listing, if any.
    fn poll(&mut self) {
        let Browse::Loading(rx) = &self.browse else {
            return;
        };
        let listing = match rx.try_recv() {
            Ok(l) => l,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Listing::Failed("listing stopped".into()),
        };
        self.focus_search |= matches!(listing, Listing::Repos(_));
        self.browse = Browse::Done(listing);
    }
}

pub(super) struct CreateDialog {
    name: String,
    /// "Initialize in", as typed or picked.
    dir: String,
    branch: String,
    default_dir: Option<PathBuf>,
    /// Last branch name checked with git, and whether it was valid (git runs once per edit).
    checked: Option<(String, bool)>,
}

impl CreateDialog {
    pub(super) fn new(space: &crate::config::Space) -> Self {
        let default_dir = space.default_clone_dir.clone();
        CreateDialog {
            name: String::new(),
            dir: default_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            branch: "main".into(),
            default_dir,
            checked: None,
        }
    }

    /// Canonical parent, trimmed name and branch, or why the repo cannot be created yet.
    fn target(&mut self, home: Option<&Path>) -> Result<(PathBuf, String, String), String> {
        let parent = paths::existing_dir(&self.dir, home)?;
        paths::check_name(&self.name)?;
        let branch = self.branch.trim().to_string();
        if branch.is_empty() {
            return Err("Enter a default branch".into());
        }
        let valid = match &self.checked {
            Some((b, ok)) if *b == branch => *ok,
            _ => {
                let ok = project::check_branch_name(&branch).is_ok();
                self.checked = Some((branch.clone(), ok));
                ok
            }
        };
        if !valid {
            return Err(format!("{branch} is not a valid branch name"));
        }
        let name = self.name.trim().to_string();
        let root = parent.join(&name);
        if root.exists() {
            return Err(format!("{} already exists", root.display()));
        }
        Ok((parent, name, branch))
    }
}

/// Sidebar label for a space's GitHub host.
fn github_label(host: &str) -> String {
    if host == "github.com" {
        "GitHub.com".into()
    } else {
        format!("GitHub Enterprise ({host})")
    }
}

pub(super) struct NewWorktreeDialog {
    pub(super) base: String,
    pub(super) branch: String,
    /// Create a worktree for the branch; otherwise create it and switch to it (normal clone).
    pub(super) worktree: bool,
}

/// Confirm deleting a local branch; asked again with `not_merged` when `-d` refuses.
pub(super) struct DeleteBranchDialog {
    pub(super) branch: String,
    /// `origin/<branch>`, when it exists.
    pub(super) remote: Option<String>,
    /// Also delete `remote` once the local branch is gone.
    pub(super) also_remote: bool,
    pub(super) not_merged: bool,
}

#[derive(Default)]
pub(super) struct Dialogs {
    pub(super) space: Option<spaces::SpaceEdit>,
    pub(super) clone: Option<CloneDialog>,
    pub(super) create: Option<CreateDialog>,
    pub(super) new_worktree: Option<NewWorktreeDialog>,
    /// Dirty worktree awaiting a forced-removal confirmation.
    pub(super) confirm_remove: Option<PathBuf>,
    pub(super) delete_branch: Option<DeleteBranchDialog>,
    /// Remote branch (`origin/feature/x`) awaiting a delete confirmation.
    pub(super) delete_remote: Option<String>,
    pub(super) confirm_drop_stash: Option<DropStash>,
    pub(super) convert: Option<ConvertDialog>,
}

/// Convert-to-worktree-layout confirm screen; `plan` holds the refusal when it cannot run.
pub(super) struct ConvertDialog {
    pub root: PathBuf,
    pub plan: Result<Plan, String>,
}

/// A stash drop awaiting confirmation, bound to the tab and worktree it was requested in.
pub(super) struct DropStash {
    pub root: PathBuf,
    pub worktree: PathBuf,
    pub hash: String,
    pub label: String,
}

impl Dialogs {
    /// Any dialog, modal or the space editor is showing.
    pub(super) fn any_open(&self) -> bool {
        self.space.is_some()
            || self.clone.is_some()
            || self.create.is_some()
            || self.new_worktree.is_some()
            || self.confirm_remove.is_some()
            || self.confirm_drop_stash.is_some()
            || self.convert.is_some()
            || self.delete_branch.is_some()
            || self.delete_remote.is_some()
    }
}

/// Red, bold button for a destructive confirmation.
fn danger(label: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).color(theme::ON_ACCENT).strong())
        .fill(theme::DELETED)
}

impl SpinneyApp {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut edit) = self.dialogs.space.take() {
            match spaces::editor(ctx, &mut edit) {
                Some(true) => self.save_space(edit),
                Some(false) => {} // cancelled; buffer (and any typed PAT) dropped
                None => self.dialogs.space = Some(edit),
            }
        }
        if let Some(mut d) = self.dialogs.clone.take() {
            match clone_dialog(ctx, &mut d) {
                Some(true) => self.start_clone(d),
                Some(false) if d.edit_space => {
                    self.dialogs.space = self.space().as_ref().map(spaces::SpaceEdit::open)
                }
                Some(false) => {}
                None => self.dialogs.clone = Some(d),
            }
        }
        if let Some(mut d) = self.dialogs.create.take() {
            match create_dialog(ctx, &mut d) {
                Some(true) => self.create_repo(d),
                Some(false) => {}
                None => self.dialogs.create = Some(d),
            }
        }
        if let Some(mut d) = self.dialogs.new_worktree.take() {
            let mut result = None;
            let title = if d.worktree {
                "New worktree"
            } else {
                "New branch"
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(theme::caption(&format!("New branch from {}", d.base)));
                    ui.text_edit_singleline(&mut d.branch);
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary("Create")).clicked() {
                            result = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            result = Some(false);
                        }
                    });
                });
            match result {
                Some(true) => self.new_worktree(d),
                Some(false) => {}
                None => self.dialogs.new_worktree = Some(d),
            }
        }
        if let Some(path) = self.dialogs.confirm_remove.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("confirm_remove")).show(ctx, |ui| {
                ui.label(format!(
                    "{} has uncommitted changes. Remove it anyway?",
                    path.display()
                ));
                ui.horizontal(|ui| {
                    if ui.add(danger("Remove")).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => {
                    let mut errors = vec![];
                    self.remove_worktree(&path, true, &mut errors);
                    self.errors.extend(errors);
                }
                Some(false) => {}
                None => self.dialogs.confirm_remove = Some(path),
            }
        }
        if let Some(drop) = self.dialogs.confirm_drop_stash.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("confirm_drop_stash")).show(ctx, |ui| {
                ui.label(format!(
                    "Drop {}? Its saved changes will be lost permanently.",
                    drop.label
                ));
                ui.horizontal(|ui| {
                    let button = egui::Button::new(
                        egui::RichText::new("Drop").color(theme::ON_ACCENT).strong(),
                    )
                    .fill(theme::DELETED);
                    if ui.add(button).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => {
                    let ctx = self.ctx.clone();
                    let mut errors = vec![];
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.root == drop.root) {
                        let action = super::Action::DropStashConfirmed(drop.hash.clone());
                        let notice = tab.stash_action(action, &drop.worktree, &ctx, &mut errors);
                        self.notices.extend(notice);
                    }
                    self.errors.extend(errors);
                }
                Some(false) => {}
                None => self.dialogs.confirm_drop_stash = Some(drop),
            }
        }
        if let Some(d) = self.dialogs.convert.take() {
            match (convert_dialog(ctx, &d), d.plan) {
                (Some(true), Ok(plan)) => self.start_convert(d.root, plan),
                (None, plan) => self.dialogs.convert = Some(ConvertDialog { root: d.root, plan }),
                _ => {}
            }
        }
        if let Some(mut d) = self.dialogs.delete_branch.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("delete_branch")).show(ctx, |ui| {
                if d.not_merged {
                    ui.label(format!(
                        "{} is not fully merged. Delete it anyway?",
                        d.branch
                    ));
                } else {
                    ui.label(format!("Delete branch {}?", d.branch));
                }
                if let Some(remote) = &d.remote {
                    ui.checkbox(&mut d.also_remote, format!("Also delete {remote}"));
                }
                ui.horizontal(|ui| {
                    let label = if d.not_merged {
                        "Delete anyway"
                    } else {
                        "Delete"
                    };
                    if ui.add(danger(label)).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => self.delete_branch(d),
                Some(false) => {}
                None => self.dialogs.delete_branch = Some(d),
            }
        }
        if let Some(name) = self.dialogs.delete_remote.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("delete_remote")).show(ctx, |ui| {
                ui.label(format!("Delete {name} on the remote?"));
                ui.horizontal(|ui| {
                    if ui.add(danger("Delete")).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => self.delete_remote(name),
                Some(false) => {}
                None => self.dialogs.delete_remote = Some(name),
            }
        }
    }

    /// Close the project's tab (so nothing runs git in it while files move) and convert on
    /// a worker thread; `convert_done` reopens it.
    pub(super) fn start_convert(&mut self, root: PathBuf, plan: Plan) {
        let Some(space) = self.space() else { return };
        if self.jobs.busy(&root) {
            self.errors.push(format!(
                "{} has background work running; convert it when that finishes",
                root.display()
            ));
            return;
        }
        if let Some(i) = self.tabs.iter().position(|t| t.root == root) {
            self.close_tab(i);
        }
        let target = root.join(&plan.target_dir_name);
        self.converting.push((root.clone(), target));
        let id = space.id.clone();
        self.jobs.spawn(JobKind::Convert, root, id, move || {
            convert::convert(&plan, &space).map(|_| ())
        });
    }

    /// Reopen the project after a conversion (converted, or rolled back on failure).
    pub(super) fn convert_done(&mut self, root: PathBuf, result: anyhow::Result<()>) {
        let target = self
            .converting
            .iter()
            .position(|(r, _)| *r == root)
            .map(|i| self.converting.swap_remove(i).1);
        match &result {
            Ok(()) => {
                let mut notice = format!("{} now uses the worktree layout", folder_name(&root));
                if let Some(t) = &target {
                    notice += &convert_strays_note(&root, t);
                }
                self.notices.push(notice);
            }
            Err(e) => self.errors.push(format!("Convert failed: {e:#}")),
        }
        let Some(space) = self.space() else { return };
        if !visible_projects(&self.config).contains(&root) {
            return; // the space was switched meanwhile
        }
        self.open_tab(&root, &space);
        if result.is_err() {
            return;
        }
        let (ctx, mut errors) = (self.ctx.clone(), vec![]);
        if let (Some(target), Some(tab)) = (target, self.tab_mut(&root)) {
            tab.active_worktree = target;
            tab.refresh(&ctx, &mut errors);
        }
        self.errors.extend(errors);
        if !self.jobs.running(JobKind::CommitGraph, &root) {
            self.jobs.commit_graph(root.clone(), root.join(".bare"));
        }
    }

    /// `-d` (or `-D` once confirmed unmerged); then the remote branch as a background job.
    fn delete_branch(&mut self, mut d: DeleteBranchDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        match actions::delete_branch(&tab.active_worktree, &d.branch, d.not_merged) {
            Ok(actions::DeleteOutcome::Deleted) => {
                tab.refresh(&ctx, &mut errors);
                if let Some(remote) = d.remote.filter(|_| d.also_remote) {
                    self.delete_remote(remote);
                }
            }
            Ok(actions::DeleteOutcome::NotMerged) => {
                d.not_merged = true;
                self.dialogs.delete_branch = Some(d);
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.errors.extend(errors);
    }

    /// `git push <remote> --delete` on a worker, like a push.
    fn delete_remote(&mut self, name: String) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        self.jobs
            .spawn(JobKind::DeleteRemote, root, String::new(), move || {
                actions::delete_remote_branch(&wt, &name)
            });
    }

    /// Open `root` as a tab (or focus its tab) and add it to the active space.
    pub(super) fn open_folder(&mut self, root: PathBuf) {
        let Some(space) = self.space() else { return };
        if let Some(i) = self.tabs.iter().position(|t| t.root == root) {
            self.active_tab = i;
            self.new_tab = false;
            return;
        }
        let before = self.tabs.len();
        self.open_tab(&root, &space);
        if self.tabs.len() > before && add_project(&mut self.config, &space.id, &root) {
            self.save_config();
        }
    }

    fn start_clone(&mut self, d: CloneDialog) {
        let Some(space) = self.space() else { return };
        if space.id != d.space_id {
            self.errors
                .push("The space changed since this dialog opened; open Clone again".into());
            return;
        }
        let (parent, name) = match d.target(paths::home().as_deref()) {
            Ok(t) => t,
            Err(e) => {
                self.errors.push(e);
                self.dialogs.clone = Some(d);
                return;
            }
        };
        let url = d.url.trim().to_string();
        let id = space.id.clone();
        self.jobs
            .spawn(JobKind::Clone, parent.join(&name), id, move || {
                project::clone_into(&url, &parent, &name, &space).map(|_| ())
            });
    }

    fn create_repo(&mut self, mut d: CreateDialog) {
        let Some(space) = self.space() else { return };
        let created = d
            .target(paths::home().as_deref())
            .and_then(|(parent, name, branch)| {
                project::create_new(&parent, &name, &branch, &space).map_err(|e| format!("{e:#}"))
            });
        match created {
            Ok(root) => self.open_folder(root),
            Err(e) => {
                self.errors.push(e);
                self.dialogs.create = Some(d);
            }
        }
    }

    fn new_worktree(&mut self, d: NewWorktreeDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let branch = d.branch.trim();
            if d.worktree {
                match project::add_worktree(&tab.root, branch, &d.base) {
                    Ok(path) => tab.active_worktree = path,
                    Err(e) => errors.push(format!("{e:#}")),
                }
            } else {
                match actions::create_branch(&tab.active_worktree, branch, &d.base) {
                    Ok(()) => checkout(tab, branch, &mut errors),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            tab.refresh(&ctx, &mut errors);
        }
        self.errors.extend(errors);
    }
}

/// Names of entries that appeared in `root` while converting (they did not move), if any.
fn convert_strays_note(root: &Path, target: &Path) -> String {
    match convert::strays(root, target) {
        Ok(s) if s.is_empty() => String::new(),
        Ok(s) => {
            let names: Vec<_> = s.iter().map(|p| p.display().to_string()).collect();
            format!(
                ". These appeared in the project folder while converting and were not moved \
                 into {}: {}",
                folder_name(target),
                names.join(", ")
            )
        }
        Err(e) => format!(". Could not check the project folder for leftovers: {e:#}"),
    }
}

/// Items listed by name in the preview; the rest are counted.
const PREVIEW_ITEMS: usize = 6;

/// Before/after folder trees for the convert confirm screen.
fn convert_preview(name: &str, plan: &Plan) -> (String, String) {
    let shown = plan.items.len().min(PREVIEW_ITEMS);
    let mut items: Vec<String> = plan.items[..shown]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if plan.items.len() > shown {
        items.push(format!("… {} more", plan.items.len() - shown));
    }
    let tree = |lines: &[String], indent: &str| -> String {
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let branch = if i + 1 == lines.len() {
                    "└─"
                } else {
                    "├─"
                };
                format!("{indent}{branch} {l}\n")
            })
            .collect()
    };
    let mut before_lines = vec![".git/".to_string()];
    before_lines.extend(items.iter().cloned());
    let before = format!("{name}/\n{}", tree(&before_lines, ""));
    let target = &plan.target_dir_name;
    let after_top = [
        ".bare/   ← was .git/".to_string(),
        ".git     ← file: gitdir: ./.bare".to_string(),
        format!("{target}/"),
    ];
    let after = format!("{name}/\n{}{}", tree(&after_top, ""), tree(&items, "   "));
    (before, after)
}

fn convert_dialog(ctx: &egui::Context, d: &ConvertDialog) -> Option<bool> {
    let mut result = None;
    egui::Modal::new(egui::Id::unique("convert_dialog")).show(ctx, |ui| {
        ui.set_max_width(480.0);
        ui.heading("Convert to worktree layout");
        ui.add_space(6.0);
        match &d.plan {
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("Can't convert: {e}"));
                ui.add_space(6.0);
                if ui.button("Close").clicked() {
                    result = Some(false);
                }
            }
            Ok(plan) => {
                let (before, after) = convert_preview(&folder_name(&d.root), plan);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.label(theme::caption("Before"));
                        ui.monospace(before);
                    });
                    ui.add_space(24.0);
                    ui.vertical(|ui| {
                        ui.label(theme::caption("After"));
                        ui.monospace(after);
                    });
                });
                ui.add_space(6.0);
                let n = plan.items.len();
                ui.label(format!(
                    "{n} item{} move into {}/, including uncommitted, untracked and ignored \
                     files. Nothing is copied or deleted; if anything fails, every change is \
                     undone.",
                    if n == 1 { "" } else { "s" },
                    plan.target_dir_name
                ));
                ui.colored_label(
                    theme::WARN,
                    "Close this project in your IDE and terminals first.",
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::primary("Convert")).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            }
        }
    });
    result
}

/// Height of a repository row: name line plus description line.
const REPO_ROW_H: f32 = 40.0;
/// Width of the dialog's right pane.
const PANE_W: f32 = 390.0;

/// GitKraken-style "Clone a Repository" modal; `Some(true)` = clone, `Some(false)` = cancel
/// (or `edit_space`), `None` = still open.
fn clone_dialog(ctx: &egui::Context, d: &mut CloneDialog) -> Option<bool> {
    d.poll();
    if d.source == Source::GitHub && matches!(d.browse, Browse::NotLoaded) {
        d.load(ctx);
    }
    let (mut result, mut browse) = (None, false);
    let target = d.target(paths::home().as_deref());
    let modal = egui::Modal::new(egui::Id::unique("clone_dialog")).show(ctx, |ui| {
        ui.heading("Clone a Repository");
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(170.0);
                ui.label(theme::section("Source"));
                let github = d.github.clone();
                for (source, label) in [(Source::Url, "Clone with URL"), (Source::GitHub, &github)]
                {
                    let item = egui::Button::selectable(d.source == source, label)
                        .min_size(egui::vec2(170.0, 0.0))
                        .right_text("");
                    if ui.add(item).clicked() {
                        d.set_source(source);
                    }
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                browse = where_to_clone(ui, &mut d.where_dir);
                match d.source {
                    Source::Url => {
                        ui.label(theme::caption("URL"));
                        let url =
                            ui.add(egui::TextEdit::singleline(&mut d.url).desired_width(PANE_W));
                        if url.changed() && !d.name_edited {
                            d.name = repo_name_from_url(d.url.trim());
                        }
                    }
                    Source::GitHub => {
                        if repo_browser(ui, d) && d.target(paths::home().as_deref()).is_ok() {
                            result = Some(true);
                        }
                    }
                }
                if let Some(r) = name_and_clone(ui, d, &target) {
                    result = result.or(Some(r));
                }
            });
        });
    });
    // The click that closed the native picker must not count as a backdrop click.
    if result.is_none() && !browse && modal.should_close() {
        result = Some(false);
    }
    if browse {
        let typed = paths::expand_tilde(&d.where_dir, paths::home().as_deref());
        let start = Some(typed).filter(|p| p.is_dir()).or(d.default_dir.clone());
        if let Some(dir) = paths::pick_folder("Where to clone to", start.as_deref()) {
            d.where_dir = dir.display().to_string();
            ctx.request_repaint(); // show the pick now, not on the next input
        }
    }
    if d.edit_space {
        result = Some(false);
    }
    result
}

/// "Where to clone to" [path][Browse]; true when Browse was clicked.
fn where_to_clone(ui: &mut egui::Ui, where_dir: &mut String) -> bool {
    ui.label(theme::caption("Where to clone to"));
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(where_dir).desired_width(PANE_W - 70.0));
        ui.button("Browse").clicked()
    })
    .inner
}

/// Name, full-path preview, validation and the Clone / Cancel buttons.
fn name_and_clone(
    ui: &mut egui::Ui,
    d: &mut CloneDialog,
    target: &Result<(PathBuf, String), String>,
) -> Option<bool> {
    let mut result = None;
    ui.label(theme::caption("Name"));
    let name = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(PANE_W));
    if name.changed() {
        d.name_edited = true;
    }
    ui.add_space(6.0);
    ui.label(theme::caption("Full path"));
    let home = paths::home();
    let full = paths::expand_tilde(&d.where_dir, home.as_deref()).join(d.name.trim());
    ui.monospace(format!("{}/", full.display()));
    ui.weak("worktrees go in folders inside this one");
    ui.add_space(6.0);
    if let Err(reason) = target {
        ui.colored_label(ui.visuals().error_fg_color, reason);
    }
    ui.horizontal(|ui| {
        let clone = theme::primary("Clone the repo!");
        if ui.add_enabled(target.is_ok(), clone).clicked() {
            result = Some(true);
        }
        if ui.button("Cancel").clicked() {
            result = Some(false);
        }
    });
    result
}

/// Search field and repository list of the space's GitHub; true on a double-click.
fn repo_browser(ui: &mut egui::Ui, d: &mut CloneDialog) -> bool {
    let mut picked: Option<(github::Repo, bool)> = None;
    let mut refresh = false;
    ui.label(theme::caption("Repository"));
    match &d.browse {
        Browse::NotLoaded | Browse::Loading(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading repositories…");
            });
        }
        Browse::Done(Listing::NoToken) => {
            ui.label("Connect this space to its GitHub account to browse repositories");
            d.edit_space = ui.button("Edit space").clicked();
        }
        Browse::Done(Listing::Failed(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            refresh = ui.button("↻ Retry").clicked();
        }
        Browse::Done(Listing::Repos(list)) => {
            let repos = &list.repos;
            ui.horizontal(|ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut d.query)
                        .hint_text("Search repositories")
                        .desired_width(PANE_W - 34.0),
                );
                if std::mem::take(&mut d.focus_search) {
                    search.request_focus();
                }
                refresh = ui.button("↻").on_hover_text("Refresh").clicked();
            });
            let shown = github::filter(repos, &d.query);
            if shown.is_empty() {
                ui.weak(if repos.is_empty() {
                    "No repositories found"
                } else {
                    "No repositories match"
                });
            } else {
                picked = repo_list(ui, &shown, &d.url);
            }
            if list.truncated {
                ui.weak(format!("Showing the first {} repositories", repos.len()));
            }
        }
    }
    if refresh {
        d.load(ui.ctx());
    }
    let Some((repo, double)) = picked else {
        return false;
    };
    d.select_repo(&repo);
    double
}

/// Scrollable repository rows; the clicked repo and whether it was a double-click.
fn repo_list(
    ui: &mut egui::Ui,
    repos: &[&github::Repo],
    selected_url: &str,
) -> Option<(github::Repo, bool)> {
    let mut picked = None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .max_height(REPO_ROW_H * 6.0)
            .auto_shrink([false, true])
            .show_rows(ui, REPO_ROW_H, repos.len(), |ui, range| {
                for repo in &repos[range] {
                    let size = Vec2::new(PANE_W, REPO_ROW_H);
                    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                    let selected = repo.clone_url == selected_url;
                    paint_repo_row(ui, rect, repo, selected, resp.hovered());
                    if resp.clicked() || resp.double_clicked() {
                        picked = Some(((*repo).clone(), resp.double_clicked()));
                    }
                }
            });
    });
    picked
}

fn paint_repo_row(ui: &egui::Ui, rect: Rect, repo: &github::Repo, selected: bool, hovered: bool) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let bg = if selected {
        theme::SEL_BG
    } else if hovered {
        theme::HOVER
    } else {
        Color32::TRANSPARENT
    };
    painter.rect_filled(rect, 4.0, bg);
    if selected {
        let edge = Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
        painter.rect_filled(edge, 1.0, theme::ACCENT);
    }
    let x = rect.left() + 8.0;
    let top = rect.top() + REPO_ROW_H * 0.3;
    let name = painter.layout_no_wrap(
        repo.full_name.clone(),
        FontId::proportional(13.5),
        theme::TEXT,
    );
    let at = Pos2::new(x, top - name.size().y / 2.0);
    let right = at.x + name.size().x;
    painter.galley(at, name.clone(), theme::TEXT);
    // Faux bold: the bundled proportional font has a single weight.
    painter.galley(at + Vec2::new(0.6, 0.0), name, theme::TEXT);
    if repo.private {
        graph_view::pill(
            &painter,
            right + 8.0,
            top,
            "private",
            theme::BORDER,
            theme::MUTED,
            false,
        );
    }
    if let Some(desc) = repo.description.as_deref().filter(|d| !d.is_empty()) {
        painter.text(
            Pos2::new(x, rect.top() + REPO_ROW_H * 0.72),
            Align2::LEFT_CENTER,
            desc.lines().next().unwrap_or_default(),
            FontId::proportional(12.0),
            theme::MUTED,
        );
    }
}
/// GitKraken-style "Create a Repository" modal; `Some(true)` = create, `Some(false)` = cancel,
/// `None` = still open.
fn create_dialog(ctx: &egui::Context, d: &mut CreateDialog) -> Option<bool> {
    let (mut result, mut browse) = (None, false);
    let target = d.target(paths::home().as_deref());
    let modal = egui::Modal::new(egui::Id::unique("create_dialog")).show(ctx, |ui| {
        ui.heading("Create a Repository");
        ui.add_space(6.0);
        ui.label(theme::caption("Name"));
        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(390.0));
        ui.label(theme::caption("Initialize in"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut d.dir).desired_width(320.0));
            browse = ui.button("Browse").clicked();
        });
        ui.label(theme::caption("Default branch"));
        ui.add(egui::TextEdit::singleline(&mut d.branch).desired_width(390.0));
        ui.add_space(6.0);
        ui.label(theme::caption("Full path"));
        let home = paths::home();
        let root = paths::expand_tilde(&d.dir, home.as_deref()).join(d.name.trim());
        ui.monospace(format!("{}/", root.display()));
        let wt = project::worktree_dir_name(d.branch.trim());
        ui.monospace(format!("{}/", root.join(wt).display()));
        ui.add_space(6.0);
        if let Err(reason) = &target {
            ui.colored_label(ui.visuals().error_fg_color, reason);
        }
        ui.horizontal(|ui| {
            let create = theme::primary("Create Repository");
            if ui.add_enabled(target.is_ok(), create).clicked() {
                result = Some(true);
            }
            if ui.button("Cancel").clicked() {
                result = Some(false);
            }
        });
    });
    // The click that closed the native picker must not count as a backdrop click.
    if result.is_none() && !browse && modal.should_close() {
        result = Some(false);
    }
    if browse {
        let typed = paths::expand_tilde(&d.dir, paths::home().as_deref());
        let start = Some(typed).filter(|p| p.is_dir()).or(d.default_dir.clone());
        if let Some(dir) = paths::pick_folder("Initialize in", start.as_deref()) {
            d.dir = dir.display().to_string();
            ctx.request_repaint(); // show the pick now, not on the next input
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Space;

    fn dialog(where_dir: &Path, url: &str, name: &str) -> CloneDialog {
        let mut d = CloneDialog::new(&Space::default());
        d.where_dir = where_dir.display().to_string();
        d.url = url.into();
        d.name = name.into();
        d
    }

    fn plan_with(items: usize) -> Plan {
        Plan {
            root: PathBuf::from("/tmp/proj"),
            git_dir: PathBuf::from("/tmp/proj/.git"),
            target_dir_name: "feature-x".into(),
            worktree_id: "feature-x".into(),
            items: (0..items).map(|i| PathBuf::from(format!("f{i}"))).collect(),
            branch: Some("feature/x".into()),
        }
    }

    #[test]
    fn convert_preview_shows_layout_and_counts_extra_items() {
        let (before, after) = convert_preview("proj", &plan_with(8));
        assert!(before.starts_with("proj/\n├─ .git/\n├─ f0\n"), "{before}");
        assert!(before.ends_with("└─ … 2 more\n"), "{before}");
        assert!(
            after.contains(".bare/") && after.contains("gitdir: ./.bare"),
            "{after}"
        );
        assert!(after.contains("└─ feature-x/\n   ├─ f0\n"), "{after}");
        assert!(after.ends_with("   └─ … 2 more\n"), "{after}");
        let (_, after) = convert_preview("proj", &plan_with(1));
        assert!(after.ends_with("feature-x/\n   └─ f0\n"), "{after}");
    }

    #[test]
    fn success_notice_lists_strays() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for d in [".bare", "main"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        std::fs::write(root.join(".git"), "gitdir: ./.bare\n").unwrap();
        assert_eq!(convert_strays_note(root, &root.join("main")), "");
        std::fs::write(root.join(".DS_Store"), "x").unwrap();
        let note = convert_strays_note(root, &root.join("main"));
        assert!(note.contains("not moved into main: .DS_Store"), "{note}");
    }

    #[test]
    fn not_now_persists_per_project() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let mut app = super::super::tests::test_app(super::super::tests::cfg(), &cfg_path);
        let root = PathBuf::from("/tmp/p1");
        app.apply(super::super::Action::DismissConvert(root.clone()));
        app.apply(super::super::Action::DismissConvert(root.clone()));
        let saved = crate::config::Config::load_from(&cfg_path).unwrap();
        assert_eq!(saved.convert_dismissed, [root]);
    }

    #[test]
    fn convert_refusal_is_shown_instead_of_running() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        app.apply(super::super::Action::RequestConvert(
            tmp.path().to_path_buf(),
        ));
        let d = app.dialogs.convert.as_ref().expect("dialog open");
        assert!(d.plan.is_err(), "not a repo, so plan refuses");
    }

    #[test]
    fn any_open_sees_every_dialog() {
        let mut d = Dialogs::default();
        assert!(!d.any_open());
        d.space = Some(spaces::SpaceEdit::new_space());
        assert!(d.any_open());
        d.space = None;
        d.clone = Some(CloneDialog::new(&Space::default()));
        assert!(d.any_open());
        d.clone = None;
        d.create = Some(CreateDialog::new(&Space::default()));
        assert!(d.any_open());
        d.create = None;
        d.new_worktree = Some(NewWorktreeDialog {
            base: "main".into(),
            branch: String::new(),
            worktree: true,
        });
        assert!(d.any_open());
        d.new_worktree = None;
        d.confirm_remove = Some(PathBuf::from("/tmp/wt"));
        assert!(d.any_open());
        d.confirm_remove = None;
        d.confirm_drop_stash = Some(DropStash {
            root: PathBuf::from("/tmp/r"),
            worktree: PathBuf::from("/tmp/r"),
            hash: "h".into(),
            label: "l".into(),
        });
        assert!(d.any_open());
        d.confirm_drop_stash = None;
        d.convert = Some(ConvertDialog {
            root: PathBuf::from("/tmp/r"),
            plan: Err("no".into()),
        });
        assert!(d.any_open());
        d.convert = None;
        d.delete_branch = Some(DeleteBranchDialog {
            branch: "topic".into(),
            remote: None,
            also_remote: false,
            not_merged: false,
        });
        assert!(d.any_open());
        d.delete_branch = None;
        d.delete_remote = Some("origin/topic".into());
        assert!(d.any_open());
    }

    #[test]
    fn clone_target_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let canon = tmp.path().canonicalize().unwrap();
        let url = "https://github.com/org/proj.git";
        let d = dialog(tmp.path(), url, " proj ");
        assert_eq!(d.target(None).unwrap(), (canon.clone(), "proj".into()));
        assert!(dialog(tmp.path(), "  ", "proj").target(None).is_err());
        assert!(dialog(tmp.path(), url, "..").target(None).is_err());
        assert!(dialog(&tmp.path().join("nope"), url, "proj")
            .target(None)
            .is_err());
        std::fs::create_dir(tmp.path().join("proj")).unwrap();
        let err = d.target(None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn prefills_from_space() {
        let space = Space {
            host: "ghe.example.com".into(),
            default_clone_dir: Some(PathBuf::from("/tmp/code")),
            ..Default::default()
        };
        let d = CloneDialog::new(&space);
        assert_eq!(d.where_dir, "/tmp/code");
        assert_eq!(d.github, "GitHub Enterprise (ghe.example.com)");
        assert_eq!(github_label("github.com"), "GitHub.com");
    }

    fn repo(name: &str) -> github::Repo {
        github::Repo {
            full_name: format!("octo-org/{name}"),
            name: name.into(),
            owner: "octo-org".into(),
            private: false,
            description: None,
            clone_url: format!("https://example.com/octo-org/{name}.git"),
            pushed_at: None,
        }
    }

    #[test]
    fn selecting_a_repo_sets_url_and_name_until_edited() {
        let mut d = CloneDialog::new(&Space::default());
        d.set_source(Source::GitHub);
        d.select_repo(&repo("widget"));
        assert_eq!(d.url, "https://example.com/octo-org/widget.git");
        assert_eq!(d.name, "widget");
        d.select_repo(&repo("gadget"));
        assert_eq!(d.name, "gadget");
        d.name = "mine".into();
        d.name_edited = true;
        d.select_repo(&repo("widget"));
        assert_eq!(d.url, "https://example.com/octo-org/widget.git");
        assert_eq!(d.name, "mine");
    }

    #[test]
    fn switching_source_clears_an_unedited_pick() {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = dialog(tmp.path(), "https://example.com/a/typed.git", "typed");
        d.set_source(Source::GitHub);
        assert!(d.focus_search);
        assert_eq!((d.url.as_str(), d.name.as_str()), ("", ""));
        assert_eq!(d.target(None).unwrap_err(), "Select a repository");
        d.select_repo(&repo("widget"));
        d.set_source(Source::Url);
        assert_eq!(d.url, "");
        assert_eq!(d.target(None).unwrap_err(), "Enter a URL");
        d.name = "kept".into();
        d.name_edited = true;
        d.set_source(Source::GitHub);
        assert_eq!(d.name, "kept");
    }

    #[test]
    fn listing_arrives_through_poll() {
        let mut d = CloneDialog::new(&Space::default());
        let (tx, rx) = std::sync::mpsc::channel();
        d.browse = Browse::Loading(rx);
        d.poll();
        assert!(matches!(d.browse, Browse::Loading(_)));
        let list = github::RepoList {
            repos: vec![repo("widget")],
            truncated: false,
        };
        tx.send(Listing::Repos(list)).unwrap();
        d.poll();
        assert!(matches!(&d.browse, Browse::Done(Listing::Repos(l)) if l.repos.len() == 1));
        assert!(d.focus_search);
        let (tx, rx) = std::sync::mpsc::channel::<Listing>();
        d.browse = Browse::Loading(rx);
        drop(tx);
        d.poll();
        assert!(matches!(d.browse, Browse::Done(Listing::Failed(_))));
    }

    #[test]
    fn clone_refuses_a_dialog_from_another_space() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = super::super::tests::cfg();
        let mut app = super::super::tests::test_app(cfg, &tmp.path().join("config.toml"));
        let other = app.config.spaces[1].clone();
        assert_ne!(app.space().unwrap().id, other.id);
        let mut d = CloneDialog::new(&other);
        d.where_dir = tmp.path().display().to_string();
        d.url = "https://example.com/octo-org/widget.git".into();
        d.name = "widget".into();
        app.start_clone(d);
        assert!(!app.jobs.any(JobKind::Clone), "no clone started");
        assert!(
            app.errors.iter().any(|e| e.contains("space")),
            "{:?}",
            app.errors
        );
        assert!(app.dialogs.clone.is_none());
    }

    #[test]
    fn create_target_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let canon = tmp.path().canonicalize().unwrap();
        let mut d = CreateDialog::new(&Space::default());
        assert_eq!(d.branch, "main");
        d.dir = tmp.path().display().to_string();
        d.name = " fresh ".into();
        let want = (canon, "fresh".to_string(), "main".to_string());
        assert_eq!(d.target(None).unwrap(), want);
        for bad in ["", "a..b", "-x"] {
            d.branch = bad.into();
            assert!(d.target(None).is_err(), "{bad:?}");
        }
        d.branch = " feature/x ".into();
        assert_eq!(d.target(None).unwrap().2, "feature/x");
        d.name = "a/b".into();
        assert!(d.target(None).is_err());
        d.name = "fresh".into();
        std::fs::create_dir(tmp.path().join("fresh")).unwrap();
        assert!(d.target(None).unwrap_err().contains("already exists"));
    }
}
