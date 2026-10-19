use super::{active_space, paths, theme, Action};
use crate::config::{Config, Space};
use crate::creds;
use crate::github::{self, DeviceCode, Poll, GITHUB_CLIENT_ID};
use eframe::egui;
use std::ops::ControlFlow;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

const SCOPE_HINT: &str = "Needs repo contents read/write, pull requests read/write, metadata read.";

/// "● Space ▾" menu: pick a space, or create/edit one.
pub fn switcher(ui: &mut egui::Ui, cfg: &Config, actions: &mut Vec<Action>) {
    let active = active_space(cfg);
    let current = active.map_or("No space", |s| s.name.as_str());
    let button = egui::Button::new((
        egui::RichText::new("●").color(theme::ACCENT),
        egui::RichText::new(current).color(theme::TEXT),
        egui::RichText::new("▾").color(theme::MUTED),
    ))
    .frame(false);
    egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
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
    e.poll_auth(ctx);
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
                    ui.label(theme::caption(label));
                    ui.text_edit_singleline(value);
                    ui.end_row();
                }
                ui.label(theme::caption("Default clone folder"));
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut e.clone_dir);
                    browse = ui.button("Browse").clicked();
                });
                ui.end_row();
                if !e.github_com() {
                    ui.label(theme::caption("Personal access token"));
                    pat_field(ui, e);
                    ui.end_row();
                }
            });
            if e.github_com() {
                ui.add_space(6.0);
                account(ui, e);
                ui.add_space(6.0);
            } else {
                ui.label(theme::caption(SCOPE_HINT));
            }
            ui.horizontal(|ui| {
                // Saving drops the editor (and an unfinished sign-in) with it.
                let busy = matches!(e.auth, Auth::Starting(_) | Auth::Waiting { .. });
                if ui.add_enabled(!busy, theme::primary("Save")).clicked() {
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
    if !e.github_com() {
        e.drop_flow(); // a github.com sign-in never belongs to another host
    }
    result
}

fn pat_field(ui: &mut egui::Ui, e: &mut SpaceEdit) {
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
}

/// "GitHub account" section of a github.com space: device-flow sign-in, or a pasted PAT.
fn account(ui: &mut egui::Ui, e: &mut SpaceEdit) {
    ui.label(theme::section("GitHub account"));
    let (mut next, mut sign_out) = (None, false);
    match &mut e.auth {
        Auth::SignedOut | Auth::Failed(_) => {
            if let Auth::Failed(why) = &e.auth {
                ui.label(egui::RichText::new(why.as_str()).color(theme::DELETED));
            }
            let label = if matches!(e.auth, Auth::Failed(_)) {
                "Try again"
            } else {
                "Sign in with GitHub"
            };
            if ui.add(theme::primary(label)).clicked() {
                next = Some(start(ui.ctx()));
            }
            ui.label(theme::caption("or paste a personal access token"));
            pat_field(ui, e);
            ui.label(theme::caption(SCOPE_HINT));
        }
        Auth::Starting(_) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(theme::caption("Contacting GitHub…"));
                if ui.button("Cancel").clicked() {
                    next = Some(Auth::SignedOut);
                }
            });
        }
        Auth::Waiting {
            user_code,
            verification_uri,
            ..
        } => {
            theme::card().show(ui, |ui| {
                ui.label("Enter this code on GitHub");
                ui.label(
                    egui::RichText::new(user_code.as_str())
                        .monospace()
                        .size(24.0)
                        .strong()
                        .color(theme::ACCENT_TEXT),
                );
                ui.horizontal(|ui| {
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(user_code.clone());
                    }
                    if ui.button("Open GitHub").clicked() {
                        open_in_browser(verification_uri);
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(theme::caption("Waiting for approval…"));
                    if ui.button("Cancel").clicked() {
                        next = Some(Auth::SignedOut);
                    }
                });
            });
        }
        Auth::SignedIn { held, confirm } => {
            ui.label(egui::RichText::new("Signed in to GitHub ✓").color(theme::ADDED));
            if held.is_some() {
                ui.label(theme::caption(
                    "Saved to the Keychain when you save the space.",
                ));
            }
            if *confirm {
                ui.horizontal(|ui| {
                    ui.label(theme::caption("Sign out and remove the token?"));
                    if ui.button("Sign out").clicked() {
                        sign_out = true;
                    }
                    if ui.button("Keep").clicked() {
                        *confirm = false;
                    }
                });
            } else if ui.button("Sign out").clicked() {
                *confirm = true;
            }
        }
    }
    if let Some(a) = next {
        e.auth = a; // dropping a Starting/Waiting state stops its thread
    }
    if sign_out {
        if let Some(id) = e.sign_out() {
            if let Err(err) = creds::delete_token(&id) {
                e.auth = Auth::Failed(format!("Could not remove the token: {err}"));
            }
        }
    }
}

