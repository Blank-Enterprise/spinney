//! Centre panel for a conflicted file: its worktree text, read-only, with each conflict's
//! sides tinted and labelled with where they came from.

use super::{theme, Action};
use crate::conflict::{self, Chunk};
use crate::refs;
use eframe::egui::{self, Align, Align2, Color32, RichText, Sense, Vec2};
use std::path::Path;

pub struct ConflictView {
    pub path: String,
    pub content: Content,
    /// The checked-out branch, to name git's `HEAD` label.
    pub head: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Content {
    Text(Vec<Chunk>),
    Binary,
    /// Not in the worktree: one side deleted it.
    Missing,
}

impl ConflictView {
    pub fn load(worktree: &Path, path: &str) -> ConflictView {
        let content = match std::fs::read(worktree.join(path)) {
            Err(_) => Content::Missing,
            Ok(bytes) if bytes.contains(&0) => Content::Binary,
            Ok(bytes) => Content::Text(conflict::parse(&String::from_utf8_lossy(&bytes))),
        };
        ConflictView {
            path: path.to_string(),
            content,
            head: refs::current_branch(worktree).ok().flatten(),
        }
    }
}

/// What a displayed line is: plain, a side of a conflict, or one of git's marker lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Same,
    Ours,
    Base,
    Theirs,
    /// `<<<<<<<`, `|||||||` and `=======`: each starts the section below it.
    Start,
    BaseStart,
    Separator,
    /// `>>>>>>>`
    End,
}

/// One displayed line: its 1-based line number in the file, text, and for a marker that
/// starts a section, the label naming that section's side.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub n: usize,
    pub kind: Kind,
    pub text: String,
    pub label: Option<String>,
}

/// git's label for a side, with the branch name added to a bare `HEAD`.
pub fn label(raw: &str, head: Option<&str>) -> String {
    match (raw, head) {
        ("HEAD", Some(b)) => format!("HEAD ({b})"),
        ("", _) => "(unnamed)".to_string(),
        _ => raw.to_string(),
    }
}

/// The file's lines again, in order, each with what it is.
pub fn rows(chunks: &[Chunk], head: Option<&str>) -> Vec<Row> {
    let mut out = vec![];
    let mut push = |kind, text: String, label: Option<String>| {
        let n = out.len() + 1;
        out.push(Row {
            n,
            kind,
            text,
            label,
        });
    };
    let marker = |m: &str, raw: &str| {
        if raw.is_empty() {
            m.to_string()
        } else {
            format!("{m} {raw}")
        }
    };
    for chunk in chunks {
        let b = match chunk {
            Chunk::Same(lines) => {
                for l in lines {
                    push(Kind::Same, l.clone(), None);
                }
                continue;
            }
            Chunk::Conflict(b) => b,
        };
        let ours = label(&b.ours_label, head);
        push(Kind::Start, marker("<<<<<<<", &b.ours_label), Some(ours));
        for l in &b.ours {
            push(Kind::Ours, l.clone(), None);
        }
        if let Some((raw, lines)) = &b.base {
            let base = format!("base: {}", label(raw, None));
            push(Kind::BaseStart, marker("|||||||", raw), Some(base));
            for l in lines {
                push(Kind::Base, l.clone(), None);
            }
        }
        let theirs = label(&b.theirs_label, head);
        push(Kind::Separator, "=======".into(), Some(theirs));
        for l in &b.theirs {
            push(Kind::Theirs, l.clone(), None);
        }
        push(Kind::End, marker(">>>>>>>", &b.theirs_label), None);
    }
    out
}

/// Background for a row of `kind`; marker rows are stronger than the lines they head.
fn fill(kind: Kind) -> Color32 {
    let (base, alpha) = match kind {
        Kind::Same => return Color32::TRANSPARENT,
        Kind::Ours => (theme::ACCENT, 0.12),
        Kind::Base => (theme::UNTRACKED, 0.12),
        Kind::Theirs => (theme::RENAMED, 0.12),
        Kind::Start => (theme::ACCENT, 0.3),
        Kind::BaseStart => (theme::UNTRACKED, 0.3),
        Kind::Separator => (theme::RENAMED, 0.3),
        Kind::End => (theme::RENAMED, 0.3),
    };
    base.gamma_multiply(alpha)
}

const ROW_H: f32 = 18.0;

