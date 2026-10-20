//! Drag a branch (a sidebar row or a graph label) onto another to merge, rebase or
//! fast-forward. Dropping opens a small menu; each item goes through the same action as
//! the branch menu, so merge and rebase still ask first.

use super::{theme, Action};
use eframe::egui::{self, Pos2};

/// The branch being dragged, by its full name (`main`, `origin/feat/x`).
#[derive(Clone, Debug, PartialEq)]
pub struct BranchDrag(pub String);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropChoice {
    /// Merge the dragged branch into the target, which is checked out.
    Merge,
    /// Rebase the dragged branch, which is checked out, onto the target.
    Rebase,
    /// Move the target, checked out nowhere, up to the dragged branch.
    FastForward,
}

/// One item of the drop menu; `disabled` says why it can't be used.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub kind: DropChoice,
    pub label: String,
    pub disabled: Option<String>,
}

/// What the menu needs to know about the two branches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropFacts<'a> {
    /// The checked-out branch; None when HEAD is detached.
    pub current: Option<&'a str>,
    pub dragged_remote: bool,
    pub target_remote: bool,
    /// The target is local, checked out nowhere, and behind the dragged branch.
    pub fast_forward: bool,
}

/// The drop menu for `dragged` onto `target`. Merge and rebase run in the checked-out
/// branch, so they show disabled until the right one is checked out; fast-forward shows
/// only where it is possible.
pub fn drop_choices(dragged: &str, target: &str, f: DropFacts) -> Vec<Choice> {
    if dragged == target {
        return vec![];
    }
    let check_out = |b: &str| Some(format!("Check out {b} first"));
    let mut choices = vec![];
    if !f.target_remote {
        choices.push(Choice {
            kind: DropChoice::Merge,
            label: format!("Merge {dragged} into {target}…"),
            disabled: (f.current != Some(target))
                .then(|| check_out(target))
                .flatten(),
        });
    }
    if !f.dragged_remote {
        choices.push(Choice {
            kind: DropChoice::Rebase,
            label: format!("Rebase {dragged} onto {target}…"),
            disabled: (f.current != Some(dragged))
                .then(|| check_out(dragged))
                .flatten(),
        });
    }
    if f.fast_forward {
        choices.push(Choice {
            kind: DropChoice::FastForward,
            label: format!("Fast-forward {target} to {dragged}"),
            disabled: None,
        });
    }
    choices
}

/// The action an item runs: the branch menu's own merge and rebase confirmations.
pub fn choice_action(kind: DropChoice, dragged: &str, target: &str) -> Action {
    match kind {
        DropChoice::Merge => Action::AskMerge(dragged.to_string()),
        DropChoice::Rebase => Action::AskRebase(target.to_string()),
        DropChoice::FastForward => Action::FastForwardTo(target.to_string(), dragged.to_string()),
    }
}

/// An open drop menu, tied to the project it was dropped in.
pub struct DropMenu {
    pub root: std::path::PathBuf,
    pub dragged: String,
    pub target: String,
    pub choices: Vec<Choice>,
    pub pos: Pos2,
}

/// Make `r` a drag source for `branch` and a drop target for other branches: outlined
/// while one hovers over it, and on release the drop is pushed as an action.
pub fn drag_and_drop(ui: &egui::Ui, r: &egui::Response, branch: &str, actions: &mut Vec<Action>) {
    r.dnd_set_drag_payload(BranchDrag(branch.to_string()));
    let other = |p: &BranchDrag| p.0 != branch;
    if r.dnd_hover_payload::<BranchDrag>()
        .is_some_and(|p| other(&p))
    {
        let stroke = egui::Stroke::new(1.5, theme::ACCENT);
        ui.painter()
            .rect_stroke(r.rect, 4.0, stroke, egui::StrokeKind::Inside);
    }
    if let Some(p) = r.dnd_release_payload::<BranchDrag>().filter(|p| other(p)) {
        let pos = ui.ctx().pointer_latest_pos().unwrap_or(r.rect.center());
        actions.push(Action::DropBranch(p.0.clone(), branch.to_string(), pos));
    }
}

/// The dragged branch's name beside the pointer while a drag is under way.
pub fn preview(ctx: &egui::Context) {
    let Some(p) = egui::DragAndDrop::payload::<BranchDrag>(ctx) else {
        return;
    };
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    egui::Area::new(egui::Id::unique("branch_drag_preview"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(12.0, 8.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(&p.0));
        });
}

