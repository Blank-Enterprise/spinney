//! Top banner while a merge, rebase, cherry-pick or revert is stopped partway: Continue,
//! Skip (rebase and cherry-pick) and Abort…, the last two confirmed first.

use super::{theme, Action};
use crate::actions::Operation;
use eframe::egui::{self, Align, Layout, RichText};
use std::path::PathBuf;

/// What Skip or Abort… asks to confirm: both throw work away.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Skip,
    Abort,
}

/// A Skip or Abort waiting for confirmation, tied to the worktree and operation it was
/// asked about so it never runs against anything else.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub root: PathBuf,
    pub worktree: PathBuf,
    pub op: Operation,
    pub step: Step,
}

/// The banner's icon, the confirmation bars' warning sign. The first choice, ⑂ (U+2442),
/// has no glyph in the fonts the app draws with, so it showed as an empty box.
pub const ICON: &str = "⚠";

/// "Merge", "Cherry-pick": the operation at the start of a sentence.
pub fn title(op: Operation) -> String {
    let name = op.command();
    name[..1].to_uppercase() + &name[1..]
}

/// The banner's line, e.g. "Merge in progress: resolve 2 conflicted files, then Continue."
pub fn banner_text(op: Operation, conflicts: usize) -> String {
    let what = match conflicts {
        0 => "conflicts resolved, Continue to finish".to_string(),
        1 => "resolve 1 conflicted file, then Continue".to_string(),
        n => format!("resolve {n} conflicted files, then Continue"),
    };
    format!("{} in progress: {what}.", title(op))
}

/// Why Continue is disabled, or None when it can run.
pub fn continue_blocked(conflicts: usize, busy: bool) -> Option<&'static str> {
    if conflicts > 0 {
        Some("Resolve the conflicted files first")
    } else if busy {
        Some("Waiting for the running job")
    } else {
        None
    }
}

/// The confirmation question, naming what is lost.
pub fn question(op: Operation, step: Step) -> String {
    match step {
        Step::Abort => format!(
            "Abort the {op}? The branch goes back to where it was before, and any \
             conflict resolutions so far are lost."
        ),
        Step::Skip => format!(
            "Skip this commit? Its changes are left out of the {op}, along with any \
             resolutions you made to it."
        ),
    }
}

/// The banner, or with `asking` the confirmation for that step in its place.
pub fn bar(
    ui: &mut egui::Ui,
    op: Operation,
    conflicts: usize,
    busy: bool,
    asking: Option<Step>,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        if let Some(step) = asking {
            ui.label(RichText::new(ICON).color(theme::DELETED));
            ui.label(RichText::new(question(op, step)).color(theme::TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Cancel").clicked() {
                    actions.push(Action::CancelOpStep);
                }
                let label = match step {
                    Step::Skip => "Skip",
                    Step::Abort => "Abort",
                };
                let confirm =
                    egui::Button::new(RichText::new(label).color(theme::ON_ACCENT).strong())
                        .fill(theme::DELETED)
                        .small();
                let r = ui
                    .add_enabled(!busy, confirm)
                    .on_disabled_hover_text("Waiting for the running job");
                if r.clicked() {
                    actions.push(Action::ConfirmOpStep);
                }
            });
            return;
        }
        ui.label(RichText::new(ICON).color(theme::WARN));
        ui.label(RichText::new(banner_text(op, conflicts)).color(theme::TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let r = ui
                .add_enabled(!busy, egui::Button::new("Abort…").small())
                .on_disabled_hover_text("Waiting for the running job");
            if r.clicked() {
                actions.push(Action::AskOpStep(Step::Abort));
            }
            if op.can_skip() {
                let r = ui
                    .add_enabled(!busy, egui::Button::new("Skip…").small())
                    .on_hover_text("Leave out the commit this stopped on")
                    .on_disabled_hover_text("Waiting for the running job");
                if r.clicked() {
                    actions.push(Action::AskOpStep(Step::Skip));
                }
            }
            let why = continue_blocked(conflicts, busy);
            let go = egui::Button::new(RichText::new("Continue").color(theme::ON_ACCENT).strong())
                .fill(theme::ACCENT)
                .small();
            let r = ui
                .add_enabled(why.is_none(), go)
                .on_disabled_hover_text(why.unwrap_or_default());
            if r.clicked() {
                actions.push(Action::ContinueOp);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_banner_count_conflicts() {
        assert_eq!(title(Operation::CherryPick), "Cherry-pick");
        assert_eq!(
            banner_text(Operation::Merge, 2),
            "Merge in progress: resolve 2 conflicted files, then Continue."
        );
        assert_eq!(
            banner_text(Operation::Rebase, 1),
            "Rebase in progress: resolve 1 conflicted file, then Continue."
        );
        assert_eq!(
            banner_text(Operation::Revert, 0),
            "Revert in progress: conflicts resolved, Continue to finish."
        );
    }

    #[test]
    fn continue_waits_for_conflicts_and_jobs() {
        assert_eq!(
            continue_blocked(1, false),
            Some("Resolve the conflicted files first")
        );
        assert_eq!(
            continue_blocked(0, true),
            Some("Waiting for the running job")
        );
        assert_eq!(continue_blocked(0, false), None);
    }

    #[test]
    fn questions_name_what_is_lost() {
        let q = question(Operation::Rebase, Step::Abort);
        assert!(
            q.starts_with("Abort the rebase?") && q.contains("lost"),
            "{q}"
        );
        let q = question(Operation::CherryPick, Step::Skip);
        assert!(q.contains("left out of the cherry-pick"), "{q}");
    }

    #[test]
    fn banner_icon_is_one_that_renders() {
        // Pinned to the glyph the other confirmation bars use, which draws on screen.
        assert_eq!(ICON, "⚠");
    }
}