pub fn show(ui: &mut egui::Ui, view: &ConflictView, actions: &mut Vec<Action>) {
    if !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseDiff);
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(&view.path).strong().color(Color32::WHITE));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let close =
                egui::Button::new(RichText::new("×").size(16.0).color(theme::MUTED)).frame(false);
            if ui.add(close).on_hover_text("Close (Esc)").clicked() {
                actions.push(Action::CloseDiff);
            }
            if matches!(view.content, Content::Text(_)) {
                let merge = ui
                    .add(theme::primary("Merge…"))
                    .on_hover_text("Pick a side for each conflict, or edit the result");
                if merge.clicked() {
                    actions.push(Action::OpenMerge(view.path.clone()));
                }
            }
        });
    });
    let chunks = match &view.content {
        Content::Text(chunks) => chunks,
        Content::Binary => return note(ui, "Binary file: it has no conflict blocks to show."),
        Content::Missing => return note(ui, "Deleted on one side, so there is nothing to show."),
    };
    let n = conflict::count(chunks);
    let what = match n {
        0 => "no conflict markers left".to_string(),
        1 => "1 conflict".to_string(),
        n => format!("{n} conflicts"),
    };
    ui.weak(format!("conflicted · {what} · read-only"));
    ui.add_space(2.0);
    ui.separator();
    let rows = rows(chunks, view.head.as_deref());
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let char_w = ui
        .painter()
        .layout_no_wrap("0".into(), font.clone(), Color32::WHITE)
        .size()
        .x;
    let gutter = char_w * 6.0;
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, rows.len(), |ui, range| {
            let longest = rows[range.clone()]
                .iter()
                .map(|r| r.text.chars().count() + 3 * r.text.matches('\t').count())
                .max()
                .unwrap_or(0);
            let width = (gutter + char_w * (longest + 3) as f32).max(ui.available_width());
            for row in &rows[range] {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::hover());
                let p = ui.painter();
                p.rect_filled(rect, 0.0, fill(row.kind));
                let y = rect.center().y;
                p.text(
                    egui::pos2(rect.left() + gutter - char_w, y),
                    Align2::RIGHT_CENTER,
                    row.n.to_string(),
                    font.clone(),
                    theme::MUTED,
                );
                let marker = !matches!(
                    row.kind,
                    Kind::Same | Kind::Ours | Kind::Base | Kind::Theirs
                );
                let color = if marker { theme::MUTED } else { theme::TEXT };
                let text = row.text.replace('\t', "    ");
                let at = egui::pos2(rect.left() + gutter + char_w, y);
                let r = p.text(at, Align2::LEFT_CENTER, text, font.clone(), color);
                if let Some(label) = &row.label {
                    // Named just after the marker, so it stays in view on narrow panels.
                    let at = egui::pos2(r.right() + 2.0 * char_w, y);
                    p.text(at, Align2::LEFT_CENTER, label, font.clone(), Color32::WHITE);
                }
            }
        });
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.separator();
    ui.weak(text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::parse;

    #[test]
    fn rows_number_every_line_and_label_each_side() {
        let text = "a\n<<<<<<< HEAD\nm\n||||||| base\nb\n=======\nt\n>>>>>>> topic\nz\n";
        let rows = rows(&parse(text), Some("main"));
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.n, r.kind, r.text.as_str(), r.label.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                (1, Kind::Same, "a", None),
                (2, Kind::Start, "<<<<<<< HEAD", Some("HEAD (main)")),
                (3, Kind::Ours, "m", None),
                (4, Kind::BaseStart, "||||||| base", Some("base: base")),
                (5, Kind::Base, "b", None),
                (6, Kind::Separator, "=======", Some("topic")),
                (7, Kind::Theirs, "t", None),
                (8, Kind::End, ">>>>>>> topic", None),
                (9, Kind::Same, "z", None),
            ]
        );
        // Every line of the file comes back, in order.
        let back: Vec<_> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(back, text.lines().collect::<Vec<_>>());
    }

    #[test]
    fn labels_name_head_and_unnamed_sides() {
        assert_eq!(label("HEAD", Some("main")), "HEAD (main)");
        assert_eq!(label("HEAD", None), "HEAD");
        assert_eq!(label("topic", Some("main")), "topic");
        assert_eq!(label("", None), "(unnamed)");
    }

    #[test]
    fn load_reads_text_binary_and_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        crate::git::run(d, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(d.join("t"), "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> o\n").unwrap();
        std::fs::write(d.join("b"), b"\x00bin").unwrap();
        let t = ConflictView::load(d, "t");
        assert!(matches!(&t.content, Content::Text(c) if conflict::count(c) == 1));
        assert_eq!(
            t.head.as_deref(),
            Some("main"),
            "named even before a commit"
        );
        assert_eq!(ConflictView::load(d, "b").content, Content::Binary);
        assert_eq!(ConflictView::load(d, "gone").content, Content::Missing);
    }
}
