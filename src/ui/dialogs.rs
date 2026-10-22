//! Modal dialogs: clone, create, new worktree, confirm worktree removal and branch deletion,
//! merge and rebase.

use super::{
    actions, add_project, checkout, graph_view, note_stopped, paths, project, repo_name_from_url,
    spaces, theme, toolbar::folder_name, JobKind, ProjectTab, SpinneyApp,
};
use crate::actions::{MergeMode, MergeOutcome, Operation, ResetMode, ResetPlan};
use crate::todo::{self, TodoAction, TodoItem, TodoPlan};
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
    pub(super) mode: BranchMode,
}

/// What the new-branch dialog does with the branch it creates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum BranchMode {
    /// Create a worktree for it, beside the project.
    Worktree,
    /// Create it and switch to it.
    Switch,
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

/// Confirm deleting a sidebar folder's branches, naming each one; asked again with
/// `not_merged` for the ones `-d` refuses.
pub(super) struct DeleteFolderDialog {
    pub(super) folder: String,
    /// On the remote (`origin/feat/a`, …) rather than local branches.
    pub(super) remote: bool,
    pub(super) branches: Vec<String>,
    /// Checked out in a worktree, so left alone.
    pub(super) kept: Vec<String>,
    /// `origin/<branch>` for those of `branches` that have one.
    pub(super) remotes: Vec<String>,
    /// Also delete `remotes` once their local branches are gone.
    pub(super) also_remote: bool,
    pub(super) not_merged: bool,
}

impl DeleteFolderDialog {
    /// The modal's question.
    pub(super) fn question(&self) -> String {
        let n = self.branches.len();
        let s = if n == 1 { "" } else { "es" };
        if self.remote {
            format!("Delete {n} branch{s} in {} on the remote?", self.folder)
        } else if self.not_merged {
            let are = if n == 1 { "is" } else { "are" };
            format!(
                "{n} branch{s} in {} {are} not fully merged. Delete anyway?",
                self.folder
            )
        } else {
            format!("Delete {n} branch{s} in {}?", self.folder)
        }
    }
}

/// `origin/<b>` for each local branch `b` in `names` that has one in `branches`.
pub(super) fn folder_remotes(branches: &[refs::Branch], names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|b| format!("origin/{b}"))
        .filter(|r| branches.iter().any(|b| b.remote && &b.name == r))
        .collect()
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

/// Tag commit `sha` (asked from `label`'s menu) in `worktree`, maybe pushing it to origin.
pub(super) struct TagDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) sha: String,
    pub(super) label: String,
    pub(super) name: String,
    /// Empty for a lightweight tag.
    pub(super) message: String,
    pub(super) push: bool,
    /// The repository has an `origin` to push to.
    pub(super) can_push: bool,
    /// Last name checked with git, and why it was refused (git runs once per edit).
    checked: Option<(String, Option<String>)>,
}

impl TagDialog {
    pub(super) fn new(
        root: PathBuf,
        worktree: PathBuf,
        sha: String,
        label: String,
        can_push: bool,
    ) -> Self {
        TagDialog {
            root,
            worktree,
            sha,
            label,
            name: String::new(),
            message: String::new(),
            push: false,
            can_push,
            checked: None,
        }
    }

    /// The trimmed name, or why Create is disabled: invalid, or already a tag.
    pub(super) fn target(&mut self) -> Result<String, String> {
        let name = self.name.trim().to_string();
        let refused = match &self.checked {
            Some((n, why)) if *n == name => why.clone(),
            _ => {
                let why = match actions::check_tag_name(&name) {
                    Err(e) => Some(e.to_string()),
                    Ok(()) => match actions::tag_exists(&self.worktree, &name) {
                        Ok(true) => Some(format!("A tag named {name} already exists")),
                        Ok(false) => None,
                        Err(e) => Some(format!("{e:#}")),
                    },
                };
                self.checked = Some((name.clone(), why.clone()));
                why
            }
        };
        match refused {
            Some(why) => Err(why),
            None => Ok(name),
        }
    }
}

/// The tag dialog's name for what is tagged: a branch's short name, or a short SHA.
pub(super) fn tag_target_label(target: &str) -> &str {
    ["refs/heads/", "refs/remotes/"]
        .iter()
        .find_map(|p| target.strip_prefix(p))
        .unwrap_or_else(|| base_label(target))
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
        lost.push(match &p.dirty_files[..] {
            [] => "Its uncommitted and untracked files will be lost.".to_string(),
            files => format!(
                "Its uncommitted and untracked files will be lost: {}.",
                file_list(files)
            ),
        });
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
    /// Dirty worktree awaiting a forced-removal confirmation, with the files it would lose.
    pub(super) confirm_remove: Option<(PathBuf, Vec<String>)>,
    pub(super) remove_with_branch: Option<RemoveWithBranchDialog>,
    pub(super) delete_branch: Option<DeleteBranchDialog>,
    /// Remote branch (`origin/feature/x`) awaiting a delete confirmation.
    pub(super) delete_remote: Option<String>,
    pub(super) delete_tag: Option<DeleteTagDialog>,
    pub(super) delete_folder: Option<DeleteFolderDialog>,
    pub(super) rename_branch: Option<RenameBranchDialog>,
    pub(super) confirm_drop_stash: Option<DropStash>,
    pub(super) merge: Option<MergeDialog>,
    pub(super) rebase: Option<RebaseDialog>,
    /// The menu shown after dropping one branch onto another.
    pub(super) drop_menu: Option<super::branch_drop::DropMenu>,
    pub(super) leave_detached: Option<LeaveDetached>,
    pub(super) reset: Option<ResetDialog>,
    pub(super) revert: Option<RevertDialog>,
    pub(super) interactive: Option<InteractiveDialog>,
    pub(super) tag: Option<TagDialog>,
}

/// The interactive rebase editor: `plan` as listed, `items` as the user is editing them.
pub(super) struct InteractiveDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) plan: TodoPlan,
    pub(super) items: Vec<TodoItem>,
    /// Commits it rewrites that are already on the upstream.
    pub(super) pushed: usize,
}

/// What the editor changes, e.g. "3 commits: 1 reworded, 1 squashed, 1 dropped".
pub(super) fn interactive_summary(plan: &TodoPlan, items: &[TodoItem]) -> String {
    let count = |a: TodoAction| items.iter().filter(|i| i.action == a).count();
    let moved = plan.items.iter().zip(items).any(|(p, i)| p.sha != i.sha);
    let mut parts = vec![];
    for (a, what) in [
        (TodoAction::Reword, "reworded"),
        (TodoAction::Squash, "squashed"),
        (TodoAction::Fixup, "fixed up"),
        (TodoAction::Drop, "dropped"),
    ] {
        let n = count(a);
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    if moved {
        parts.push("reordered".into());
    }
    let commits = match items.len() {
        1 => "1 commit".to_string(),
        n => format!("{n} commits"),
    };
    if parts.is_empty() {
        format!("{commits}, unchanged")
    } else {
        format!("{commits}: {}", parts.join(", "))
    }
}

/// Revert merge commit `sha` in `worktree`, keeping the side of parent `mainline`.
pub(super) struct RevertDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) sha: String,
    /// Each parent's SHA and subject, in order.
    pub(super) parents: Vec<(String, String)>,
    /// 1-based, as `git revert -m` takes it.
    pub(super) mainline: usize,
}