/// Draw the drop menu; returns the chosen item, or `Some(None)` when it was dismissed.
pub fn menu(ctx: &egui::Context, m: &DropMenu) -> Option<Option<DropChoice>> {
    let mut result = None;
    let area = egui::Area::new(egui::Id::unique("branch_drop_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(m.pos)
        .show(ctx, |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                ui.label(theme::caption(&format!("{} → {}", m.dragged, m.target)));
                if m.choices.is_empty() {
                    ui.weak("Nothing to do here");
                }
                for c in &m.choices {
                    let r = ui.add_enabled(c.disabled.is_none(), egui::Button::new(&c.label));
                    let r = match &c.disabled {
                        Some(why) => r.on_disabled_hover_text(why),
                        None => r,
                    };
                    if r.clicked() {
                        result = Some(Some(c.kind));
                    }
                }
                ui.separator();
                if ui.button("Cancel").clicked() {
                    result = Some(None);
                }
            });
        });
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    if result.is_none() && (escape || area.response.clicked_elsewhere()) {
        result = Some(None);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(current: Option<&str>) -> DropFacts<'_> {
        DropFacts {
            current,
            dragged_remote: false,
            target_remote: false,
            fast_forward: false,
        }
    }

    fn kinds(c: &[Choice]) -> Vec<(DropChoice, bool)> {
        c.iter().map(|c| (c.kind, c.disabled.is_none())).collect()
    }

    #[test]
    fn merge_needs_the_target_checked_out_and_rebase_the_dragged() {
        let c = drop_choices("feat/x", "main", facts(Some("main")));
        assert_eq!(c[0].label, "Merge feat/x into main…");
        assert_eq!(c[1].label, "Rebase feat/x onto main…");
        assert_eq!(
            kinds(&c),
            [(DropChoice::Merge, true), (DropChoice::Rebase, false)]
        );
        assert_eq!(c[1].disabled.as_deref(), Some("Check out feat/x first"));
        let c = drop_choices("feat/x", "main", facts(Some("feat/x")));
        assert_eq!(
            kinds(&c),
            [(DropChoice::Merge, false), (DropChoice::Rebase, true)]
        );
        let c = drop_choices("feat/x", "main", facts(None));
        assert!(c.iter().all(|c| c.disabled.is_some()), "detached");
        assert!(drop_choices("main", "main", facts(Some("main"))).is_empty());
    }

    #[test]
    fn remote_branches_and_fast_forward_offer_only_what_is_possible() {
        let remote_dragged = DropFacts {
            dragged_remote: true,
            ..facts(Some("main"))
        };
        let c = drop_choices("origin/main", "main", remote_dragged);
        assert_eq!(
            kinds(&c),
            [(DropChoice::Merge, true)],
            "no rebasing origin/main"
        );
        let remote_target = DropFacts {
            target_remote: true,
            ..facts(Some("feat/x"))
        };
        let c = drop_choices("feat/x", "origin/main", remote_target);
        assert_eq!(
            kinds(&c),
            [(DropChoice::Rebase, true)],
            "no merging into origin/main"
        );
        let ff = DropFacts {
            fast_forward: true,
            ..facts(Some("feat/x"))
        };
        let c = drop_choices("feat/x", "main", ff);
        assert_eq!(c[2].label, "Fast-forward main to feat/x");
        assert_eq!(c[2].disabled, None);
    }

    #[test]
    fn dropping_opens_the_menu_and_fast_forward_moves_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = super::super::tests::cfg();
        let mut app = super::super::tests::test_app(cfg, &tmp.path().join("config.toml"));
        let proj = tempfile::tempdir().unwrap();
        let d = proj.path();
        let g = |args: &[&str]| crate::git::run(d, args).unwrap().trim().to_string();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "Test User"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["commit", "-q", "--allow-empty", "-m", "init"]);
        g(&["branch", "base"]);
        g(&["commit", "-q", "--allow-empty", "-m", "more"]);
        let space = app.space().unwrap();
        app.open_tab(d, &space);
        let pos = Pos2::new(10.0, 10.0);
        let drop = |app: &mut super::super::SpinneyApp, x: &str, y: &str| {
            app.apply(Action::DropBranch(x.into(), y.into(), pos));
            let m = app.dialogs.drop_menu.take().expect("menu open");
            assert_eq!((m.dragged.as_str(), m.target.as_str()), (x, y));
            kinds(&m.choices)
        };

        // base is behind main and checked out nowhere: it can be fast-forwarded.
        let c = drop(&mut app, "main", "base");
        let want = [
            (DropChoice::Merge, false),
            (DropChoice::Rebase, true),
            (DropChoice::FastForward, true),
        ];
        assert_eq!(c, want);
        // main is checked out, so it is merged into rather than moved.
        let c = drop(&mut app, "base", "main");
        assert_eq!(c, [(DropChoice::Merge, true), (DropChoice::Rebase, false)]);

        app.apply(choice_action(DropChoice::FastForward, "main", "base"));
        assert!(app.errors.is_empty(), "{:?}", app.errors);
        assert_eq!(g(&["rev-parse", "base"]), g(&["rev-parse", "main"]));
        app.apply(choice_action(DropChoice::Merge, "base", "main"));
        assert!(app.dialogs.merge.is_some(), "merge still asks first");
    }

    #[test]
    fn choices_reuse_the_branch_menu_actions() {
        let is = |a: Action, want: &str| match a {
            Action::AskMerge(b) => assert_eq!(format!("merge {b}"), want),
            Action::AskRebase(b) => assert_eq!(format!("rebase onto {b}"), want),
            Action::FastForwardTo(b, to) => assert_eq!(format!("ff {b} to {to}"), want),
            _ => panic!("unexpected action"),
        };
        is(
            choice_action(DropChoice::Merge, "feat/x", "main"),
            "merge feat/x",
        );
        is(
            choice_action(DropChoice::Rebase, "feat/x", "main"),
            "rebase onto main",
        );
        is(
            choice_action(DropChoice::FastForward, "feat/x", "main"),
            "ff main to feat/x",
        );
    }
}
