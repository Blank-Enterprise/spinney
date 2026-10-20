//! Modal dialogs: clone, create, new worktree, confirm worktree removal and branch deletion,
//! convert, merge and rebase.

use super::{
    actions, add_project, graph_view, paths, project, repo_name_from_url, spaces, theme,
    toolbar::folder_name, visible_projects, JobKind, ProjectTab, SpinneyApp,
};
use crate::actions::{MergeMode, MergeOutcome};
use crate::convert::{self, Plan};
use crate::{creds, github, refs};
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
    /// "Use the worktree layout": `.bare` plus a folder per branch, or a normal clone.
    worktrees: bool,
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
            worktrees: true,
        }
    }

    fn layout(&self) -> project::Layout {
        if self.worktrees {
            project::Layout::WorktreeRoot
        } else {
            project::Layout::NormalClone
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
    pub(super) mode: BranchMode,
}

/// What the new-branch dialog does with the branch it creates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum BranchMode {
    /// Create a worktree for it (worktree layout).
    Worktree,
    /// Create it and switch to it (normal clone).
    Switch,
    /// Only create it (a commit's "Create branch here…" in the worktree layout).
    Create,
}

/// The dialog's name for `base`: a full commit hash is shortened, a branch name kept.
pub(super) fn base_label(base: &str) -> &str {
    let full = base.len() == 40 && base.bytes().all(|b| b.is_ascii_hexdigit());
    if full {
        &base[..8]
    } else {
        base
    }
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

/// Rename local `branch` (asked in `worktree`) to `name`, and maybe on its remote too.
pub(super) struct RenameBranchDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) branch: String,
    pub(super) name: String,
    /// Offered when the upstream has the same name on its remote.
    pub(super) remote: Option<actions::RemoteRename>,
    pub(super) also_remote: bool,
    /// Last name checked with git, and whether it was valid (git runs once per edit).
    checked: Option<(String, bool)>,
}

impl RenameBranchDialog {
    pub(super) fn new(
        root: PathBuf,
        worktree: PathBuf,
        branch: String,
        remote: Option<actions::RemoteRename>,
    ) -> Self {
        RenameBranchDialog {
            root,
            worktree,
            name: branch.clone(),
            branch,
            remote,
            also_remote: false,
            checked: None,
        }
    }

    /// The trimmed new name, or why Rename is disabled.
    fn target(&mut self) -> Result<String, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("Enter a branch name".into());
        }
        if name == self.branch {
            return Err("Enter a new name".into());
        }
        let valid = match &self.checked {
            Some((n, ok)) if *n == name => *ok,
            _ => {
                let ok = project::check_branch_name(&name).is_ok();
                self.checked = Some((name.clone(), ok));
                ok
            }
        };
        if !valid {
            return Err(format!("{name} is not a valid branch name"));
        }
        Ok(name)
    }
}

/// Remove a worktree and delete its branch, as planned (and checked) when asked.
pub(super) struct RemoveWithBranchDialog {
    pub(super) root: PathBuf,
    pub(super) plan: actions::RemoveWithBranch,
}

/// The question, what it loses, and the button label for removing a worktree with its
/// branch.
fn remove_with_branch_text(p: &actions::RemoveWithBranch) -> (String, Vec<String>, &'static str) {
    let question = format!(
        "Remove worktree {} and delete branch {}?",
        folder_name(&p.path),
        p.branch
    );
    let mut lost = vec![];
    if p.dirty {
        lost.push("Its uncommitted and untracked files will be lost.".to_string());
    }
    let commits = match p.unmerged {
        0 => None,
        1 => Some("1 commit".to_string()),
        n => Some(format!("{n} commits")),
    };
    if let Some(c) = commits {
        lost.push(format!(
            "{} is not fully merged: {c} not on {} will be deleted with it.",
            p.branch, p.base
        ));
    }
    let button = if p.unmerged > 0 {
        "Remove and delete anyway"
    } else {
        "Remove and delete"
    };
    (question, lost, button)
}

/// Merge `branch` into `into` (checked out in `worktree`), in the chosen mode.
pub(super) struct MergeDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) branch: String,
    pub(super) into: String,
    pub(super) mode: MergeMode,
}

