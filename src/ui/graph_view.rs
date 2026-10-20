use super::{branch_drop, theme, Action};
use crate::actions::Operation;
use crate::graph::Row;
use crate::project::Layout;
use crate::status::{Change, Status};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use std::collections::HashSet;

/// Lane colours cycle through this many hues.
pub const HUES: usize = theme::LANES.len();

const ROW_H: f32 = 26.0;
const LANE_W: f32 = 16.0;
const DOT_R: f32 = 5.0;
/// Right-hand columns: author, then relative time.
const AUTHOR_W: f32 = 140.0;
const TIME_W: f32 = 70.0;
/// Fetch the next page once the view is this close to the end.
const PREFETCH: usize = 100;
/// Text on a lane-coloured pill.
const ON_PILL: Color32 = Color32::from_rgb(0x10, 0x14, 0x1c);

pub struct GraphOutput {
    pub clicked: Option<usize>,
    pub wip_clicked: bool,
    pub near_end: bool,
    /// A branch label dropped onto another label.
    pub drops: Vec<Action>,
}

/// Branch names the labels are matched against.
#[derive(Clone, Copy)]
pub struct Names<'a> {
    /// Remote-tracking branches, drawn neutral.
    pub remotes: &'a HashSet<&'a str>,
    /// Every branch, local or remote: the labels that can be dragged and dropped on.
    pub branches: &'a HashSet<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PillKind {
    /// The checked-out branch (or a detached HEAD).
    Head,
    /// Local branch or tag: lane colour.
    Local,
    /// Remote-tracking branch: neutral.
    Remote,
}

#[derive(Debug, PartialEq)]
pub struct Pill<'a> {
    pub text: &'a str,
    pub kind: PillKind,
}

/// Split git's `%D` decoration into pills. `remotes` holds the remote-tracking branch names.
pub fn parse_refs<'a>(refs: &'a str, remotes: &HashSet<&str>) -> Vec<Pill<'a>> {
    refs.split(", ")
        .filter(|r| !r.is_empty())
        .map(|r| {
            if let Some(b) = r.strip_prefix("HEAD -> ") {
                Pill {
                    text: b,
                    kind: PillKind::Head,
                }
            } else if r == "HEAD" {
                Pill {
                    text: r,
                    kind: PillKind::Head,
                }
            } else if remotes.contains(r) || r.ends_with("/HEAD") {
                Pill {
                    text: r,
                    kind: PillKind::Remote,
                }
            } else {
                let text = r.strip_prefix("tag: ").unwrap_or(r);
                Pill {
                    text,
                    kind: PillKind::Local,
                }
            }
        })
        .collect()
}

/// What the commit menu needs to know about the tab.
pub struct MenuContext {
    pub layout: Layout,
    /// Why entries that move HEAD are disabled, when they are (see `clean_blocked`).
    pub dirty: Option<String>,
}

/// Why checking out another commit must wait: uncommitted changes to tracked files, or an
/// operation in progress. Untracked files are left to git, as `actions::ensure_clean` does.
pub fn clean_blocked(status: &Status, op: Option<Operation>) -> Option<String> {
    if let Some(op) = op {
        return Some(format!("Finish or abort the {op} first"));
    }
    let tracked = |f: &crate::status::FileStatus| f.change != Change::Untracked;
    let dirty = !status.staged.is_empty()
        || !status.conflicted.is_empty()
        || status.unstaged.iter().any(tracked);
    dirty.then(|| "Commit or stash your changes first".to_string())
}

/// One entry of the commit menu: its label, the action, and why it is disabled (if it is).
#[derive(Debug, PartialEq)]
pub struct MenuItem {
    pub label: &'static str,
    pub action: MenuAction,
    pub blocked: Option<String>,
}

/// What a commit menu entry does, before it is bound to a commit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuAction {
    CopySha,
    CopyMessage,
    Checkout,
    Branch,
    Worktree,
}

