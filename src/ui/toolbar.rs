//! Top of the window: the project tab strip and the repository toolbar.

use super::{active_space, spaces, theme, Action, JobKind, ProjectTab, SpinneyApp};
use crate::config::PullStrategy;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Vec2,
};
use std::path::Path;

/// Inactive tab text.
const TAB_TEXT: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa7);

impl SpinneyApp {
    pub(super) fn tab_strip(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let mut rects = Vec::with_capacity(self.tabs.len());
            let mut dragged = None;
            for (i, tab) in self.tabs.iter().enumerate() {
                let active = i == self.active_tab && !self.new_tab;
                let (label, close, rect) = tab_button(ui, &tab.name(), active, true);
                rects.push(rect);
                if label.clicked() {
                    actions.push(Action::SelectTab(i));
                }
                if close {
                    actions.push(Action::CloseTab(i));
                }
                if label.dragged() || label.drag_stopped() {
                    dragged = Some((i, label));
                }
            }
            if let Some((from, label)) = dragged {
                drag_tab(ui, from, &label, &rects, actions);
            }
            if self.new_tab {
                let closable = !self.tabs.is_empty(); // with no tabs the page shows anyway
                if tab_button(ui, "New Tab", true, closable).1 {
                    actions.push(Action::CloseNewTab);
                }
            }
            let has_space = active_space(&self.config).is_some();
            let plus =
                egui::Button::new(RichText::new("+").size(16.0).color(TAB_TEXT)).frame(false);
            if ui
                .add_enabled(has_space, plus)
                .on_hover_text("New tab")
                .clicked()
            {
                actions.push(Action::NewTab);
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                spaces::switcher(ui, &self.config, actions);
                if self.jobs.any(JobKind::Clone) {
                    ui.add(egui::Spinner::new().size(14.0))
                        .on_hover_text("Cloning…");
                }
            });
        });
    }
}

/// While tab `from` is dragged, mark where it would land; on release, move it there.
fn drag_tab(
    ui: &egui::Ui,
    from: usize,
    label: &egui::Response,
    rects: &[egui::Rect],
    actions: &mut Vec<Action>,
) {
    let Some(x) = label.interact_pointer_pos().map(|p| p.x) else {
        return;
    };
    let centres: Vec<f32> = rects.iter().map(|r| r.center().x).collect();
    let slot = drop_slot(x, &centres);
    let to = moved_index(from, slot);
    if label.drag_stopped() {
        if to != from {
            actions.push(Action::MoveTab { from, to });
        }
        return;
    }
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    if to != from {
        let at = match slot {
            0 => rects[0].left(),
            n => rects[n - 1].right() + 1.0,
        };
        let y = rects[from].y_range();
        ui.painter()
            .vline(at, y, egui::Stroke::new(2.0, theme::ACCENT));
    }
}

/// The gap a tab dropped at `x` goes into: 0 before the first tab, `centres.len()` after the last.
fn drop_slot(x: f32, centres: &[f32]) -> usize {
    centres.iter().filter(|&&c| c < x).count()
}

/// The index a tab moved from `from` into gap `slot` ends up at, once it's out of the list.
fn moved_index(from: usize, slot: usize) -> usize {
    if slot > from {
        slot - 1
    } else {
        slot
    }
}

/// A rounded-top tab, draggable by its name; returns (the name label's response, close
/// clicked, the tab's rect).
fn tab_button(
    ui: &mut egui::Ui,
    name: &str,
    active: bool,
    closable: bool,
) -> (egui::Response, bool, egui::Rect) {
    let (fill, color) = if active {
        (theme::BG, Color32::WHITE)
    } else {
        (Color32::TRANSPARENT, TAB_TEXT)
    };
    let mut close = false;
    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius {
            nw: 7,
            ne: 7,
            sw: 0,
            se: 0,
        })
        .inner_margin(Margin {
            left: 14,
            right: if closable { 8 } else { 14 },
            top: 6,
            bottom: 6,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let label = egui::Label::new(RichText::new(name).size(12.5).color(color))
                    .selectable(false)
                    .sense(Sense::click_and_drag());
                let label = ui.add(label);
                if closable {
                    let x = egui::Button::new(RichText::new("×").color(TAB_TEXT))
                        .frame(false)
                        .small();
                    close = ui.add(x).on_hover_text("Close tab").clicked();
                }
                label
            })
            .inner
        });
    (frame.inner, close, frame.response.rect)
}

