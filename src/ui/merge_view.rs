//! Built-in merge tool: each conflict's two sides at the top, with a pick for each, and
//! the editable result below. Save writes the result and marks the file resolved.

use super::conflict_view::label;
use super::{theme, Action};
use crate::conflict::{self, Block, Chunk, Endings, Pick};
use crate::refs;
use anyhow::{bail, Result};
use eframe::egui::{self, Align, Color32, RichText};
use std::path::Path;

pub struct MergeEditor {
    pub path: String,
    /// The file as it was read, so Save can tell if something else changed it since.
    pub original: String,
    chunks: Vec<Chunk>,
    endings: Endings,
    /// One per conflict, in file order.
    picks: Vec<Option<Pick>>,
    /// The text Save writes: the picks applied, then any typing.
    pub result: String,
    /// The checked-out branch, to name git's `HEAD` label.
    head: Option<String>,
    /// A question waiting on the inline bar.
    pub ask: Option<Ask>,
}

/// What the inline bar asks before throwing away the user's work in the editor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ask {
    Close,
    StartOver,
}

impl MergeEditor {
    pub fn load(worktree: &Path, path: &str) -> Result<MergeEditor> {
        let full = worktree.join(path);
        if !std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_file()) {
            bail!("{path} is not a file in the worktree, so there is nothing to merge here");
        }
        let bytes = std::fs::read(&full)?;
        let Ok(text) = String::from_utf8(bytes) else {
            bail!("{path} is binary or not UTF-8 text; use mine or theirs instead");
        };
        if text.contains('\0') {
            bail!("{path} is binary; use mine or theirs instead");
        }
        let chunks = conflict::parse(&text);
        let mut e = MergeEditor {
            path: path.to_string(),
            picks: vec![None; conflict::count(&chunks)],
            endings: Endings::of(&text),
            chunks,
            result: String::new(),
            original: text,
            head: refs::current_branch(worktree).ok().flatten(),
            ask: None,
        };
        e.result = e.merged();
        Ok(e)
    }

    /// The result the picks alone give.
    fn merged(&self) -> String {
        conflict::merge(&self.chunks, &self.picks, self.endings)
    }

    /// The result was typed in, so a pick would overwrite that typing.
    pub fn edited(&self) -> bool {
        self.result != self.merged()
    }

    /// Closing now would lose picks or typing.
    pub fn dirty(&self) -> bool {
        self.result != self.original
    }

    /// Pick (or with the same pick again, un-pick) conflict `i`. Ignored once the result
    /// was edited by hand: Start over first.
    pub fn pick(&mut self, i: usize, pick: Pick) {
        if self.edited() {
            return;
        }
        if let Some(p) = self.picks.get_mut(i) {
            *p = if *p == Some(pick) { None } else { Some(pick) };
            self.result = self.merged();
        }
    }

    pub fn start_over(&mut self) {
        self.picks.iter_mut().for_each(|p| *p = None);
        self.result = self.merged();
        self.ask = None;
    }

    fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.chunks.iter().filter_map(|c| match c {
            Chunk::Conflict(b) => Some(b),
            Chunk::Same(_) => None,
        })
    }
}

/// The four ways to settle a conflict, as button labels naming each side.
pub fn pick_labels(ours: &str, theirs: &str) -> [(Pick, String); 4] {
    [
        (Pick::Ours, format!("Use {ours}")),
        (Pick::Theirs, format!("Use {theirs}")),
        (Pick::OursThenTheirs, format!("Both, {ours} first")),
        (Pick::TheirsThenOurs, format!("Both, {theirs} first")),
    ]
}

/// The inline bar's question and its button.
pub fn ask_text(ask: Ask) -> (&'static str, &'static str) {
    match ask {
        Ask::Close => ("Close the merge? Your picks and edits are lost.", "Close"),
        Ask::StartOver => (
            "Start over? Your edits to the result are lost.",
            "Start over",
        ),
    }
}

/// Close straight away, or ask first when that would lose picks or typing.
fn close(e: &MergeEditor) -> Action {
    if e.dirty() {
        Action::AskMergeEditor(Ask::Close)
    } else {
        Action::CloseDiff
    }
}

pub fn show(ui: &mut egui::Ui, e: &mut MergeEditor, actions: &mut Vec<Action>) {
    let typing = ui.ctx().egui_wants_keyboard_input();
    if !typing && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(close(e));
    }
    header(ui, e, actions);
    if let Some(ask) = e.ask {
        ask_bar(ui, ask, actions);
    }
    ui.add_space(2.0);
    ui.separator();
    let edited = e.edited();
    let half = (ui.available_height() * 0.5).max(120.0);
    egui::Panel::top("merge_sides")
        .resizable(true)
        .default_size(half)
        .frame(egui::Frame::new())
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| sides(ui, e, edited, actions));
        });
    ui.add_space(4.0);
    ui.label(theme::section("Result"));
    egui::ScrollArea::vertical()
        .id_salt("merge_result")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut e.result)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(12),
            );
        });
}