impl MenuAction {
    fn action(self, sha: &str) -> Action {
        let sha = sha.to_string();
        match self {
            MenuAction::CopySha => Action::CopyText(sha),
            MenuAction::CopyMessage => Action::CopyCommitMessage(sha),
            MenuAction::Checkout => Action::CheckoutCommit(sha),
            MenuAction::Branch => Action::BranchAt(sha),
            MenuAction::Worktree => Action::NewWorktreeFrom(sha),
        }
    }
}

/// The commit menu's entries in groups (separated in the menu), with enabled states.
pub fn menu_items(ctx: &MenuContext) -> Vec<Vec<MenuItem>> {
    let item = |label, action, blocked: Option<String>| MenuItem {
        label,
        action,
        blocked,
    };
    let worktrees = ctx.layout == Layout::WorktreeRoot;
    // A normal clone's new branch is switched to, moving HEAD like a checkout.
    let branch_blocked = if worktrees { None } else { ctx.dirty.clone() };
    let worktree_blocked = (!worktrees).then(|| {
        "This project is a normal clone: right-click its tab and choose Convert to worktree \
         layout"
            .to_string()
    });
    vec![
        vec![
            item("Copy SHA", MenuAction::CopySha, None),
            item("Copy message", MenuAction::CopyMessage, None),
        ],
        vec![item(
            "Checkout this commit",
            MenuAction::Checkout,
            ctx.dirty.clone(),
        )],
        vec![
            item("Create branch here…", MenuAction::Branch, branch_blocked),
            item("New worktree here…", MenuAction::Worktree, worktree_blocked),
        ],
    ]
}

fn commit_menu(ui: &mut egui::Ui, row: &Row, ctx: &MenuContext, actions: &mut Vec<Action>) {
    for (i, group) in menu_items(ctx).into_iter().enumerate() {
        if i > 0 {
            ui.separator();
        }
        for item in group {
            let r = ui
                .add_enabled(item.blocked.is_none(), egui::Button::new(item.label))
                .on_disabled_hover_text(item.blocked.unwrap_or_default());
            if r.clicked() {
                actions.push(item.action.action(&row.commit.sha));
                ui.close();
            }
        }
    }
}

fn color(lane: usize) -> Color32 {
    theme::LANES[lane_color_index(lane)]
}

/// The history list. With `wip` (changed file count) a "// WIP" row comes first and is
/// selected while no commit is. Right-clicking a commit opens its menu.
pub fn show(
    ui: &mut egui::Ui,
    rows: &[Row],
    selected: Option<usize>,
    wip: Option<usize>,
    names: Names,
    menu: &MenuContext,
    actions: &mut Vec<Action>,
) -> GraphOutput {
    let mut out = GraphOutput {
        clicked: None,
        wip_clicked: false,
        near_end: false,
        drops: vec![],
    };
    if rows.is_empty() && wip.is_none() {
        ui.weak("No commits yet.");
        return out;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let skip = usize::from(wip.is_some());
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, rows.len() + skip, |ui, range| {
            out.near_end = range.end + PREFETCH >= rows.len() + skip;
            let commits = range.start.saturating_sub(skip)..range.end.saturating_sub(skip);
            let lanes = rows[commits]
                .iter()
                .map(|r| r.lanes_in.len().max(r.lanes_out.len()).max(r.lane + 1))
                .max()
                .unwrap_or(1);
            let text_x = LANE_W * lanes as f32 + 8.0;
            for k in range {
                let width = ui.available_width().max(text_x + 400.0 + AUTHOR_W + TIME_W);
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                let (is_sel, row) = match k.checked_sub(skip) {
                    None => (selected.is_none(), None),
                    Some(i) => (selected == Some(i), Some(i)),
                };
                let bg = if is_sel {
                    theme::SEL_BG
                } else if resp.hovered() {
                    theme::HOVER
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 4.0, bg);
                match row {
                    None => {
                        out.wip_clicked |= resp.clicked();
                        paint_wip(ui, rect, wip.unwrap_or(0));
                    }
                    Some(i) => {
                        if resp.clicked() {
                            out.clicked = Some(i);
                        }
                        let pills = paint_row(ui, rect, &rows[i], text_x, now, names);
                        // Branch labels drag onto each other; a plain click still selects.
                        for (j, (r, name)) in pills.into_iter().enumerate() {
                            let label = ui.interact(r, resp.id.with(j), Sense::click_and_drag());
                            if label.clicked() {
                                out.clicked = Some(i);
                            }
                            branch_drop::drag_and_drop(ui, &label, name, &mut out.drops);
                        }
                        resp.context_menu(|ui| commit_menu(ui, &rows[i], menu, actions));
                    }
                }
            }
        });
    out
}