/// Current branch of the tab's active worktree, for the toolbar.
pub(super) fn branch_label(tab: &ProjectTab) -> String {
    tab.branches
        .iter()
        .find(|b| b.head && !b.remote)
        .map(|b| b.name.clone())
        .or_else(|| {
            tab.worktrees
                .iter()
                .find(|w| w.path == tab.active_worktree)
                .and_then(|w| w.branch.clone())
        })
        .unwrap_or_else(|| "detached".into())
}

pub(super) fn folder_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

pub(super) fn toolbar(
    ui: &mut egui::Ui,
    tab: &ProjectTab,
    projects: &[std::path::PathBuf],
    busy: bool,
    running: impl Fn(JobKind) -> bool,
    pull: PullStrategy,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        picker(ui, "repository", &tab.name(), |ui| {
            for root in projects {
                if ui
                    .selectable_label(*root == tab.root, folder_name(root))
                    .clicked()
                {
                    actions.push(Action::OpenProject(root.clone()));
                }
            }
        });
        let branch = branch_label(tab);
        // A detached HEAD has no branch name to start from.
        let base = if branch == "detached" {
            "HEAD".to_string()
        } else {
            branch.clone()
        };
        picker(ui, "worktree · branch", &branch, |ui| {
            for b in tab.branches.iter().filter(|b| !b.remote) {
                if ui.selectable_label(b.head, &b.name).clicked() && !b.head {
                    actions.push(Action::Checkout(b.name.clone()));
                }
            }
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Right to left: the last button comes first.
            if tool_button(ui, "›_", "Terminal", true, false, "Open Terminal here") {
                actions.push(Action::OpenTerminal);
            }
            if tool_button(ui, "⊞", "Worktree", true, false, "New worktree") {
                actions.push(Action::NewWorktreeFrom(base.clone()));
            }
            if tool_button(ui, "⎇", "Branch", true, false, "New branch") {
                actions.push(Action::NewBranch(base));
            }
            // Not while a pull or push may be changing the same worktree's files.
            let waiting = "Waiting for the running job";
            let top = tab.stashes.first();
            let pop = if busy {
                waiting
            } else {
                "Apply and remove the top stash"
            };
            if tool_button(ui, "⤓", "Pop", top.is_some() && !busy, false, pop) {
                actions.extend(top.map(|s| Action::PopStash(s.hash.clone())));
            }
            let stash = if busy { waiting } else { "Stash all changes" };
            if tool_button(ui, "⤒", "Stash", !busy, false, stash) {
                actions.push(Action::Stash);
            }
            let hint = |label: &str| {
                if busy {
                    waiting.to_string()
                } else {
                    label.to_string()
                }
            };
            if tool_button(
                ui,
                "↑",
                "Push",
                !busy,
                running(JobKind::Push),
                &hint("Push"),
            ) {
                actions.push(Action::Push);
            }
            // Pull's ▾ sits between it and Push (right to left).
            ui.add_enabled_ui(!busy, |ui| {
                let more = egui::Button::new(RichText::new("▾").color(theme::MUTED))
                    .frame(false)
                    .min_size(Vec2::new(14.0, 40.0));
                egui::containers::menu::MenuButton::from_button(more)
                    .ui(ui, |ui| pull_menu(ui, pull, actions))
                    .0
                    .on_hover_text("Pull with…")
                    .on_disabled_hover_text(waiting);
            });
            let pull_hint = hint(&format!("Pull: {}", pull.label().to_lowercase()));
            if tool_button(ui, "↓", "Pull", !busy, running(JobKind::Pull), &pull_hint) {
                actions.push(Action::Pull(pull));
            }
            if tool_button(
                ui,
                "↻",
                "Fetch",
                !busy,
                running(JobKind::Fetch),
                &hint("Fetch"),
            ) {
                actions.push(Action::Fetch);
            }
        });
    });
}

