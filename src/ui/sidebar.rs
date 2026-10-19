use super::{theme, toolbar::folder_name, Action, ProjectTab};
use crate::project::Layout;
use eframe::egui::{self, RichText};

/// Case-insensitive substring match of the sidebar filter on a branch or worktree name.
pub fn filter_matches(query: &str, name: &str) -> bool {
    name.to_lowercase().contains(&query.trim().to_lowercase())
}

/// Full-width sidebar row; `right` is muted, right-aligned text.
fn row(ui: &mut egui::Ui, on: bool, text: String, right: String) -> egui::Response {
    let b = egui::Button::selectable(on, text)
        .right_text(RichText::new(right).color(theme::MUTED))
        .min_size(egui::vec2(ui.available_width(), 0.0));
    ui.add(b)
}

fn section(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    n: usize,
    open: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    egui::CollapsingHeader::new(theme::section(&format!("{title} · {n}")))
        .id_salt(id)
        .default_open(open)
        .show(ui, add);
}

pub fn show(ui: &mut egui::Ui, tab: &mut ProjectTab, actions: &mut Vec<Action>) {
    ui.add(
        egui::TextEdit::singleline(&mut tab.filter)
            .hint_text("Filter")
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
    let tab = &*tab;
    let q = tab.filter.as_str();
    egui::ScrollArea::vertical().show(ui, |ui| {
        let local: Vec<_> = tab
            .branches
            .iter()
            .filter(|b| !b.remote && filter_matches(q, &b.name))
            .collect();
        section(ui, "local", "Local", local.len(), true, |ui| {
            for b in local {
                let mut track = String::new();
                if b.ahead > 0 {
                    track += &format!("↑{}", b.ahead);
                }
                if b.behind > 0 {
                    track += &format!(" ↓{}", b.behind);
                }
                let text = if b.head {
                    format!("✓ {}", b.name)
                } else {
                    b.name.clone()
                };
                let r = row(ui, b.head, text, track.trim().to_string());
                branch_menu(&r, tab.layout, &b.name, actions);
            }
        });
        let remote: Vec<_> = tab
            .branches
            .iter()
            .filter(|b| b.remote && filter_matches(q, &b.name))
            .collect();
        section(ui, "remote", "Remote", remote.len(), false, |ui| {
            for b in remote {
                let r = row(ui, false, b.name.clone(), String::new());
                branch_menu(&r, tab.layout, &b.name, actions);
            }
        });
        let worktrees: Vec<_> = tab
            .worktrees
            .iter()
            .filter(|w| {
                filter_matches(q, &folder_name(&w.path))
                    || w.branch.as_deref().is_some_and(|b| filter_matches(q, b))
            })
            .collect();
        section(ui, "worktrees", "Worktrees", worktrees.len(), true, |ui| {
            for wt in worktrees {
                let name = folder_name(&wt.path);
                let branch = match &wt.branch {
                    Some(b) if *b != name => b.clone(),
                    Some(_) => String::new(),
                    None => "detached".into(),
                };
                let on = wt.path == tab.active_worktree;
                let r = row(ui, on, format!("▣ {name}"), branch)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_is_case_insensitive_substring() {
        assert!(filter_matches("", "main"));
        assert!(filter_matches("  ", "main"));
        assert!(filter_matches("FEAT", "feat/music-sync"));
        assert!(filter_matches("sync ", "feat/Music-Sync"));
        assert!(!filter_matches("fix", "main"));
    }
}
