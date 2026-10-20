//! Centre-panel diff: side-by-side or inline, with line/hunk selection for staging.

use super::{theme, Action, Center, Discard};
use crate::diff::{side_by_side, FileDiff, LineKind};
use crate::stage::{build_patch, discard_patch, Direction};
use crate::status::{Change, FileStatus};
use eframe::egui::{self, Align, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Vec2};
use std::collections::HashSet;

/// Selected `(hunk_idx, line_idx)` lines of the displayed diff, all in one hunk.
#[derive(Default, Debug, PartialEq)]
pub struct Selection {
    pub lines: HashSet<(usize, usize)>,
    /// Last plain click; shift-click extends from here within the same hunk.
    anchor: Option<(usize, usize)>,
}

impl Selection {
    /// Toggle the Add/Del line `at`; with shift and an anchor in the same hunk, add the
    /// change lines between them instead. A click in another hunk starts a new selection.
    pub fn click(&mut self, fd: &FileDiff, at: (usize, usize), shift: bool) {
        if !selectable(fd, at) {
            return;
        }
        if self.lines.iter().any(|&(hi, _)| hi != at.0) {
            self.lines.clear();
            self.anchor = None;
        }
        match self.anchor {
            Some(a) if shift && a.0 == at.0 => {
                let (lo, hi) = (a.1.min(at.1), a.1.max(at.1));
                self.lines.extend(
                    (lo..=hi)
                        .map(|li| (at.0, li))
                        .filter(|&p| selectable(fd, p)),
                );
            }
            _ => {
                if !self.lines.remove(&at) {
                    self.lines.insert(at);
                }
                self.anchor = Some(at);
            }
        }
    }
}

/// Add/Del lines can be selected; context lines cannot.
pub fn selectable(fd: &FileDiff, (hi, li): (usize, usize)) -> bool {
    fd.hunks
        .get(hi)
        .and_then(|h| h.lines.get(li))
        .is_some_and(|l| l.kind != LineKind::Context)
}

/// Why line and hunk actions (stage, unstage, discard) are unavailable for this side of a
/// file, or None when they are: only UTF-8 text files whose change is a plain modification
/// have a diff that can be applied back in parts.
pub fn line_actions_blocked(change: &Change, fd: &FileDiff) -> Option<&'static str> {
    if fd.binary {
        return Some("This is a binary file");
    }
    if fd.lossy {
        return Some("This file isn't UTF-8 text");
    }
    match change {
        Change::Modified => None,
        Change::Added => Some("This file is new"),
        Change::Untracked => Some("This file is untracked"),
        Change::Deleted => Some("This file was deleted"),
        Change::Renamed { .. } => Some("This file was renamed"),
        Change::TypeChanged { .. } => Some("This file changed type"),
        Change::Conflicted(_) => Some("This file has conflicts"),
    }
}

/// The diff header's hint: the reason, then what to do instead on that side.
pub fn line_hint(reason: &str, staged: bool) -> String {
    let what = if staged {
        "unstage the whole file"
    } else {
        "stage or discard the whole file"
    };
    format!("{reason}, so {what}")
}

/// Every Add/Del line of hunk `hi`.
pub fn hunk_lines(fd: &FileDiff, hi: usize) -> HashSet<(usize, usize)> {
    let n = fd.hunks.get(hi).map_or(0, |h| h.lines.len());
    (0..n)
        .map(|li| (hi, li))
        .filter(|&p| selectable(fd, p))
        .collect()
}

/// Ask to discard the `lines` of an unstaged `fd`; None when there is no patch to apply.
pub fn ask_discard(fd: &FileDiff, lines: &HashSet<(usize, usize)>) -> Option<Action> {
    let patch = discard_patch(fd, lines)?;
    Some(Action::AskDiscard(Discard::Lines {
        path: fd.path.clone(),
        patch,
        n: lines.len(),
    }))
}

/// A working-copy diff of one side of a file: unstaged (index→worktree) or staged.
pub struct WorkDiff {
    pub staged: bool,
    /// The file's change on that side, from status.
    pub change: Change,
    pub fd: FileDiff,
    pub sel: Selection,
}

impl WorkDiff {
    fn dir(&self) -> Direction {
        if self.staged {
            Direction::Unstage
        } else {
            Direction::Stage
        }
    }
}

const ROW_H: f32 = 18.0;

enum Row {
    Hunk(usize),
    Line(usize, usize),
    Pair(usize, Option<usize>, Option<usize>),
}

