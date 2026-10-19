//! Modal dialogs: open folder, clone, new worktree, confirm worktree removal.

use super::{add_project, paths, project, repo_name_from_url, spaces, JobKind, SpinneyApp};
use eframe::egui;
use std::path::{Path, PathBuf};

pub(super) struct CloneDialog {
    /// "Where to clone to", as typed or picked.
    where_dir: String,
    url: String,
    name: String,
    /// Once the user types a name, the URL stops overwriting it.
    name_edited: bool,
    /// The space's default clone folder: prefill and Browse fallback.
    default_dir: Option<PathBuf>,
    /// Label for the space's GitHub source.
    github: String,
}

impl CloneDialog {
    pub(super) fn new(space: &crate::config::Space) -> Self {
        let default_dir = space.default_clone_dir.clone();
        CloneDialog {
            where_dir: default_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            url: String::new(),
            name: String::new(),
            name_edited: false,
            default_dir,
            github: github_label(&space.host),
        }
    }

    /// Canonical parent folder and trimmed name, or why the clone cannot start yet.
    fn target(&self, home: Option<&Path>) -> Result<(PathBuf, String), String> {
        let parent = paths::existing_dir(&self.where_dir, home)?;
        if self.url.trim().is_empty() {
            return Err("Enter a URL".into());
        }
        paths::check_name(&self.name)?;
        let name = self.name.trim().to_string();
        let root = parent.join(&name);
        if root.exists() {
            return Err(format!("{} already exists", root.display()));
        }
        Ok((parent, name))
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

/// GitKraken-style "Clone a Repository" modal; `Some(true)` = clone, `Some(false)` = cancel,
/// `None` = still open.
fn clone_dialog(ctx: &egui::Context, d: &mut CloneDialog) -> Option<bool> {
    let (mut result, mut browse) = (None, false);
    let target = d.target(paths::home().as_deref());
    let modal = egui::Modal::new(egui::Id::unique("clone_dialog")).show(ctx, |ui| {
        ui.heading("Clone a Repository");
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(170.0);
                let _ = ui.selectable_label(true, "Clone with URL");
                ui.add_enabled(false, egui::Button::selectable(false, d.github.as_str()))
                    .on_disabled_hover_text("Repository browser comes in a later version");
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.label("Where to clone to");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.where_dir).desired_width(320.0));
                    browse = ui.button("Browse").clicked();
                });
                ui.label("URL");
                let url = ui.add(egui::TextEdit::singleline(&mut d.url).desired_width(390.0));
                if url.changed() && !d.name_edited {
                    d.name = repo_name_from_url(d.url.trim());
                }
                ui.label("Name");
                let name = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(390.0));
                if name.changed() {
                    d.name_edited = true;
                }
                ui.add_space(6.0);
                ui.label("Full path");
                let home = paths::home();
                let full = paths::expand_tilde(&d.where_dir, home.as_deref()).join(d.name.trim());
                ui.monospace(format!("{}/", full.display()));
                ui.weak("worktrees go in folders inside this one");
                ui.add_space(6.0);
                if let Err(reason) = &target {
                    ui.colored_label(ui.visuals().error_fg_color, reason);
                }
                ui.horizontal(|ui| {
                    let clone = egui::Button::new("Clone the repo!");
                    if ui.add_enabled(target.is_ok(), clone).clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
        });
    });
    if result.is_none() && modal.should_close() {
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
}