/// Rebase `current` (checked out in `worktree`) onto `onto`; `pushed` commits it rewrites
/// are already on the upstream.
pub(super) struct RebaseDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) current: String,
    pub(super) onto: String,
    pub(super) pushed: usize,
}

#[derive(Default)]
pub(super) struct Dialogs {
    pub(super) space: Option<spaces::SpaceEdit>,
    pub(super) clone: Option<CloneDialog>,
    pub(super) create: Option<CreateDialog>,
    pub(super) new_worktree: Option<NewWorktreeDialog>,
    /// Dirty worktree awaiting a forced-removal confirmation.
    pub(super) confirm_remove: Option<PathBuf>,
    pub(super) remove_with_branch: Option<RemoveWithBranchDialog>,
    pub(super) delete_branch: Option<DeleteBranchDialog>,
    /// Remote branch (`origin/feature/x`) awaiting a delete confirmation.
    pub(super) delete_remote: Option<String>,
    pub(super) rename_branch: Option<RenameBranchDialog>,
    pub(super) confirm_drop_stash: Option<DropStash>,
    pub(super) convert: Option<ConvertDialog>,
    pub(super) merge: Option<MergeDialog>,
    pub(super) rebase: Option<RebaseDialog>,
    /// The menu shown after dropping one branch onto another.
    pub(super) drop_menu: Option<super::branch_drop::DropMenu>,
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
            || self.remove_with_branch.is_some()
            || self.confirm_drop_stash.is_some()
            || self.convert.is_some()
            || self.delete_branch.is_some()
            || self.delete_remote.is_some()
            || self.rename_branch.is_some()
            || self.merge.is_some()
            || self.rebase.is_some()
            || self.drop_menu.is_some()
    }
}

/// Red, bold button for a destructive confirmation.
fn danger(label: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).color(theme::ON_ACCENT).strong())
        .fill(theme::DELETED)
}

/// Conflicted files named in a stopped merge or rebase's message; the rest are counted.
const CONFLICTS_SHOWN: usize = 5;

/// "a.txt, b.txt", or the first few "and 2 more".
fn conflict_list(files: &[String]) -> String {
    let mut list = files[..files.len().min(CONFLICTS_SHOWN)].join(", ");
    if files.len() > CONFLICTS_SHOWN {
        list += &format!(" and {} more", files.len() - CONFLICTS_SHOWN);
    }
    list
}

/// "Merge stopped on conflicts in a.txt, b.txt. Resolve them, then Continue."
pub(super) fn conflict_message(op: &str, files: &[String]) -> String {
    let list = conflict_list(files);
    format!("{op} stopped on conflicts in {list}. Resolve them, then Continue.")
}

/// "Squash stopped: conflicts in a.txt. Nothing was changed."
fn squash_conflict_message(files: &[String]) -> String {
    let list = conflict_list(files);
    format!("Squash stopped: conflicts in {list}. Nothing was changed.")
}

