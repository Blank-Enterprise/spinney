use super::{theme, toolbar::folder_name, Action, ProjectTab};
use crate::project::{Layout, Worktree};
use crate::refs::{Branch, Tag};
use eframe::egui::{self, RichText};
use std::path::Path;

/// Branches grouped into folders by their `/` segments: `feat/login` sits in `feat`.
#[derive(Debug, PartialEq)]
pub enum Node<'a> {
    Folder {
        name: String,
        /// Segments up to and including this folder (`feat/ui`): a stable id.
        path: String,
        children: Vec<Node<'a>>,
    },
    Branch(&'a Branch),
}

/// Folders first (by name), then branches in their listed order, at every level.
pub fn branch_tree<'a>(branches: &[&'a Branch]) -> Vec<Node<'a>> {
    let mut root = vec![];
    for &b in branches {
        let mut level = &mut root;
        let mut path = String::new();
        let segments: Vec<&str> = b.name.split('/').collect();
        for seg in &segments[..segments.len() - 1] {
            if !path.is_empty() {
                path.push('/');
            }
            path += seg;
            let i = match level
                .iter()
                .position(|n| matches!(n, Node::Folder { name, .. } if name == seg))
            {
                Some(i) => i,
                None => {
                    level.push(Node::Folder {
                        name: seg.to_string(),
                        path: path.clone(),
                        children: vec![],
                    });
                    level.len() - 1
                }
            };
            let Node::Folder { children, .. } = &mut level[i] else {
                unreachable!("position matched a folder")
            };
            level = children;
        }
        level.push(Node::Branch(b));
    }
    sort(&mut root);
    root
}

fn sort(nodes: &mut [Node]) {
    // Stable: branches keep their order; folders sort by name ahead of them.
    nodes.sort_by(|a, b| match (a, b) {
        (Node::Folder { name: x, .. }, Node::Folder { name: y, .. }) => x.cmp(y),
        (Node::Folder { .. }, Node::Branch(_)) => std::cmp::Ordering::Less,
        (Node::Branch(_), Node::Folder { .. }) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    for n in nodes {
        if let Node::Folder { children, .. } = n {
            sort(children);
        }
    }
}

/// Branches under these nodes.
fn count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| match n {
            Node::Branch(_) => 1,
            Node::Folder { children, .. } => count(children),
        })
        .sum()
}

/// What a branch's menu needs to know about the project.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    layout: Layout,
    /// The checked-out branch; None when HEAD is detached.
    current: Option<&'a str>,
    /// Branches checked out in any of the project's worktrees.
    checked_out: &'a [&'a str],
}

/// Draw `nodes`: folders start open and collapse; a filter opens them all so every match shows.
fn tree(
    ui: &mut egui::Ui,
    id: &str,
    nodes: &[Node],
    filtering: bool,
    cx: Ctx,
    actions: &mut Vec<Action>,
) {
    for n in nodes {
        match n {
            Node::Folder {
                name,
                path,
                children,
            } => {
                let label = RichText::new(format!("📁 {name}")).color(theme::TEXT);
                let header = egui::CollapsingHeader::new(label)
                    .id_salt((id, path))
                    .default_open(true);
                let header = if filtering {
                    header.open(Some(true))
                } else {
                    header
                };
                let r = header.show(ui, |ui| tree(ui, id, children, filtering, cx, actions));
                r.header_response
                    .on_hover_text(format!("{path} · {}", count(children)));
            }
            Node::Branch(b) => branch_row(ui, b, cx, actions),
        }
    }
}

/// One branch: its last name segment, ✓ when checked out, ahead/behind on the right.
fn branch_row(ui: &mut egui::Ui, b: &Branch, cx: Ctx, actions: &mut Vec<Action>) {
    let leaf = b.name.rsplit('/').next().unwrap_or(&b.name);
    let mut track = String::new();
    if b.ahead > 0 {
        track += &format!("↑{}", b.ahead);
    }
    if b.behind > 0 {
        track += &format!(" ↓{}", b.behind);
    }
    let text = if b.head {
        format!("✓ {leaf}")
    } else {
        leaf.to_string()
    };
    let r = row(ui, b.head, text, track.trim().to_string())
        .interact(egui::Sense::drag())
        .on_hover_text(&b.name);
    super::branch_drop::drag_and_drop(ui, &r, &b.name, actions);
    branch_menu(&r, b, cx, actions);
}