/// A merge parent's radio label: "Parent 1: 1a2b3c4d main edit".
pub(super) fn parent_label(n: usize, sha: &str, subject: &str) -> String {
    format!("Parent {n}: {} {subject}", &sha[..sha.len().min(8)])
}

/// Reset the branch checked out in `worktree` as `plan` describes, in `mode`.
pub(super) struct ResetDialog {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) plan: ResetPlan,
    pub(super) mode: ResetMode,
}

/// The reset confirmation: its question, what happens, and warnings naming what is lost.
pub(super) fn reset_text(plan: &ResetPlan, mode: ResetMode) -> (String, String, Vec<String>) {
    let branch = &plan.branch;
    let short = &plan.target[..plan.target.len().min(8)];
    let name = match mode {
        ResetMode::Soft => "soft",
        ResetMode::Mixed => "mixed",
        ResetMode::Hard => "hard",
    };
    let to = plan.upstream.as_deref().unwrap_or(short);
    let question = format!("Reset {branch} to {to} ({name})?");
    let moves = match (&plan.upstream, plan.dropped) {
        (Some(up), _) => format!("{branch} moves to {up} as of the last fetch."),
        (None, 0) => format!("{branch} stays at this commit."),
        (None, 1) => format!("{branch} moves back 1 commit."),
        (None, n) => format!("{branch} moves back {n} commits."),
    };
    let what = match mode {
        _ if !plan.checked_out => "It isn't checked out here, so no files change.",
        ResetMode::Soft => "Their changes stay staged, and your uncommitted changes are kept.",
        ResetMode::Mixed => {
            "Their changes, and anything staged, become unstaged changes; no file is changed."
        }
        ResetMode::Hard => {
            "Their changes and every uncommitted change to tracked files are removed from \
             the files. Untracked files are kept."
        }
    };
    let mut warnings = vec![];
    if let Some(up) = plan.upstream.as_ref().filter(|_| plan.dropped > 0) {
        let commits = match plan.dropped {
            1 => "1 local commit".to_string(),
            n => format!("{n} local commits"),
        };
        warnings.push(format!(
            "{commits} not on {up} will be dropped from {branch}; only the reflog can recover them."
        ));
    }
    if mode == ResetMode::Hard && !plan.changed.is_empty() {
        let files = match plan.changed.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        warnings.push(format!(
            "Uncommitted changes to {files} will be lost and can't be recovered: {}.",
            file_list(&plan.changed)
        ));
    }
    if plan.pushed > 0 {
        let commits = match plan.pushed {
            1 => "1 of these commits is".to_string(),
            n => format!("{n} of these commits are"),
        };
        warnings.push(format!(
            "{commits} already pushed. Pushing {branch} afterwards needs a force push."
        ));
    }
    (question, format!("{moves} {what}"), warnings)
}

/// Leaving a detached HEAD at `head` would strand `lost` commits that no ref has.
pub(super) struct LeaveDetached {
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) head: String,
    pub(super) lost: usize,
    pub(super) to: super::LeaveTo,
}

/// The leave-detached-HEAD question, naming what would be lost.
pub(super) fn leave_question(lost: usize) -> String {
    let commits = match lost {
        1 => "1 commit made on this detached HEAD isn't".to_string(),
        n => format!("{n} commits made on this detached HEAD aren't"),
    };
    format!(
        "{commits} on any branch or tag. If you leave, only git's reflog can find them, \
         and git eventually deletes them. Create a branch to keep them."
    )
}

/// The worktree Remove confirmation: what is lost for a dirty one, or for a missing one
/// (moved, not deleted, it stops being a worktree where it is now). `files` are the dirty
/// one's uncommitted and untracked files.
pub fn remove_question(path: &Path, missing: bool, files: &[String]) -> String {
    if missing {
        format!(
            "{} is missing. Remove it from the project? If it was moved, it stops being a \
             worktree where it is now; use Locate… to keep it.",
            path.display()
        )
    } else if files.is_empty() {
        format!(
            "{} has uncommitted changes. Remove it anyway?",
            path.display()
        )
    } else {
        format!(
            "{} has uncommitted changes to {}. They will be lost. Remove it anyway?",
            path.display(),
            file_list(files)
        )
    }
}

/// A tag delete awaiting confirmation, bound to the tab and worktree it was asked in.
pub(super) struct DeleteTagDialog {
    pub root: PathBuf,
    pub worktree: PathBuf,
    pub tag: refs::Tag,
    /// Also `git push origin --delete refs/tags/<name>`, as a job.
    pub also_remote: bool,
}

/// What deleting `tag` loses: the name (and an annotated tag's message), never the commit.
pub fn delete_tag_prompt(tag: &refs::Tag) -> String {
    let sha = tag.target.get(..7).unwrap_or(&tag.target);
    let what = if tag.annotated {
        "The tag and its message will be lost"
    } else {
        "The tag will be lost"
    };
    format!(
        "Delete tag {} at {sha}? {what}; the commit stays.",
        tag.name
    )
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
            || self.delete_branch.is_some()
            || self.delete_remote.is_some()
            || self.delete_tag.is_some()
            || self.delete_folder.is_some()
            || self.rename_branch.is_some()
            || self.merge.is_some()
            || self.rebase.is_some()
            || self.drop_menu.is_some()
            || self.leave_detached.is_some()
            || self.reset.is_some()
            || self.revert.is_some()
            || self.interactive.is_some()
            || self.tag.is_some()
    }
}

/// The buttons of the leave-detached-HEAD confirmation.
enum LeaveChoice {
    Branch,
    Leave,
    Cancel,
}

/// The interactive rebase editor's body: one row per commit, oldest first, with its
/// action, ↑/↓ to reorder, and a message box where one is needed.
fn interactive_editor(ui: &mut egui::Ui, d: &mut InteractiveDialog, result: &mut Option<bool>) {
    ui.label(format!(
        "Interactive rebase of {} onto {}",
        d.plan.branch,
        &d.plan.base[..8]
    ));
    ui.label(theme::caption(
        "Oldest first. Squash and fixup fold into the commit above.",
    ));
    ui.add_space(4.0);
    let mut swap = None;
    let mut set = None;
    let n = d.items.len();
    egui::ScrollArea::vertical()
        .max_height(420.0)
        .show(ui, |ui| {
            for k in 0..n {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(k > 0, egui::Button::new("↑").small())
                        .clicked()
                    {
                        swap = Some((k - 1, k));
                    }
                    if ui
                        .add_enabled(k + 1 < n, egui::Button::new("↓").small())
                        .clicked()
                    {
                        swap = Some((k, k + 1));
                    }
                    let current = d.items[k].action;
                    egui::ComboBox::from_id_salt(("todo_action", k))
                        .width(80.0)
                        .selected_text(current.label())
                        .show_ui(ui, |ui| {
                            for a in TodoAction::ALL {
                                if ui.selectable_label(a == current, a.label()).clicked() {
                                    set = Some((k, a));
                                }
                            }
                        });
                    let item = &d.items[k];
                    ui.label(theme::caption(&item.sha[..8]));
                    let subject = egui::RichText::new(&item.subject);
                    ui.label(if item.action == TodoAction::Drop {
                        subject.strikethrough().color(theme::MUTED)
                    } else {
                        subject
                    });
                });
                if todo::edits_message(&d.items, k) {
                    ui.add(
                        egui::TextEdit::multiline(&mut d.items[k].message)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                }
            }
        });
    if let Some((a, b)) = swap {
        d.items.swap(a, b);
    }
    if let Some((k, a)) = set {
        todo::set_action(&mut d.items, k, a);
    }
    ui.add_space(4.0);
    ui.label(theme::caption(&interactive_summary(&d.plan, &d.items)));
    let warning = rebase_warning(d.pushed, &d.plan.branch);
    if let Some(w) = &warning {
        ui.colored_label(theme::WARN, format!("⚠ {w}"));
    }
    let problem = todo::problem(&d.items);
    if let Some(p) = &problem {
        ui.colored_label(theme::DELETED, p);
    }
    ui.horizontal(|ui| {
        let start = if warning.is_some() {
            danger("Start")
        } else {
            theme::primary("Start")
        };
        if ui.add_enabled(problem.is_none(), start).clicked() {
            *result = Some(true);
        }
        if ui.button("Cancel").clicked() {
            *result = Some(false);
        }
    });
}

