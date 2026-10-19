use crate::graph::Row;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

/// Lane colours cycle through this many hues.
pub const HUES: usize = super::theme::LANES.len();

const ROW_H: f32 = 22.0;
const LANE_W: f32 = 14.0;
/// Fetch the next page once the view is this close to the end.
const PREFETCH: usize = 100;

pub struct GraphOutput {
    pub clicked: Option<usize>,
    pub near_end: bool,
}

fn color(lane: usize) -> Color32 {
    super::theme::LANES[lane_color_index(lane)]
}

pub fn show(ui: &mut egui::Ui, rows: &[Row], selected: Option<usize>) -> GraphOutput {
    let mut out = GraphOutput {
        clicked: None,
        near_end: false,
    };
    if rows.is_empty() {
        ui.weak("No commits yet.");
        return out;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, rows.len(), |ui, range| {
            out.near_end = range.end + PREFETCH >= rows.len();
            let lanes = rows[range.clone()]
                .iter()
                .map(|r| r.lanes_in.len().max(r.lanes_out.len()).max(r.lane + 1))
                .max()
                .unwrap_or(1);
            let text_x = LANE_W * lanes as f32 + 8.0;
            for i in range {
                let width = ui.available_width().max(text_x + 400.0);
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                if resp.clicked() {
                    out.clicked = Some(i);
                }
                if selected == Some(i) {
                    ui.painter()
                        .rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                }
                paint_row(ui, rect, &rows[i], text_x, now);
            }
        });
    out
}

fn paint_row(ui: &egui::Ui, rect: Rect, row: &Row, text_x: f32, now: i64) {
    let painter = ui.painter();
    let x = |lane: usize| rect.left() + LANE_W * lane as f32 + LANE_W / 2.0;
    let (top, mid, bottom) = (rect.top(), rect.center().y, rect.bottom());
    let stroke = |lane: usize| Stroke::new(1.5, color(lane));
    for (i, slot) in row.lanes_in.iter().enumerate() {
        let Some(s) = *slot else { continue };
        if s == row.id {
            painter.line_segment(
                [Pos2::new(x(i), top), Pos2::new(x(row.lane), mid)],
                stroke(i),
            );
        } else if let Some(j) = find_lane(&row.lanes_out, s, i) {
            painter.line_segment([Pos2::new(x(i), top), Pos2::new(x(j), bottom)], stroke(j));
        }
    }
    for &p in &row.parent_ids {
        if let Some(j) = find_lane(&row.lanes_out, p, row.lane) {
            painter.line_segment(
                [Pos2::new(x(row.lane), mid), Pos2::new(x(j), bottom)],
                stroke(j),
            );
        }
    }
    painter.circle_filled(Pos2::new(x(row.lane), mid), 4.0, color(row.lane));

    let font = FontId::proportional(13.0);
    let text = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let mut cx = rect.left() + text_x;
    for r in row.commit.refs.split(", ").filter(|r| !r.is_empty()) {
        let galley = painter.layout_no_wrap(r.to_string(), FontId::proportional(11.0), text);
        let size = galley.size();
        let pill = Rect::from_min_size(
            Pos2::new(cx, mid - size.y / 2.0 - 1.0),
            size + Vec2::new(8.0, 2.0),
        );
        painter.rect(
            pill,
            3.0,
            ui.visuals().extreme_bg_color,
            Stroke::new(1.0, color(row.lane)),
            StrokeKind::Inside,
        );
        painter.galley(pill.min + Vec2::new(4.0, 1.0), galley, text);
        cx = pill.right() + 4.0;
    }
    let subject = painter.text(
        Pos2::new(cx, mid),
        Align2::LEFT_CENTER,
        &row.commit.subject,
        font.clone(),
        text,
    );
    let meta = format!(
        "{}  ·  {}",
        row.commit.author,
        relative_time(now, row.commit.time)
    );
    painter.text(
        Pos2::new(subject.right() + 12.0, mid),
        Align2::LEFT_CENTER,
        meta,
        font,
        weak,
    );
}

/// Slot in `lanes` holding commit `id`, preferring `prefer` when several do.
fn find_lane(lanes: &[Option<u32>], id: u32, prefer: usize) -> Option<usize> {
    if lanes.get(prefer) == Some(&Some(id)) {
        return Some(prefer);
    }
    lanes.iter().position(|&l| l == Some(id))
}

pub fn lane_color_index(lane: usize) -> usize {
    lane % HUES
}

/// Compact age of `t` relative to `now` (both unix seconds).
pub fn relative_time(now: i64, t: i64) -> String {
    let secs = now - t;
    let (n, unit) = match secs {
        ..60 => return "just now".into(),
        60..3_600 => (secs / 60, "m"),
        3_600..86_400 => (secs / 3_600, "h"),
        86_400..2_592_000 => (secs / 86_400, "d"),
        2_592_000..31_536_000 => (secs / 2_592_000, "mo"),
        _ => (secs / 31_536_000, "y"),
    };
    format!("{n}{unit} ago")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_colours_cycle_through_eight() {
        assert_eq!(HUES, 8);
        assert_eq!(lane_color_index(0), 0);
        assert_eq!(lane_color_index(7), 7);
        assert_eq!(lane_color_index(8), 0);
        assert_eq!(lane_color_index(19), 3);
    }

    #[test]
    fn relative_times() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now + 50), "just now");
        assert_eq!(relative_time(now, now - 30), "just now");
        assert_eq!(relative_time(now, now - 5 * 60), "5m ago");
        assert_eq!(relative_time(now, now - 3 * 3600), "3h ago");
        assert_eq!(relative_time(now, now - 2 * 86400), "2d ago");
        assert_eq!(relative_time(now, now - 60 * 86400), "2mo ago");
        assert_eq!(relative_time(now, now - 800 * 86400), "2y ago");
    }
}
