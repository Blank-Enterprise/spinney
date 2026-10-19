//! GitKraken-style New Tab page: Open / Clone / Create and the space's recent projects.

use super::{paths, Action};
use crate::config::Space;
use eframe::egui;
use std::path::Path;

pub fn show(ui: &mut egui::Ui, space: &Space, search: &mut String, actions: &mut Vec<Action>) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(16.0);
        ui.heading("Repositories");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for (label, hint, action) in [
                ("Open", "Open a project folder", Action::OpenFolder),
                ("Clone", "Clone a repository", Action::CloneUrl),
                ("Create", "Create a new repository", Action::CreateRepo),
            ] {
                if ui.button(label).on_hover_text(hint).clicked() {
                    actions.push(action);
                }
            }
        });
        ui.add_space(8.0);
        ui.add(
            egui::TextEdit::singleline(search)
                .hint_text("Search")
                .desired_width(360.0),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Recent").strong());
        let home = paths::home();
        let mut any = false;
        for root in &space.projects {
            let name = folder_name(root);
            let shown = paths::abbreviate_home(root, home.as_deref());
            if !matches(search, &name, &shown) {
                continue;
            }
            any = true;
            let row = egui::Button::selectable(
                false,
                (egui::RichText::new(name), egui::RichText::new(shown).weak()),
            );
            if ui.add(row).clicked() {
                actions.push(Action::OpenProject(root.clone()));
            }
        }
        if !any {
            ui.weak(if space.projects.is_empty() {
                "No projects yet"
            } else {
                "No matches"
            });
        }
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