fn lane_x(rect: Rect, lane: usize) -> f32 {
    rect.left() + LANE_W * lane as f32 + LANE_W / 2.0
}

/// Pill with `text` at `x`, vertically centred on `mid`; returns its right edge.
pub(super) fn pill(
    painter: &egui::Painter,
    x: f32,
    mid: f32,
    text: &str,
    fill: Color32,
    fg: Color32,
    bold: bool,
) -> f32 {
    let font = FontId::proportional(11.0);
    let galley = painter.layout_no_wrap(text.to_string(), font, fg);
    let size = galley.size();
    let r = Rect::from_min_size(
        Pos2::new(x, mid - size.y / 2.0 - 1.5),
        size + Vec2::new(14.0, 3.0),
    );
    painter.rect_filled(r, 9.0, fill);
    let at = r.min + Vec2::new(7.0, 1.5);
    painter.galley(at, galley.clone(), fg);
    if bold {
        // Faux bold: the bundled proportional font has a single weight.
        painter.galley(at + Vec2::new(0.6, 0.0), galley, fg);
    }
    r.right() + 6.0
}

fn paint_wip(ui: &egui::Ui, rect: Rect, files: usize) {
    let painter = ui.painter();
    let mid = rect.center().y;
    let c = Pos2::new(lane_x(rect, 0), mid);
    let ring: Vec<Pos2> = (0..=24)
        .map(|k| {
            let a = k as f32 / 24.0 * std::f32::consts::TAU;
            c + DOT_R * Vec2::angled(a)
        })
        .collect();
    painter.extend(Shape::dashed_line(
        &ring,
        Stroke::new(1.5, theme::ACCENT_TEXT),
        2.0,
        2.0,
    ));
    let x = rect.left() + LANE_W + 8.0;
    let x = pill(painter, x, mid, "// WIP", theme::BORDER, theme::TEXT, false);
    let noun = if files == 1 { "file" } else { "files" };
    painter.text(
        Pos2::new(x, mid),
        Align2::LEFT_CENTER,
        format!("± {files} {noun}"),
        FontId::proportional(13.0),
        theme::MUTED,
    );
}

