//! The Worktrees view: every worktree of the project (Spinney's, agents', any tool's), how
//! far each is from the default branch, and what the selected one changed since it branched.

use super::sidebar::{remove_blocked, worktree_hover, worktree_label};
use super::{theme, Action};
use crate::project::Worktree;
use crate::review::{self, Change};
use eframe::egui::{self, RichText};
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
    /// The worktree whose changes are listed.
    pub selected: Option<PathBuf>,
    pub changes: Vec<Change>,
    /// Why `changes` couldn't be read, if it couldn't.
    pub error: Option<String>,
}

impl Review {
    /// Recompute every worktree's standing, and the selection's changes if it's still there.
    pub fn load(&mut self, root: &Path, worktrees: &[Worktree]) {
        let default = review::default_branch(root);
        self.standings = worktrees
            .iter()
            .map(|w| {
                let base = review::base_for(&w.path, w.branch.as_deref(), default.as_deref());
                let here = !w.prunable && w.path.is_dir();
                let counts = here
                    .then(|| review::ahead_behind(&w.path, &base).ok())
                    .flatten();
                Standing {
                    path: w.path.clone(),
                    base,
                    counts,
                }
            })
            .collect();
        match self.selected.clone() {
            Some(p) if self.standing(&p).is_some_and(|s| s.counts.is_some()) => self.select(p),
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
        match review::changes_since(&path, &base) {
            Ok(c) => {
                self.changes = c;
                self.error = None;
            }
            Err(e) => {
                self.changes.clear();
                self.error = Some(format!("{e:#}"));
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

/// "↑2 ↓1 vs origin/main", "up to date with origin/main", or why there's nothing.
pub fn standing_text(s: &Standing) -> String {
    let base = match s.base.as_str() {
        "@{upstream}" => "its upstream",
        "HEAD" => return "uncommitted changes only".into(),
        b => b,
    };
    match s.counts {
        None => "missing".into(),
        Some((0, 0)) => format!("up to date with {base}"),
        Some((a, b)) => {
            let mut parts = vec![];
            if a > 0 {
                parts.push(format!("↑{a}"));
            }
            if b > 0 {
                parts.push(format!("↓{b}"));
            }
            format!("{} vs {base}", parts.join(" "))
        }
    }
}

/// "+12 −3", or "binary".
pub fn lines_text(c: &Change) -> String {
    match c.lines {
        Some((a, r)) => format!("+{a} −{r}"),
        None => "binary".into(),
    }
}

pub fn show(ui: &mut egui::Ui, worktrees: &[Worktree], review: &Review, actions: &mut Vec<Action>) {
    ui.label(theme::section("Worktrees"));
    theme::card().show(ui, |ui| {
        for wt in worktrees {
            let (text, tag) = worktree_label(wt);
            let standing = review.standing(&wt.path);
            let right = standing.map(standing_text).unwrap_or_default();
            let right = match tag.is_empty() {
                true => right,
                false => format!("{tag} · {right}"),
            };
            let on = review.selected.as_deref() == Some(&wt.path);
            let b = egui::Button::selectable(on, text)
                .right_text(RichText::new(right).color(theme::MUTED))
                .truncate()
                .min_size(egui::vec2(ui.available_width(), 0.0));
            let r = ui.add(b).on_hover_ui(|ui| {
                for line in worktree_hover(wt) {
                    ui.label(line);
                }
            });
            let usable = standing.is_some_and(|s| s.counts.is_some());
            if r.clicked() && usable {
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
    ui.add_space(10.0);
    let Some(selected) = &review.selected else {
        ui.weak("Select a worktree to see what it changed since it branched.");
        return;
    };
    let name = worktrees
        .iter()
        .find(|w| &w.path == selected)
        .map(|w| worktree_label(w).0)
        .unwrap_or_default();
    ui.label(theme::section(&format!("Changes in {name}")));
    if let Some(e) = &review.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
        return;
    }
    if review.changes.is_empty() {
        ui.weak("No changes since it branched");
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (i, c) in review.changes.iter().enumerate() {
            let right = format!("{} · {}", c.origin.label(), lines_text(c));
            let b = egui::Button::selectable(false, c.path.as_str())
                .right_text(RichText::new(right).color(theme::MUTED))
                .truncate()
                .min_size(egui::vec2(ui.available_width(), 0.0));
            if ui.add(b).clicked() {
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

    #[test]
    fn standing_reads_naturally() {
        let t = |b, c| standing_text(&standing(b, c));
        assert_eq!(t("origin/main", Some((2, 1))), "↑2 ↓1 vs origin/main");
        assert_eq!(t("origin/main", Some((3, 0))), "↑3 vs origin/main");
        assert_eq!(
            t("origin/main", Some((0, 0))),
            "up to date with origin/main"
        );
        assert_eq!(t("@{upstream}", Some((0, 4))), "↓4 vs its upstream");
        assert_eq!(t("HEAD", Some((0, 0))), "uncommitted changes only");
        assert_eq!(t("origin/main", None), "missing");
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
