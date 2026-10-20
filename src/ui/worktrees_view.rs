//! The Worktrees view: every worktree of the project (Spinney's, agents', any tool's), the
//! agents running in each, and how far each is from the default branch. The selected one's
//! changes since it branched fill the right-hand panel, as the working copy's do in Git view.

use super::sidebar::{remove_blocked, worktree_hover, worktree_label};
use super::toolbar::folder_name;
use super::{theme, Action, Center, ProjectTab};
use crate::agents;
use crate::project::Worktree;
use crate::review::{self, Change};
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
    }

    /// List what `path` changed since it branched.
    pub fn select(&mut self, path: PathBuf) {
        let Some(base) = self.standing(&path).map(|s| s.base.clone()) else {
            return self.clear_selection();
        };
        self.changes.clear();
        self.error = None;
        if !path.is_dir() {
            self.error =
                Some("Its folder is missing. Right-click it to Locate… or Remove it.".into());
        } else {
            match review::changes_since(&path, &base) {
                Ok(c) => self.changes = c,
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
        self.selected = Some(path);
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
    let width = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(egui::vec2(width, 40.0), Sense::click());
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
    let inner = rect.shrink2(egui::vec2(8.0, 4.0));
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
    r
}

/// The centre: every worktree, selectable.
pub fn show(ui: &mut egui::Ui, worktrees: &[Worktree], review: &Review, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(theme::section(&format!("Worktrees · {}", worktrees.len())));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let b = ui
                .small_button("Refresh")
                .on_hover_text("Reload worktrees, their standing and the agents running in them");
            if b.clicked() {
                actions.push(Action::RefreshWorktrees);
            }
        });
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            theme::card().show(ui, |ui| {
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
            ui.add_space(6.0);
            ui.weak(
                "Agents are found by the folder their process runs in. One working here from a \
                 session started elsewhere isn't shown.",
            );
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
    ui.horizontal(|ui| {
        ui.label(RichText::new(&name).strong());
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if wt.is_some_and(here) && ui.small_button("Open in Git view").clicked() {
                actions.push(Action::OpenInGit(selected.clone()));
            }
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
    fn review_header_names_the_worktree() {
        assert_eq!(
            review_label("committed", "agent-1"),
            "committed · in agent-1"
        );
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