/// Pull's ▾ menu, in the toolbar and the rejected-push bar: one pull with the chosen
/// strategy. `default` is what plain Pull does.
pub(super) fn pull_menu(ui: &mut egui::Ui, default: PullStrategy, actions: &mut Vec<Action>) {
    for s in PullStrategy::ALL {
        if ui.button(pull_item(s, default)).clicked() {
            actions.push(Action::Pull(s));
            ui.close();
        }
    }
    ui.separator();
    // Until the Preferences window (#84) has a place for it.
    ui.label(theme::caption(
        "Default: pull_strategy in config.toml, else git's pull.rebase / pull.ff",
    ));
}

/// A pull menu item, marking the one plain Pull uses.
fn pull_item(s: PullStrategy, default: PullStrategy) -> String {
    if s == default {
        format!("{} · default", s.label())
    } else {
        s.label().to_string()
    }
}

/// Muted caption over a frameless "value ▾" menu button.
fn picker(ui: &mut egui::Ui, caption: &str, value: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(caption).size(10.0).color(theme::MUTED));
        let button = egui::Button::new((
            RichText::new(value).strong().color(Color32::WHITE),
            RichText::new("▾").color(theme::MUTED),
        ))
        .frame(false);
        egui::containers::menu::MenuButton::from_button(button).ui(ui, add);
    });
}

/// Icon over a small label; a spinner sits beside the icon while its job runs.
fn tool_button(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    enabled: bool,
    running: bool,
    hint: &str,
) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(54.0, 40.0), Sense::click());
        let p = ui.painter();
        if resp.hovered() && enabled {
            p.rect_filled(rect, theme::WIDGET_R, theme::HOVER);
        }
        let c = rect.center().x;
        p.text(
            egui::pos2(c, rect.top() + 13.0),
            Align2::CENTER_CENTER,
            icon,
            FontId::proportional(16.0),
            theme::TEXT,
        );
        p.text(
            egui::pos2(c, rect.bottom() - 9.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(10.5),
            theme::MUTED,
        );
        if running {
            let at = egui::Rect::from_center_size(
                egui::pos2(c + 15.0, rect.top() + 13.0),
                Vec2::splat(12.0),
            );
            egui::Spinner::new()
                .color(theme::ACCENT_TEXT)
                .paint_at(ui, at);
        }
        let resp = if enabled {
            resp.on_hover_text(hint)
        } else {
            resp.on_disabled_hover_text(hint)
        };
        resp.clicked()
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_menu_marks_the_default() {
        let items: Vec<String> = PullStrategy::ALL
            .into_iter()
            .map(|s| pull_item(s, PullStrategy::Rebase))
            .collect();
        assert_eq!(items, ["Fast-forward only", "Merge", "Rebase · default"]);
    }

    #[test]
    fn a_dragged_tab_lands_in_the_gap_under_the_pointer() {
        let centres = [50.0, 150.0, 250.0]; // three tabs, 100 wide
        assert_eq!(drop_slot(10.0, &centres), 0, "before the first");
        assert_eq!(drop_slot(120.0, &centres), 1);
        assert_eq!(drop_slot(400.0, &centres), 3, "after the last");
        // Tab 0 dragged to the end lands last; into its own gaps it stays put.
        assert_eq!(moved_index(0, 3), 2);
        assert_eq!(moved_index(0, 0), 0);
        assert_eq!(moved_index(0, 1), 0);
        // Tab 2 dragged to the front.
        assert_eq!(moved_index(2, 0), 0);
        assert_eq!(moved_index(2, 3), 2, "onto itself");
    }
}