fn rows(fd: &FileDiff, inline: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    for (hi, h) in fd.hunks.iter().enumerate() {
        rows.push(Row::Hunk(hi));
        if inline {
            rows.extend((0..h.lines.len()).map(|li| Row::Line(hi, li)));
        } else {
            rows.extend(
                side_by_side(h)
                    .into_iter()
                    .map(|(l, r)| Row::Pair(hi, l, r)),
            );
        }
    }
    rows
}

pub fn show(ui: &mut egui::Ui, center: &mut Center, inline: bool, actions: &mut Vec<Action>) {
    if !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseDiff);
    }
    let origin = match center {
        Center::Review(_, origin) => *origin,
        _ => "committed",
    };
    let (fd, work) = match center {
        Center::Diff(w) => {
            let w = &mut **w;
            (&w.fd, Some((w.staged, &w.change, w.dir(), &mut w.sel)))
        }
        Center::CommitFile(fd) | Center::Review(fd, _) => (&**fd, None),
        Center::Graph | Center::Conflict(_) | Center::Merge(_) => return,
    };

    let blocked = work
        .as_ref()
        .and_then(|(_, change, ..)| line_actions_blocked(change, fd));
    let lines_ok = work.is_some() && blocked.is_none();
    ui.horizontal(|ui| {
        ui.label(RichText::new(&fd.path).strong().color(Color32::WHITE));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let close =
                egui::Button::new(RichText::new("×").size(16.0).color(theme::MUTED)).frame(false);
            if ui.add(close).on_hover_text("Close (Esc)").clicked() {
                actions.push(Action::CloseDiff);
            }
            if segmented(ui, inline) {
                actions.push(Action::ToggleInline);
            }
        });
    });
    ui.horizontal(|ui| {
        let Some((staged, change, dir, sel)) = &work else {
            ui.weak(origin);
            return;
        };
        ui.weak(if *staged { "staged" } else { "unstaged" });
        if let Some(t) = change.type_label() {
            ui.weak(t);
        }
        let file = FileStatus {
            path: fd.path.clone(),
            change: (*change).clone(),
        };
        let label = if *staged {
            "Unstage file"
        } else {
            "Stage file"
        };
        if ui.button(label).clicked() {
            actions.push(Action::StageFile(file, *staged));
        }
        let n = sel.lines.len();
        if lines_ok && n > 0 {
            let verb = if *staged { "Unstage" } else { "Stage" };
            let noun = if n == 1 { "line" } else { "lines" };
            if ui.button(format!("{verb} {n} {noun}")).clicked() {
                if let Some(p) = build_patch(fd, &sel.lines, *dir) {
                    actions.push(Action::ApplyPatch(p, *dir));
                }
            }
            if !*staged && ui.button(format!("Discard {n} {noun}")).clicked() {
                actions.extend(ask_discard(fd, &sel.lines));
            }
        }
        if let Some(reason) = blocked {
            ui.label(RichText::new(line_hint(reason, *staged)).color(theme::MUTED));
        }
    });
    ui.add_space(2.0);
    ui.separator();
    if fd.binary {
        ui.weak("Binary file.");
        return;
    }
    if fd.hunks.is_empty() {
        ui.weak("No textual changes.");
        return;
    }
    let rows = rows(fd, inline);
    let hunk_stage = work.as_ref().filter(|_| lines_ok).map(|w| (w.0, w.2));
    let mut sel = work.and_then(|w| lines_ok.then_some(w.3));
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let char_w = ui
        .painter()
        .layout_no_wrap("0".into(), font.clone(), Color32::WHITE)
        .size()
        .x;
    let gutter = char_w * 6.0;
    let shift = ui.input(|i| i.modifiers.shift);
    let text_len = |hi: usize, li: Option<usize>| {
        li.map_or(0, |li| {
            let t = &fd.hunks[hi].lines[li].text;
            t.chars().count() + 3 * t.matches('\t').count()
        })
    };
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, rows.len(), |ui, range| {
            let longest = rows[range.clone()]
                .iter()
                .map(|r| match *r {
                    Row::Hunk(hi) => fd.hunks[hi].header.chars().count() + 30,
                    Row::Line(hi, li) => text_len(hi, Some(li)),
                    Row::Pair(hi, l, r) => text_len(hi, l).max(text_len(hi, r)),
                })
                .max()
                .unwrap_or(0);
            let text_w = char_w * (longest + 3) as f32;
            let avail = ui.available_width();
            let (width, half) = if inline {
                ((2.0 * gutter + text_w).max(avail), 0.0)
            } else {
                let half = (gutter + text_w).max(avail / 2.0);
                (2.0 * half, half)
            };
            for row in &rows[range] {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::hover());
                let mut cell = Cell {
                    ui,
                    fd,
                    sel: sel.as_deref_mut(),
                    font: &font,
                    gutter,
                    shift,
                };
                match *row {
                    Row::Hunk(hi) => hunk_header(cell.ui, rect, fd, hi, hunk_stage, &font, actions),
                    Row::Line(hi, li) => {
                        let (g, t) = split(rect, 2.0 * gutter);
                        cell.line(g, t, hi, li, true, true);
                    }
                    Row::Pair(hi, l, r) => {
                        let (left, right) = split(rect, half);
                        for (side, idx, old) in [(left, l, true), (right, r, false)] {
                            let (g, t) = split(side, gutter);
                            match idx {
                                Some(li) => cell.line(g, t, hi, li, old, false),
                                None => {
                                    cell.ui.painter().rect_filled(side, 0.0, theme::PANEL);
                                }
                            }
                        }
                    }
                }
            }
        });
}