/// Case-insensitive substring match of the sidebar filter on a branch or worktree name.
pub fn filter_matches(query: &str, name: &str) -> bool {
    name.to_lowercase().contains(&query.trim().to_lowercase())
}

/// Full-width sidebar row; `right` is muted, right-aligned text. Text that doesn't fit is
/// cut with "…": wrapping would stack a squeezed label one character per line.
fn row(ui: &mut egui::Ui, on: bool, text: String, right: String) -> egui::Response {
    let b = egui::Button::selectable(on, text)
        .right_text(RichText::new(right).color(theme::MUTED))
        .truncate()
        .min_size(egui::vec2(ui.available_width(), 0.0));
    ui.add(b)
}

/// The id of a Claude Code agent whose worktree this is (`.claude/worktrees/agent-<id>`).
pub fn agent_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let parent = path.parent()?;
    let agents = parent.file_name()? == "worktrees" && parent.parent()?.file_name()? == ".claude";
    (agents && name.starts_with("agent-")).then_some(name)
}

/// A worktree's row: its branch (what you know it as) or the detached commit, and a muted
/// "agent" tag for an agent's worktree. The folder goes in the hover.
pub fn worktree_label(wt: &Worktree) -> (String, String) {
    let text = match &wt.branch {
        Some(b) => b.clone(),
        None => format!("detached at {}", short_sha(&wt.head)),
    };
    let tag = if agent_id(&wt.path).is_some() {
        "agent"
    } else {
        ""
    };
    (text, tag.into())
}

/// Hover lines for a worktree: the agent (if any), branch, folder and full path.
pub fn worktree_hover(wt: &Worktree) -> Vec<String> {
    let mut lines = vec![];
    if let Some(id) = agent_id(&wt.path) {
        lines.push(format!("Claude Code agent worktree · {id}"));
    }
    lines.push(match &wt.branch {
        Some(b) => format!("Branch {b} at {}", short_sha(&wt.head)),
        None => format!("Detached at {}", short_sha(&wt.head)),
    });
    lines.push(format!("Folder {}", folder_name(&wt.path)));
    lines.push(wt.path.display().to_string());
    lines
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The sidebar's sections, top to bottom.
pub const SECTIONS: usize = 5;
const HEADER_H: f32 = 22.0;
/// The divider between two sections, dragged to resize them.
const HANDLE_H: f32 = 6.0;
/// The shortest an open section's body gets, however its neighbours are resized.
const MIN_BODY: f32 = 150.0;
/// The first split between open sections: Local gets the most.
const FIRST_SHARES: [f32; SECTIONS] = [3.0, 2.0, 1.0, 1.0, 1.0];

/// Which sidebar sections are open, and how tall each one's body is. A closed section
/// keeps its height, to come back at that size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sections {
    pub open: [bool; SECTIONS],
    /// 0 until first laid out.
    pub heights: [f32; SECTIONS],
}

impl Default for Sections {
    /// Branches open; worktrees, stashes and tags closed until asked for.
    fn default() -> Self {
        Sections {
            open: [true, true, false, false, false],
            heights: [0.0; SECTIONS],
        }
    }
}

impl Sections {
    fn open_list(&self) -> Vec<usize> {
        (0..SECTIONS).filter(|&k| self.open[k]).collect()
    }

    /// Fit the open bodies to `space`. The first time, split it between them; after that
    /// only the bottom open section takes up a change, and when the sidebar gets shorter
    /// the ones above it give way in turn down to `min`, so the top ones stay put. With
    /// no room for every minimum, they share it equally.
    pub fn fit(&mut self, space: f32, min: f32) {
        let open = self.open_list();
        let Some(&last) = open.last() else {
            return;
        };
        let h = &mut self.heights;
        let sum: f32 = open.iter().map(|&k| h[k]).sum();
        if sum <= 0.0 {
            let total: f32 = open.iter().map(|&k| FIRST_SHARES[k]).sum();
            for &k in &open {
                h[k] = space * FIRST_SHARES[k] / total;
            }
            return;
        }
        if space >= sum {
            h[last] += space - sum;
            return;
        }
        let mut want = sum - space;
        for &k in open.iter().rev() {
            let take = (h[k] - min).max(0.0).min(want);
            h[k] -= take;
            want -= take;
        }
        if want > 0.0 {
            for &k in &open {
                h[k] = space / open.len() as f32;
            }
        }
    }