/// Paint one commit; returns the visible branch labels and their rects.
fn paint_row<'a>(
    ui: &egui::Ui,
    rect: Rect,
    row: &'a Row,
    text_x: f32,
    now: i64,
    names: Names,
) -> Vec<(Rect, &'a str)> {
    let painter = ui.painter();
    let x = |lane: usize| lane_x(rect, lane);
    let (top, mid, bottom) = (rect.top(), rect.center().y, rect.bottom());
    let stroke = |lane: usize| Stroke::new(2.0, color(lane));
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
    painter.circle(
        Pos2::new(x(row.lane), mid),
        DOT_R,
        color(row.lane),
        Stroke::new(1.5, theme::BG),
    );

    let time_right = rect.right() - 8.0;
    let author_left = time_right - TIME_W - AUTHOR_W;
    let main = painter.with_clip_rect(
        Rect::from_min_max(rect.min, Pos2::new(author_left - 12.0, bottom))
            .intersect(ui.clip_rect()),
    );
    let mut cx = rect.left() + text_x;
    let mut labels = vec![];
    for p in parse_refs(&row.commit.refs, names.remotes) {
        let (fill, fg) = match p.kind {
            PillKind::Remote => (theme::BORDER, theme::TEXT),
            _ => (color(row.lane), ON_PILL),
        };
        let left = cx;
        cx = pill(&main, cx, mid, p.text, fill, fg, p.kind == PillKind::Head);
        let r = Rect::from_min_max(Pos2::new(left, top), Pos2::new(cx - 6.0, bottom));
        let r = r.intersect(main.clip_rect());
        // Tags and a detached HEAD aren't branches.
        if names.branches.contains(p.text) && r.is_positive() {
            labels.push((r, p.text));
        }
    }
    main.text(
        Pos2::new(cx, mid),
        Align2::LEFT_CENTER,
        &row.commit.subject,
        FontId::proportional(13.0),
        theme::TEXT,
    );
    let small = FontId::proportional(12.0);
    painter
        .with_clip_rect(
            Rect::from_min_max(
                Pos2::new(author_left, top),
                Pos2::new(time_right - TIME_W, bottom),
            )
            .intersect(ui.clip_rect()),
        )
        .text(
            Pos2::new(author_left, mid),
            Align2::LEFT_CENTER,
            &row.commit.author,
            small.clone(),
            theme::MUTED,
        );
    painter.text(
        Pos2::new(time_right, mid),
        Align2::RIGHT_CENTER,
        relative_time(now, row.commit.time),
        small,
        theme::MUTED,
    );
    labels
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
    fn refs_parse_into_pills() {
        let remotes: HashSet<&str> = ["origin/main"].into_iter().collect();
        let pills = parse_refs(
            "HEAD -> main, origin/main, origin/HEAD, tag: v1, feat/x",
            &remotes,
        );
        let got: Vec<_> = pills.iter().map(|p| (p.text, p.kind)).collect();
        use PillKind::*;
        assert_eq!(
            got,
            [
                ("main", Head),
                ("origin/main", Remote),
                ("origin/HEAD", Remote),
                ("v1", Local),
                ("feat/x", Local),
            ]
        );
        assert_eq!(parse_refs("HEAD", &remotes)[0].kind, Head, "detached");
        assert!(parse_refs("", &remotes).is_empty());
    }

    fn fs(path: &str, change: Change) -> crate::status::FileStatus {
        crate::status::FileStatus {
            path: path.into(),
            change,
        }
    }

    #[test]
    fn untracked_files_alone_do_not_block_checkout() {
        let mut st = Status::default();
        assert_eq!(clean_blocked(&st, None), None);
        st.unstaged.push(fs("new.txt", Change::Untracked));
        assert_eq!(clean_blocked(&st, None), None);
        st.unstaged.push(fs("a.txt", Change::Modified));
        let why = "Commit or stash your changes first";
        assert_eq!(clean_blocked(&st, None).as_deref(), Some(why));
        let staged = Status {
            staged: vec![fs("b.txt", Change::Added)],
            ..Status::default()
        };
        assert_eq!(clean_blocked(&staged, None).as_deref(), Some(why));
        let op = clean_blocked(&Status::default(), Some(Operation::Rebase));
        assert_eq!(op.as_deref(), Some("Finish or abort the rebase first"));
    }

    fn enabled(ctx: &MenuContext) -> Vec<(&'static str, bool)> {
        menu_items(ctx)
            .into_iter()
            .flatten()
            .map(|i| (i.label, i.blocked.is_none()))
            .collect()
    }

    #[test]
    fn commit_menu_disables_what_needs_a_clean_worktree() {
        let clean = MenuContext {
            layout: Layout::WorktreeRoot,
            dirty: None,
        };
        assert!(enabled(&clean).iter().all(|(_, on)| *on), "all enabled");
        let dirty = MenuContext {
            layout: Layout::WorktreeRoot,
            dirty: Some("Commit or stash your changes first".into()),
        };
        assert_eq!(
            enabled(&dirty),
            [
                ("Copy SHA", true),
                ("Copy message", true),
                ("Checkout this commit", false),
                ("Create branch here…", true),
                ("New worktree here…", true),
            ]
        );
        let normal = MenuContext {
            layout: Layout::NormalClone,
            ..dirty
        };
        let got = enabled(&normal);
        assert_eq!(got[3], ("Create branch here…", false), "it switches");
        assert_eq!(got[4], ("New worktree here…", false));
        let checkout = &menu_items(&normal)[1][0];
        assert_eq!(
            checkout.blocked.as_deref(),
            Some("Commit or stash your changes first")
        );
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
