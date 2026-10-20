//! The Worktrees view, toggled at the top of the sidebar: the sidebar lists every worktree of
//! the project (Spinney's, agents', any tool's) with the agents running in each and how far it
//! is from the default branch, and under it the selected one's commits since it branched. The
//! right panel lists all its changes or a picked commit's files, and the first file opens in
//! the centre, as in Git view.

use super::sidebar::{remove_blocked, worktree_hover, worktree_label};
use super::toolbar::folder_name;
use super::{theme, Action, Center, ProjectTab};
use crate::agents;
use crate::diff::FileDiff;
use crate::project::Worktree;
use crate::review::{self, Change, Commit};
use eframe::egui::{self, Align, RichText, Sense, TextWrapMode};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What the centre shows: the commit graph or the Worktrees view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Git,
    Worktrees,
}

/// A worktree's place against the default branch.
#[derive(Clone, Debug, PartialEq)]
pub struct Standing {
    pub path: PathBuf,
    /// What it's compared against (`origin/main`, `@{upstream}`, `HEAD`).
    pub base: String,
    /// Commits ahead of and behind `base`; None for a missing worktree or when git fails.
    pub counts: Option<(usize, usize)>,
}

#[derive(Default)]
pub struct Review {
    pub standings: Vec<Standing>,
    /// Agents running in each worktree, by path.
    pub agents: HashMap<PathBuf, Vec<&'static str>>,
    /// The worktree whose changes are listed.
    pub selected: Option<PathBuf>,
    pub changes: Vec<Change>,
    /// Why `changes` couldn't be read, if it couldn't.
    pub error: Option<String>,
    /// The selected worktree's commits since it branched, newest first.
    pub commits: Vec<Commit>,
    /// The commit picked in the graph (an index into `commits`); None shows all changes.
    pub commit: Option<usize>,
    /// The picked commit's files.
    pub commit_files: Vec<FileDiff>,
}

impl Review {
    /// Recompute every worktree's standing and agents, and the selection's changes if it's
    /// still listed.
    pub fn load(&mut self, root: &Path, worktrees: &[Worktree]) {
        let default = review::default_branch(root);
        self.standings = worktrees
            .iter()
            .map(|w| {
                let base = review::base_for(&w.path, w.branch.as_deref(), default.as_deref());
                let counts = here(w)
                    .then(|| review::ahead_behind(&w.path, &base).ok())
                    .flatten();
                Standing {
                    path: w.path.clone(),
                    base,
                    counts,
                }
            })
            .collect();
        let present: Vec<&Path> = worktrees
            .iter()
            .filter(|w| here(w))
            .map(|w| w.path.as_path())
            .collect();
        self.agents = agents::running(&present);
        match self.selected.clone() {
            Some(p) if self.standing(&p).is_some() => self.select(p),
            _ => self.clear_selection(),
        }
    }

    fn standing(&self, path: &Path) -> Option<&Standing> {
        self.standings.iter().find(|s| s.path == path)
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.changes.clear();
        self.error = None;
        self.commits.clear();
        self.clear_commit();
    }

    fn clear_commit(&mut self) {
        self.commit = None;
        self.commit_files.clear();
    }

    /// Show commit `i`'s files, or all changes since the branch point for None.
    pub fn pick_commit(&mut self, i: Option<usize>) -> anyhow::Result<()> {
        self.clear_commit();
        let (Some(i), Some(wt)) = (i, self.selected.as_deref()) else {
            return Ok(());
        };
        let Some(c) = self.commits.get(i) else {
            return Ok(());
        };
        self.commit_files = crate::diff::commit_diff(wt, &c.sha)?;
        self.commit = Some(i);
        Ok(())
    }

