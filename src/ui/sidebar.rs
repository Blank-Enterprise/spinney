use super::{theme, toolbar::folder_name, Action, ProjectTab};
use crate::project::{Layout, Worktree};
use crate::refs::Branch;
use eframe::egui::{self, RichText};
use std::path::Path;

/// Branches grouped by their `/` segments, GitKraken style: `feat/login` sits in `feat`.
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

/// Draw `nodes`: folders start open and collapse; a filter opens them all so every match shows.
fn tree(
    ui: &mut egui::Ui,
    id: &str,
    nodes: &[Node],
    filtering: bool,
    layout: Layout,
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
                let r = header.show(ui, |ui| tree(ui, id, children, filtering, layout, actions));
                r.header_response
                    .on_hover_text(format!("{path} · {}", count(children)));
            }
            Node::Branch(b) => branch_row(ui, b, layout, actions),
        }
    }
}

/// One branch: its last name segment, ✓ when checked out, ahead/behind on the right.
fn branch_row(ui: &mut egui::Ui, b: &Branch, layout: Layout, actions: &mut Vec<Action>) {
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
    let r = row(ui, b.head, text, track.trim().to_string()).on_hover_text(&b.name);
    branch_menu(&r, layout, b, actions);
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
        section(ui, "local", "Local", local.len(), true, |ui| {
            let nodes = branch_tree(&local);
            tree(ui, "local", &nodes, filtering, tab.layout, actions);
        });
        let remote: Vec<_> = tab
            .branches
            .iter()
            .filter(|b| b.remote && filter_matches(q, &b.name))
            .collect();
        section(ui, "remote", "Remote", remote.len(), true, |ui| {
            // `origin/feat/x`: the remote is the first folder level.
            let nodes = branch_tree(&remote);
            tree(ui, "remote", &nodes, filtering, tab.layout, actions);
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
    });
}

fn branch_menu(r: &egui::Response, layout: Layout, b: &Branch, actions: &mut Vec<Action>) {
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
        ui.separator();
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
    fn branches_group_into_folders_like_gitkraken() {
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
    fn filter_is_case_insensitive_substring() {
        assert!(filter_matches("", "main"));
        assert!(filter_matches("  ", "main"));
        assert!(filter_matches("FEAT", "feat/music-sync"));
        assert!(filter_matches("sync ", "feat/Music-Sync"));
        assert!(!filter_matches("fix", "main"));
    }
}
