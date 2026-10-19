use super::{Action, ProjectTab};
use crate::project::Layout;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, tab: &ProjectTab, actions: &mut Vec<Action>) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::CollapsingHeader::new("Local")
            .default_open(true)
            .show(ui, |ui| {
                for b in tab.branches.iter().filter(|b| !b.remote) {
                    let mut text = b.name.clone();
                    if b.ahead > 0 {
                        text += &format!("  ↑{}", b.ahead);
                    }
                    if b.behind > 0 {
                        text += &format!("  ↓{}", b.behind);
                    }
                    let r = ui.selectable_label(b.head, text);
                    branch_menu(&r, tab.layout, &b.name, actions);
                }
            });
        egui::CollapsingHeader::new("Remote")
            .default_open(false)
            .show(ui, |ui| {
                for b in tab.branches.iter().filter(|b| b.remote) {
                    let r = ui.selectable_label(false, &b.name);
                    branch_menu(&r, tab.layout, &b.name, actions);
                }
            });
        egui::CollapsingHeader::new("Worktrees")
            .default_open(true)
            .show(ui, |ui| {
                for wt in &tab.worktrees {
                    let name = wt.path.file_name().map_or_else(
                        || wt.path.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    );
                    let label = match &wt.branch {
                        Some(b) if *b != name => format!("{name} ({b})"),
                        Some(_) => name,
                        None => format!("{name} (detached)"),
                    };
                    let r = ui
                        .selectable_label(wt.path == tab.active_worktree, label)
                        .on_hover_text(wt.path.display().to_string());
                    if r.clicked() {
                        actions.push(Action::ActivateWorktree(wt.path.clone()));
                    }
                    r.context_menu(|ui| {
                        if ui.button("Remove").clicked() {
                            actions.push(Action::RemoveWorktree(wt.path.clone()));
                            ui.close();
                        }
                    });
                }
            });
    });
}

fn branch_menu(r: &egui::Response, layout: Layout, name: &str, actions: &mut Vec<Action>) {
    r.context_menu(|ui| {
        if ui.button("Checkout").clicked() {
            actions.push(Action::Checkout(name.to_string()));
            ui.close();
        }
        // Worktrees live beside `.bare`; in a normal clone they would nest in the working tree.
        let r = ui
            .add_enabled(
                layout == Layout::WorktreeRoot,
                egui::Button::new("New worktree from here"),
            )
            .on_disabled_hover_text(
                "Converting normal clones to the worktree layout comes in a later version",
            );
        if r.clicked() {
            actions.push(Action::NewWorktreeFrom(name.to_string()));
            ui.close();
        }
    });
}
