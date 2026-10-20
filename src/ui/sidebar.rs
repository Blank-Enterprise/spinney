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

fn section(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    n: usize,
    open: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    egui::CollapsingHeader::new(theme::section(&format!("{title} · {n}")))
        .id_salt(id)
        .default_open(open)
        .show(ui, add);
}

pub fn show(ui: &mut egui::Ui, tab: &mut ProjectTab, actions: &mut Vec<Action>) {
    ui.add(
        egui::TextEdit::singleline(&mut tab.filter)
            .hint_text("Filter")
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
    let tab = &*tab;
    let q = tab.filter.as_str();
    egui::ScrollArea::vertical().show(ui, |ui| {
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
        section(ui, "local", "Local", local.len(), true, |ui| {
            let nodes = branch_tree(&local);
            tree(ui, "local", &nodes, filtering, cx, actions);
        });
        let remote: Vec<_> = tab
            .branches
            .iter()
            .filter(|b| b.remote && filter_matches(q, &b.name))
            .collect();
        section(ui, "remote", "Remote", remote.len(), true, |ui| {
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
        section(ui, "worktrees", "Worktrees", worktrees.len(), true, |ui| {
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
        section(ui, "stashes", "Stashes", tab.stashes.len(), true, |ui| {
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
        // Last: a long tag list shouldn't push worktrees and stashes out of view.
        let tags: Vec<_> = tab
            .tags
            .iter()
            .filter(|t| filter_matches(q, &t.name))
            .collect();
        section(ui, "tags", "Tags", tags.len(), true, |ui| {
            for t in tags {
                tag_row(ui, t, actions);
            }
        });
    });
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
