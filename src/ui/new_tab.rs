//! The New Tab page: Open / Clone / Create and the space's recent projects.

use super::{paths, theme, Action};
use crate::config::Space;
use eframe::egui::{self, Color32, RichText, Stroke};
use std::path::Path;

pub fn show(ui: &mut egui::Ui, space: &Space, search: &mut String, actions: &mut Vec<Action>) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.set_max_width(640.0);
        ui.add_space(20.0);
        ui.heading(RichText::new("Repositories").color(Color32::WHITE));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            for (label, hint, action) in [
                ("Open", "Open a project folder", Action::OpenFolder),
                ("Clone", "Clone a repository", Action::CloneUrl),
                ("Create", "Create a new repository", Action::CreateRepo),
            ] {
                let card = egui::Button::new((
                    RichText::new(label)
                        .strong()
                        .size(14.0)
                        .color(Color32::WHITE),
                    RichText::new(hint).size(11.5).color(theme::MUTED),
                ))
                .fill(theme::PANEL)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(theme::CARD_R)
                .min_size(egui::vec2(0.0, 44.0));
                if ui.add(card).clicked() {
                    actions.push(action);
                }
            }
        });
        ui.add_space(14.0);
        ui.add(
            egui::TextEdit::singleline(search)
                .hint_text("Search")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(10.0);
        ui.label(theme::section("Recent"));
        theme::card().show(ui, |ui| {
            let home = paths::home();
            let mut any = false;
            for root in &space.projects {
                let name = folder_name(root);
                let shown = paths::abbreviate_home(root, home.as_deref());
                if !matches(search, &name, &shown) {
                    continue;
                }
                any = true;
                let row = egui::Button::selectable(false, RichText::new(name))
                    .right_text(RichText::new(shown).color(theme::MUTED))
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                let r = ui.add(row);
                if r.clicked() {
                    actions.push(Action::OpenProject(root.clone()));
                }
                r.context_menu(|ui| {
                    if ui.button("Remove from Recent").clicked() {
                        actions.push(Action::RemoveRecent(root.clone()));
                        ui.close();
                    }
                });
            }
            if !any {
                ui.weak(if space.projects.is_empty() {
                    "No projects yet"
                } else {
                    "No matches"
                });
            }
        });
    });
}

fn folder_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Case-insensitive substring match of `query` on the name or the shown path.
fn matches(query: &str, name: &str, path: &str) -> bool {
    let q = query.trim().to_lowercase();
    name.to_lowercase().contains(&q) || path.to_lowercase().contains(&q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_name_or_path_ignoring_case() {
        assert!(matches("", "proj", "~/code/proj"));
        assert!(matches("  ", "proj", "~/code/proj"));
        assert!(matches("PRO", "proj", "~/code/proj"));
        assert!(matches("code", "proj", "~/Code/proj"));
        assert!(!matches("other", "proj", "~/code/proj"));
    }
}