    /// List what `path` changed since it branched.
    pub fn select(&mut self, path: PathBuf) {
        let Some(base) = self.standing(&path).map(|s| s.base.clone()) else {
            return self.clear_selection();
        };
        // A refresh of the same worktree keeps the picked commit if it's still there.
        let picked = (self.selected.as_ref() == Some(&path))
            .then(|| self.commit.and_then(|i| self.commits.get(i)))
            .flatten()
            .map(|c| c.sha.clone());
        self.changes.clear();
        self.error = None;
        self.commits.clear();
        self.clear_commit();
        if !path.is_dir() {
            self.error =
                Some("Its folder is missing. Right-click it to Locate… or Remove it.".into());
        } else {
            match review::changes_since(&path, &base) {
                Ok(c) => self.changes = c,
                Err(e) => self.error = Some(format!("{e:#}")),
            }
            self.commits = review::commits_since(&path, &base).unwrap_or_default();
        }
        self.selected = Some(path);
        let again = picked.and_then(|sha| self.commits.iter().position(|c| c.sha == sha));
        if again.is_some() {
            let _ = self.pick_commit(again);
        }
    }

    /// The paths the right panel lists: the picked commit's files, or all changes.
    pub fn listed(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self.commit {
            Some(_) => Box::new(self.commit_files.iter().map(|f| f.path.as_str())),
            None => Box::new(self.changes.iter().map(|c| c.path.as_str())),
        }
    }

    /// The selected worktree's base, for a file's diff.
    pub fn base(&self) -> Option<(&Path, &str)> {
        let p = self.selected.as_deref()?;
        Some((p, self.standing(p)?.base.as_str()))
    }
}

fn here(w: &Worktree) -> bool {
    !w.prunable && w.path.is_dir()
}

/// "↑2 ↓1 vs origin/main", "up to date with origin/main", or None when it couldn't be
/// compared.
pub fn standing_text(s: &Standing) -> Option<String> {
    let (ahead, behind) = s.counts?;
    let base = match s.base.as_str() {
        "@{upstream}" => "its upstream",
        "HEAD" => return Some("uncommitted changes only".into()),
        b => b,
    };
    if (ahead, behind) == (0, 0) {
        return Some(format!("up to date with {base}"));
    }
    let mut parts = vec![];
    if ahead > 0 {
        parts.push(format!("↑{ahead}"));
    }
    if behind > 0 {
        parts.push(format!("↓{behind}"));
    }
    Some(format!("{} vs {base}", parts.join(" ")))
}

/// A row's second line: the folder, the tag (missing, locked, agent) and the standing.
pub fn row_detail(wt: &Worktree, standing: Option<&Standing>) -> String {
    let (_, tag) = worktree_label(wt);
    let mut parts = vec![folder_name(&wt.path)];
    if !tag.is_empty() {
        parts.push(tag);
    }
    match standing.and_then(standing_text) {
        Some(s) => parts.push(s),
        None if wt.prunable => parts.push("moved or deleted".into()),
        None => {}
    }
    parts.join(" · ")
}

/// "+12 −3", or "binary".
pub fn lines_text(c: &Change) -> String {
    match c.lines {
        Some((a, r)) => format!("+{a} −{r}"),
        None => "binary".into(),
    }
}

/// The diff header for a reviewed file: where its change lives, and which worktree.
pub fn review_label(origin: &str, worktree: &str) -> String {
    format!("{origin} · in {worktree}")
}

/// A two-line selectable row: the name, with the agents running there on the right; the
/// detail under it. The name and the agents stay whole, the detail is what gets cut.
fn worktree_row(
    ui: &mut egui::Ui,
    on: bool,
    name: &str,
    detail: &str,
    running: &str,
) -> egui::Response {
    two_line_row(ui, on, 0.0, name, detail, running).0
}

