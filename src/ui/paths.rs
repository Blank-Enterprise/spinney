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

/// Native file picker, starting in `start` when that is a folder. Blocks like `pick_folder`.
pub fn pick_file(title: &str, start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(dir) = start.filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
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

/// Display only: `path` with the `home` prefix shown as `~`.
pub fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A typed folder that must exist: `~`-expanded, absolute, canonicalized.
pub fn existing_dir(input: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if input.trim().is_empty() {
        return Err("Choose a folder".into());
    }
    let p = absolute(input, home)?;
    if !p.is_dir() {
        return Err(format!("{} is not a folder", p.display()));
    }
    p.canonicalize()
        .map_err(|e| format!("{}: {e}", p.display()))
}

/// A new project folder name: non-empty, no `/`, not `.` or `..`.
pub fn check_name(name: &str) -> Result<(), String> {
    match name.trim() {
        "" => Err("Enter a name".into()),
        n @ ("." | "..") => Err(format!("{n} is not a valid name")),
        n if n.contains('/') => Err("Name cannot contain /".into()),
        _ => Ok(()),
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
    fn home_is_abbreviated_for_display() {
        let h = Some(Path::new(HOME));
        let p = Path::new("/tmp/fakehome/code/proj");
        assert_eq!(abbreviate_home(p, h), "~/code/proj");
        assert_eq!(abbreviate_home(Path::new(HOME), h), "~");
        assert_eq!(
            abbreviate_home(Path::new("/tmp/fakehomeX/a"), h),
            "/tmp/fakehomeX/a"
        );
        assert_eq!(abbreviate_home(p, None), "/tmp/fakehome/code/proj");
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

    #[test]
    fn existing_dir_is_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        let canon = tmp.path().canonicalize().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let typed = format!(" {}/sub/.. ", tmp.path().display());
        assert_eq!(existing_dir(&typed, None).unwrap(), canon);
        assert_eq!(
            existing_dir("~/sub", Some(tmp.path())).unwrap(),
            canon.join("sub")
        );
        assert!(existing_dir("", None).is_err());
        assert!(existing_dir("sub", Some(tmp.path())).is_err(), "relative");
        assert!(existing_dir(&format!("{}/nope", tmp.path().display()), None).is_err());
        let file = tmp.path().join("f");
        std::fs::write(&file, "").unwrap();
        assert!(existing_dir(&file.display().to_string(), None).is_err());
    }

    #[test]
    fn names_are_single_segments() {
        assert!(check_name("proj").is_ok());
        assert!(check_name(" proj.v2 ").is_ok());
        for bad in ["", "  ", ".", "..", "a/b", "/"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }
}