/// Request a device code on a worker thread; it wakes the UI once, when GitHub answers.
fn start(ctx: &egui::Context) -> Auth {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if tx.send(github::start_device_flow(GITHUB_CLIENT_ID)).is_ok() {
            ctx.request_repaint();
        }
    });
    Auth::Starting(rx)
}

/// Poll for the token every `interval` seconds until a final answer, expiry, or until `stop`
/// is dropped (Cancel, or the editor closed). Pending replies never wake the UI.
fn poll_loop(
    dc: DeviceCode,
    stop: Receiver<()>,
    tx: Sender<anyhow::Result<Poll>>,
    ctx: egui::Context,
) {
    let deadline = Instant::now() + Duration::from_secs(dc.expires_in);
    let mut interval = dc.interval;
    let result = loop {
        if let Err(RecvTimeoutError::Disconnected) | Ok(()) =
            stop.recv_timeout(Duration::from_secs(interval))
        {
            return;
        }
        if Instant::now() >= deadline {
            break Ok(Poll::Expired);
        }
        match github::poll_once(GITHUB_CLIENT_ID, &dc.device_code, interval) {
            Ok(p) => match after_poll(interval, p) {
                ControlFlow::Continue(n) => interval = n,
                ControlFlow::Break(p) => break Ok(p),
            },
            Err(err) => break Err(err),
        }
    };
    if tx.send(result).is_ok() {
        ctx.request_repaint();
    }
}

/// Keep polling (at the given interval) or stop with a final answer.
fn after_poll(interval: u64, poll: Poll) -> ControlFlow<Poll, u64> {
    match poll {
        Poll::Pending => ControlFlow::Continue(interval),
        Poll::SlowDown(n) => ControlFlow::Continue(n),
        done => ControlFlow::Break(done),
    }
}

/// Open GitHub's device page, only if it really is on github.com.
fn open_in_browser(uri: &str) {
    if let Some(url) = browser_url(uri) {
        let url = url.to_string();
        std::thread::spawn(move || {
            let _ = std::process::Command::new("open").arg(url).status();
        });
    }
}

fn browser_url(uri: &str) -> Option<&str> {
    uri.starts_with("https://github.com/").then_some(uri)
}

/// GitHub sign-in state of the space editor (github.com spaces only).
#[derive(Default)]
enum Auth {
    #[default]
    SignedOut,
    /// Inline error; offers "Try again".
    Failed(String),
    Starting(Receiver<anyhow::Result<DeviceCode>>),
    /// Showing the user code; dropping `_stop` stops the poll thread.
    Waiting {
        user_code: String,
        verification_uri: String,
        rx: Receiver<anyhow::Result<Poll>>,
        _stop: Sender<()>,
    },
    /// A token is in the Keychain (`held: None`), or held until the space is saved.
    SignedIn {
        held: Option<String>,
        confirm: bool,
    },
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
    /// Host of the space as saved (normalized); empty for a new space.
    saved_host: String,
    auth: Auth,
}

impl SpaceEdit {
    pub fn new_space() -> Self {
        SpaceEdit {
            host: "github.com".into(),
            ..Default::default()
        }
    }

    /// Edit an existing space, reading from the Keychain whether it has a token.
    pub fn open(s: &Space) -> Self {
        let stored = matches!(creds::get_token(&s.id), Ok(Some(t)) if !t.trim().is_empty());
        SpaceEdit::from_space(s, stored)
    }

