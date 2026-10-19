//! Right panel: working-copy status, staging and commit; or a selected commit's files.
//! Also the ProjectTab handlers behind those controls.

use super::diff_view::{Selection, WorkDiff};
use super::{theme, Action, Center, ProjectTab};
use crate::actions;
use crate::diff::{commit_diff, file_diff};
use crate::stage::{self, Direction};
use crate::status::{self, Change, FileStatus, Status};
use eframe::egui::{self, Align, Color32, Key, KeyboardShortcut, Modifiers, RichText};

/// Commit needs a non-blank summary and something staged.
pub fn commit_enabled(summary: &str, status: &Status) -> bool {
    !summary.trim().is_empty() && !status.staged.is_empty()
}

/// The message git gets: the summary, then a blank line and the description if there is one.
pub fn commit_message(summary: &str, description: &str) -> String {
    let mut m = summary.trim().to_string();
    if !description.trim().is_empty() {
        m += "\n\n";
        m += description.trim_end();
    }
    m
}

/// Every path of `files`, both sides of a rename (for "Stage all" / "Unstage all").
pub fn change_paths(staged: &[FileStatus]) -> Vec<&str> {
    let mut paths = vec![];
    for f in staged {
        paths.push(f.path.as_str());
        if let Change::Renamed { from } = &f.change {
            paths.push(from.as_str());
        }
    }
    paths
}

/// Status badge letter and colour.
fn badge(c: &Change) -> (&'static str, Color32) {
    match c {
        Change::Modified => ("M", theme::WARN),
        Change::Added => ("A", theme::ADDED),
        Change::Deleted => ("D", theme::DELETED),
        Change::Renamed { .. } => ("R", theme::RENAMED),
        Change::Untracked => ("?", theme::UNTRACKED),
        Change::Conflicted => ("!", theme::DELETED),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum List {
    Conflicted,
    Unstaged,
    Staged,
}

const COMMIT_KEY: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);

/// `shortcuts` is false while a dialog is open, so ⌘↵ there never commits behind it.
pub fn show(ui: &mut egui::Ui, tab: &mut ProjectTab, shortcuts: bool, actions: &mut Vec<Action>) {
    if let Some(i) = tab.selected_commit {
        commit_files(ui, tab, i, actions);
        return;
    }
    let open = match &tab.center {
        Center::Diff(w) => Some((w.fd.path.clone(), w.staged)),
        _ => None,
    };
    let enabled = commit_enabled(&tab.commit_summary, &tab.status);
    // Before the text fields see the key, so ⌘↵ never inserts a newline.
    if shortcuts && enabled && ui.input_mut(|i| i.consume_shortcut(&COMMIT_KEY)) {
        actions.push(Action::Commit);
    }
    let n = tab.status.changed_files();
    let branch = super::toolbar::branch_label(tab);
    let noun = if n == 1 { "change" } else { "changes" };
    ui.label(RichText::new(format!("{n} {noun} on {branch}")).strong());
    ui.add_space(4.0);
    egui::Panel::bottom("commit_box")
        .frame(egui::Frame::new().inner_margin(egui::Margin {
            top: 8,
            ..Default::default()
        }))
        .show_separator_line(false)
        .show(ui, |ui| commit_card(ui, tab, enabled, actions));
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let st = &tab.status;
            if !st.conflicted.is_empty() {
                file_list(ui, List::Conflicted, &st.conflicted, &open, false, actions);
            }
            let conflicts = !st.conflicted.is_empty();
            file_list(ui, List::Unstaged, &st.unstaged, &open, conflicts, actions);
            file_list(ui, List::Staged, &st.staged, &open, false, actions);
        });
}

fn commit_card(ui: &mut egui::Ui, tab: &mut ProjectTab, enabled: bool, actions: &mut Vec<Action>) {
    theme::card().show(ui, |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut tab.commit_summary)
                .hint_text("Commit summary")
                .frame(egui::Frame::new())
                .desired_width(f32::INFINITY),
        );
        ui.separator();
        ui.add(
            egui::TextEdit::multiline(&mut tab.commit_desc)
                .hint_text("Description")
                .frame(egui::Frame::new())
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        let k = tab.status.staged.len();
        let label = format!("Commit {k} {}", if k == 1 { "file" } else { "files" });
        let button = theme::primary(label).min_size(egui::vec2(ui.available_width(), 28.0));
        let r = ui.add_enabled(enabled, button).on_hover_text("⌘↵");
        if r.clicked() {
            actions.push(Action::Commit);
        }
    });
}

