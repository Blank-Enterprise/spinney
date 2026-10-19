//! Right panel: working-copy status, staging and commit; or a selected commit's files.
//! Also the ProjectTab handlers behind those controls.

use super::diff_view::{Selection, WorkDiff};
use super::{Action, Center, ProjectTab};
use crate::actions;
use crate::diff::{commit_diff, file_diff};
use crate::stage::{self, Direction};
use crate::status::{self, Change, FileStatus, Status};
use eframe::egui;

/// Commit needs a non-blank message and something staged.
pub fn commit_enabled(message: &str, status: &Status) -> bool {
    !message.trim().is_empty() && !status.staged.is_empty()
}

fn letter(c: &Change) -> &'static str {
    match c {
        Change::Modified => "M",
        Change::Added => "A",
        Change::Deleted => "D",
        Change::Renamed { .. } => "R",
        Change::Untracked => "U",
        Change::Conflicted => "!",
    }
}

#[derive(Clone, Copy, PartialEq)]
enum List {
    Conflicted,
    Unstaged,
    Staged,
}

pub fn show(ui: &mut egui::Ui, tab: &mut ProjectTab, actions: &mut Vec<Action>) {
    if let Some(i) = tab.selected_commit {
        commit_files(ui, tab, i, actions);
        return;
    }
    let open = match &tab.center {
        Center::Diff(w) => Some((w.fd.path.clone(), w.staged)),
        _ => None,
    };
    egui::Panel::bottom("commit_box").show(ui, |ui| {
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut tab.commit_msg)
                .hint_text("Commit message")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        let enabled = commit_enabled(&tab.commit_msg, &tab.status);
        if ui
            .add_enabled(enabled, egui::Button::new("Commit"))
            .clicked()
        {
            actions.push(Action::Commit);
        }
        ui.add_space(4.0);
    });
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let st = &tab.status;
            if !st.conflicted.is_empty() {
                file_list(ui, List::Conflicted, &st.conflicted, &open, actions);
            }
            file_list(ui, List::Unstaged, &st.unstaged, &open, actions);
            file_list(ui, List::Staged, &st.staged, &open, actions);
        });
}

fn file_list(
    ui: &mut egui::Ui,
    list: List,
    files: &[FileStatus],
    open: &Option<(String, bool)>,
    actions: &mut Vec<Action>,
) {
    let (title, button) = match list {
        List::Conflicted => ("Conflicted", "Stage"),
        List::Unstaged => ("Unstaged", "Stage"),
        List::Staged => ("Staged", "Unstage"),
    };
    let staged = list == List::Staged;
    ui.strong(format!("{title} ({})", files.len()));
    for f in files {
        ui.horizontal(|ui| {
            if ui.small_button(button).clicked() {
                actions.push(Action::StageFile(f.clone(), staged));
            }
            let is_open =
                list != List::Conflicted && open.as_ref() == Some(&(f.path.clone(), staged));
            let r = ui.selectable_label(is_open, format!("{}  {}", letter(&f.change), f.path));
            let r = match &f.change {
                Change::Renamed { from } => r.on_hover_text(format!("renamed from {from}")),
                _ => r,
            };
            // Conflicted files have a combined diff the parser does not read.
            if r.clicked() && list != List::Conflicted {
                actions.push(Action::OpenDiff(f.clone(), staged));
            }
        });
    }
    ui.add_space(8.0);
}

fn commit_files(ui: &mut egui::Ui, tab: &ProjectTab, i: usize, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let sha = tab.rows.get(i).map_or("", |r| &r.commit.sha);
        ui.strong(format!("Commit {}", sha.get(..8).unwrap_or(sha)));
        if ui.small_button("Working copy").clicked() {
            actions.push(Action::DeselectCommit);
        }
    });
    ui.separator();
    if tab.commit_files.is_empty() {
        ui.weak("No file changes.");
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (k, f) in tab.commit_files.iter().enumerate() {
                let open = matches!(&tab.center, Center::CommitFile(fd) if fd.path == f.path);
                if ui.selectable_label(open, &f.path).clicked() {
                    actions.push(Action::OpenCommitFile(k));
                }
            }
        });
}

