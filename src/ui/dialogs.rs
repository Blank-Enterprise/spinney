//! Modal dialogs: open folder, clone, new worktree, confirm worktree removal.

use super::{add_project, project, repo_name_from_url, spaces, JobKind, SpinneyApp};
use eframe::egui;
use std::path::PathBuf;

pub(super) struct CloneDialog {
    pub(super) url: String,
    pub(super) dest: String,
    pub(super) name: String,
    pub(super) name_edited: bool,
}

pub(super) struct NewWorktreeDialog {
    pub(super) base: String,
    pub(super) branch: String,
}

#[derive(Default)]
pub(super) struct Dialogs {
    pub(super) space: Option<spaces::SpaceEdit>,
    pub(super) open_folder: Option<String>,
    pub(super) clone: Option<CloneDialog>,
    pub(super) new_worktree: Option<NewWorktreeDialog>,
    /// Dirty worktree awaiting a forced-removal confirmation.
    pub(super) confirm_remove: Option<PathBuf>,
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
        if let Some(mut path) = self.dialogs.open_folder.take() {
            let mut result = None;
            egui::Window::new("Open folder")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Project folder path:");
                    ui.add(egui::TextEdit::singleline(&mut path).desired_width(360.0));
                    ui.horizontal(|ui| {
                        if ui.button("Open").clicked() {
                            result = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            result = Some(false);
                        }
                    });
                });
            match result {
                Some(true) => self.open_folder(PathBuf::from(path.trim())),
                Some(false) => {}
                None => self.dialogs.open_folder = Some(path),
            }
        }
        if let Some(mut d) = self.dialogs.clone.take() {
            match clone_dialog(ctx, &mut d) {
                Some(true) => self.start_clone(d),
                Some(false) => {}
                None => self.dialogs.clone = Some(d),
            }
        }
        if let Some(mut d) = self.dialogs.new_worktree.take() {
            let mut result = None;
            egui::Window::new("New worktree")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!("New branch from {}:", d.base));
                    ui.text_edit_singleline(&mut d.branch);
                    ui.horizontal(|ui| {
                        if ui.button("Create").clicked() {
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
                    if ui.button("Remove").clicked() {
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
    }

    fn open_folder(&mut self, root: PathBuf) {
        let Some(space) = self.space() else { return };
        if let Some(i) = self.tabs.iter().position(|t| t.root == root) {
            self.active_tab = i;
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
        let (url, parent, name) = (
            d.url.trim().to_string(),
            PathBuf::from(d.dest.trim()),
            d.name.trim().to_string(),
        );
        if url.is_empty() || name.is_empty() || parent.as_os_str().is_empty() {
            self.errors
                .push("clone needs a URL, destination and name".into());
            self.dialogs.clone = Some(d);
            return;
        }
        let root = parent.join(&name);
        let id = space.id.clone();
        self.jobs.spawn(JobKind::Clone, root, id, move || {
            project::clone_into(&url, &parent, &name, &space).map(|_| ())
        });
    }

    fn new_worktree(&mut self, d: NewWorktreeDialog) {
        let ctx = self.ctx.clone();
        let mut errors = vec![];
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            match project::add_worktree(&tab.root, d.branch.trim(), &d.base) {
                Ok(path) => tab.active_worktree = path,
                Err(e) => errors.push(format!("{e:#}")),
            }
            tab.refresh(&ctx, &mut errors);
        }
        self.errors.extend(errors);
    }
}

/// Clone dialog; `Some(true)` = clone, `Some(false)` = cancel, `None` = still open.
fn clone_dialog(ctx: &egui::Context, d: &mut CloneDialog) -> Option<bool> {
    let mut result = None;
    egui::Window::new("Clone")
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("clone_grid").num_columns(2).show(ui, |ui| {
                ui.label("URL");
                if ui.text_edit_singleline(&mut d.url).changed() && !d.name_edited {
                    d.name = repo_name_from_url(d.url.trim());
                }
                ui.end_row();
                ui.label("Destination");
                ui.text_edit_singleline(&mut d.dest);
                ui.end_row();
                ui.label("Name");
                if ui.text_edit_singleline(&mut d.name).changed() {
                    d.name_edited = true;
                }
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button("Clone").clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    result = Some(false);
                }
            });
        });
    result
}