/// "Side by side | Inline" segmented control; true when the other mode was clicked.
fn segmented(ui: &mut egui::Ui, inline: bool) -> bool {
    let mut toggle = false;
    egui::Frame::new()
        .fill(theme::FIELD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::WIDGET_R)
        .inner_margin(2)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            // Right to left: Inline is added first so it lands on the right.
            for (label, mode) in [("Inline", true), ("Side by side", false)] {
                let b = egui::Button::selectable(inline == mode, RichText::new(label).size(11.5))
                    .small();
                toggle |= ui.add(b).clicked() && inline != mode;
            }
        });
    toggle
}

fn split(r: Rect, at: f32) -> (Rect, Rect) {
    let x = r.left() + at;
    (
        Rect::from_min_max(r.min, Pos2::new(x, r.bottom())),
        Rect::from_min_max(Pos2::new(x, r.top()), r.max),
    )
}

/// Tabs drawn as four spaces.
fn display(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\t') {
        text.replace('\t', "    ").into()
    } else {
        text.into()
    }
}

fn hunk_header(
    ui: &mut egui::Ui,
    rect: Rect,
    fd: &FileDiff,
    hi: usize,
    stage: Option<(bool, Direction)>,
    font: &FontId,
    actions: &mut Vec<Action>,
) {
    ui.painter().rect_filled(rect, 0.0, theme::PANEL);
    let mut x = rect.left() + 4.0;
    if let Some((staged, dir)) = stage {
        let label = if staged { "Unstage hunk" } else { "Stage hunk" };
        let b = Rect::from_min_size(Pos2::new(x, rect.top() + 1.0), Vec2::new(90.0, ROW_H - 2.0));
        let button = egui::Button::new(RichText::new(label).size(11.0)).small();
        if ui.put(b, button).clicked() {
            if let Some(p) = build_patch(fd, &hunk_lines(fd, hi), dir) {
                actions.push(Action::ApplyPatch(p, dir));
            }
        }
        x = b.right() + 8.0;
        if !staged {
            let b = Rect::from_min_size(Pos2::new(x - 4.0, b.top()), b.size());
            let button = egui::Button::new(RichText::new("Discard hunk").size(11.0)).small();
            if ui.put(b, button).clicked() {
                actions.extend(ask_discard(fd, &hunk_lines(fd, hi)));
            }
            x = b.right() + 8.0;
        }
    }
    ui.painter().text(
        Pos2::new(x, rect.center().y),
        Align2::LEFT_CENTER,
        &fd.hunks[hi].header,
        font.clone(),
        theme::MUTED,
    );
}

/// What one diff line needs to paint itself and handle gutter clicks.
struct Cell<'a> {
    ui: &'a mut egui::Ui,
    fd: &'a FileDiff,
    /// Present only when line staging is allowed for this file.
    sel: Option<&'a mut Selection>,
    font: &'a FontId,
    gutter: f32,
    shift: bool,
}