impl ProjectTab {
    /// Reload status and the open working-copy diff. The diff closes when its file left
    /// that side's list; the selection survives only if the diff is unchanged.
    pub(super) fn reload_changes(&mut self, errors: &mut Vec<String>) {
        match status::status(&self.active_worktree) {
            Ok(s) => self.status = s,
            Err(e) => errors.push(format!("status: {e:#}")),
        }
        let Center::Diff(w) = &mut self.center else {
            return;
        };
        let side = if w.staged {
            &self.status.staged
        } else {
            &self.status.unstaged
        };
        let Some(f) = side.iter().find(|f| f.path == w.fd.path) else {
            self.center = Center::Graph;
            return;
        };
        match file_diff(&self.active_worktree, &f.path, w.staged) {
            Ok(fd) => {
                w.change = f.change.clone();
                if fd != w.fd {
                    w.fd = fd;
                    w.sel = Selection::default();
                }
            }
            Err(e) => {
                errors.push(format!("diff: {e:#}"));
                self.center = Center::Graph;
            }
        }
    }

    pub(super) fn open_diff(&mut self, f: FileStatus, staged: bool, errors: &mut Vec<String>) {
        match file_diff(&self.active_worktree, &f.path, staged) {
            Ok(fd) => {
                self.center = Center::Diff(Box::new(WorkDiff {
                    staged,
                    change: f.change,
                    fd,
                    sel: Selection::default(),
                }))
            }
            Err(e) => errors.push(format!("diff: {e:#}")),
        }
    }

    /// Stage, or with `unstage` unstage, the whole file (both paths of a rename).
    pub(super) fn stage_file(&mut self, f: &FileStatus, unstage: bool, errors: &mut Vec<String>) {
        let wt = &self.active_worktree;
        let res = if unstage {
            stage::unstage_file(wt, &f.path).and_then(|()| match &f.change {
                Change::Renamed { from } => stage::unstage_file(wt, from),
                _ => Ok(()),
            })
        } else {
            stage::stage_file(wt, &f.path)
        };
        if let Err(e) = res {
            errors.push(format!("{e:#}"));
        }
        self.after_stage(errors);
    }

    pub(super) fn apply_patch(&mut self, patch: &str, dir: Direction, errors: &mut Vec<String>) {
        if let Err(e) = stage::apply(&self.active_worktree, patch, dir) {
            errors.push(format!("{e:#}"));
        }
        self.after_stage(errors);
    }

    fn after_stage(&mut self, errors: &mut Vec<String>) {
        if let Center::Diff(w) = &mut self.center {
            w.sel = Selection::default();
        }
        self.reload_changes(errors);
    }

    /// Commit the staged changes; on success clear the message and refresh everything.
    pub(super) fn commit(&mut self, ctx: &egui::Context, errors: &mut Vec<String>) {
        match actions::commit(&self.active_worktree, &self.commit_msg) {
            Ok(()) => {
                self.commit_msg.clear();
                self.refresh(ctx, errors);
            }
            Err(e) => errors.push(format!("commit: {e:#}")),
        }
    }

    pub(super) fn select_commit(&mut self, i: usize, errors: &mut Vec<String>) {
        let Some(sha) = self.rows.get(i).map(|r| r.commit.sha.clone()) else {
            return;
        };
        self.selected_commit = Some(i);
        self.commit_files = commit_diff(&self.active_worktree, &sha).unwrap_or_else(|e| {
            errors.push(format!("commit files: {e:#}"));
            vec![]
        });
    }

    pub(super) fn open_commit_file(&mut self, k: usize) {
        if let Some(fd) = self.commit_files.get(k) {
            self.center = Center::CommitFile(Box::new(fd.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Change, FileStatus};

    #[test]
    fn commit_needs_message_and_staged_files() {
        let mut st = Status::default();
        assert!(!commit_enabled("msg", &st));
        st.staged.push(FileStatus {
            path: "f".into(),
            change: Change::Modified,
        });
        assert!(commit_enabled("msg", &st));
        assert!(!commit_enabled("", &st));
        assert!(!commit_enabled("  \n\t", &st));
    }
}