    /// Open or close section `k`, moving as little else as possible. A closing section's
    /// height goes to the nearest open section below it (above, when none is below). An
    /// opening one takes its old height back from the open sections below it, then above,
    /// nearest first, none going under `min`. `fit` tidies up after.
    pub fn toggle(&mut self, k: usize, space: f32, min: f32) {
        let h = &mut self.heights;
        if self.open[k] {
            self.open[k] = false;
            let below = (k + 1..SECTIONS).find(|&o| self.open[o]);
            if let Some(to) = below.or_else(|| (0..k).rev().find(|&o| self.open[o])) {
                h[to] += h[k];
            }
        } else {
            let others: Vec<usize> = (k + 1..SECTIONS)
                .chain((0..k).rev())
                .filter(|&o| self.open[o])
                .collect();
            self.open[k] = true;
            let want = if h[k] > 0.0 {
                h[k]
            } else {
                space / (others.len() + 1) as f32
            }
            .max(min);
            let mut got = 0.0;
            for &o in &others {
                let take = (h[o] - min).max(0.0).min(want - got);
                h[o] -= take;
                got += take;
            }
            h[k] = if others.is_empty() {
                space
            } else {
                got.max(min)
            };
        }
        self.fit(space, min);
    }
}

/// Move the divider below section `i` by `delta` (down when positive). The nearest open
/// section on the side it moves away from grows; open sections on the side it moves into
/// shrink in turn, nearest first, none below `min`. It stops when they are all at `min`.
pub fn drag(h: &mut [f32], open: &[bool], i: usize, delta: f32, min: f32) {
    let above: Vec<usize> = (0..=i).rev().filter(|&k| open[k]).collect();
    let below: Vec<usize> = (i + 1..h.len()).filter(|&k| open[k]).collect();
    let (grow, shrink) = if delta > 0.0 {
        (above.first(), &below)
    } else {
        (below.first(), &above)
    };
    let Some(&grow) = grow else {
        return;
    };
    let mut want = delta.abs();
    let mut moved = 0.0;
    for &k in shrink {
        let take = (h[k] - min).max(0.0).min(want);
        h[k] -= take;
        want -= take;
        moved += take;
    }
    h[grow] += moved;
}

/// Draws the sections one after another, each body in its own scroll area, with a
/// divider between each pair.
struct Split {
    s: Sections,
    /// A header clicked this frame; it opens or closes after the frame is drawn.
    toggled: Option<usize>,
    /// Spacing for the bodies' contents (the split itself packs with none).
    spacing: egui::Vec2,
    min: f32,
}

impl Split {
    fn section(
        &mut self,
        ui: &mut egui::Ui,
        k: usize,
        title: &str,
        n: usize,
        add: impl FnOnce(&mut egui::Ui),
    ) {
        let w = ui.available_width();
        let (rect, r) = ui.allocate_exact_size(egui::vec2(w, HEADER_H), egui::Sense::click());
        if r.clicked() {
            self.toggled = Some(k);
        }
        if r.hovered() {
            ui.painter()
                .rect_filled(rect, theme::WIDGET_R, theme::HOVER);
        }
        let icon = egui::Rect::from_center_size(
            rect.left_center() + egui::vec2(9.0, 0.0),
            egui::vec2(8.0, 8.0),
        );
        let rotation = if self.s.open[k] {
            0.0
        } else {
            -std::f32::consts::FRAC_PI_2
        };
        ui.painter()
            .add(egui::Shape::rotated_triangle(icon, rotation, theme::MUTED));
        let text = egui::WidgetText::from(theme::section(&format!("{title} · {n}"))).into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            w - 20.0,
            egui::TextStyle::Body,
        );
        let pos = egui::pos2(rect.left() + 20.0, rect.center().y - text.size().y / 2.0);
        ui.painter().galley(pos, text, theme::MUTED);

        if self.s.open[k] {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(w, self.s.heights[k]), egui::Sense::hover());
            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("sidebar_section", k))
                    .max_rect(rect),
            );
            body.spacing_mut().item_spacing = self.spacing;
            egui::ScrollArea::vertical()
                .id_salt(("sidebar_section", title))
                .auto_shrink([false, false])
                .show(&mut body, add);
        }
        if k + 1 < SECTIONS {
            self.divider(ui, k);
        }
    }

    /// The divider below section `k`; it drags only with an open section on each side.
    fn divider(&mut self, ui: &mut egui::Ui, k: usize) {
        let w = ui.available_width();
        let (rect, r) = ui.allocate_exact_size(egui::vec2(w, HANDLE_H), egui::Sense::drag());
        let open = self.s.open;
        let live = open[..=k].contains(&true) && open[k + 1..].contains(&true);
        let hot = live && (r.hovered() || r.dragged());
        if hot {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if live && r.dragged() {
            drag(&mut self.s.heights, &open, k, r.drag_delta().y, self.min);
        }
        let color = if hot { theme::ACCENT } else { theme::BORDER };
        ui.painter().hline(
            rect.x_range(),
            rect.center().y,
            egui::Stroke::new(1.0, color),
        );
    }
}