/// The row behind `worktree_row` and the commit graph's rows, with `indent` left free (for
/// the graph's line and dot). Returns the row's rect too.
fn two_line_row(
    ui: &mut egui::Ui,
    on: bool,
    indent: f32,
    name: &str,
    detail: &str,
    running: &str,
) -> (egui::Response, egui::Rect) {
    let width = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(egui::vec2(width, 38.0), Sense::click());
    let fill = if on {
        Some(theme::SEL_STRONG)
    } else if r.hovered() {
        Some(theme::HOVER)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, theme::WIDGET_R, fill);
    }
    let mut inner = rect.shrink2(egui::vec2(8.0, 4.0));
    inner.min.x += indent;
    let galley = |text: RichText, max: f32| {
        egui::WidgetText::from(text).into_galley(
            ui,
            Some(TextWrapMode::Truncate),
            max.max(0.0),
            egui::TextStyle::Body,
        )
    };
    let agent = (!running.is_empty()).then(|| {
        galley(
            RichText::new(format!("● {running}"))
                .size(12.0)
                .color(theme::ACCENT_TEXT),
            inner.width() / 2.0,
        )
    });
    let agent_w = agent.as_ref().map_or(0.0, |g| g.size().x + 10.0);
    let name = galley(
        RichText::new(name).color(theme::TEXT).strong(),
        inner.width() - agent_w,
    );
    let detail = galley(
        RichText::new(detail).size(11.5).color(theme::MUTED),
        inner.width(),
    );
    let p = ui.painter();
    p.galley(inner.left_top(), name, theme::TEXT);
    if let Some(g) = agent {
        p.galley(
            egui::pos2(inner.right() - g.size().x, inner.top() + 1.0),
            g,
            theme::TEXT,
        );
    }
    p.galley(
        egui::pos2(inner.left(), inner.bottom() - detail.size().y),
        detail,
        theme::MUTED,
    );
    (r, rect)
}

/// A commit's second line: "a1b2c3d · Ada · 2 hours ago".
pub fn commit_detail(c: &Commit) -> String {
    format!(
        "{} · {} · {}",
        &c.sha[..c.sha.len().min(7)],
        c.author,
        c.when
    )
}

/// The Commits section's last line, under its commits. A `HEAD` base (detached, or a branch
/// with nothing to compare with) has no commits, so it says so rather than "branched from HEAD".
pub fn base_text(base: &str) -> String {
    match base {
        "HEAD" => "No commits: comparing with HEAD, uncommitted changes only".into(),
        "@{upstream}" => "since its upstream".into(),
        b => format!("branched from {b}"),
    }
}

/// The "All changes" row's detail: "3 files since it branched, uncommitted too".
pub fn all_changes_text(files: usize, base: &str) -> String {
    let what = match files {
        1 => "1 file".to_string(),
        k => format!("{k} files"),
    };
    match base {
        "HEAD" => format!("{what}, uncommitted"),
        "@{upstream}" => format!("{what} since its upstream, uncommitted too"),
        _ => format!("{what} since it branched, uncommitted too"),
    }
}

/// The Commits section: one line of dots, "All changes" on top, then each commit, newest
/// first, then the commit it branched from.
fn commit_graph(ui: &mut egui::Ui, review: &Review, base: &str, actions: &mut Vec<Action>) {
    const LANE: f32 = 18.0;
    let n = review.commits.len();
    let more = if n >= review::MAX_COMMITS { "+" } else { "" };
    ui.label(theme::section(&format!("Commits · {n}{more}")));
    ui.add_space(2.0);
    // Draw the line through a row's lane: from its centre up, down, or both.
    let lane = |ui: &egui::Ui, rect: egui::Rect, up: bool, down: bool, hollow: bool| {
        let c = egui::pos2(rect.left() + 8.0 + LANE / 2.0, rect.center().y);
        let stroke = egui::Stroke::new(2.0, theme::ACCENT);
        let p = ui.painter();
        if up {
            p.line_segment([egui::pos2(c.x, rect.top()), c], stroke);
        }
        if down {
            p.line_segment([c, egui::pos2(c.x, rect.bottom())], stroke);
        }
        if hollow {
            p.circle(c, 4.5, theme::BG, stroke);
        } else {
            p.circle_filled(c, 4.5, theme::ACCENT);
        }
    };
    egui::ScrollArea::vertical()
        .id_salt("worktree_commits")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let (r, rect) = two_line_row(
                ui,
                review.commit.is_none(),
                LANE,
                "All changes",
                &all_changes_text(review.changes.len(), base),
                "",
            );
            lane(ui, rect, false, true, true);
            if r.clicked() {
                actions.push(Action::ReviewCommit(None));
            }
            for (i, c) in review.commits.iter().enumerate() {
                let on = review.commit == Some(i);
                let (r, rect) = two_line_row(ui, on, LANE, &c.subject, &commit_detail(c), "");
                lane(ui, rect, true, true, false);
                let r = r.on_hover_text(format!("{}\n{}", c.subject, commit_detail(c)));
                if r.clicked() {
                    actions.push(Action::ReviewCommit(Some(i)));
                }
            }
            // Where it branched: the base, muted and not selectable.
            let width = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::hover());
            let c = egui::pos2(rect.left() + 8.0 + LANE / 2.0, rect.center().y);
            let p = ui.painter();
            p.line_segment(
                [egui::pos2(c.x, rect.top()), c],
                egui::Stroke::new(2.0, theme::ACCENT),
            );
            p.circle_filled(c, 3.5, theme::MUTED);
            p.text(
                egui::pos2(rect.left() + 8.0 + LANE + 8.0, c.y),
                egui::Align2::LEFT_CENTER,
                base_text(base),
                egui::FontId::proportional(11.5),
                theme::MUTED,
            );
        });
}

