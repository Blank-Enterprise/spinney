//! The New Tab page: Open / Clone / Create and the space's Favourites and Recent projects.

use super::{paths, theme, Action};
use crate::config::{Collapsed, ProjectList, Space};
use eframe::egui::{self, AtomExt, Color32, RichText, Stroke};
use std::path::{Path, PathBuf};

pub fn show(
    ui: &mut egui::Ui,
    space: &Space,
    collapsed: Collapsed,
    search: &mut String,
    actions: &mut Vec<Action>,
) {
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
        lists(ui, space, collapsed, search, actions);
    });
}

/// The Favourites and Recent lists, each collapsible: on the New Tab page and in the tab
/// bar's dropdown, so the two can't drift apart.
pub fn lists(
    ui: &mut egui::Ui,
    space: &Space,
    collapsed: Collapsed,
    search: &str,
    actions: &mut Vec<Action>,
) {
    let home = paths::home();
    let (favourites, recent) = sections(space, search, home.as_deref());
    if !favourites.is_empty() {
        ui.add_space(10.0);
        if list_header(
            ui,
            ProjectList::Favourites,
            favourites.len(),
            collapsed,
            actions,
        ) {
            theme::card().show(ui, |ui| {
                for root in favourites {
                    project_row(ui, root, true, home.as_deref(), actions);
                }
            });
        }
    }
    ui.add_space(10.0);
    if list_header(ui, ProjectList::Recent, recent.len(), collapsed, actions) {
        theme::card().show(ui, |ui| {
            let mut missing = 0;
            for root in &recent {
                missing += usize::from(project_row(ui, root, false, home.as_deref(), actions));
            }
            // Never automatic: an unmounted drive or share looks the same as a deleted folder.
            if missing > 0 && ui.small_button("Remove missing").clicked() {
                actions.push(Action::RemoveMissing);
            }
            if recent.is_empty() {
                ui.weak(if space.projects.is_empty() {
                    "No projects yet"
                } else if search.trim().is_empty() {
                    "Every project is a favourite"
                } else {
                    "No matches"
                });
            }
        });
    }
}

/// A list's clickable heading, ▾ open or ▸ collapsed; returns whether it's open.
fn list_header(
    ui: &mut egui::Ui,
    list: ProjectList,
    count: usize,
    collapsed: Collapsed,
    actions: &mut Vec<Action>,
) -> bool {
    let shut = collapsed.get(list);
    let header = egui::Label::new(theme::section(&header_text(list, count, shut)))
        .selectable(false)
        .sense(egui::Sense::click());
    if ui
        .add(header)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        actions.push(Action::ToggleCollapsed(list));
    }
    !shut
}

/// "▾ Favourites", or "▸ Recent · 12" when collapsed, so the count still shows.
fn header_text(list: ProjectList, count: usize, collapsed: bool) -> String {
    let name = match list {
        ProjectList::Favourites => "Favourites",
        ProjectList::Recent => "Recent",
    };
    if collapsed {
        format!("▸ {name} · {count}")
    } else {
        format!("▾ {name}")
    }
}