pub fn show(ui: &mut egui::Ui, tab: &mut ProjectTab, actions: &mut Vec<Action>) {
    ui.add(
        egui::TextEdit::singleline(&mut tab.filter)
            .hint_text("Filter")
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
    let spacing = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing.y = 0.0;
    let fixed = SECTIONS as f32 * HEADER_H + (SECTIONS - 1) as f32 * HANDLE_H;
    let space = (ui.available_height() - fixed).max(0.0);
    let min = MIN_BODY;
    let mut s = tab.sections;
    s.fit(space, min);
    let mut split = Split {
        s,
        toggled: None,
        spacing,
        min,
    };
    {
        let tab = &*tab;
        let q = tab.filter.as_str();
        let local: Vec<_> = tab
            .branches
            .iter()
            .filter(|b| !b.remote && filter_matches(q, &b.name))
            .collect();
        let filtering = !q.trim().is_empty();
        // None when HEAD is detached.
        let current = tab
            .branches
            .iter()
            .find(|b| b.head && !b.remote)
            .map(|b| b.name.as_str());
        let checked_out: Vec<&str> = tab
            .worktrees
            .iter()
            .filter_map(|w| w.branch.as_deref())
            .collect();
        let cx = Ctx {
            layout: tab.layout,
            current,
            checked_out: &checked_out,
        };
        split.section(ui, 0, "Local", local.len(), |ui| {
            let nodes = branch_tree(&local);
            tree(ui, "local", &nodes, filtering, cx, actions);
        });
        let remote: Vec<_> = tab
            .branches
            .iter()
            .filter(|b| b.remote && filter_matches(q, &b.name))
            .collect();
        split.section(ui, 1, "Remote", remote.len(), |ui| {
            // `origin/feat/x`: the remote is the first folder level.
            let nodes = branch_tree(&remote);
            tree(ui, "remote", &nodes, filtering, cx, actions);
        });
        let worktrees: Vec<_> = tab
            .worktrees
            .iter()
            .filter(|w| {
                filter_matches(q, &folder_name(&w.path))
                    || w.branch.as_deref().is_some_and(|b| filter_matches(q, b))
            })
            .collect();
        split.section(ui, 2, "Worktrees", worktrees.len(), |ui| {
            for wt in worktrees {
                let (text, tag) = worktree_label(wt);
                let on = wt.path == tab.active_worktree;
                let r = row(ui, on, format!("▣ {text}"), tag).on_hover_ui(|ui| {
                    // The agent line leads, in the accent colour; the path is muted.
                    let lines = worktree_hover(wt);
                    let agent = agent_id(&wt.path).is_some();
                    for (i, line) in lines.iter().enumerate() {
                        let color = match i {
                            0 if agent => theme::ACCENT_TEXT,
                            _ if i + 1 == lines.len() => theme::MUTED,
                            _ => theme::TEXT,
                        };
                        ui.label(RichText::new(line).color(color));
                    }
                });
                if r.clicked() {
                    actions.push(Action::ActivateWorktree(wt.path.clone()));
                }
                r.context_menu(|ui| {
                    if ui.button("Remove").clicked() {
                        actions.push(Action::RemoveWorktree(wt.path.clone()));
                        ui.close();
                    }
                    let both = ui.add_enabled(
                        wt.branch.is_some(),
                        egui::Button::new("Remove worktree and delete branch…"),
                    );
                    if both.on_disabled_hover_text("Detached: no branch").clicked() {
                        actions.push(Action::AskRemoveWithBranch(wt.path.clone()));
                        ui.close();
                    }
                });
            }
        });
        split.section(ui, 3, "Stashes", tab.stashes.len(), |ui| {
            for st in &tab.stashes {
                let r = row(
                    ui,
                    false,
                    st.message.clone(),
                    st.branch.clone().unwrap_or_default(),
                );
                r.context_menu(|ui| {
                    if ui.button("Apply").clicked() {
                        actions.push(Action::ApplyStash(st.hash.clone()));
                        ui.close();
                    }
                    if ui.button("Pop").clicked() {
                        actions.push(Action::PopStash(st.hash.clone()));
                        ui.close();
                    }
                    if ui.button("Drop").clicked() {
                        let label = match &st.branch {
                            Some(b) => format!("\"{}\" (on {b})", st.message),
                            None => format!("\"{}\"", st.message),
                        };
                        actions.push(Action::RequestDropStash(st.hash.clone(), label));
                        ui.close();
                    }
                });
            }
        });
        let tags: Vec<_> = tab
            .tags
            .iter()
            .filter(|t| filter_matches(q, &t.name))
            .collect();
        split.section(ui, 4, "Tags", tags.len(), |ui| {
            for t in tags {
                tag_row(ui, t, actions);
            }
        });
    }
    if let Some(k) = split.toggled {
        split.s.toggle(k, space, min);
    }
    tab.sections = split.s;
}

/// "v1.0 · Annotated tag at 1a2b3c4", or "· Tag at …" for a lightweight one.
pub fn tag_hover(t: &Tag) -> String {
    let kind = if t.annotated { "Annotated tag" } else { "Tag" };
    format!("{} · {kind} at {}", t.name, short_sha(&t.target))
}

/// One tag: its full name (`release/1.0` stays whole, not foldered), the commit on the right.
fn tag_row(ui: &mut egui::Ui, t: &Tag, actions: &mut Vec<Action>) {
    let r = row(ui, false, t.name.clone(), short_sha(&t.target).into()).on_hover_text(tag_hover(t));
    r.context_menu(|ui| {
        // The peeled commit, so it goes through the detached-HEAD checks like a commit does.
        if ui.button("Checkout").clicked() {
            actions.push(Action::CheckoutCommit(t.target.clone()));
            ui.close();
        }
        if ui.button("Copy name").clicked() {
            actions.push(Action::CopyText(t.name.clone()));
            ui.close();
        }
        if ui.button("Push to origin").clicked() {
            actions.push(Action::PushTag(t.name.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button("Delete…").clicked() {
            actions.push(Action::AskDeleteTag(t.clone()));
            ui.close();
        }
    });
}

/// "Merge…" / "Rebase…" menu labels for `branch`; None for the checked-out branch itself.
/// With HEAD detached (`current` None) they still show, disabled.
pub fn merge_labels(current: Option<&str>, branch: &str) -> Option<(String, String)> {
    match current {
        Some(c) if c == branch => None,
        Some(c) => Some((
            format!("Merge {branch} into {c}…"),
            format!("Rebase {c} onto {branch}…"),
        )),
        None => Some((format!("Merge {branch}…"), format!("Rebase onto {branch}…"))),
    }
}

/// "Fast-forward to origin/x" for a local branch behind its upstream, and why it is
/// disabled (if it is): checked out somewhere, or diverged. None when not behind.
pub fn fast_forward_item(b: &Branch, checked_out: &[&str]) -> Option<(String, Option<String>)> {
    let up = b
        .upstream
        .as_deref()
        .filter(|_| !b.remote && b.behind > 0)?;
    let why = if checked_out.contains(&b.name.as_str()) {
        Some(format!("{} is checked out: pull it there instead", b.name))
    } else if b.ahead > 0 {
        Some(format!(
            "{} has diverged from {up}: check it out to merge or rebase",
            b.name
        ))
    } else {
        None
    };
    Some((format!("Fast-forward to {up}"), why))
}

fn branch_menu(r: &egui::Response, b: &Branch, cx: Ctx, actions: &mut Vec<Action>) {
    let Ctx {
        layout, current, ..
    } = cx;
    let name = b.name.as_str();
    r.context_menu(|ui| {
        if ui.button("Checkout").clicked() {
            actions.push(Action::Checkout(name.to_string()));
            ui.close();
        }
        // Worktrees live beside `.bare`; in a normal clone they would nest in the working tree.
        let r = ui
            .add_enabled(
                layout == Layout::WorktreeRoot,
                egui::Button::new("New worktree from here"),
            )
            .on_disabled_hover_text(
                "This project is a normal clone: right-click its tab and choose Convert to \
                 worktree layout",
            );
        if r.clicked() {
            actions.push(Action::NewWorktreeFrom(name.to_string()));
            ui.close();
        }
        // The full ref, so a tag of the same name isn't the one tagged.
        if ui.button("Create tag here…").clicked() {
            let prefix = if b.remote {
                "refs/remotes"
            } else {
                "refs/heads"
            };
            actions.push(Action::TagAt(format!("{prefix}/{name}")));
            ui.close();
        }
        if let Some((label, why)) = fast_forward_item(b, cx.checked_out) {
            let r = ui.add_enabled(why.is_none(), egui::Button::new(label));
            let r = match &why {
                Some(why) => r.on_disabled_hover_text(why),
                None => r,
            };
            if r.clicked() {
                actions.push(Action::FastForward(name.to_string()));
                ui.close();
            }
        }
        if let Some((merge, rebase)) = merge_labels(current, name) {
            ui.separator();
            let items = [
                (merge, Action::AskMerge(name.to_string())),
                (rebase, Action::AskRebase(name.to_string())),
            ];
            for (label, action) in items {
                let r = ui
                    .add_enabled(current.is_some(), egui::Button::new(label))
                    .on_disabled_hover_text("Check out a branch first");
                if r.clicked() {
                    actions.push(action);
                    ui.close();
                }
            }
        }
        ui.separator();
        if !b.remote && ui.button("Rename…").clicked() {
            actions.push(Action::AskRename(name.to_string()));
            ui.close();
        }
        let (label, action) = if b.remote {
            (
                "Delete remote branch…",
                Action::AskDeleteRemote(name.to_string()),
            )
        } else {
            ("Delete branch…", Action::AskDeleteBranch(name.to_string()))
        };
        if ui.button(label).clicked() {
            actions.push(action);
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt(path: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            head: "83174b7246c76c9b6426a95147a715f0d26bf6c6".into(),
            is_bare: false,
        }
    }

    #[test]
    fn agent_worktrees_are_recognised_by_path() {
        let id = |p: &str| agent_id(Path::new(p)).map(str::to_string);
        assert_eq!(
            id("/p/spinney/.claude/worktrees/agent-a152de48b1153d030").as_deref(),
            Some("agent-a152de48b1153d030")
        );
        assert_eq!(id("/p/spinney/feat-x"), None);
        assert_eq!(
            id("/p/spinney/.claude/worktrees/feat-x"),
            None,
            "not an agent"
        );
        assert_eq!(id("/p/worktrees/agent-1"), None, "not under .claude");
        assert_eq!(id("/"), None);
    }

    #[test]
    fn worktrees_are_labelled_by_branch() {
        let long = "feat/merge-rebase-from-branch-menu";
        let agent = wt(
            "/p/spinney/.claude/worktrees/agent-a152de48b1153d030",
            Some(long),
        );
        assert_eq!(worktree_label(&agent), (long.into(), "agent".into()));
        let main = wt("/p/spinney", Some("main"));
        assert_eq!(worktree_label(&main), ("main".into(), String::new()));
        let detached = wt("/p/spinney/hotfix", None);
        assert_eq!(
            worktree_label(&detached),
            ("detached at 83174b7".into(), String::new())
        );
    }

    #[test]
    fn worktree_hover_names_agent_folder_and_path() {
        let path = "/p/spinney/.claude/worktrees/agent-a152de48b1153d030";
        let hover = worktree_hover(&wt(path, Some("feat/x")));
        assert_eq!(
            hover,
            [
                "Claude Code agent worktree · agent-a152de48b1153d030",
                "Branch feat/x at 83174b7",
                "Folder agent-a152de48b1153d030",
                path,
            ]
        );
        let hover = worktree_hover(&wt("/p/spinney", None));
        assert_eq!(hover[0], "Detached at 83174b7", "no agent line");
    }

    fn branch(name: &str) -> Branch {
        Branch {
            name: name.into(),
            remote: false,
            upstream: None,
            ahead: 0,
            behind: 0,
            head: false,
        }
    }

    /// The tree as indented lines: folders end in `/`.
    fn outline(nodes: &[Node], depth: usize, out: &mut Vec<String>) {
        for n in nodes {
            match n {
                Node::Folder { name, children, .. } => {
                    out.push(format!("{}{name}/", "  ".repeat(depth)));
                    outline(children, depth + 1, out);
                }
                Node::Branch(b) => out.push(format!("{}{}", "  ".repeat(depth), b.name)),
            }
        }
    }

    #[test]
    fn branches_group_into_folders_by_slash() {
        let names = [
            "main",
            "feat/ui/menu",
            "fix/crash",
            "feat/login",
            "dev",
            "feat/ui/bar",
        ];
        let bs: Vec<Branch> = names.iter().map(|n| branch(n)).collect();
        let refs: Vec<&Branch> = bs.iter().collect();
        let nodes = branch_tree(&refs);
        let mut out = vec![];
        outline(&nodes, 0, &mut out);
        assert_eq!(
            out,
            [
                "feat/",
                "  ui/",
                "    feat/ui/menu",
                "    feat/ui/bar",
                "  feat/login",
                "fix/",
                "  fix/crash",
                "main",
                "dev",
            ]
        );
        let Node::Folder { path, children, .. } = &nodes[0] else {
            panic!("feat is a folder")
        };
        assert_eq!(path, "feat");
        assert!(matches!(&children[0], Node::Folder { path, .. } if path == "feat/ui"));
        assert_eq!(count(&nodes), 6);
    }

    #[test]
    fn branches_start_open_and_the_rest_closed() {
        let mut s = Sections::default();
        assert_eq!(s.open, [true, true, false, false, false]);
        s.fit(500.0, 40.0);
        assert_eq!(s.heights, [300.0, 200.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn fit_changes_only_the_bottom_until_it_hits_the_minimum() {
        let mut s = Sections {
            open: [true, false, true, false, true],
            heights: [100.0, 50.0, 100.0, 0.0, 100.0],
        };
        s.fit(400.0, 40.0);
        assert_eq!(s.heights, [100.0, 50.0, 100.0, 0.0, 200.0]);
        s.fit(200.0, 40.0);
        assert_eq!(s.heights, [100.0, 50.0, 60.0, 0.0, 40.0]);
        // No room for every minimum: an equal share each.
        s.fit(90.0, 40.0);
        assert_eq!(s.heights, [30.0, 50.0, 30.0, 0.0, 30.0]);
    }

    #[test]
    fn closing_gives_its_space_to_the_next_open_section_below() {
        let mut s = Sections {
            open: [true, true, true, false, true],
            heights: [100.0, 100.0, 100.0, 0.0, 100.0],
        };
        s.toggle(1, 400.0, 40.0);
        assert_eq!(s.open, [true, false, true, false, true]);
        assert_eq!(
            s.heights,
            [100.0, 100.0, 200.0, 0.0, 100.0],
            "Local stays put"
        );
        // The bottom one gives its space to the nearest above.
        s.toggle(4, 400.0, 40.0);
        assert_eq!(s.heights, [100.0, 100.0, 300.0, 0.0, 100.0]);
    }

    #[test]
    fn opening_takes_its_old_height_back_from_below_first() {
        let mut s = Sections {
            open: [true, false, true, false, true],
            heights: [100.0, 80.0, 200.0, 0.0, 100.0],
        };
        s.toggle(1, 400.0, 40.0);
        assert_eq!(s.heights, [100.0, 80.0, 120.0, 0.0, 100.0]);
        // More than below can spare: the rest comes from above.
        let mut s = Sections {
            open: [true, false, true, false, false],
            heights: [200.0, 150.0, 100.0, 0.0, 0.0],
        };
        s.toggle(1, 300.0, 40.0);
        assert_eq!(s.heights, [110.0, 150.0, 40.0, 0.0, 0.0]);
    }

    #[test]
    fn opening_never_squashes_others_below_the_minimum() {
        // Never opened: it asks for an equal share; the others only give down to 40.
        let mut s = Sections {
            open: [true, true, false, false, false],
            heights: [60.0, 60.0, 0.0, 0.0, 0.0],
        };
        s.toggle(2, 120.0, 40.0);
        assert_eq!(s.heights, [40.0, 40.0, 40.0, 0.0, 0.0]);
        // Alone, it takes everything.
        let mut s = Sections {
            open: [false; SECTIONS],
            heights: [0.0; SECTIONS],
        };
        s.toggle(3, 300.0, 40.0);
        assert_eq!(s.heights[3], 300.0);
    }

    #[test]
    fn dragging_down_grows_above_and_shrinks_below_in_turn() {
        let open = [true; SECTIONS];
        let mut h = [100.0, 100.0, 100.0, 100.0, 100.0];
        drag(&mut h, &open, 1, 30.0, 40.0);
        assert_eq!(h, [100.0, 130.0, 70.0, 100.0, 100.0]);
        // Past the next section's minimum, the one after it gives way too.
        drag(&mut h, &open, 1, 50.0, 40.0);
        assert_eq!(h, [100.0, 180.0, 40.0, 80.0, 100.0]);
        // All below at the minimum: it stops, and the total never changes.
        drag(&mut h, &open, 1, 1000.0, 40.0);
        assert_eq!(h, [100.0, 280.0, 40.0, 40.0, 40.0]);
        assert_eq!(h.iter().sum::<f32>(), 500.0);
    }

    #[test]
    fn dragging_up_shrinks_above_nearest_first() {
        let open = [true; SECTIONS];
        let mut h = [100.0, 60.0, 100.0, 100.0, 100.0];
        drag(&mut h, &open, 1, -50.0, 40.0);
        assert_eq!(h, [70.0, 40.0, 150.0, 100.0, 100.0]);
    }

    #[test]
    fn dragging_skips_closed_sections() {
        let open = [true, false, true, false, true];
        let mut h = [100.0, 0.0, 100.0, 0.0, 100.0];
        // The divider under closed Remote moves Local and Worktrees.
        drag(&mut h, &open, 1, 20.0, 40.0);
        assert_eq!(h, [120.0, 0.0, 80.0, 0.0, 100.0]);
        // With nothing open below, there is nothing to move.
        let open = [true, false, false, false, false];
        let mut h = [300.0, 0.0, 0.0, 0.0, 0.0];
        drag(&mut h, &open, 0, 20.0, 40.0);
        assert_eq!(h, [300.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn remotes_are_the_first_folder_level() {
        let bs: Vec<Branch> = ["origin/main", "upstream/main", "origin/feat/x"]
            .iter()
            .map(|n| branch(n))
            .collect();
        let refs: Vec<&Branch> = bs.iter().collect();
        let mut out = vec![];
        outline(&branch_tree(&refs), 0, &mut out);
        assert_eq!(
            out,
            [
                "origin/",
                "  feat/",
                "    origin/feat/x",
                "  origin/main",
                "upstream/",
                "  upstream/main"
            ]
        );
    }

    #[test]
    fn merge_items_hidden_for_the_checked_out_branch() {
        assert_eq!(merge_labels(Some("main"), "main"), None);
        assert_eq!(
            merge_labels(Some("main"), "origin/main"),
            Some((
                "Merge origin/main into main…".into(),
                "Rebase main onto origin/main…".into()
            ))
        );
        let (merge, rebase) = merge_labels(None, "feat/x").unwrap();
        assert_eq!(
            (merge.as_str(), rebase.as_str()),
            ("Merge feat/x…", "Rebase onto feat/x…")
        );
    }

    #[test]
    fn fast_forward_offered_only_when_behind() {
        let mut b = branch("main");
        b.upstream = Some("origin/main".into());
        assert_eq!(fast_forward_item(&b, &[]), None, "up to date");
        b.behind = 2;
        assert_eq!(
            fast_forward_item(&b, &["feat/x"]),
            Some(("Fast-forward to origin/main".into(), None))
        );
        let (_, why) = fast_forward_item(&b, &["main"]).unwrap();
        assert_eq!(
            why.as_deref(),
            Some("main is checked out: pull it there instead")
        );
        b.ahead = 1;
        let (_, why) = fast_forward_item(&b, &[]).unwrap();
        assert!(why.unwrap().contains("diverged from origin/main"));
        b.upstream = None;
        assert_eq!(fast_forward_item(&b, &[]), None, "no upstream");
        let mut r = branch("origin/main");
        (r.remote, r.behind, r.upstream) = (true, 1, Some("x".into()));
        assert_eq!(fast_forward_item(&r, &[]), None, "remote branch");
    }

    #[test]
    fn tag_hover_names_kind_and_commit() {
        let mut t = Tag {
            name: "v1.2.0".into(),
            target: "83174b7246c76c9b6426a95147a715f0d26bf6c6".into(),
            annotated: true,
        };
        assert_eq!(tag_hover(&t), "v1.2.0 · Annotated tag at 83174b7");
        t.annotated = false;
        assert_eq!(tag_hover(&t), "v1.2.0 · Tag at 83174b7");
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        assert!(filter_matches("", "main"));
        assert!(filter_matches("  ", "main"));
        assert!(filter_matches("FEAT", "feat/music-sync"));
        assert!(filter_matches("sync ", "feat/Music-Sync"));
        assert!(!filter_matches("fix", "main"));
    }
}