fn header(ui: &mut egui::Ui, e: &MergeEditor, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(&e.path).strong().color(Color32::WHITE));
        let picked = e.picks.iter().filter(|p| p.is_some()).count();
        let what = match (e.edited(), e.picks.len()) {
            (true, _) => "edited by hand".to_string(),
            (false, 0) => "no conflict markers left".to_string(),
            (false, n) => format!("{picked} of {n} picked"),
        };
        ui.weak(format!("merging · {what}"));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let x =
                egui::Button::new(RichText::new("×").size(16.0).color(theme::MUTED)).frame(false);
            if ui.add(x).on_hover_text("Close (Esc)").clicked() {
                actions.push(close(e));
            }
            let save = ui
                .add(theme::primary("Save"))
                .on_hover_text("Write the result and mark the file resolved");
            if save.clicked() {
                actions.push(Action::SaveMerge);
            }
            let over = ui.add_enabled(e.dirty(), egui::Button::new("Start over"));
            if over.clicked() {
                actions.push(if e.edited() {
                    Action::AskMergeEditor(Ask::StartOver)
                } else {
                    Action::StartOverMerge
                });
            }
        });
    });
}

fn ask_bar(ui: &mut egui::Ui, ask: Ask, actions: &mut Vec<Action>) {
    let (question, button) = ask_text(ask);
    egui::Frame::new()
        .fill(theme::DIFF_DEL)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(question).color(theme::TEXT));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Keep editing").clicked() {
                        actions.push(Action::CancelMergeEditor);
                    }
                    let go =
                        egui::Button::new(RichText::new(button).color(theme::ON_ACCENT).strong())
                            .fill(theme::DELETED)
                            .small();
                    if ui.add(go).clicked() {
                        actions.push(Action::ConfirmMergeEditor);
                    }
                });
            });
        });
}

/// Each conflict: its two sides next to each other, and the picks under them.
fn sides(ui: &mut egui::Ui, e: &MergeEditor, edited: bool, actions: &mut Vec<Action>) {
    let n = e.picks.len();
    if n == 0 {
        ui.weak("No conflict markers left: edit the result if needed, then Save.");
    }
    let head = e.head.as_deref();
    let why = "You edited the result: Start over to pick again";
    for (i, b) in e.blocks().enumerate() {
        let (ours, theirs) = (label(&b.ours_label, head), label(&b.theirs_label, head));
        theme::card().show(ui, |ui| {
            ui.label(theme::caption(&format!("Conflict {} of {n}", i + 1)));
            ui.columns(2, |cols| {
                side(&mut cols[0], &ours, &b.ours, theme::ACCENT);
                side(&mut cols[1], &theirs, &b.theirs, theme::RENAMED);
            });
            ui.horizontal_wrapped(|ui| {
                for (pick, text) in pick_labels(&ours, &theirs) {
                    let on = e.picks[i] == Some(pick);
                    let r = ui
                        .add_enabled(!edited, egui::Button::selectable(on, text))
                        .on_disabled_hover_text(why);
                    if r.clicked() {
                        actions.push(Action::PickConflict(i, pick));
                    }
                }
            });
        });
        ui.add_space(6.0);
    }
}

fn side(ui: &mut egui::Ui, name: &str, lines: &[String], color: Color32) {
    ui.label(RichText::new(name).strong().color(color));
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if lines.is_empty() {
                ui.weak("(nothing)");
            }
            for l in lines {
                let text = l.replace('\t', "    ");
                ui.add(egui::Label::new(RichText::new(text).monospace()).truncate());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflicted(text: &str) -> (tempfile::TempDir, MergeEditor) {
        let tmp = tempfile::tempdir().unwrap();
        crate::git::run(tmp.path(), &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(tmp.path().join("f"), text).unwrap();
        let e = MergeEditor::load(tmp.path(), "f").unwrap();
        (tmp, e)
    }

    #[test]
    fn picks_rebuild_the_result_until_it_is_typed_in() {
        let text = "a\n<<<<<<< HEAD\nm\n=======\nt\n>>>>>>> topic\nb\n";
        let (_tmp, mut e) = conflicted(text);
        assert_eq!(e.result, text);
        assert!(!e.dirty() && !e.edited());
        e.pick(0, Pick::TheirsThenOurs);
        assert_eq!(e.result, "a\nt\nm\nb\n");
        assert!(e.dirty() && !e.edited());
        e.pick(0, Pick::TheirsThenOurs);
        assert_eq!(e.result, text, "the same pick again un-picks");
        e.pick(0, Pick::Ours);
        e.result.push_str("typed\n");
        assert!(e.edited());
        e.pick(0, Pick::Theirs);
        assert_eq!(e.result, "a\nm\nb\ntyped\n", "typing is never overwritten");
        e.ask = Some(Ask::StartOver);
        e.start_over();
        assert_eq!((e.result.as_str(), e.ask), (text, None));
    }

    #[test]
    fn load_refuses_binary_and_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("b"), b"\x00bin").unwrap();
        assert!(MergeEditor::load(d, "b").is_err());
        assert!(MergeEditor::load(d, "gone").is_err());
        std::os::unix::fs::symlink("b", d.join("l")).unwrap();
        assert!(MergeEditor::load(d, "l").is_err(), "not through a symlink");
    }

    #[test]
    fn labels_name_both_sides() {
        let labels = pick_labels("HEAD (main)", "topic").map(|(_, l)| l);
        assert_eq!(
            labels,
            [
                "Use HEAD (main)",
                "Use topic",
                "Both, HEAD (main) first",
                "Both, topic first"
            ]
        );
        assert_eq!(ask_text(Ask::Close).1, "Close");
        assert!(ask_text(Ask::StartOver).0.contains("edits"));
    }
}