fn file_list(
    ui: &mut egui::Ui,
    list: List,
    files: &[FileStatus],
    open: &Option<(String, bool)>,
    conflicts: bool,
    actions: &mut Vec<Action>,
) {
    let (title, button) = match list {
        List::Conflicted => ("Conflicted", "Stage"),
        List::Unstaged => ("Unstaged", "Stage"),
        List::Staged => ("Staged", "Unstage"),
    };
    let staged = list == List::Staged;
    ui.horizontal(|ui| {
        ui.label(theme::section(&format!("{title} · {}", files.len())));
        let all = match list {
            List::Unstaged => Some(("Stage all", Action::StageAll)),
            List::Staged => Some(("Unstage all", Action::UnstageAll)),
            List::Conflicted => None,
        };
        if let Some((label, action)) = all {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let b = egui::Button::new(RichText::new(label).size(11.5)).small();
                let r = ui
                    .add_enabled(!files.is_empty() && !conflicts, b)
                    .on_disabled_hover_text(if conflicts {
                        "Resolve conflicts first"
                    } else {
                        "Nothing to do"
                    });
                if r.clicked() {
                    actions.push(action);
                }
            });
        }
    });
    for f in files {
        // `horizontal` bounds the row height; a bare centred right-to-left layout fills it all.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let b = egui::Button::new(RichText::new(button).size(11.5)).small();
                if ui.add(b).clicked() {
                    actions.push(Action::StageFile(f.clone(), staged));
                }
                let is_open =
                    list != List::Conflicted && open.as_ref() == Some(&(f.path.clone(), staged));
                let (letter, color) = badge(&f.change);
                let row = egui::Button::selectable(
                    is_open,
                    (
                        RichText::new(format!(" {letter} "))
                            .size(10.5)
                            .strong()
                            .color(color)
                            .background_color(color.gamma_multiply(0.2)),
                        RichText::new(&f.path).size(12.5),
                    ),
                )
                .right_text("") // grows, keeping the content left-aligned
                .truncate()
                .min_size(egui::vec2(ui.available_width(), 0.0));
                let r = ui.add(row);
                let r = match &f.change {
                    Change::Renamed { from } => r.on_hover_text(format!("renamed from {from}")),
                    _ => r,
                };
                // Conflicted files have a combined diff the parser does not read.
                if r.clicked() && list != List::Conflicted {
                    actions.push(Action::OpenDiff(f.clone(), staged));
                }
            })
        });
    }
    ui.add_space(8.0);
}

fn commit_files(ui: &mut egui::Ui, tab: &ProjectTab, i: usize, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let sha = tab.rows.get(i).map_or("", |r| &r.commit.sha);
        ui.label(RichText::new(format!("Commit {}", sha.get(..8).unwrap_or(sha))).strong());
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("Working copy").clicked() {
                actions.push(Action::DeselectCommit);
            }
        });
    });
    if let Some(r) = tab.rows.get(i) {
        ui.label(RichText::new(&r.commit.subject).color(theme::TEXT));
    }
    ui.add_space(6.0);
    ui.label(theme::section(&format!(
        "Files · {}",
        tab.commit_files.len()
    )));
    if tab.commit_files.is_empty() {
        ui.weak("No file changes.");
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (k, f) in tab.commit_files.iter().enumerate() {
                let open = matches!(&tab.center, Center::CommitFile(fd) if fd.path == f.path);
                let row = egui::Button::selectable(open, RichText::new(&f.path).size(12.5))
                    .right_text("")
                    .truncate()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if ui.add(row).clicked() {
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

    /// Stage every listed unstaged file, or with `unstage` unstage every staged one, in one git call.
    pub(super) fn stage_all(&mut self, unstage: bool, errors: &mut Vec<String>) {
        let wt = &self.active_worktree;
        let res = if unstage {
            stage::unstage_files(wt, &change_paths(&self.status.staged))
        } else if self.status.conflicted.is_empty() {
            stage::stage_paths(wt, &change_paths(&self.status.unstaged))
        } else {
            Ok(()) // the button is disabled; never mark conflicts resolved by accident
        };
        if let Err(e) = res {
            errors.push(format!("{e:#}"));
        }
        self.after_stage(errors);
    }

    /// Commit the staged changes; on success clear the message and refresh everything.
    pub(super) fn commit(&mut self, ctx: &egui::Context, errors: &mut Vec<String>) {
        let message = commit_message(&self.commit_summary, &self.commit_desc);
        match actions::commit(&self.active_worktree, &message) {
            Ok(()) => {
                self.commit_summary.clear();
                self.commit_desc.clear();
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

    #[test]
    fn message_is_summary_plus_optional_description() {
        assert_eq!(commit_message(" fix: x ", ""), "fix: x");
        assert_eq!(commit_message("fix: x", " \n "), "fix: x");
        assert_eq!(
            commit_message("fix: x", "why\nmore\n"),
            "fix: x\n\nwhy\nmore"
        );
    }

    #[test]
    fn unstage_all_covers_both_paths_of_a_rename() {
        let staged = [
            FileStatus {
                path: "a".into(),
                change: Change::Added,
            },
            FileStatus {
                path: "new".into(),
                change: Change::Renamed { from: "old".into() },
            },
        ];
        assert_eq!(change_paths(&staged), ["a", "new", "old"]);
    }
}