/// Git | Worktrees, across the top of the sidebar. Returns the view clicked, when it isn't
/// the current one.
pub fn toggle(ui: &mut egui::Ui, current: View) -> Option<View> {
    let mut picked = None;
    egui::Frame::new()
        .fill(theme::FIELD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::WIDGET_R)
        .inner_margin(2)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let w = (ui.available_width() - 2.0) / 2.0;
            ui.horizontal(|ui| {
                for (label, view) in [("Git", View::Git), ("Worktrees", View::Worktrees)] {
                    let b =
                        egui::Button::selectable(current == view, RichText::new(label).size(12.5))
                            .min_size(egui::vec2(w, 24.0));
                    if ui.add(b).clicked() && current != view {
                        picked = Some(view);
                    }
                }
            });
        });
    picked
}

/// The sidebar in Worktrees view: every worktree (that the filter lets through), selectable.
pub fn list(
    ui: &mut egui::Ui,
    worktrees: &[&Worktree],
    review: &Review,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.label(theme::section(&format!("Worktrees · {}", worktrees.len())))
            .on_hover_text(
                "Agents are found by the folder their process runs in. One working here from a \
                 session started elsewhere isn't shown.",
            );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let b = ui
                .small_button("↻")
                .on_hover_text("Reload worktrees, their standing and the agents running in them");
            if b.clicked() {
                actions.push(Action::RefreshWorktrees);
            }
        });
    });
    ui.add_space(4.0);
    // With a worktree picked, the list gives up the lower part to its commits.
    let base = review
        .selected
        .as_deref()
        .filter(|_| review.error.is_none())
        .and_then(|p| review.standing(p))
        .map(|s| s.base.clone());
    let height = match base {
        Some(_) => (ui.available_height() * 0.4).max(80.0),
        None => f32::INFINITY,
    };
    egui::ScrollArea::vertical()
        .id_salt("worktree_list")
        .max_height(height)
        .auto_shrink([false, base.is_some()])
        .show(ui, |ui| {
            for wt in worktrees {
                let (name, _) = worktree_label(wt);
                let detail = row_detail(wt, review.standing(&wt.path));
                let running = review
                    .agents
                    .get(&wt.path)
                    .map(|a| agents::agents_text(a))
                    .unwrap_or_default();
                let on = review.selected.as_deref() == Some(&wt.path);
                let r = worktree_row(ui, on, &name, &detail, &running).on_hover_ui(|ui| {
                    if !running.is_empty() {
                        ui.label(
                            RichText::new(format!("Running here: {running}"))
                                .color(theme::ACCENT_TEXT),
                        );
                    }
                    ui.label(&detail);
                    for line in worktree_hover(wt) {
                        ui.label(line);
                    }
                });
                if r.clicked() {
                    actions.push(Action::ReviewWorktree(wt.path.clone()));
                }
                r.context_menu(|ui| {
                    if !wt.prunable && ui.button("Open in Git view").clicked() {
                        actions.push(Action::OpenInGit(wt.path.clone()));
                        ui.close();
                    }
                    if wt.prunable && ui.button("Locate…").clicked() {
                        actions.push(Action::LocateWorktree(wt.path.clone()));
                        ui.close();
                    }
                    let blocked = remove_blocked(wt);
                    let remove = ui.add_enabled(blocked.is_none(), egui::Button::new("Remove"));
                    if remove
                        .on_disabled_hover_text(blocked.unwrap_or_default())
                        .clicked()
                    {
                        actions.push(Action::RemoveWorktree(wt.path.clone()));
                        ui.close();
                    }
                });
            }
        });
    if let Some(base) = base {
        ui.add_space(8.0);
        ui.separator();
        commit_graph(ui, review, &base, actions);
    }
}