/// A project's row: a star to toggle Favourites, then its name and path, which open it.
/// The path shrinks with an ellipsis so the name stays on one line. A missing project is
/// greyed out and tagged, and can only be located or removed. Returns whether it's missing.
fn project_row(
    ui: &mut egui::Ui,
    root: &Path,
    favourite: bool,
    home: Option<&Path>,
    actions: &mut Vec<Action>,
) -> bool {
    let tag = project_tag(root);
    let missing = !tag.is_empty();
    ui.horizontal(|ui| {
        let (star, tip) = if favourite {
            ("★", "Remove from Favourites")
        } else {
            ("☆", "Add to Favourites")
        };
        let color = if favourite {
            theme::ACCENT_TEXT
        } else {
            theme::MUTED
        };
        let star = egui::Button::new(RichText::new(star).color(color)).frame(false);
        if ui.add(star).on_hover_text(tip).clicked() {
            actions.push(Action::ToggleFavourite(root.to_path_buf()));
        }
        let shown = paths::abbreviate_home(root, home);
        let path = RichText::new(&shown).color(theme::MUTED).atom_shrink(true);
        let mut label = egui::Atoms::new(RichText::new(folder_name(root)));
        // Only a missing row gets the tag: an empty one would still add spacing and shift
        // the row's layout against its neighbours.
        if missing {
            label = egui::Atoms::new((
                RichText::new(folder_name(root)).color(theme::MUTED),
                RichText::new(tag).weak().small(),
            ));
        }
        let row = egui::Button::selectable(false, label)
            .right_text(path)
            .truncate()
            .min_size(egui::vec2(ui.available_width(), 0.0));
        let hover = match missing {
            true => format!("{shown} is missing: right-click to locate or remove it"),
            false => shown,
        };
        let r = ui.add(row).on_hover_text(hover);
        // A missing folder can't be opened, only located or removed.
        if r.clicked() && !missing {
            actions.push(Action::OpenProject(root.to_path_buf()));
        }
        r.context_menu(|ui| {
            if missing && ui.button("Locate…").clicked() {
                actions.push(Action::LocateProject(root.to_path_buf()));
                ui.close();
            }
            if ui.button(tip).clicked() {
                actions.push(Action::ToggleFavourite(root.to_path_buf()));
                ui.close();
            }
            if ui.button("Remove from Recent").clicked() {
                actions.push(Action::RemoveRecent(root.to_path_buf()));
                ui.close();
            }
        });
    });
    missing
}

/// A project row's muted tag: "missing" when its folder is gone (deleted, moved, or on a
/// drive that isn't mounted), else empty.
fn project_tag(root: &Path) -> &'static str {
    if root.exists() {
        ""
    } else {
        "missing"
    }
}

/// The space's (Favourites, Recent) matching `search`: Favourites in the order starred,
/// and Recent without them, so a project is listed once.
fn sections<'a>(
    space: &'a Space,
    search: &str,
    home: Option<&Path>,
) -> (Vec<&'a PathBuf>, Vec<&'a PathBuf>) {
    let hit = |root: &&PathBuf| {
        matches(
            search,
            &folder_name(root),
            &paths::abbreviate_home(root, home),
        )
    };
    let favourites = space
        .pinned
        .iter()
        .filter(|p| space.projects.contains(p))
        .filter(hit)
        .collect();
    let recent = space
        .projects
        .iter()
        .filter(|p| !space.pinned.contains(p))
        .filter(hit)
        .collect();
    (favourites, recent)
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

    #[test]
    fn favourites_list_first_and_leave_recent() {
        let p = |s: &str| PathBuf::from(s);
        let space = Space {
            projects: vec![p("/p/alpha"), p("/p/beta"), p("/p/gamma")],
            pinned: vec![p("/p/gamma"), p("/p/alpha"), p("/p/gone")],
            ..Default::default()
        };
        let (fav, recent) = sections(&space, "", None);
        assert_eq!(
            fav,
            [&p("/p/gamma"), &p("/p/alpha")],
            "starred order, Recent only"
        );
        assert_eq!(recent, [&p("/p/beta")]);
        let (fav, recent) = sections(&space, "AL", None);
        assert_eq!(fav, [&p("/p/alpha")]);
        assert!(recent.is_empty());
    }

    #[test]
    fn recent_marks_a_missing_project() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(project_tag(dir.path()), "");
        assert_eq!(project_tag(Path::new("/p/gone")), "missing");
    }

    #[test]
    fn a_collapsed_header_keeps_its_count() {
        assert_eq!(
            header_text(ProjectList::Favourites, 3, false),
            "▾ Favourites"
        );
        assert_eq!(header_text(ProjectList::Recent, 12, true), "▸ Recent · 12");
    }
}
