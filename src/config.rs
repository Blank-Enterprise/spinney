#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)] // a field added later must not break older configs (and the credential helper)
pub struct Space {
    pub id: String,
    pub name: String,
    pub host: String,
    pub author_name: String,
    pub author_email: String,
    pub default_clone_dir: Option<PathBuf>,
    pub projects: Vec<PathBuf>,
    /// Private key git uses over SSH for this space's host; `None` = HTTPS with the token.
    pub ssh_key: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub spaces: Vec<Space>,
    pub active_space: Option<String>,
    pub diff_inline: bool,
}

impl Space {
    /// Lowercase slug of `name` plus `-` and 6 hex chars from the current time.
    pub fn new_id(name: &str) -> String {
        let slug = name
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
            .unwrap_or(0);
        format!("{slug}-{:06x}", nanos & 0xff_ffff)
    }

    pub fn api_base(&self) -> String {
        if self.host == "github.com" {
            "https://api.github.com".into()
        } else {
            format!("https://{}/api/v3", self.host)
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_default()
            .join("Spinney")
            .join("config.toml")
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Atomic: write `<path>.tmp`, then rename over `path`.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/config.toml");
        let cfg = Config {
            spaces: vec![Space {
                id: "work-abc123".into(),
                name: "Work".into(),
                host: "github.com".into(),
                author_name: "Test User".into(),
                author_email: "test@example.com".into(),
                default_clone_dir: Some(PathBuf::from("/tmp/code")),
                projects: vec![PathBuf::from("/tmp/code/p")],
                ssh_key: Some(PathBuf::from("/tmp/keys/id_ed25519")),
            }],
            active_space: Some("work-abc123".into()),
            diff_inline: true,
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), cfg);
    }

    #[test]
    fn space_missing_fields_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let toml = "[[spaces]]\nid = \"w-1\"\nname = \"Work\"\nhost = \"github.com\"\n\
                    author_name = \"Test User\"\nauthor_email = \"test@example.com\"\n";
        std::fs::write(&path, toml).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.spaces[0].id, "w-1");
        assert!(cfg.spaces[0].projects.is_empty());
        assert_eq!(cfg.spaces[0].default_clone_dir, None);
        assert_eq!(cfg.spaces[0].ssh_key, None, "older configs stay on HTTPS");
    }

    #[test]
    fn config_missing_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&tmp.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn api_base_enterprise() {
        let s = Space {
            host: "ghe.example.com".into(),
            ..Default::default()
        };
        assert_eq!(s.api_base(), "https://ghe.example.com/api/v3");
        let s = Space {
            host: "github.com".into(),
            ..Default::default()
        };
        assert_eq!(s.api_base(), "https://api.github.com");
    }

    #[test]
    fn new_id_is_slug_with_suffix() {
        let id = Space::new_id("My  Work_Space!");
        let (slug, suffix) = id.rsplit_once('-').unwrap();
        assert_eq!(slug, "my-work-space");
        assert_eq!(suffix.len(), 6);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