    /// Edit buffer for `s`; `stored` = a token for it is in the Keychain.
    pub fn from_space(s: &Space, stored: bool) -> Self {
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
            saved_host: normalize_host(&s.host),
            auth: if stored {
                Auth::SignedIn {
                    held: None,
                    confirm: false,
                }
            } else {
                Auth::SignedOut
            },
        }
    }

    /// The editor's host is github.com (device-flow sign-in available).
    fn github_com(&self) -> bool {
        normalize_host(&self.host) == "github.com"
    }

    /// The token to save with the space: a held sign-in token (github.com only), else the
    /// entered PAT trimmed (pasted tokens often carry a newline); `None` if blank.
    pub fn token(&self) -> Option<&str> {
        match &self.auth {
            Auth::SignedIn { held: Some(t), .. } if self.github_com() => Some(t),
            _ => Some(self.pat.trim()).filter(|t| !t.is_empty()),
        }
    }

    /// Drop an in-flight sign-in and any held token (the host is no longer github.com).
    fn drop_flow(&mut self) {
        let keep = matches!(
            self.auth,
            Auth::SignedOut | Auth::SignedIn { held: None, .. }
        );
        if !keep {
            self.auth = Auth::SignedOut;
        }
    }

    /// A sign-in token arrived. Stored right away (returns `(space id, token)` for the
    /// Keychain) only for a saved github.com space; otherwise held until the space is saved.
    fn accept_token(&mut self, token: String) -> Option<(String, String)> {
        self.pat.clear();
        let store_now = self.saved_host == "github.com" && self.github_com();
        let (held, store) = match &self.id {
            Some(id) if store_now => (None, Some((id.clone(), token))),
            _ => (Some(token), None),
        };
        self.auth = Auth::SignedIn {
            held,
            confirm: false,
        };
        store
    }

    /// Confirmed sign-out; returns the space id whose Keychain token must be deleted.
    fn sign_out(&mut self) -> Option<String> {
        let stored = matches!(self.auth, Auth::SignedIn { held: None, .. });
        self.auth = Auth::SignedOut;
        stored.then(|| self.id.clone()).flatten()
    }

    /// Take finished background replies; opens GitHub once when the code arrives.
    fn poll_auth(&mut self, ctx: &egui::Context) {
        match &self.auth {
            Auth::Starting(rx) => match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.auth = Auth::Failed("Sign-in stopped".into())
                }
                Ok(Err(err)) => self.auth = Auth::Failed(format!("{err:#}")),
                Ok(Ok(dc)) => {
                    let (stop_tx, stop_rx) = mpsc::channel();
                    let (tx, rx) = mpsc::channel();
                    open_in_browser(&dc.verification_uri);
                    self.auth = Auth::Waiting {
                        user_code: dc.user_code.clone(),
                        verification_uri: dc.verification_uri.clone(),
                        rx,
                        _stop: stop_tx,
                    };
                    let ctx = ctx.clone();
                    std::thread::spawn(move || poll_loop(dc, stop_rx, tx, ctx));
                }
            },
            Auth::Waiting { rx, .. } => match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.auth = Auth::Failed("Sign-in stopped".into())
                }
                Ok(Err(err)) => self.auth = Auth::Failed(format!("{err:#}")),
                Ok(Ok(Poll::Token(token))) => {
                    if let Some((id, token)) = self.accept_token(token) {
                        if let Err(err) = creds::set_token(&id, &token) {
                            self.auth = Auth::Failed(format!("Could not save the token: {err}"));
                        }
                    }
                }
                Ok(Ok(Poll::Denied)) => {
                    self.auth = Auth::Failed("Sign-in was cancelled on GitHub".into())
                }
                Ok(Ok(_)) => self.auth = Auth::Failed("The code expired".into()),
            },
            _ => {}
        }
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
        let mut e = SpaceEdit::from_space(&cfg.spaces[0], false);
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

    fn saved_github_space() -> (Config, SpaceEdit) {
        let mut cfg = Config::default();
        let mut e = SpaceEdit::new_space();
        e.name = "Work".into();
        e.author_name = "Test User".into();
        e.author_email = "test@example.com".into();
        e.apply(&mut cfg).unwrap();
        let e = SpaceEdit::from_space(&cfg.spaces[0], false);
        (cfg, e)
    }

    fn held(e: &SpaceEdit) -> Option<&str> {
        match &e.auth {
            Auth::SignedIn { held, .. } => held.as_deref(),
            _ => None,
        }
    }

    #[test]
    fn poll_thread_keeps_going_until_a_final_answer() {
        assert_eq!(after_poll(5, Poll::Pending), ControlFlow::Continue(5));
        assert_eq!(after_poll(5, Poll::SlowDown(10)), ControlFlow::Continue(10));
        for done in [Poll::Denied, Poll::Expired, Poll::Token("t".into())] {
            assert_eq!(after_poll(5, done.clone()), ControlFlow::Break(done));
        }
    }

    #[test]
    fn only_github_device_pages_are_opened() {
        let ok = "https://github.com/login/device";
        assert_eq!(browser_url(ok), Some(ok));
        for evil in [
            "https://github.com.evil.com/login/device",
            "https://evil.com/?https://github.com/",
            "https://github.com@evil.com/",
            "http://github.com/login/device",
            "file:///Applications/Calculator.app",
            "-a Calculator",
            "",
        ] {
            assert_eq!(browser_url(evil), None, "{evil:?}");
        }
    }

    #[test]
    fn token_for_a_saved_github_space_is_stored_at_once() {
        let (cfg, mut e) = saved_github_space();
        e.pat = "typed".into();
        let id = cfg.spaces[0].id.clone();
        assert_eq!(e.accept_token("tok".into()), Some((id, "tok".to_string())));
        assert_eq!(held(&e), None);
        assert!(matches!(e.auth, Auth::SignedIn { held: None, .. }));
        assert_eq!(e.token(), None, "typed PAT superseded by the sign-in");
    }

    #[test]
    fn token_is_held_for_a_new_or_rehosted_space() {
        let mut e = SpaceEdit::new_space();
        e.pat = "typed".into();
        assert_eq!(e.accept_token("tok".into()), None);
        assert_eq!(held(&e), Some("tok"));
        assert_eq!(e.token(), Some("tok"));

        // Saved as an Enterprise space, host now edited to github.com: wait for Save.
        let (mut cfg, _) = saved_github_space();
        cfg.spaces[0].host = "ghe.example.com".into();
        let mut e = SpaceEdit::from_space(&cfg.spaces[0], false);
        e.host = "github.com".into();
        assert_eq!(e.accept_token("tok".into()), None);
        assert_eq!(e.token(), Some("tok"));
    }

    #[test]
    fn leaving_github_drops_the_held_token_and_the_flow() {
        let mut e = SpaceEdit::new_space();
        e.accept_token("tok".into());
        e.host = "ghe.example.com".into();
        assert_eq!(e.token(), None, "never saved for another host");
        e.drop_flow();
        assert!(matches!(e.auth, Auth::SignedOut));

        let (tx, rx) = mpsc::channel::<()>();
        let (_ptx, prx) = mpsc::channel();
        e.auth = Auth::Waiting {
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://github.com/login/device".into(),
            rx: prx,
            _stop: tx,
        };
        e.drop_flow();
        assert!(matches!(e.auth, Auth::SignedOut));
        assert!(rx.recv().is_err(), "poll thread told to stop");

        let (cfg, _) = saved_github_space();
        let mut e = SpaceEdit::from_space(&cfg.spaces[0], true);
        e.drop_flow();
        assert!(
            matches!(e.auth, Auth::SignedIn { held: None, .. }),
            "Keychain kept"
        );
    }

    #[test]
    fn sign_out_deletes_only_a_stored_token() {
        let (cfg, _) = saved_github_space();
        let mut e = SpaceEdit::from_space(&cfg.spaces[0], true);
        assert_eq!(e.sign_out(), Some(cfg.spaces[0].id.clone()));
        assert!(matches!(e.auth, Auth::SignedOut));
        let mut new = SpaceEdit::new_space();
        new.accept_token("tok".into());
        assert_eq!(new.sign_out(), None, "held token just dropped");
        assert_eq!(new.token(), None);
    }
}