impl Cell<'_> {
    /// Paint line `li` of hunk `hi` with its gutter in `g` and text in `t`. Inline rows
    /// show both line numbers and a +/- prefix; a side-by-side half shows one number.
    fn line(&mut self, g: Rect, t: Rect, hi: usize, li: usize, old: bool, inline: bool) {
        let l = &self.fd.hunks[hi].lines[li];
        let at = (hi, li);
        let selected = self.sel.as_ref().is_some_and(|s| s.lines.contains(&at));
        let bg = match (l.kind, selected) {
            (LineKind::Add, false) => Some(theme::DIFF_ADD),
            (LineKind::Add, true) => Some(theme::DIFF_ADD_SEL),
            (LineKind::Del, false) => Some(theme::DIFF_DEL),
            (LineKind::Del, true) => Some(theme::DIFF_DEL_SEL),
            (LineKind::Context, _) => None,
        };
        let row = g.union(t);
        let painter = self
            .ui
            .painter()
            .with_clip_rect(row.intersect(self.ui.clip_rect()));
        if let Some(bg) = bg {
            painter.rect_filled(row, 0.0, bg);
        }
        if selected {
            let edge = Rect::from_min_size(row.min, Vec2::new(3.0, row.height()));
            painter.rect_filled(edge, 0.0, theme::ACCENT);
        }
        let weak = theme::MUTED;
        let no = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
        let nums = if inline {
            vec![
                (no(l.old_no), g.left() + self.gutter),
                (no(l.new_no), g.right()),
            ]
        } else if old {
            vec![(no(l.old_no), g.right())]
        } else {
            vec![(no(l.new_no), g.right())]
        };
        for (n, x) in nums {
            painter.text(
                Pos2::new(x - 6.0, g.center().y),
                Align2::RIGHT_CENTER,
                n,
                self.font.clone(),
                weak,
            );
        }
        let text = display(&l.text);
        let text = if inline {
            let p = match l.kind {
                LineKind::Add => '+',
                LineKind::Del => '-',
                LineKind::Context => ' ',
            };
            format!("{p}{text}")
        } else {
            text.into_owned()
        };
        painter.text(
            Pos2::new(t.left() + 4.0, t.center().y),
            Align2::LEFT_CENTER,
            text,
            self.font.clone(),
            theme::TEXT,
        );
        let Some(sel) = self.sel.as_deref_mut() else {
            return;
        };
        if !selectable(self.fd, at) {
            return;
        }
        let id = self.ui.scope_id().with(("gutter", hi, li));
        let resp = self
            .ui
            .interact(g, id, Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            sel.click(self.fd, at, self.shift);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{file_diff, Hunk, Line};
    use crate::git;
    use crate::stage::{self, Direction};
    use std::path::Path;

    fn line(kind: LineKind) -> Line {
        Line {
            kind,
            old_no: None,
            new_no: None,
            text: String::new(),
            no_newline: false,
        }
    }

    fn hunk(kinds: &[LineKind]) -> Hunk {
        Hunk {
            old_start: 1,
            old_len: 0,
            new_start: 1,
            new_len: 0,
            header: String::new(),
            lines: kinds.iter().map(|&k| line(k)).collect(),
        }
    }

    use LineKind::{Add, Context as Ctx, Del};

    /// Hunk 0: ctx del del add ctx; hunk 1: ctx add ctx.
    fn fd() -> FileDiff {
        FileDiff {
            path: "f".into(),
            binary: false,
            lossy: false,
            hunks: vec![hunk(&[Ctx, Del, Del, Add, Ctx]), hunk(&[Ctx, Add, Ctx])],
        }
    }

    fn set(v: &[(usize, usize)]) -> HashSet<(usize, usize)> {
        v.iter().copied().collect()
    }

    #[test]
    fn click_toggles_change_lines_only() {
        let fd = fd();
        let mut s = Selection::default();
        s.click(&fd, (0, 0), false);
        assert!(s.lines.is_empty(), "context line ignored");
        s.click(&fd, (0, 1), false);
        s.click(&fd, (0, 3), false);
        assert_eq!(s.lines, set(&[(0, 1), (0, 3)]));
        s.click(&fd, (0, 1), false);
        assert_eq!(s.lines, set(&[(0, 3)]));
        s.click(&fd, (9, 9), false);
        assert_eq!(s.lines, set(&[(0, 3)]), "out of range ignored");
    }

    #[test]
    fn shift_click_selects_range_within_hunk() {
        let fd = fd();
        let mut s = Selection::default();
        s.click(&fd, (0, 3), false);
        s.click(&fd, (0, 1), true);
        assert_eq!(s.lines, set(&[(0, 1), (0, 2), (0, 3)]));
        // Range skips context lines.
        let mut s = Selection::default();
        s.click(&fd, (0, 1), false);
        s.click(&fd, (0, 4), true); // context target: ignored
        assert_eq!(s.lines, set(&[(0, 1)]));
    }

    #[test]
    fn clicking_another_hunk_starts_over() {
        let fd = fd();
        let mut s = Selection::default();
        s.click(&fd, (0, 1), false);
        s.click(&fd, (0, 2), false);
        s.click(&fd, (1, 1), true); // shift with anchor in another hunk: plain toggle
        assert_eq!(s.lines, set(&[(1, 1)]));
        s.click(&fd, (0, 3), false);
        assert_eq!(s.lines, set(&[(0, 3)]));
    }

    #[test]
    fn shift_without_anchor_toggles() {
        let fd = fd();
        let mut s = Selection::default();
        s.click(&fd, (0, 2), true);
        assert_eq!(s.lines, set(&[(0, 2)]));
    }

    #[test]
    fn selectable_and_hunk_lines() {
        let fd = fd();
        assert!(!selectable(&fd, (0, 0)));
        assert!(selectable(&fd, (0, 1)) && selectable(&fd, (1, 1)));
        assert!(!selectable(&fd, (2, 0)) && !selectable(&fd, (0, 9)));
        assert_eq!(hunk_lines(&fd, 0), set(&[(0, 1), (0, 2), (0, 3)]));
        assert_eq!(hunk_lines(&fd, 1), set(&[(1, 1)]));
        assert!(hunk_lines(&fd, 5).is_empty());
    }

    #[test]
    fn line_actions_blocked_gives_a_reason_per_case() {
        use crate::status::{Conflict, Kind, Side};
        let fd = fd();
        let why = |c: Change, fd: &FileDiff| line_actions_blocked(&c, fd);
        assert_eq!(why(Change::Modified, &fd), None);
        let cases = [
            (Change::Added, "This file is new"),
            (Change::Untracked, "This file is untracked"),
            (Change::Deleted, "This file was deleted"),
            (
                Change::Renamed { from: "x".into() },
                "This file was renamed",
            ),
            (
                Change::TypeChanged {
                    from: Kind::File,
                    to: Kind::Symlink,
                },
                "This file changed type",
            ),
            (
                Change::Conflicted(Conflict {
                    ours: Side::Modified,
                    theirs: Side::Modified,
                }),
                "This file has conflicts",
            ),
        ];
        for (c, want) in cases {
            assert_eq!(why(c.clone(), &fd), Some(want), "{c:?}");
        }
        let bin = FileDiff {
            binary: true,
            hunks: vec![],
            ..fd.clone()
        };
        assert_eq!(why(Change::Modified, &bin), Some("This is a binary file"));
        // Binary wins over the kind of change: it is the reason that would remain.
        assert_eq!(why(Change::Added, &bin), Some("This is a binary file"));
        let lossy = FileDiff {
            lossy: true,
            ..fd.clone()
        };
        assert_eq!(
            why(Change::Modified, &lossy),
            Some("This file isn't UTF-8 text")
        );
    }

    #[test]
    fn line_hint_names_the_whole_file_action_per_side() {
        assert_eq!(
            line_hint("This file isn't UTF-8 text", false),
            "This file isn't UTF-8 text, so stage or discard the whole file"
        );
        assert_eq!(
            line_hint("This file is new", true),
            "This file is new, so unstage the whole file"
        );
    }

    fn g(d: &Path, args: &[&str]) -> String {
        git::run(d, args).unwrap()
    }

    /// Drives the same calls as the diff view: select one line, build the patch, apply it.
    #[test]
    fn stage_one_selected_line_of_two_hunks() {
        let base: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let work = base
            .replacen("2\n", "TWO\nnew\n", 1)
            .replace("18\n", "EIGHTEEN\n");
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        g(d, &["init", "-q"]);
        std::fs::write(d.join("g.txt"), &base).unwrap();
        g(d, &["add", "."]);
        g(
            d,
            &[
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        std::fs::write(d.join("g.txt"), &work).unwrap();

        let st = crate::status::status(d).unwrap();
        assert_eq!(st.unstaged[0].change, Change::Modified);
        let fd = file_diff(d, "g.txt", false).unwrap();
        assert_eq!(fd.hunks.len(), 2);
        assert_eq!(line_actions_blocked(&st.unstaged[0].change, &fd), None);
        let li = fd.hunks[0]
            .lines
            .iter()
            .position(|l| l.kind == Add && l.text == "new")
            .unwrap();
        let mut sel = Selection::default();
        sel.click(&fd, (0, li), false);
        let patch = stage::build_patch(&fd, &sel.lines, Direction::Stage).unwrap();
        stage::apply(d, &patch, Direction::Stage).unwrap();

        assert_eq!(
            g(d, &["show", ":g.txt"]),
            base.replacen("2\n", "2\nnew\n", 1)
        );
        let cached = g(d, &["diff", "--cached"]);
        let changes: Vec<&str> = cached
            .lines()
            .filter(|l| {
                (l.starts_with('+') && !l.starts_with("+++"))
                    || (l.starts_with('-') && !l.starts_with("---"))
            })
            .collect();
        assert_eq!(changes, ["+new"], "{cached}");
    }
}