/// Red, bold button for a destructive confirmation.
fn danger(label: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).color(theme::ON_ACCENT).strong())
        .fill(theme::DELETED)
}

/// Files a message names (conflicts, or changes a confirmation loses); the rest are counted.
const FILES_SHOWN: usize = 5;

/// "a.txt, b.txt", or the first few "and 2 more".
pub(super) fn file_list(files: &[String]) -> String {
    let mut list = files[..files.len().min(FILES_SHOWN)].join(", ");
    if files.len() > FILES_SHOWN {
        list += &format!(" and {} more", files.len() - FILES_SHOWN);
    }
    list
}

/// "Merge stopped on conflicts in a.txt, b.txt. Resolve them, then Continue."
pub(super) fn conflict_message(op: &str, files: &[String]) -> String {
    let list = file_list(files);
    format!("{op} stopped on conflicts in {list}. Resolve them, then Continue.")
}

/// "Stash applied with conflicts in a.txt, b.txt", the files the apply or pop conflicted.
pub(super) fn stash_conflict_notice(files: &[String]) -> String {
    match files {
        [] => "Stash applied with conflicts".to_string(),
        _ => format!("Stash applied with conflicts in {}", file_list(files)),
    }
}

/// "Squash stopped: conflicts in a.txt. Nothing was changed."
fn squash_conflict_message(files: &[String]) -> String {
    let list = file_list(files);
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
                BranchMode::Switch => "New branch",
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
        if let Some((path, files)) = self.dialogs.confirm_remove.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("confirm_remove")).show(ctx, |ui| {
                ui.label(remove_question(&path, !path.is_dir(), &files));
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
                None => self.dialogs.confirm_remove = Some((path, files)),
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
        if let Some(mut d) = self.dialogs.delete_folder.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("delete_folder")).show(ctx, |ui| {
                ui.label(d.question());
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for b in &d.branches {
                            ui.label(egui::RichText::new(b).monospace());
                        }
                    });
                if !d.kept.is_empty() {
                    let kept = d.kept.join(", ");
                    let text = format!("Kept, checked out in a worktree: {kept}");
                    ui.label(egui::RichText::new(text).color(theme::MUTED));
                }
                if !d.remotes.is_empty() {
                    let n = d.remotes.len();
                    let s = if n == 1 { "" } else { "es" };
                    let label = format!("Also delete {n} matching branch{s} on origin");
                    ui.checkbox(&mut d.also_remote, label)
                        .on_hover_text(d.remotes.join("\n"));
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
                Some(true) => self.delete_folder(d),
                Some(false) => {}
                None => self.dialogs.delete_folder = Some(d),
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
        if let Some(mut d) = self.dialogs.delete_tag.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("delete_tag")).show(ctx, |ui| {
                ui.label(delete_tag_prompt(&d.tag));
                ui.checkbox(&mut d.also_remote, "Also delete on origin");
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
                Some(true) => self.delete_tag(d),
                Some(false) => {}
                None => self.dialogs.delete_tag = Some(d),
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
        if let Some(mut d) = self.dialogs.interactive.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("interactive"))
                .show(ctx, |ui| interactive_editor(ui, &mut d, &mut result));
            match result {
                Some(true) => self.interactive_rebase(d),
                Some(false) => {}
                None => self.dialogs.interactive = Some(d),
            }
        }
        if let Some(mut d) = self.dialogs.tag.take() {
            let mut result = None;
            let target = d.target();
            egui::Modal::new(egui::Id::unique("create_tag")).show(ctx, |ui| {
                ui.label(format!("Create tag at {}", d.label));
                ui.text_edit_singleline(&mut d.name);
                ui.label(theme::caption(
                    "Message (optional): with one it's an annotated tag, without one a \
                     lightweight tag.",
                ));
                ui.add(egui::TextEdit::multiline(&mut d.message).desired_rows(3));
                ui.add_enabled(
                    d.can_push,
                    egui::Checkbox::new(&mut d.push, "Push to origin"),
                )
                .on_disabled_hover_text("This repository has no origin remote");
                if let Err(reason) = &target {
                    if !d.name.trim().is_empty() {
                        ui.colored_label(ui.visuals().error_fg_color, reason);
                    }
                }
                ui.horizontal(|ui| {
                    let create = theme::primary("Create");
                    if ui.add_enabled(target.is_ok(), create).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match (result, target) {
                (Some(true), Ok(name)) => self.create_tag(d, name),
                (Some(false), _) => {}
                _ => self.dialogs.tag = Some(d),
            }
        }
        if let Some(mut d) = self.dialogs.revert.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("revert")).show(ctx, |ui| {
                ui.label(format!("Revert merge {}", &d.sha[..d.sha.len().min(8)]));
                ui.label(theme::caption(
                    "Keep the side of one parent; the changes merged in from the other are \
                     undone in a new commit.",
                ));
                ui.add_space(4.0);
                for (k, (sha, subject)) in d.parents.iter().enumerate() {
                    ui.radio_value(&mut d.mainline, k + 1, parent_label(k + 1, sha, subject));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::primary("Revert")).clicked() {
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
                    let (root, wt) = (d.root.clone(), d.worktree.clone());
                    self.revert(&root, &wt, &d.sha, Some(d.mainline), &mut errors);
                    self.errors.extend(errors);
                }
                Some(false) => {}
                None => self.dialogs.revert = Some(d),
            }
        }
        if let Some(d) = self.dialogs.reset.take() {
            let mut result = None;
            let (question, what, warnings) = reset_text(&d.plan, d.mode);
            egui::Modal::new(egui::Id::unique("reset")).show(ctx, |ui| {
                ui.label(question);
                ui.label(theme::caption(&what));
                for w in &warnings {
                    ui.colored_label(theme::WARN, format!("⚠ {w}"));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let reset = if d.mode == ResetMode::Hard || !warnings.is_empty() {
                        danger("Reset")
                    } else {
                        theme::primary("Reset")
                    };
                    if ui.add(reset).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => self.reset(d),
                Some(false) => {}
                None => self.dialogs.reset = Some(d),
            }
        }
        if let Some(d) = self.dialogs.leave_detached.take() {
            let mut result = None;
            egui::Modal::new(egui::Id::unique("leave_detached")).show(ctx, |ui| {
                ui.label(super::detached_text(&d.head));
                ui.label(leave_question(d.lost));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::primary("Create branch…")).clicked() {
                        result = Some(LeaveChoice::Branch);
                    }
                    if ui.add(danger("Leave anyway")).clicked() {
                        result = Some(LeaveChoice::Leave);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(LeaveChoice::Cancel);
                    }
                });
            });
            match result {
                Some(LeaveChoice::Leave) => self.leave_detached(d),
                Some(LeaveChoice::Branch) => {
                    self.dialogs.new_worktree = Some(NewWorktreeDialog {
                        base: d.head,
                        branch: String::new(),
                        mode: BranchMode::Switch,
                    })
                }
                Some(LeaveChoice::Cancel) => {}
                None => self.dialogs.leave_detached = Some(d),
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

    /// Open the interactive rebase editor for the commits after `base`.
    pub(super) fn ask_interactive(&mut self, base: &str, errors: &mut Vec<String>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if self.jobs.network_busy(&tab.root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let wt = tab.active_worktree.clone();
        if let Err(e) = actions::ensure_clean(&wt) {
            return errors.push(format!("{e:#}"));
        }
        let plan = match todo::plan(&wt, base) {
            Ok(p) => p,
            Err(e) => return errors.push(format!("{e:#}")),
        };
        match actions::rebase_rewrites_pushed(&wt, &plan.base) {
            Ok(pushed) => {
                self.dialogs.interactive = Some(InteractiveDialog {
                    root: tab.root.clone(),
                    worktree: wt,
                    items: plan.items.clone(),
                    plan,
                    pushed,
                })
            }
            Err(e) => errors.push(format!("upstream: {e:#}")),
        }
    }

    /// Run the edited list synchronously, like rebase; conflicts go to the banner.
    pub(super) fn interactive_rebase(&mut self, d: InteractiveDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        if self.jobs.network_busy(&d.root) {
            return self
                .errors
                .push("Wait for the running fetch, pull or push".into());
        }
        let tab = self
            .tabs
            .iter_mut()
            .find(|t| t.root == d.root && t.active_worktree == d.worktree);
        let Some(tab) = tab else {
            return self
                .errors
                .push("The project or worktree changed; try again".into());
        };
        let res = todo::start(&d.worktree, &d.plan, &d.items);
        tab.refresh(&ctx, &mut errors);
        match res {
            Ok(MergeOutcome::Conflicts(files)) => {
                let text = conflict_message("Rebase", &files);
                note_stopped(&mut self.stopped, &d.worktree, Operation::Rebase, text)
            }
            Ok(_) => {}
            Err(e) => {
                errors.push(format!("{e:#}"));
            }
        }
        self.errors.extend(errors);
    }

    /// Open the tag dialog for `target` (a SHA or a branch's full ref), pinned to the
    /// commit it names now.
    pub(super) fn ask_tag(&mut self, target: String, errors: &mut Vec<String>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        let sha = match actions::resolve_commit(&wt, &target) {
            Ok(sha) => sha,
            Err(e) => return errors.push(format!("{e:#}")),
        };
        let can_push = actions::has_remote(&wt, "origin").unwrap_or(false);
        let label = tag_target_label(&target).to_string();
        self.dialogs.tag = Some(TagDialog::new(root, wt, sha, label, can_push));
    }

    /// Create the tag now; pushing it runs as a job, like other pushes.
    pub(super) fn create_tag(&mut self, d: TagDialog, name: String) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let Some(tab) = self.tabs.iter_mut().find(|t| t.root == d.root) else {
            return;
        };
        let res = actions::create_tag(&d.worktree, &name, &d.sha, Some(&d.message));
        tab.refresh(&ctx, &mut errors);
        match res {
            Ok(()) if d.push && d.can_push => {
                let wt = d.worktree;
                self.jobs
                    .spawn(JobKind::PushTag, d.root, String::new(), move || {
                        actions::push_tag(&wt, "origin", &name)
                    });
            }
            Ok(()) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.errors.extend(errors);
    }

    /// Revert `sha` at once, or for a merge commit ask which parent to keep first.
    pub(super) fn ask_revert(&mut self, sha: String, errors: &mut Vec<String>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        let parents = match actions::parents(&wt, &sha) {
            Ok(p) => p,
            Err(e) => return errors.push(format!("{e:#}")),
        };
        if parents.len() < 2 {
            return self.revert(&root, &wt, &sha, None, errors);
        }
        let parents = parents
            .into_iter()
            .map(|p| {
                let subject = actions::commit_message(&wt, &p)
                    .map(|m| m.lines().next().unwrap_or_default().to_string())
                    .unwrap_or_default();
                (p, subject)
            })
            .collect();
        self.dialogs.revert = Some(RevertDialog {
            root,
            worktree: wt,
            sha,
            parents,
            mainline: 1,
        });
    }

    /// Revert synchronously, like merge; conflicts go to the in-progress banner.
    fn revert(
        &mut self,
        root: &Path,
        worktree: &Path,
        sha: &str,
        mainline: Option<usize>,
        errors: &mut Vec<String>,
    ) {
        let ctx = self.ctx.clone();
        // A pull or push may be moving the same branch (the toolbar disables it too).
        if self.jobs.network_busy(root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let tab = self.tabs.iter_mut().find(|t| t.root == root);
        let Some(tab) = tab.filter(|t| t.active_worktree == worktree) else {
            return errors.push("The project or worktree changed; try again".into());
        };
        let res = actions::revert(worktree, sha, mainline);
        tab.refresh(&ctx, errors);
        match res {
            Ok(MergeOutcome::Conflicts(files)) => {
                let text = conflict_message("Revert", &files);
                note_stopped(&mut self.stopped, worktree, Operation::Revert, text)
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
    }

    /// Cherry-pick synchronously, like merge; conflicts go to the in-progress banner.
    pub(super) fn cherry_pick(&mut self, shas: &[String], errors: &mut Vec<String>) {
        let ctx = self.ctx.clone();
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        // A pull or push may be moving the same branch (the toolbar disables it too).
        if self.jobs.network_busy(&tab.root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        let res = actions::cherry_pick(&tab.active_worktree, shas);
        tab.refresh(&ctx, errors);
        match res {
            Ok(MergeOutcome::Conflicts(files)) => {
                let (wt, text) = (
                    &tab.active_worktree,
                    conflict_message("Cherry-pick", &files),
                );
                note_stopped(&mut self.stopped, wt, Operation::CherryPick, text)
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
    }

    /// Open the reset confirmation, refusing now what would fail anyway.
    pub(super) fn ask_reset(&mut self, sha: &str, mode: ResetMode, errors: &mut Vec<String>) {
        self.ask_reset_with(|wt| actions::reset_plan(wt, sha), mode, errors);
    }

    /// Open the hard reset confirmation for moving local `branch` to its upstream.
    pub(super) fn ask_reset_to_upstream(&mut self, branch: &str, errors: &mut Vec<String>) {
        let plan = |wt: &Path| actions::reset_to_upstream_plan(wt, branch);
        self.ask_reset_with(plan, ResetMode::Hard, errors);
    }

    fn ask_reset_with(
        &mut self,
        plan: impl FnOnce(&Path) -> anyhow::Result<ResetPlan>,
        mode: ResetMode,
        errors: &mut Vec<String>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        // A pull or push may be moving the same branch (the toolbar disables it too).
        if self.jobs.network_busy(&tab.root) {
            return errors.push("Wait for the running fetch, pull or push".into());
        }
        match plan(&tab.active_worktree) {
            Ok(plan) => {
                self.dialogs.reset = Some(ResetDialog {
                    root: tab.root.clone(),
                    worktree: tab.active_worktree.clone(),
                    plan,
                    mode,
                })
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
    }

    /// Reset as confirmed; `actions::reset` refuses if anything shown has changed.
    pub(super) fn reset(&mut self, d: ResetDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        if self.jobs.network_busy(&d.root) {
            return self
                .errors
                .push("Wait for the running fetch, pull or push".into());
        }
        let tab = self
            .tabs
            .iter_mut()
            .find(|t| t.root == d.root && t.active_worktree == d.worktree);
        let Some(tab) = tab else {
            return self
                .errors
                .push("The project or worktree changed; try again".into());
        };
        if let Err(e) = actions::reset(&d.worktree, &d.plan, d.mode) {
            errors.push(format!("{e:#}"));
        }
        tab.refresh(&ctx, &mut errors);
        self.errors.extend(errors);
    }

    /// Leave the detached HEAD as confirmed, unless HEAD moved since it was asked.
    pub(super) fn leave_detached(&mut self, d: LeaveDetached) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let tab = self
            .tabs
            .iter_mut()
            .find(|t| t.root == d.root && t.active_worktree == d.worktree);
        let Some(tab) = tab else {
            return self
                .errors
                .push("The project or worktree changed; try again".into());
        };
        match actions::detached_head(&d.worktree) {
            Ok(Some(h)) if h == d.head => match &d.to {
                super::LeaveTo::Branch(name) => checkout(tab, name, &mut errors),
                super::LeaveTo::Commit(sha) => {
                    if let Err(e) = actions::checkout_detached(&d.worktree, sha) {
                        errors.push(format!("{e:#}"));
                    }
                }
            },
            Ok(_) => errors.push("HEAD moved since you were asked; try again".into()),
            Err(e) => errors.push(format!("{e:#}")),
        }
        tab.refresh(&ctx, &mut errors);
        self.errors.extend(errors);
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
        let (mut notice, mut stopped) = (None, None);
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
            Ok(MergeOutcome::Conflicts(files)) => stopped = Some(conflict_message("Merge", &files)),
            Ok(MergeOutcome::SquashConflicts(files)) => {
                errors.push(squash_conflict_message(&files))
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.notices.extend(notice);
        if let Some(text) = stopped {
            note_stopped(&mut self.stopped, &d.worktree, Operation::Merge, text);
        }
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
                let text = conflict_message("Rebase", &files);
                note_stopped(&mut self.stopped, &d.worktree, Operation::Rebase, text)
            }
            // Only a squash merge undoes itself.
            Ok(MergeOutcome::SquashConflicts(_)) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.errors.extend(errors);
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

    /// Open the folder dialog, leaving out (and naming) branches a worktree has checked out.
    pub(super) fn ask_delete_folder(
        &mut self,
        folder: String,
        branches: Vec<String>,
        remote: bool,
        errors: &mut Vec<String>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (branches, kept) = if remote {
            (branches, vec![])
        } else {
            match project::list_worktrees(&tab.active_worktree) {
                Ok(wts) => branches
                    .into_iter()
                    .partition(|b| !wts.iter().any(|w| w.branch.as_ref() == Some(b))),
                Err(e) => return errors.push(format!("{e:#}")),
            }
        };
        if branches.is_empty() {
            return errors.push(format!(
                "Every branch in {folder} is checked out in a worktree"
            ));
        }
        let remotes = if remote {
            vec![]
        } else {
            folder_remotes(&tab.branches, &branches)
        };
        self.dialogs.delete_folder = Some(DeleteFolderDialog {
            folder,
            remote,
            branches,
            kept,
            remotes,
            also_remote: false,
            not_merged: false,
        });
    }

    /// Remote: one push job. Local: `-d` each now (`-D` once confirmed unmerged), the
    /// unmerged ones asked about again, then their remote branches as a job.
    fn delete_folder(&mut self, mut d: DeleteFolderDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let (root, wt) = (tab.root.clone(), tab.active_worktree.clone());
        let mut remote = vec![];
        if d.remote {
            remote = d.branches;
        } else {
            let out = actions::delete_branches(&wt, &d.branches, d.not_merged);
            tab.refresh(&ctx, &mut errors);
            errors.extend(out.errors);
            let names = |list: &[String]| -> Vec<String> {
                list.iter().map(|b| format!("origin/{b}")).collect()
            };
            if d.also_remote {
                let gone = names(&out.deleted);
                remote = d
                    .remotes
                    .iter()
                    .filter(|r| gone.contains(r))
                    .cloned()
                    .collect();
            }
            if !out.not_merged.is_empty() {
                let left = names(&out.not_merged);
                d.remotes.retain(|r| left.contains(r));
                d.branches = out.not_merged;
                d.kept.clear();
                d.not_merged = true;
                self.dialogs.delete_folder = Some(d);
            }
        }
        if !remote.is_empty() {
            self.jobs
                .spawn(JobKind::DeleteRemote, root, String::new(), move || {
                    actions::delete_remote_branches(&wt, &remote)
                });
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

    /// `git tag -d` now (local and fast); deleting it on origin runs as a job, like a push.
    pub(super) fn delete_tag(&mut self, d: DeleteTagDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        let Some(tab) = self.tabs.iter_mut().find(|t| t.root == d.root) else {
            return;
        };
        // Same rule as the remote branch delete: one network job per project at a time.
        if d.also_remote && self.jobs.network_busy(&d.root) {
            return self
                .errors
                .push("Wait for the running fetch, pull or push".into());
        }
        let res = actions::delete_tag(&d.worktree, &d.tag.name);
        tab.refresh(&ctx, &mut errors);
        match res {
            Ok(()) if d.also_remote => {
                let (wt, name) = (d.worktree, d.tag.name);
                self.jobs
                    .spawn(JobKind::DeleteRemote, d.root, String::new(), move || {
                        actions::delete_remote_tag(&wt, "origin", &name)
                    });
            }
            Ok(()) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
        self.errors.extend(errors);
    }

    /// Open `picked` as a tab (or focus its tab) and add it to the active space. A worktree
    /// folder (Spinney's, an agent's, an IDE's) opens its project with that worktree active.
    pub(super) fn open_folder(&mut self, picked: PathBuf) {
        let Some(space) = self.space() else { return };
        // Keep the path as picked when it is the project itself (git returns `/private/…`).
        let root = match project::project_root(&picked) {
            Ok(r) if !project::same_dir(&r, &picked) => r,
            _ => picked.clone(),
        };
        let i = match self.tabs.iter().position(|t| t.root == root) {
            Some(i) => i,
            None => {
                let before = self.tabs.len();
                self.open_tab(&root, &space);
                if self.tabs.len() == before {
                    return;
                }
                if add_project(&mut self.config, &space.id, &root) {
                    self.save_config();
                }
                self.tabs.len() - 1
            }
        };
        self.active_tab = i;
        self.new_tab = false;
        self.remember_active();
        let picked = picked.canonicalize().unwrap_or(picked);
        let ctx = self.ctx.clone();
        let tab = &mut self.tabs[i];
        if picked != tab.active_worktree && tab.worktrees.iter().any(|w| w.path == picked) {
            tab.active_worktree = picked;
            tab.refresh(&ctx, &mut self.errors);
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
            let wt = tab.active_worktree.clone();
            let res = match d.mode {
                BranchMode::Worktree => {
                    project::add_worktree(&tab.root, branch, &d.base).map(|path| {
                        tab.active_worktree = path;
                    })
                }
                BranchMode::Switch => actions::create_and_switch(&wt, branch, &d.base),
            };
            if let Err(e) = res {
                errors.push(format!("{e:#}"));
            }
            tab.refresh(&ctx, &mut errors);
        }
        self.errors.extend(errors);
    }
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
    ui.weak("worktrees go beside it, in a .worktrees folder");
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
        d.confirm_remove = Some((PathBuf::from("/tmp/wt"), vec![]));
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
        d.delete_tag = Some(DeleteTagDialog {
            root: PathBuf::from("/tmp/r"),
            worktree: PathBuf::from("/tmp/r"),
            tag: refs::Tag {
                name: "v1".into(),
                target: "abc".into(),
                annotated: false,
            },
            also_remote: false,
        });
        assert!(d.any_open());
        d.delete_tag = None;
        d.delete_folder = Some(DeleteFolderDialog {
            folder: "feat".into(),
            remote: true,
            branches: vec!["origin/feat/a".into()],
            kept: vec![],
            remotes: vec![],
            also_remote: false,
            not_merged: false,
        });
        assert!(d.any_open());
        d.delete_folder = None;
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

    fn reset_plan(dropped: usize, pushed: usize, changed: &[&str]) -> ResetPlan {
        ResetPlan {
            branch: "main".into(),
            head: "h".repeat(40),
            target: "83174b7246c76c9b6426a95147a715f0d26bf6c6".into(),
            dropped,
            pushed,
            changed: changed.iter().map(|s| s.to_string()).collect(),
            upstream: None,
            checked_out: true,
        }
    }

    #[test]
    fn reset_to_upstream_text_names_the_dropped_local_commits() {
        let up = |dropped, changed: &[&str], checked_out| ResetPlan {
            upstream: Some("origin/main".into()),
            checked_out,
            ..reset_plan(dropped, 0, changed)
        };
        let (q, what, warn) = reset_text(&up(2, &["a.txt"], true), ResetMode::Hard);
        assert_eq!(q, "Reset main to origin/main (hard)?");
        assert!(
            what.starts_with("main moves to origin/main as of the last fetch. Their changes"),
            "{what}"
        );
        assert_eq!(
            warn,
            [
                "2 local commits not on origin/main will be dropped from main; only the reflog \
                 can recover them.",
                "Uncommitted changes to 1 file will be lost and can't be recovered: a.txt."
            ]
        );
        let (_, _, warn) = reset_text(&up(0, &[], true), ResetMode::Hard);
        assert!(warn.is_empty(), "only behind: nothing lost");
        let (_, what, warn) = reset_text(&up(1, &[], false), ResetMode::Hard);
        assert!(what.ends_with("It isn't checked out here, so no files change."));
        assert!(warn[0].starts_with("1 local commit not on origin/main"));
    }

    #[test]
    fn reset_text_names_what_is_lost() {
        let (q, what, warn) = reset_text(&reset_plan(2, 0, &["a.txt"]), ResetMode::Soft);
        assert_eq!(q, "Reset main to 83174b72 (soft)?");
        assert!(what.starts_with("main moves back 2 commits. Their changes stay staged"));
        assert!(warn.is_empty(), "soft keeps the changes: {warn:?}");
        let (_, _, warn) = reset_text(&reset_plan(1, 0, &["a.txt", "b.txt"]), ResetMode::Hard);
        assert_eq!(
            warn,
            ["Uncommitted changes to 2 files will be lost and can't be recovered: a.txt, b.txt."]
        );
        let (_, what, warn) = reset_text(&reset_plan(0, 0, &[]), ResetMode::Hard);
        assert!(what.starts_with("main stays at this commit."), "{what}");
        assert!(warn.is_empty());
        let (_, _, warn) = reset_text(&reset_plan(3, 1, &[]), ResetMode::Mixed);
        assert_eq!(
            warn,
            ["1 of these commits is already pushed. Pushing main afterwards needs a force push."]
        );
        let (_, _, warn) = reset_text(&reset_plan(3, 2, &[]), ResetMode::Mixed);
        assert!(warn[0].starts_with("2 of these commits are already pushed"));
    }

    #[test]
    fn hard_reset_from_the_graph_asks_then_resets() {
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
        g(&["switch", "-q", "-c", "side"]);
        g(&["commit", "-q", "--allow-empty", "-m", "side"]);
        let side = g(&["rev-parse", "HEAD"]);
        g(&["switch", "-q", "main"]);
        g(&["commit", "-q", "--allow-empty", "-m", "second"]);
        std::fs::write(d.join("a.txt"), "edited\n").unwrap();
        let space = app.space().unwrap();
        app.open_tab(d, &space);
        for _ in 0..500 {
            app.poll();
            if app.tabs[0].rows.len() == 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let tab = &app.tabs[0];
        let on: Vec<_> = (0..3)
            .map(|i| (tab.rows[i].commit.subject.as_str(), tab.on_head[i]))
            .collect();
        assert!(
            on.contains(&("side", false)) && on.contains(&("init", true)),
            "{on:?}"
        );

        // Off the branch's history: refused before asking.
        app.apply(Action::AskReset(side, ResetMode::Soft));
        assert!(app
            .errors
            .iter()
            .any(|e| e.contains("isn't on main's history")));
        assert!(app.dialogs.reset.is_none());
        app.errors.clear();

        app.apply(Action::AskReset(init.clone(), ResetMode::Hard));
        let r = app.dialogs.reset.take().expect("asked");
        assert_eq!(
            (r.plan.dropped, &r.plan.changed[..]),
            (1, &["a.txt".to_string()][..])
        );
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "edited\n"
        );
        app.reset(r);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["rev-parse", "HEAD"]), init);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "a\n");
    }

    #[test]
    fn cherry_pick_from_the_graph_applies_or_stops_in_progress() {
        use super::super::Action;
        use crate::actions::Operation;
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
        g(&["switch", "-q", "-c", "topic"]);
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "b"]);
        let b = g(&["rev-parse", "HEAD"]);
        std::fs::write(d.join("a.txt"), "topic\n").unwrap();
        g(&["commit", "-qam", "topic a"]);
        let topic_a = g(&["rev-parse", "HEAD"]);
        g(&["switch", "-q", "main"]);
        std::fs::write(d.join("a.txt"), "main\n").unwrap();
        g(&["commit", "-qam", "main a"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        app.apply(Action::CherryPick(vec![b]));
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["log", "-1", "--format=%s"]), "b");

        app.apply(Action::CherryPick(vec![topic_a]));
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].operation, Some(Operation::CherryPick));
        assert_eq!(app.tabs[0].status.conflicted.len(), 1);
        assert!(app
            .stopped
            .iter()
            .any(|n| n.text.starts_with("Cherry-pick stopped")));
    }

    #[test]
    fn reverting_a_merge_asks_for_the_mainline_first() {
        use super::super::Action;
        use crate::actions::Operation;
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
        g(&["switch", "-q", "-c", "topic"]);
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "add b"]);
        g(&["switch", "-q", "main"]);
        g(&["merge", "-q", "--no-ff", "--no-edit", "topic"]);
        let m = g(&["rev-parse", "HEAD"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        app.apply(Action::Revert(m.clone()));
        let mut r = app.dialogs.revert.take().expect("asks for the mainline");
        assert_eq!(r.parents.len(), 2);
        assert_eq!(r.parents[0], (init.clone(), "init".to_string()));
        assert_eq!(r.parents[1].1, "add b");
        assert_eq!(g(&["rev-parse", "HEAD"]), m, "nothing yet");
        r.mainline = 1;
        let (root, wt) = (r.root.clone(), r.worktree.clone());
        let mut errors = vec![];
        app.revert(&root, &wt, &r.sha, Some(r.mainline), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!d.join("b.txt").exists());

        // A normal commit reverts at once; a conflicting one stops under the banner.
        std::fs::write(d.join("a.txt"), "later\n").unwrap();
        g(&["commit", "-qam", "later"]);
        app.apply(Action::Revert(init));
        assert!(app.dialogs.revert.is_none());
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].operation, Some(Operation::Revert));
        assert!(app
            .stopped
            .iter()
            .any(|n| n.text.starts_with("Revert stopped")));
    }

    #[test]
    fn interactive_rebase_from_the_graph_edits_then_runs() {
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
        g(&["commit", "-q", "--allow-empty", "-m", "base"]);
        let base = g(&["rev-parse", "HEAD"]);
        for name in ["one", "two", "three"] {
            g(&["commit", "-q", "--allow-empty", "-m", name]);
        }
        let space = app.space().unwrap();
        app.open_tab(d, &space);

        app.apply(Action::AskInteractiveRebase(base));
        let mut r = app.dialogs.interactive.take().expect("editor open");
        assert_eq!(r.items.len(), 3);
        assert_eq!(
            interactive_summary(&r.plan, &r.items),
            "3 commits, unchanged"
        );
        r.items.swap(0, 2);
        todo::set_action(&mut r.items, 1, TodoAction::Drop);
        assert_eq!(
            interactive_summary(&r.plan, &r.items),
            "3 commits: 1 dropped, reordered"
        );
        app.interactive_rebase(r);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["log", "--format=%s"]), "one\nthree\nbase");
    }

    #[test]
    fn parent_labels_shorten_the_sha() {
        let sha = "83174b7246c76c9b6426a95147a715f0d26bf6c6";
        assert_eq!(parent_label(2, sha, "add b"), "Parent 2: 83174b72 add b");
    }

    #[test]
    fn leave_question_names_the_commits() {
        let one = leave_question(1);
        assert!(
            one.starts_with("1 commit made on this detached HEAD isn't"),
            "{one}"
        );
        assert!(leave_question(3).starts_with("3 commits made on this detached HEAD aren't"));
    }

    #[test]
    fn leaving_detached_head_with_unsaved_commits_asks_first() {
        use super::super::{Action, LeaveTo};
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
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let init = g(&["rev-parse", "HEAD"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);
        assert_eq!(app.tabs[0].detached, None);

        // Nothing stranded: no question.
        app.apply(Action::CheckoutCommit(init.clone()));
        assert!(app.dialogs.leave_detached.is_none(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].detached.as_deref(), Some(init.as_str()));
        assert_eq!(app.tabs[0].previous_branch.as_deref(), Some("main"));

        g(&["commit", "-q", "--allow-empty", "-m", "loose"]);
        let loose = g(&["rev-parse", "HEAD"]);
        app.apply(Action::Checkout("main".into()));
        let ask = app.dialogs.leave_detached.take().expect("asked");
        assert_eq!((ask.lost, ask.head.as_str()), (1, loose.as_str()));
        assert_eq!(ask.to, LeaveTo::Branch("main".into()));
        assert_eq!(g(&["rev-parse", "HEAD"]), loose, "still there");

        app.apply(Action::CheckoutCommit(init.clone()));
        assert!(app.dialogs.leave_detached.take().is_some(), "a commit too");

        // HEAD moved meanwhile: refused.
        app.apply(Action::Checkout("main".into()));
        let stale = app.dialogs.leave_detached.take().unwrap();
        g(&["commit", "-q", "--allow-empty", "-m", "more"]);
        app.leave_detached(stale);
        assert!(app.errors.iter().any(|e| e.contains("HEAD moved")));
        assert!(crate::refs::current_branch(d).unwrap().is_none());
        app.errors.clear();

        app.apply(Action::Checkout("main".into()));
        let ask = app.dialogs.leave_detached.take().unwrap();
        assert_eq!(ask.lost, 2);
        app.leave_detached(ask);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(app.tabs[0].detached, None);
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
        assert!(app.stopped.iter().any(|n| n.text.contains("Resolve them")));
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
        assert!(app.stopped.is_empty(), "aborting drops its notice");
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
        assert_eq!(app.stopped.len(), 1, "the second merge's notice only");
        app.apply(Action::ContinueOp);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(app.tabs[0].operation, None);
        assert!(app.stopped.is_empty(), "continuing drops its notice");
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
            dirty_files: vec![],
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
        let mut p = plan(true, 0);
        p.dirty_files = (0..7).map(|i| format!("f{i}")).collect();
        let (_, lost, _) = remove_with_branch_text(&p);
        assert_eq!(
            lost,
            ["Its uncommitted and untracked files will be lost: f0, f1, f2, f3, f4 and 2 more."]
        );
    }

    #[test]
    fn remove_question_names_the_files_lost() {
        let wt = Path::new("/p/proj.worktrees/feat-x");
        let files = ["scratch.txt".to_string(), "a.txt".to_string()];
        assert_eq!(
            remove_question(wt, false, &files),
            "/p/proj.worktrees/feat-x has uncommitted changes to scratch.txt, a.txt. They \
             will be lost. Remove it anyway?"
        );
        assert_eq!(
            remove_question(wt, false, &[]),
            "/p/proj.worktrees/feat-x has uncommitted changes. Remove it anyway?"
        );
        assert!(remove_question(wt, true, &files).contains("is missing"));
    }

    #[test]
    fn stash_conflict_notice_names_the_files() {
        assert_eq!(
            stash_conflict_notice(&["a.txt".into()]),
            "Stash applied with conflicts in a.txt"
        );
        let many: Vec<String> = (0..6).map(|i| format!("f{i}")).collect();
        assert_eq!(
            stash_conflict_notice(&many),
            "Stash applied with conflicts in f0, f1, f2, f3, f4 and 1 more"
        );
        assert_eq!(stash_conflict_notice(&[]), "Stash applied with conflicts");
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
    fn delete_tag_prompt_names_what_is_lost() {
        let mut t = refs::Tag {
            name: "v1.2".into(),
            target: "83174b7246c76c9b6426a95147a715f0d26bf6c6".into(),
            annotated: false,
        };
        assert_eq!(
            delete_tag_prompt(&t),
            "Delete tag v1.2 at 83174b7? The tag will be lost; the commit stays."
        );
        t.annotated = true;
        assert!(delete_tag_prompt(&t).contains("The tag and its message will be lost"));
    }

    /// Apply finished jobs, as `poll` does, until one of `kind` has (a commit-graph job may
    /// finish first).
    fn finish_job(app: &mut SpinneyApp, kind: JobKind) {
        for _ in 0..1000 {
            if let Some(done) = app.jobs.try_recv() {
                let k = done.kind;
                app.job_done(done);
                if k == kind {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{kind:?} did not finish");
    }

    #[test]
    fn folder_delete_keeps_checked_out_and_asks_again_for_unmerged() {
        use super::super::Action;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let remote = tmp.path().join("remote.git");
        let d = tmp.path().join("repo");
        std::fs::create_dir(&d).unwrap();
        crate::git::run(tmp.path(), &["init", "-q", "--bare", "remote.git"]).unwrap();
        let g = |args: &[&str]| crate::git::run(&d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        g(&[
            "remote",
            "add",
            "origin",
            &format!("file://{}", remote.display()),
        ]);
        g(&["branch", "feat/a"]);
        g(&["push", "-q", "origin", "feat/a"]);
        g(&["branch", "feat/b"]);
        g(&["branch", "feat/c"]);
        // feat/b gets a commit main lacks; feat/c is out in another worktree.
        g(&["switch", "-q", "feat/b"]);
        g(&["commit", "-q", "--allow-empty", "-m", "wip"]);
        g(&["switch", "-q", "main"]);
        let wt = tmp.path().join("wt");
        g(&["worktree", "add", "-q", wt.to_str().unwrap(), "feat/c"]);
        let space = app.space().unwrap();
        app.open_tab(&d, &space);
        let has = |r: &str| crate::git::run(&d, &["rev-parse", "--verify", "-q", r]).is_ok();

        let branches = ["feat/a", "feat/b", "feat/c"].map(String::from).to_vec();
        app.apply(Action::AskDeleteFolder {
            folder: "feat".into(),
            branches,
            remote: false,
        });
        let mut del = app.dialogs.delete_folder.take().expect("asks first");
        assert_eq!(del.branches, ["feat/a", "feat/b"]);
        assert_eq!(del.kept, ["feat/c"]);
        assert_eq!(del.remotes, ["origin/feat/a"]);
        assert_eq!(del.question(), "Delete 2 branches in feat?");
        del.also_remote = true;
        app.delete_folder(del);
        assert!(!has("refs/heads/feat/a"));
        assert!(has("refs/heads/feat/b") && has("refs/heads/feat/c"));
        finish_job(&mut app, JobKind::DeleteRemote);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        let on_remote = crate::git::run(&remote, &["rev-parse", "--verify", "-q", "feat/a"]);
        assert!(on_remote.is_err());

        let del = app
            .dialogs
            .delete_folder
            .take()
            .expect("asks about the unmerged one");
        assert!(del.not_merged);
        assert_eq!(del.branches, ["feat/b"]);
        assert!(del.kept.is_empty() && del.remotes.is_empty());
        assert_eq!(
            del.question(),
            "1 branch in feat is not fully merged. Delete anyway?"
        );
        app.delete_folder(del);
        assert!(!has("refs/heads/feat/b"));
        assert!(has("refs/heads/feat/c"));
        assert!(app.dialogs.delete_folder.is_none());
        assert!(app.errors.is_empty(), "{:?}", app.errors);
    }

    #[test]
    fn tags_push_and_delete_on_origin_as_jobs() {
        use super::super::Action;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let remote = tmp.path().join("remote.git");
        let d = tmp.path().join("repo");
        std::fs::create_dir(&d).unwrap();
        crate::git::run(tmp.path(), &["init", "-q", "--bare", "remote.git"]).unwrap();
        let g = |args: &[&str]| crate::git::run(&d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        g(&[
            "remote",
            "add",
            "origin",
            &format!("file://{}", remote.display()),
        ]);
        g(&["tag", "v1"]);
        let on_remote =
            || crate::git::run(&remote, &["rev-parse", "--verify", "-q", "refs/tags/v1"]).is_ok();
        let space = app.space().unwrap();
        app.open_tab(&d, &space);
        assert_eq!(app.tabs[0].tags.len(), 1);

        app.apply(Action::PushTag("v1".into()));
        assert!(app.jobs.running(JobKind::PushTag, &d));
        finish_job(&mut app, JobKind::PushTag);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert!(on_remote());

        let tag = app.tabs[0].tags[0].clone();
        app.apply(Action::AskDeleteTag(tag));
        let mut del = app.dialogs.delete_tag.take().expect("asks first");
        del.also_remote = true;
        app.delete_tag(del);
        assert!(app.tabs[0].tags.is_empty(), "deleted locally at once");
        finish_job(&mut app, JobKind::DeleteRemote);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert!(!on_remote());
    }

    #[test]
    fn tag_target_labels() {
        assert_eq!(tag_target_label("refs/heads/feat/x"), "feat/x");
        assert_eq!(tag_target_label("refs/remotes/origin/main"), "origin/main");
        let sha = "83174b7246c76c9b6426a95147a715f0d26bf6c6";
        assert_eq!(tag_target_label(sha), "83174b72");
    }

    #[test]
    fn tag_dialog_creates_and_pushes_the_tag() {
        use super::super::Action;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::test_app(
            super::super::tests::cfg(),
            &tmp.path().join("config.toml"),
        );
        let proj = tempfile::tempdir().unwrap();
        let remote = proj.path().join("remote.git");
        let d = proj.path().join("repo");
        std::fs::create_dir(&d).unwrap();
        let g = |args: &[&str]| crate::git::run(&d, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        g(&["tag", "taken"]);
        let r = proj.path();
        crate::git::run(r, &["init", "-q", "--bare", "remote.git"]).unwrap();
        let url = format!("file://{}", remote.display());
        g(&["remote", "add", "origin", &url]);
        let space = app.space().unwrap();
        app.open_tab(&d, &space);

        app.apply(Action::TagAt("refs/heads/main".into()));
        let mut t = app.dialogs.tag.take().expect("dialog open");
        assert_eq!(t.label, "main");
        assert!(t.can_push);
        assert_eq!(t.target().unwrap_err(), "Enter a tag name");
        t.name = "a..b".into();
        assert_eq!(t.target().unwrap_err(), "a..b is not a valid tag name");
        t.name = "taken".into();
        assert_eq!(t.target().unwrap_err(), "A tag named taken already exists");
        t.name = " v1.0 ".into();
        t.message = "First release".into();
        t.push = true;
        let name = t.target().unwrap();
        app.create_tag(t, name);
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        let kind = g(&["cat-file", "-t", "refs/tags/v1.0"]);
        assert_eq!(kind.trim(), "tag");
        // Opening the tab also starts a commit-graph job.
        let mut done = None;
        for _ in 0..500 {
            done = app.jobs.try_recv().filter(|d| d.kind == JobKind::PushTag);
            if done.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let done = done.expect("push job finished");
        assert!(done.result.is_ok(), "{:?}", done.result);
        let pushed = crate::git::run(&remote, &["cat-file", "-t", "refs/tags/v1.0"]).unwrap();
        assert_eq!(pushed.trim(), "tag");
        let main =
            crate::git::run_ok_codes(&remote, &["rev-parse", "-q", "--verify", "main"], &[0, 1]);
        assert_eq!(main.unwrap(), "", "only the tag is pushed");
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
