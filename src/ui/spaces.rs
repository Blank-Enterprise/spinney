use super::{active_space, paths, Action};
use crate::config::{Config, Space};
use eframe::egui;

const SCOPE_HINT: &str = "Needs repo contents read/write, pull requests read/write, metadata read.";

/// Space combo box: pick a space, or create/edit one.
pub fn switcher(ui: &mut egui::Ui, cfg: &Config, actions: &mut Vec<Action>) {
    let active = active_space(cfg);
    let current = active.map_or("No space", |s| s.name.as_str());
    egui::ComboBox::from_id_salt("space")
        .selected_text(current)
        .show_ui(ui, |ui| {
            for s in &cfg.spaces {
                let is_active = active.is_some_and(|a| a.id == s.id);
                if ui.selectable_label(is_active, &s.name).clicked() && !is_active {
                    actions.push(Action::SwitchSpace(s.id.clone()));
                }
            }
            ui.separator();
            if ui.button("New space…").clicked() {
                actions.push(Action::NewSpace);
            }
            if active.is_some() && ui.button("Edit space…").clicked() {
                actions.push(Action::EditSpace);
            }
        });
}

/// Space editor window; `Some(true)` = save, `Some(false)` = cancel, `None` = still open.
pub fn editor(ctx: &egui::Context, e: &mut SpaceEdit) -> Option<bool> {
    let (mut result, mut browse) = (None, false);
    let title = if e.id.is_some() {
        "Edit space"
    } else {
        "New space"
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("space_grid").num_columns(2).show(ui, |ui| {
                for (label, value) in [
                    ("Name", &mut e.name),
                    ("Host", &mut e.host),
                    ("Author name", &mut e.author_name),
                    ("Author email", &mut e.author_email),
                ] {
                    ui.label(label);
                    ui.text_edit_singleline(value);
                    ui.end_row();
                }
                ui.label("Default clone folder");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut e.clone_dir);
                    browse = ui.button("Browse").clicked();
                });
                ui.end_row();
                ui.label("Personal access token");
                let hint = if e.id.is_some() {
                    "unchanged"
                } else {
                    "required for private repos"
                };
                ui.add(
                    egui::TextEdit::singleline(&mut e.pat)
                        .password(true)
                        .hint_text(hint),
                );
                ui.end_row();
            });
            ui.weak(SCOPE_HINT);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    result = Some(false);
                }
            });
        });
    if browse {
        let start = paths::expand_tilde(&e.clone_dir, paths::home().as_deref());
        if let Some(dir) = paths::pick_folder("Default clone folder", Some(&start)) {
            e.clone_dir = dir.display().to_string();
            ctx.request_repaint(); // show the pick now, not on the next input
        }
    }
    result
}

/// Editor buffer for a new (`id: None`) or existing space. The PAT is write-only.
#[derive(Default)]
pub struct SpaceEdit {
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub author_name: String,
    pub author_email: String,
    pub clone_dir: String,
    pub pat: String,
}

impl SpaceEdit {
    pub fn new_space() -> Self {
        SpaceEdit {
            host: "github.com".into(),
            ..Default::default()
        }
    }

    pub fn from_space(s: &Space) -> Self {
        SpaceEdit {
            id: Some(s.id.clone()),
            name: s.name.clone(),
            host: s.host.clone(),
            author_name: s.author_name.clone(),
            author_email: s.author_email.clone(),
            clone_dir: s
                .default_clone_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            pat: String::new(),
        }
    }

    /// The entered PAT, trimmed (pasted tokens often carry a newline); `None` if blank.
    pub fn token(&self) -> Option<&str> {
        Some(self.pat.trim()).filter(|t| !t.is_empty())
    }

    /// Validate and write into `cfg` (new spaces become active); returns the space id.
    pub fn apply(&self, cfg: &mut Config) -> Result<String, String> {
        let name = self.name.trim();
        let host = normalize_host(&self.host);
        if name.is_empty() || host.is_empty() {
            return Err("space needs a name and a host".into());
        }
        let author_name = self.author_name.trim();
        let author_email = self.author_email.trim();
        if author_name.is_empty() || author_email.is_empty() {
            return Err("space needs an author name and email".into());
        }
        let dir = match self.clone_dir.trim() {
            "" => None,
            d => Some(paths::absolute(d, paths::home().as_deref())?),
        };
        let fill = |s: &mut Space| {
            s.name = name.to_string();
            s.host = host.clone();
            s.author_name = author_name.to_string();
            s.author_email = author_email.to_string();
            s.default_clone_dir = dir.clone();
        };
        match &self.id {
            Some(id) => {
                let s = cfg
                    .spaces
                    .iter_mut()
                    .find(|s| &s.id == id)
                    .ok_or("space no longer exists")?;
                fill(s);
                Ok(id.clone())
            }
            None => {
                let mut s = Space {
                    id: Space::new_id(name),
                    ..Default::default()
                };
                fill(&mut s);
                let id = s.id.clone();
                cfg.spaces.push(s);
                cfg.active_space = Some(id.clone());
                Ok(id)
            }
        }
    }
}