/// The rebase confirmation's warning when it rewrites `pushed` commits of `current`.
fn rebase_warning(pushed: usize, current: &str) -> Option<String> {
    let commits = match pushed {
        0 => return None,
        1 => "1 commit".to_string(),
        n => format!("{n} commits"),
    };
    Some(format!(
        "{commits} of {current} already pushed will be rewritten. Pushing afterwards needs a \
         force push."
    ))
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
            let title = match d.mode {
                BranchMode::Worktree => "New worktree",
                BranchMode::Switch | BranchMode::Create => "New branch",
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(theme::caption(&format!(
                        "New branch from {}",
                        base_label(&d.base)
                    )));
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
        if let Some(d) = self.dialogs.remove_with_branch.take() {
            let mut result = None;
            let (question, lost, button) = remove_with_branch_text(&d.plan);
            egui::Modal::new(egui::Id::unique("remove_with_branch")).show(ctx, |ui| {
                ui.label(question);
                for line in lost {
                    ui.colored_label(theme::WARN, format!("⚠ {line}"));
                }
                ui.horizontal(|ui| {
                    if ui.add(danger(button)).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => self.remove_with_branch(d),
                Some(false) => {}
                None => self.dialogs.remove_with_branch = Some(d),
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
                    if ui.add(danger("Drop")).clicked() {
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
        if let Some(mut d) = self.dialogs.rename_branch.take() {
            let mut result = None;
            let target = d.target();
            egui::Modal::new(egui::Id::unique("rename_branch")).show(ctx, |ui| {
                ui.label(format!("Rename {}", d.branch));
                ui.text_edit_singleline(&mut d.name);
                if let Some(r) = &d.remote {
                    let new = target.as_deref().unwrap_or("the new name");
                    ui.checkbox(&mut d.also_remote, format!("Also rename on {}", r.remote));
                    ui.label(theme::caption(&format!(
                        "Pushes {new} and deletes {} on {}.",
                        d.branch, r.remote
                    )));
                }
                if let Err(reason) = &target {
                    ui.colored_label(ui.visuals().error_fg_color, reason);
                }
                ui.horizontal(|ui| {
                    let rename = theme::primary("Rename");
                    if ui.add_enabled(target.is_ok(), rename).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match (result, target) {
                (Some(true), Ok(name)) => self.rename_branch(d, name),
                (Some(false), _) => {}
                _ => self.dialogs.rename_branch = Some(d),
            }
        }
        if let Some(m) = self.dialogs.drop_menu.take() {
            // Only for the tab it was dropped in.
            let here = self.asking_tab(Some(m.root.clone())).is_some();
            match super::branch_drop::menu(ctx, &m) {
                _ if !here => {}
                Some(Some(kind)) => {
                    let action = super::branch_drop::choice_action(kind, &m.dragged, &m.target);
                    self.apply(action);
                }
                Some(None) => {}
                None => self.dialogs.drop_menu = Some(m),
            }
        }
        if let Some(mut d) = self.dialogs.merge.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("merge")).show(ctx, |ui| {
                ui.label(format!("Merge {} into {}", d.branch, d.into));
                ui.add_space(4.0);
                let modes = [
                    (MergeMode::FastForward, "Fast-forward if possible"),
                    (MergeMode::NoFf, "Always create a merge commit"),
                    (MergeMode::Squash, "Squash"),
                ];
                for (mode, label) in modes {
                    ui.radio_value(&mut d.mode, mode, label);
                }
                if d.mode == MergeMode::Squash {
                    ui.label(theme::caption(
                        "The changes are staged for you to review and commit.",
                    ));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::primary("Merge")).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => self.merge(d),
                Some(false) => {}
                None => self.dialogs.merge = Some(d),
            }
        }
        if let Some(d) = self.dialogs.rebase.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("rebase")).show(ctx, |ui| {
                ui.label(format!("Rebase {} onto {}?", d.current, d.onto));
                ui.label(theme::caption(&format!(
                    "Commits on {} that aren't on {} are replayed on top of it.",
                    d.current, d.onto
                )));
                let warning = rebase_warning(d.pushed, &d.current);
                if let Some(w) = &warning {
                    ui.colored_label(theme::WARN, format!("⚠ {w}"));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let rebase = if warning.is_some() {
                        danger("Rebase")
                    } else {
                        theme::primary("Rebase")
                    };
                    if ui.add(rebase).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => self.rebase(d),
                Some(false) => {}
                None => self.dialogs.rebase = Some(d),
            }
        }
    }

    /// Open the merge or (with `rebase`) rebase confirmation for `branch`, refusing now
    /// rather than after the confirmation what would fail anyway.
    pub(super) fn ask_merge(&mut self, branch: String, rebase: bool, errors: &mut Vec<String>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        // A pull or push may be moving the same branch (the toolbar disables it too).
        if self.jobs.network_busy(&tab.root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let (root, worktree) = (tab.root.clone(), tab.active_worktree.clone());
        let current = match refs::current_branch(&worktree) {
            Ok(Some(c)) => c,
            Ok(None) => return errors.push("Check out a branch first".into()),
            Err(e) => return errors.push(format!("{e:#}")),
        };
        if let Err(e) = actions::ensure_clean(&worktree) {
            return errors.push(format!("{e:#}"));
        }
        if !rebase {
            self.dialogs.merge = Some(MergeDialog {
                root,
                worktree,
                branch,
                into: current,
                mode: MergeMode::FastForward,
            });
            return;
        }
        match actions::rebase_rewrites_pushed(&worktree, &branch) {
            Ok(pushed) => {
                self.dialogs.rebase = Some(RebaseDialog {
                    root,
                    worktree,
                    current,
                    onto: branch,
                    pushed,
                })
            }
            Err(e) => errors.push(format!("upstream: {e:#}")),
        }
    }

    /// The tab a merge or rebase was asked in, unless its branch changed since.
    fn merge_tab(
        &mut self,
        root: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<&mut ProjectTab, String> {
        let tab = self.tabs.iter_mut().find(|t| t.root == root);
        let tab = tab.ok_or_else(|| "The project was closed".to_string())?;
        match refs::current_branch(worktree) {
            Ok(Some(c)) if c == branch => Ok(tab),
            Ok(_) => Err(format!("{branch} is no longer checked out; try again")),
            Err(e) => Err(format!("{e:#}")),
        }
    }

    /// Merge synchronously, like checkout; a squash prefills the commit card.
    fn merge(&mut self, d: MergeDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let tab = match self.merge_tab(&d.root, &d.worktree, &d.into) {
            Ok(tab) => tab,
            Err(e) => return self.errors.push(e),
        };
        let res = actions::merge(&d.worktree, &d.branch, d.mode);
        tab.refresh(&ctx, &mut errors);
        let mut notice = None;
        match res {
            Ok(MergeOutcome::Done) if d.mode == MergeMode::Squash => {
                // Committed through the normal card, never as an amend of HEAD.
                if tab.amend {
                    tab.set_amend(false, &mut errors);
                }
                if tab.commit_summary.is_empty() && tab.commit_desc.is_empty() {
                    tab.commit_summary = format!("Squash {} into {}", d.branch, d.into);
                }
                notice = Some(format!("Squashed {}: review and commit", d.branch));
            }
            Ok(MergeOutcome::Done) => {}
            Ok(MergeOutcome::UpToDate) => {
                notice = Some(format!(
                    "{} is already up to date with {}",
                    d.into, d.branch
                ))
            }
            Ok(MergeOutcome::Conflicts(files)) => notice = Some(conflict_message("Merge", &files)),
            Ok(MergeOutcome::SquashConflicts(files)) => {
                errors.push(squash_conflict_message(&files))
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.notices.extend(notice);
        self.errors.extend(errors);
    }

    /// Rebase synchronously, like checkout.
    fn rebase(&mut self, d: RebaseDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let tab = match self.merge_tab(&d.root, &d.worktree, &d.current) {
            Ok(tab) => tab,
            Err(e) => return self.errors.push(e),
        };
        let res = actions::rebase(&d.worktree, &d.onto);
        tab.refresh(&ctx, &mut errors);
        match res {
            Ok(MergeOutcome::Done) => {}
            Ok(MergeOutcome::UpToDate) => self.notices.push(format!(
                "{} is already up to date with {}",
                d.current, d.onto
            )),
            Ok(MergeOutcome::Conflicts(files)) => {
                self.notices.push(conflict_message("Rebase", &files))
            }
            // Only a squash merge undoes itself.
            Ok(MergeOutcome::SquashConflicts(_)) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.errors.extend(errors);
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
        self.forget_pending(&root);
        // Canonical, like the paths `git worktree list` gives refresh() to match against.
        let target = plan.root.join(&plan.target_dir_name);
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

    /// Open the rename dialog for local `branch`, refusing now rather than after the
    /// dialog a branch another worktree has checked out.
    pub(super) fn ask_rename(&mut self, branch: String, errors: &mut Vec<String>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if self.jobs.network_busy(&tab.root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let worktree = tab.active_worktree.clone();
        if let Err(e) = actions::ensure_not_checked_out_elsewhere(&worktree, &branch) {
            return errors.push(format!("{e:#}"));
        }
        match actions::remote_rename(&worktree, &branch) {
            Ok(remote) => {
                let d = RenameBranchDialog::new(tab.root.clone(), worktree, branch, remote);
                self.dialogs.rename_branch = Some(d);
            }
            Err(e) => errors.push(format!("upstream: {e:#}")),
        }
    }

    /// Rename locally now; the remote half runs as a job, like a push.
    pub(super) fn rename_branch(&mut self, d: RenameBranchDialog, name: String) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let Some(tab) = self.tabs.iter_mut().find(|t| t.root == d.root) else {
            return;
        };
        let res = actions::rename_branch(&d.worktree, &d.branch, &name);
        tab.refresh(&ctx, &mut errors);
        match res {
            Ok(()) => {
                if let Some(r) = d.remote.filter(|_| d.also_remote) {
                    let (wt, old) = (d.worktree, d.branch);
                    self.jobs
                        .spawn(JobKind::RenameRemote, d.root, String::new(), move || {
                            actions::rename_on_remote(&wt, &r, &old, &name)
                        });
                }
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.errors.extend(errors);
    }

    /// Remove the worktree, then delete its branch; a branch that turns out unmerged
    /// gets the usual "not fully merged" prompt.
    pub(super) fn remove_with_branch(&mut self, d: RemoveWithBranchDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let Some(tab) = self.tabs.iter_mut().find(|t| t.root == d.root) else {
            return;
        };
        let res = actions::remove_with_branch(&d.root, &d.plan);
        tab.refresh(&ctx, &mut errors);
        match res {
            Ok(actions::DeleteOutcome::Deleted) => {}
            Ok(actions::DeleteOutcome::NotMerged) => {
                self.dialogs.delete_branch = Some(DeleteBranchDialog {
                    branch: d.plan.branch,
                    remote: None,
                    also_remote: false,
                    not_merged: true,
                })
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
        let (id, layout) = (space.id.clone(), d.layout());
        self.jobs
            .spawn(JobKind::Clone, parent.join(&name), id, move || {
                project::clone_into(&url, &parent, &name, &space, layout).map(|_| ())
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
            let wt = tab.active_worktree.clone();
            let res = match d.mode {
                BranchMode::Worktree => {
                    project::add_worktree(&tab.root, branch, &d.base).map(|path| {
                        tab.active_worktree = path;
                    })
                }
                BranchMode::Switch => actions::create_and_switch(&wt, branch, &d.base),
                BranchMode::Create => actions::create_branch(&wt, branch, &d.base),
            };
            if let Err(e) = res {
                errors.push(format!("{e:#}"));
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

/// The "Clone a Repository" modal; `Some(true)` = clone, `Some(false)` = cancel
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
    ui.checkbox(&mut d.worktrees, "Use the worktree layout");
    ui.weak(if d.worktrees {
        "worktrees go in folders inside this one"
    } else {
        "a normal clone; you can convert it later"
    });
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
/// The "Create a Repository" modal; `Some(true)` = create, `Some(false)` = cancel,
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
            mode: BranchMode::Worktree,
        });
        assert!(d.any_open());
        d.new_worktree = None;
        d.confirm_remove = Some(PathBuf::from("/tmp/wt"));
        assert!(d.any_open());
        d.confirm_remove = None;
        d.remove_with_branch = Some(RemoveWithBranchDialog {
            root: PathBuf::from("/tmp/r"),
            plan: plan(false, 0),
        });
        assert!(d.any_open());
        d.remove_with_branch = None;
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
        d.delete_remote = None;
        d.rename_branch = Some(RenameBranchDialog::new(
            PathBuf::from("/tmp/r"),
            PathBuf::from("/tmp/r"),
            "topic".into(),
            None,
        ));
        assert!(d.any_open());
        d.rename_branch = None;
        d.merge = Some(MergeDialog {
            root: PathBuf::from("/tmp/r"),
            worktree: PathBuf::from("/tmp/r"),
            branch: "topic".into(),
            into: "main".into(),
            mode: MergeMode::FastForward,
        });
        assert!(d.any_open());
        d.merge = None;
        d.rebase = Some(RebaseDialog {
            root: PathBuf::from("/tmp/r"),
            worktree: PathBuf::from("/tmp/r"),
            current: "main".into(),
            onto: "topic".into(),
            pushed: 0,
        });
        assert!(d.any_open());
    }

    #[test]
    fn conflict_message_names_a_few_files() {
        let files = |n: usize| (0..n).map(|i| format!("f{i}")).collect::<Vec<_>>();
        assert_eq!(
            conflict_message("Merge", &files(2)),
            "Merge stopped on conflicts in f0, f1. Resolve them, then Continue."
        );
        assert_eq!(
            conflict_message("Rebase", &files(7)),
            "Rebase stopped on conflicts in f0, f1, f2, f3, f4 and 2 more. Resolve them, then Continue."
        );
        assert_eq!(
            squash_conflict_message(&files(1)),
            "Squash stopped: conflicts in f0. Nothing was changed."
        );
    }

    #[test]
    fn rebase_warns_only_when_rewriting_pushed_commits() {
        assert_eq!(rebase_warning(0, "main"), None);
        let one = rebase_warning(1, "main").unwrap();
        assert!(one.starts_with("1 commit of main already pushed"), "{one}");
        let two = rebase_warning(2, "main").unwrap();
        assert!(two.starts_with("2 commits of main"), "{two}");
    }

    #[test]
    fn base_label_shortens_only_full_hashes() {
        let sha = "83174b7246c76c9b6426a95147a715f0d26bf6c6";
        assert_eq!(base_label(sha), "83174b72");
        assert_eq!(base_label("main"), "main");
        assert_eq!(base_label("feature/x"), "feature/x");
        assert_eq!(base_label("HEAD"), "HEAD");
    }

    #[test]
    fn commit_menu_checks_out_and_branches_from_a_commit() {
        use super::super::Action;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        let g = |args: &[&str]| crate::git::run(d, args).unwrap().trim().to_string();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        let init = g(&["rev-parse", "HEAD"]);
        g(&["commit", "-q", "--allow-empty", "-m", "second"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        // Refused past the disabled menu item while a tracked file is changed.
        std::fs::write(d.join("a.txt"), "changed\n").unwrap();
        app.apply(Action::CheckoutCommit(init.clone()));
        assert!(app.errors.iter().any(|e| e.contains("Commit or stash")));
        assert_eq!(g(&["symbolic-ref", "--short", "HEAD"]), "main");
        app.apply(Action::BranchAt(init.clone()));
        let mut b = app.dialogs.new_worktree.take().expect("dialog open");
        assert_eq!(b.mode, BranchMode::Switch, "normal clone switches");
        b.branch = "old".into();
        app.errors.clear();
        app.new_worktree(b);
        assert!(app.errors.iter().any(|e| e.contains("Commit or stash")));
        g(&["checkout", "--", "a.txt"]);
        app.errors.clear();

        app.apply(Action::BranchAt(init.clone()));
        let mut b = app.dialogs.new_worktree.take().unwrap();
        b.branch = "old".into();
        app.new_worktree(b);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["symbolic-ref", "--short", "HEAD"]), "old");
        assert_eq!(g(&["rev-parse", "HEAD"]), init);

        g(&["switch", "-q", "main"]);
        app.apply(Action::CheckoutCommit(init.clone()));
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["rev-parse", "HEAD"]), init);
        assert!(
            crate::refs::current_branch(d).unwrap().is_none(),
            "detached"
        );
    }

    #[test]
    fn merge_is_refused_up_front_and_squash_prefills_the_commit_card() {
        use super::super::Action;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        let g = |args: &[&str]| crate::git::run(d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        g(&["switch", "-q", "-c", "topic"]);
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "b"]);
        g(&["switch", "-q", "main"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        std::fs::write(d.join("a.txt"), "changed\n").unwrap();
        app.apply(Action::AskMerge("topic".into()));
        assert!(app.errors.iter().any(|e| e.contains("Commit or stash")));
        assert!(app.dialogs.merge.is_none(), "refused before asking");
        g(&["checkout", "--", "a.txt"]);
        app.errors.clear();

        app.apply(Action::AskMerge("topic".into()));
        let mut m = app.dialogs.merge.take().expect("dialog open");
        assert_eq!((m.into.as_str(), m.mode), ("main", MergeMode::FastForward));
        m.mode = MergeMode::Squash;
        app.merge(m);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        let tab = &app.tabs[0];
        assert_eq!(tab.commit_summary, "Squash topic into main");
        assert_eq!(tab.status.staged.len(), 1);
        assert_eq!(tab.status.staged[0].path, "b.txt");
        assert!(app
            .notices
            .contains(&"Squashed topic: review and commit".into()));

        g(&["reset", "-q", "--hard"]);
        g(&["checkout", "-q", "--detach"]);
        app.apply(Action::AskRebase("topic".into()));
        assert!(app.errors.iter().any(|e| e == "Check out a branch first"));
        assert!(app.dialogs.rebase.is_none());
    }

    /// An app with one tab on a repo whose `main` and `topic` change `a.txt` differently.
    fn conflicting_app() -> (tempfile::TempDir, tempfile::TempDir, SpinneyApp) {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        let g = |args: &[&str]| crate::git::run(d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        g(&["switch", "-q", "-c", "topic"]);
        std::fs::write(d.join("a.txt"), "topic\n").unwrap();
        g(&["commit", "-qam", "topic"]);
        g(&["switch", "-q", "main"]);
        std::fs::write(d.join("a.txt"), "main\n").unwrap();
        g(&["commit", "-qam", "main"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);
        (tmp, proj, app)
    }

    /// Merge `topic` into the checked-out branch through the dialog.
    fn merge_topic(app: &mut SpinneyApp) {
        app.apply(super::super::Action::AskMerge("topic".into()));
        let m = app.dialogs.merge.take().expect("dialog open");
        app.merge(m);
    }

    #[test]
    fn conflicted_merge_shows_the_banner_until_continued_or_aborted() {
        use super::super::in_progress::Step;
        use super::super::Action;
        use crate::actions::Operation;
        let (_tmp, proj, mut app) = conflicting_app();
        let d = proj.path();
        let g = |args: &[&str]| crate::git::run(d, args).unwrap();
        let merge = merge_topic;

        merge(&mut app);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert!(app.notices.iter().any(|n| n.contains("Resolve them")));
        assert_eq!(app.tabs[0].operation, Some(Operation::Merge));
        assert_eq!(app.tabs[0].status.conflicted.len(), 1);

        // Abort asks first, and the question goes when the tab is left.
        app.apply(Action::AskOpStep(Step::Abort));
        assert_eq!(app.confirm_op.as_ref().map(|p| p.step), Some(Step::Abort));
        app.apply(Action::SelectTab(0));
        assert!(app.confirm_op.is_none());
        app.apply(Action::AskOpStep(Step::Abort));
        app.apply(Action::ConfirmOpStep);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].operation, None);
        assert!(app.tabs[0].status.conflicted.is_empty());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "main\n");

        // Continue is refused while conflicted, then finishes the merge.
        merge(&mut app);
        app.apply(Action::ContinueOp);
        assert!(
            app.errors.iter().any(|e| e.contains("Resolve")),
            "{:?}",
            app.errors
        );
        app.errors.clear();
        std::fs::write(d.join("a.txt"), "both\n").unwrap();
        let f = app.tabs[0].status.conflicted[0].clone();
        app.apply(Action::StageFile(f, false));
        assert_eq!(app.tabs[0].operation, Some(Operation::Merge));
        app.apply(Action::ContinueOp);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].operation, None);
        assert_eq!(g(&["log", "-1", "--format=%P"]).split(' ').count(), 2);
    }

    #[test]
    fn resolve_with_mine_after_markers_are_refused_then_continue() {
        use super::super::{Action, Resolve};
        use crate::actions::Version;
        let (_tmp, proj, mut app) = conflicting_app();
        let d = proj.path();
        merge_topic(&mut app);

        // Mark resolved with the markers still in: asks instead.
        app.apply(Action::MarkResolved("a.txt".into()));
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].status.conflicted.len(), 1);
        let asked = app.confirm_resolve.as_ref().map(|(_, w)| w.clone());
        assert_eq!(asked, Some(Resolve::Anyway("a.txt".into())));
        app.apply(Action::CancelResolve);

        let f = app.tabs[0].status.conflicted[0].clone();
        app.apply(Action::AskResolve(Resolve::Use(f, Version::Mine)));
        assert_eq!(app.tabs[0].status.conflicted.len(), 1, "only asked");
        app.apply(Action::ConfirmResolve);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert!(app.tabs[0].status.conflicted.is_empty());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "main\n");

        app.apply(Action::ContinueOp);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].operation, None);
        assert!(app.tabs[0].status.changed_files() == 0);
    }

    fn plan(dirty: bool, unmerged: usize) -> actions::RemoveWithBranch {
        actions::RemoveWithBranch {
            path: PathBuf::from("/p/proj/feat-x"),
            branch: "feat/x".into(),
            dirty,
            base: "origin/feat/x".into(),
            unmerged,
        }
    }

    #[test]
    fn remove_with_branch_names_what_is_lost() {
        let (q, lost, button) = remove_with_branch_text(&plan(false, 0));
        assert_eq!(q, "Remove worktree feat-x and delete branch feat/x?");
        assert!(lost.is_empty());
        assert_eq!(button, "Remove and delete");
        let (_, lost, button) = remove_with_branch_text(&plan(true, 2));
        assert_eq!(
            lost,
            [
                "Its uncommitted and untracked files will be lost.",
                "feat/x is not fully merged: 2 commits not on origin/feat/x will be deleted \
                 with it."
            ]
        );
        assert_eq!(button, "Remove and delete anyway");
        let (_, lost, _) = remove_with_branch_text(&plan(false, 1));
        assert!(lost[0].contains(": 1 commit not on"), "{lost:?}");
    }

    #[test]
    fn rename_dialog_validates_the_new_name() {
        let mut d = RenameBranchDialog::new(
            PathBuf::from("/tmp/r"),
            PathBuf::from("/tmp/r"),
            "topic".into(),
            None,
        );
        assert_eq!(d.name, "topic", "prefilled");
        assert_eq!(d.target().unwrap_err(), "Enter a new name");
        d.name = "  ".into();
        assert_eq!(d.target().unwrap_err(), "Enter a branch name");
        d.name = "a..b".into();
        assert_eq!(d.target().unwrap_err(), "a..b is not a valid branch name");
        d.name = " feat/topic ".into();
        assert_eq!(d.target().unwrap(), "feat/topic");
    }

    #[test]
    fn rename_is_refused_up_front_for_a_branch_out_elsewhere() {
        use super::super::Action;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path().join("repo");
        std::fs::create_dir(&d).unwrap();
        let g = |args: &[&str]| crate::git::run(&d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        g(&["branch", "topic"]);
        g(&["branch", "other"]);
        let elsewhere = proj.path().join("other");
        g(&[
            "worktree",
            "add",
            "-q",
            elsewhere.to_str().unwrap(),
            "other",
        ]);
        let space = app.space().unwrap();
        app.open_tab(&d, &space);

        app.apply(Action::AskRename("other".into()));
        assert!(app.dialogs.rename_branch.is_none(), "refused before asking");
        assert!(app
            .errors
            .iter()
            .any(|e| e.contains("checked out in worktree other")));
        app.errors.clear();

        app.apply(Action::AskRename("topic".into()));
        let mut r = app.dialogs.rename_branch.take().expect("dialog open");
        assert!(r.remote.is_none(), "no upstream, no remote rename");
        r.name = "feat/topic".into();
        let name = r.target().unwrap();
        app.rename_branch(r, name);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        let names: Vec<_> = app.tabs[0]
            .branches
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert!(
            names.contains(&"feat/topic") && !names.contains(&"topic"),
            "{names:?}"
        );
        assert!(!app.jobs.any(JobKind::RenameRemote));
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

    #[test]
    fn clone_defaults_to_worktree_layout() {
        let mut d = CloneDialog::new(&Space::default());
        assert_eq!(d.layout(), project::Layout::WorktreeRoot);
        d.worktrees = false;
        assert_eq!(d.layout(), project::Layout::NormalClone);
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
