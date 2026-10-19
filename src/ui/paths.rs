//! Folder paths: the native picker and typed-path handling.

use std::path::{Path, PathBuf};

/// The user's home folder, for `~` handling.
pub fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Native folder picker (NSOpenPanel). Blocks: rfd runs it modally on the calling (main)
/// thread, which is where eframe runs `ui`. Starts in `start` when that is a folder.
pub fn pick_folder(title: &str, start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(dir) = start.filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

/// Trimmed `input` with a leading `~` (alone or before `/`) replaced by `home`.
pub fn expand_tilde(input: &str, home: Option<&Path>) -> PathBuf {
    let s = input.trim();
    match (s.strip_prefix('~'), home) {
        (Some(""), Some(h)) => h.to_path_buf(),
        (Some(rest), Some(h)) if rest.starts_with('/') => h.join(&rest[1..]),
        _ => PathBuf::from(s),
    }
}

/// `input` after `~` expansion, which must be absolute.
pub fn absolute(input: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let p = expand_tilde(input, home);
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(format!("{} is not an absolute path", input.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/tmp/fakehome";

    #[test]
    fn tilde_expands_only_as_home_prefix() {
        let h = Some(Path::new(HOME));
        assert_eq!(expand_tilde("~", h), PathBuf::from(HOME));
        assert_eq!(
            expand_tilde(" ~/code ", h),
            PathBuf::from("/tmp/fakehome/code")
        );
        assert_eq!(expand_tilde("~other/x", h), PathBuf::from("~other/x"));
        assert_eq!(expand_tilde("/abs/~", h), PathBuf::from("/abs/~"));
        assert_eq!(expand_tilde("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn absolute_rejects_relative() {
        let h = Some(Path::new(HOME));
        assert_eq!(
            absolute("~/x", h).unwrap(),
            PathBuf::from("/tmp/fakehome/x")
        );
        assert_eq!(absolute("/srv", h).unwrap(), PathBuf::from("/srv"));
        assert!(absolute("code", h).is_err());
        assert!(absolute("~/x", None).is_err());
        assert!(absolute("", h).is_err());
    }
}