/// Bare lowercase host: `https://GitHub.com/` becomes `github.com`.
pub fn normalize_host(input: &str) -> String {
    let h = input.trim();
    let h = h
        .strip_prefix("https://")
        .or_else(|| h.strip_prefix("http://"))
        .unwrap_or(h);
    h.trim_end_matches('/').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn new_space_defaults_and_validation() {
        let e = SpaceEdit::new_space();
        assert_eq!(e.host, "github.com");
        let mut cfg = Config::default();
        assert!(e.apply(&mut cfg).is_err(), "name required");
        assert!(cfg.spaces.is_empty());
    }

    #[test]
    fn apply_requires_author_identity() {
        let mut cfg = Config::default();
        let mut e = SpaceEdit::new_space();
        e.name = "Work".into();
        e.author_email = "test@example.com".into();
        e.author_name = "  ".into();
        assert!(e.apply(&mut cfg).is_err(), "author name required");
        e.author_name = "Test User".into();
        e.author_email = "".into();
        assert!(e.apply(&mut cfg).is_err(), "author email required");
        assert!(cfg.spaces.is_empty());
        e.author_email = "test@example.com".into();
        assert!(e.apply(&mut cfg).is_ok());
    }

    #[test]
    fn host_is_normalized() {
        for raw in [
            "github.com",
            " https://GitHub.com/ ",
            "http://github.com",
            "https://github.com//",
        ] {
            assert_eq!(normalize_host(raw), "github.com", "{raw:?}");
        }
        assert_eq!(
            normalize_host("ghe.example.com:8443"),
            "ghe.example.com:8443"
        );
        let mut cfg = Config::default();
        let mut e = SpaceEdit::new_space();
        e.name = "Work".into();
        e.host = "https://GHE.example.com/".into();
        e.author_name = "Test User".into();
        e.author_email = "test@example.com".into();
        e.apply(&mut cfg).unwrap();
        assert_eq!(cfg.spaces[0].host, "ghe.example.com");
    }

    #[test]
    fn clone_dir_must_be_absolute() {
        let mut cfg = Config::default();
        let mut e = SpaceEdit::new_space();
        e.name = "Work".into();
        e.author_name = "Test User".into();
        e.author_email = "test@example.com".into();
        e.clone_dir = "code".into();
        assert!(e.apply(&mut cfg).is_err(), "relative clone dir rejected");
        assert!(cfg.spaces.is_empty());
    }

    #[test]
    fn token_is_trimmed() {
        let mut e = SpaceEdit::new_space();
        assert_eq!(e.token(), None);
        e.pat = " \n".into();
        assert_eq!(e.token(), None);
        e.pat = "ghp_x\n".into();
        assert_eq!(e.token(), Some("ghp_x"));
    }

    #[test]
    fn apply_creates_then_edits_keeping_projects() {
        let mut cfg = Config::default();
        let mut e = SpaceEdit::new_space();
        e.name = "Work".into();
        e.author_name = "Test User".into();
        e.author_email = "test@example.com".into();
        e.clone_dir = " /tmp/code ".into();
        e.pat = "secret".into();
        let id = e.apply(&mut cfg).unwrap();
        assert!(id.starts_with("work-"));
        assert_eq!(cfg.active_space.as_deref(), Some(id.as_str()));
        assert_eq!(
            cfg.spaces[0].default_clone_dir,
            Some(PathBuf::from("/tmp/code"))
        );
        assert!(!toml::to_string(&cfg).unwrap().contains("secret"));

        cfg.spaces[0].projects.push(PathBuf::from("/tmp/code/p"));
        let mut e = SpaceEdit::from_space(&cfg.spaces[0]);
        assert!(e.pat.is_empty(), "PAT never echoed back");
        assert_eq!(e.clone_dir, "/tmp/code");
        e.host = "ghe.example.com".into();
        e.clone_dir.clear();
        assert_eq!(e.apply(&mut cfg).unwrap(), id);
        assert_eq!(cfg.spaces.len(), 1);
        assert_eq!(cfg.spaces[0].host, "ghe.example.com");
        assert_eq!(cfg.spaces[0].default_clone_dir, None);
        assert_eq!(cfg.spaces[0].projects, [PathBuf::from("/tmp/code/p")]);
    }
}