/// The centre in Worktrees view when no file is open.
pub fn center_hint(ui: &mut egui::Ui, worktrees: &[Worktree], review: &Review) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        let Some(selected) = &review.selected else {
            ui.weak("Select a worktree on the left to see what it changed.");
            return;
        };
        let name = worktrees
            .iter()
            .find(|w| &w.path == selected)
            .map(|w| worktree_label(w).0)
            .unwrap_or_default();
        match (&review.error, review.changes.is_empty()) {
            (Some(_), _) => ui.weak(format!("{name} can't be shown.")),
            (None, true) => ui.weak(format!("{name} has no changes since it branched.")),
            (None, false) => ui.weak("Pick a file on the right to see its diff."),
        };
    });
}

/// The right-hand panel: what the selected worktree changed since it branched.
pub fn files(ui: &mut egui::Ui, tab: &ProjectTab, actions: &mut Vec<Action>) {
    let review = &tab.review;
    let Some(selected) = &review.selected else {
        ui.label(RichText::new("No worktree selected").strong());
        ui.add_space(4.0);
        ui.weak("Select a worktree to see everything it changed since it branched.");
        return;
    };
    let wt = tab.worktrees.iter().find(|w| &w.path == selected);
    let name = wt.map(|w| worktree_label(w).0).unwrap_or_default();
    // The button first, so a long name is cut to the space left of it, not drawn under it.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if wt.is_some_and(here) && ui.small_button("Open in Git view").clicked() {
                actions.push(Action::OpenInGit(selected.clone()));
            }
            ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(&name).strong()).truncate())
                    .on_hover_text(&name);
            });
        });
    });
    if let Some(base) = review.standing(selected).map(|s| s.base.as_str()) {
        let since = match base {
            "HEAD" => "Uncommitted changes".to_string(),
            "@{upstream}" => "Since its upstream".to_string(),
            b => format!("Since it branched from {b}"),
        };
        ui.weak(since);
    }
    ui.add_space(6.0);
    if let Some(e) = &review.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
        return;
    }
    if let Some(c) = review.commit.and_then(|i| review.commits.get(i)) {
        return commit_files(ui, tab, c, actions);
    }
    ui.label(theme::section(&format!("Files · {}", review.changes.len())));
    if review.changes.is_empty() {
        ui.weak("No changes.");
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, c) in review.changes.iter().enumerate() {
                let open = matches!(&tab.center, Center::Review(fd, _) if fd.path == c.path);
                let right = format!("{} · {}", c.origin.label(), lines_text(c));
                let row = egui::Button::selectable(open, RichText::new(&c.path).size(12.5))
                    .right_text(RichText::new(right).size(11.5).color(theme::MUTED))
                    .truncate()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if ui.add(row).on_hover_text(&c.path).clicked() {
                    actions.push(Action::ReviewFile(i));
                }
            }
        });
}

