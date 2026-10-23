//! Long lists (thousands of branches or untracked files) skip the rows scrolled out of
//! view, leaving their space: otherwise each row builds its widgets on every repaint.

use eframe::egui;

/// Whether rows out of view may be skipped this frame. Not while a menu is open, a branch
/// is dragged, or a button has keyboard focus: the menu's or drag's row needs its widget
/// every frame, and Tab or the arrows move focus through every row in turn.
pub fn may_skip(menu_open: bool, dragging: bool, button_focused: bool) -> bool {
    !(menu_open || dragging || button_focused)
}

/// Whether the row about to be laid out, `h` tall (spacing included) from `top`, is out of
/// `view`. Always false until a drawn row has given the height.
pub fn hidden(h: Option<f32>, top: f32, view: egui::Rangef) -> bool {
    h.is_some_and(|h| top + h < view.min || top > view.max)
}

/// Lays out rows of one height, skipping those out of view: rows in view are drawn as
/// usual, and a skipped row leaves exactly the space it would have taken.
pub struct Rows {
    /// A drawn row's height, item spacing included.
    h: Option<f32>,
    skip: bool,
}

impl Rows {
    pub fn new(ctx: &egui::Context) -> Rows {
        let menu = egui::Popup::is_any_open(ctx);
        let dragging = egui::DragAndDrop::has_any_payload(ctx);
        let focused = ctx.memory(|m| m.focused()).is_some() && !ctx.text_edit_focused();
        Rows {
            h: None,
            skip: may_skip(menu, dragging, focused),
        }
    }

    /// Draw one row with `add`, or leave its space when it's out of view. A drawn row (in
    /// its own scope) and a gap each take one auto id, so the widgets after them keep their
    /// ids, and an open menu its row, whichever rows were skipped.
    pub fn row(&mut self, ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
        let top = ui.cursor().min.y;
        if self.skip && hidden(self.h, top, ui.clip_rect().y_range()) {
            let gap = ui.spacing().item_spacing.y;
            ui.allocate_space(egui::vec2(0.0, self.h.unwrap_or_default() - gap));
            return;
        }
        ui.scope(add);
        self.h.get_or_insert(ui.cursor().min.y - top);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_skipped_only_when_nothing_needs_them() {
        assert!(may_skip(false, false, false));
        assert!(!may_skip(true, false, false), "a context menu is open");
        assert!(!may_skip(false, true, false), "a branch is dragged");
        assert!(!may_skip(false, false, true), "Tab is moving through rows");
    }

    #[test]
    fn a_row_is_hidden_only_wholly_out_of_view() {
        let view = egui::Rangef::new(100.0, 200.0);
        assert!(!hidden(None, 0.0, view), "height not known yet: draw it");
        assert!(hidden(Some(20.0), 0.0, view), "above");
        assert!(hidden(Some(20.0), 250.0, view), "below");
        assert!(!hidden(Some(20.0), 90.0, view), "partly in view at the top");
        assert!(
            !hidden(Some(20.0), 190.0, view),
            "partly in view at the bottom"
        );
        assert!(!hidden(Some(20.0), 150.0, view));
    }
}