/// The right panel with a commit picked: its message and files.
fn commit_files(ui: &mut egui::Ui, tab: &ProjectTab, c: &Commit, actions: &mut Vec<Action>) {
    let review = &tab.review;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Commit {}", &c.sha[..c.sha.len().min(8)])).strong());
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("All changes").clicked() {
                actions.push(Action::ReviewCommit(None));
            }
        });
    });
    ui.label(RichText::new(&c.subject).color(theme::TEXT));
    ui.weak(format!("{} · {}", c.author, c.when));
    ui.add_space(6.0);
    ui.label(theme::section(&format!(
        "Files · {}",
        review.commit_files.len()
    )));
    if review.commit_files.is_empty() {
        ui.weak("No file changes.");
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, f) in review.commit_files.iter().enumerate() {
                let open = matches!(&tab.center, Center::Review(fd, _) if fd.path == f.path);
                let right = match review::line_counts(f) {
                    Some((a, r)) => format!("+{a} −{r}"),
                    None => "binary".into(),
                };
                let row = egui::Button::selectable(open, RichText::new(&f.path).size(12.5))
                    .right_text(RichText::new(right).size(11.5).color(theme::MUTED))
                    .truncate()
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if ui.add(row).on_hover_text(&f.path).clicked() {
                    actions.push(Action::ReviewFile(i));
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Origin;

    fn standing(base: &str, counts: Option<(usize, usize)>) -> Standing {
        Standing {
            path: PathBuf::from("/p/wt"),
            base: base.into(),
            counts,
        }
    }

    fn wt(path: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            head: "83174b7aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            branch: branch.map(str::to_string),
            is_bare: false,
            main: false,
            locked: false,
            prunable: false,
        }
    }

    #[test]
    fn standing_reads_naturally() {
        let t = |b, c| standing_text(&standing(b, c));
        let t = |b, c| t(b, c).unwrap_or_default();
        assert_eq!(t("origin/main", Some((2, 1))), "↑2 ↓1 vs origin/main");
        assert_eq!(t("origin/main", Some((3, 0))), "↑3 vs origin/main");
        assert_eq!(
            t("origin/main", Some((0, 0))),
            "up to date with origin/main"
        );
        assert_eq!(t("@{upstream}", Some((0, 4))), "↓4 vs its upstream");
        assert_eq!(t("HEAD", Some((0, 0))), "uncommitted changes only");
        assert_eq!(t("origin/main", None), "");
        assert_eq!(
            t("HEAD", None),
            "",
            "a missing detached worktree has no standing"
        );
    }

    #[test]
    fn row_detail_names_the_folder_and_says_missing_once() {
        let agent = wt("/p/proj/.claude/worktrees/agent-1", Some("feat/x"));
        let s = standing("origin/main", Some((2, 0)));
        assert_eq!(
            row_detail(&agent, Some(&s)),
            "agent-1 · agent · ↑2 vs origin/main"
        );
        let gone = Worktree {
            prunable: true,
            ..wt("/p/proj.worktrees/old", Some("old"))
        };
        assert_eq!(
            row_detail(&gone, Some(&standing("origin/main", None))),
            "old · missing · moved or deleted"
        );
        let main = wt("/p/proj", Some("main"));
        assert_eq!(row_detail(&main, None), "proj");
    }

    #[test]
    fn commit_detail_has_short_sha_author_and_age() {
        let c = Commit {
            sha: "83174b7aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            subject: "s".into(),
            author: "Ada".into(),
            when: "2 hours ago".into(),
        };
        assert_eq!(commit_detail(&c), "83174b7 · Ada · 2 hours ago");
    }

    #[test]
    fn review_header_names_the_worktree() {
        assert_eq!(
            review_label("committed", "agent-1"),
            "committed · in agent-1"
        );
    }

    #[test]
    fn commits_section_names_the_base_sensibly() {
        assert_eq!(base_text("main"), "branched from main");
        assert_eq!(base_text("@{upstream}"), "since its upstream");
        assert_eq!(
            base_text("HEAD"),
            "No commits: comparing with HEAD, uncommitted changes only"
        );
        assert_eq!(
            all_changes_text(3, "main"),
            "3 files since it branched, uncommitted too"
        );
        assert_eq!(all_changes_text(1, "HEAD"), "1 file, uncommitted");
    }

    #[test]
    fn line_counts_or_binary() {
        let c = |lines| Change {
            path: "a".into(),
            origin: Origin::Committed,
            lines,
        };
        assert_eq!(lines_text(&c(Some((12, 3)))), "+12 −3");
        assert_eq!(lines_text(&c(None)), "binary");
    }
}
