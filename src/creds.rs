#![allow(dead_code)]

use crate::config::{Config, Space};
use crate::git;
use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

const SERVICE: &str = "spinney";

pub fn set_token(space_id: &str, token: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, space_id)?.set_password(token)?;
    Ok(())
}

pub fn get_token(space_id: &str) -> Result<Option<String>> {
    match keyring::Entry::new(SERVICE, space_id)?.get_password() {
        Ok(t) => Ok(Some(t)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_token(space_id: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, space_id)?.delete_credential()?;
    Ok(())
}

/// Pure core of the `get` operation. Answers only for https requests to exactly the space's
/// host, so the PAT never goes to submodule, redirect or insteadOf targets elsewhere.
pub fn helper_response(stdin: &str, host: &str, token: Option<&str>) -> String {
    let mut https = false;
    let mut host_ok = false;
    for line in stdin.lines() {
        match line.split_once('=') {
            Some(("protocol", v)) => https = v == "https",
            Some(("host", v)) => host_ok = v.eq_ignore_ascii_case(host),
            _ => {}
        }
    }
    match token {
        Some(t) if https && host_ok => format!("username=x-access-token\npassword={t}\n"),
        _ => String::new(),
    }
}

/// Entry point for `spinney credential <space-id> <op>`; `args` is `[space_id, op]`.
pub fn helper_main(args: &[String]) -> i32 {
    let [space_id, op, ..] = args else {
        eprintln!("usage: spinney credential <space-id> <get|store|erase>");
        return 1;
    };
    if op != "get" {
        return 0; // store/erase are no-ops; unknown ops are tolerated per the git protocol
    }
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let cfg = match Config::load_from(&Config::path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("spinney credential: {e}");
            return 1;
        }
    };
    let Some(space) = cfg.spaces.iter().find(|s| &s.id == space_id) else {
        return 0;
    };
    match get_token(space_id) {
        Ok(token) => {
            print!("{}", helper_response(&input, &space.host, token.as_deref()));
            0
        }
        Err(e) => {
            eprintln!("spinney credential: {e}");
            1
        }
    }
}

/// Single-quote `exe` for `sh -c`, escaping embedded quotes as `'\''`.
fn helper_command(exe: &str, space_id: &str) -> String {
    format!("!'{}' credential {}", exe.replace('\'', "'\\''"), space_id)
}

fn helper_value(space: &Space) -> Result<String> {
    let exe = std::env::current_exe().context("cannot locate spinney executable")?;
    Ok(helper_command(&exe.to_string_lossy(), &space.id))
}

/// Write author identity and the Spinney-only credential helper into a repo's local config.
/// `git_dir` is a bare repo or a `.git` directory.
pub fn configure_repo(git_dir: &Path, space: &Space) -> Result<()> {
    let helper = helper_value(space)?;
    git::run(git_dir, &["config", "--local", "user.name", &space.author_name])?;
    git::run(git_dir, &["config", "--local", "user.email", &space.author_email])?;
    // The empty value resets helpers inherited from system/global config.
    git::run(git_dir, &["config", "--local", "--replace-all", "credential.helper", ""])?;
    git::run(git_dir, &["config", "--local", "--add", "credential.helper", &helper])?;
    Ok(())
}

/// `-c` args (to place before the git subcommand) for runs that happen before any repo config.
pub fn git_env_args(space: &Space) -> Result<Vec<String>> {
    Ok(vec![
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        format!("credential.helper={}", helper_value(space)?),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> Space {
        Space {
            id: "test-space".into(),
            name: "Test".into(),
            host: "github.com".into(),
            author_name: "Test User".into(),
            author_email: "test@example.com".into(),
            default_clone_dir: None,
            projects: vec![],
        }
    }

    #[test]
    fn helper_get_with_token() {
        assert_eq!(
            helper_response("protocol=https\nhost=github.com\n\n", "github.com", Some("abc")),
            "username=x-access-token\npassword=abc\n"
        );
        assert_eq!(
            helper_response("protocol=https\nhost=GitHub.com\n\n", "github.com", Some("abc")),
            "username=x-access-token\npassword=abc\n"
        );
    }

    #[test]
    fn helper_get_rejects_other_host_or_http() {
        let other = "protocol=https\nhost=evil.example.com\n\n";
        assert_eq!(helper_response(other, "github.com", Some("abc")), "");
        let http = "protocol=http\nhost=github.com\n\n";
        assert_eq!(helper_response(http, "github.com", Some("abc")), "");
        let port = "protocol=https\nhost=github.com:8443\n\n";
        assert_eq!(helper_response(port, "github.com", Some("abc")), "");
    }

    #[test]
    fn helper_command_escapes_quotes() {
        assert_eq!(helper_command("/a b/it's/spinney", "id"), "!'/a b/it'\\''s/spinney' credential id");
    }

    #[test]
    fn helper_get_without_token_empty() {
        assert_eq!(helper_response("protocol=https\nhost=github.com\n", "github.com", None), "");
    }

    #[test]
    fn configure_repo_resets_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git::run(dir, &["init", "-q", "--bare"]).unwrap();
        git::run(dir, &["config", "--local", "credential.helper", "osxkeychain"]).unwrap();
        configure_repo(dir, &space()).unwrap();
        let out = git::run(dir, &["config", "--local", "--get-all", "credential.helper"]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out:?}");
        assert_eq!(lines[0], "");
        assert!(lines[1].starts_with("!'") && lines[1].ends_with("' credential test-space"));
        let email = git::run(dir, &["config", "--local", "user.email"]).unwrap();
        assert_eq!(email.trim(), "test@example.com");
    }

    #[test]
    fn git_env_args_shape() {
        let a = git_env_args(&space()).unwrap();
        assert_eq!(a[..3], ["-c", "credential.helper=", "-c"]);
        assert!(a[3].starts_with("credential.helper=!'"));
    }

    /// Touches the real Keychain; run manually: cargo test smoke -- --ignored
    #[test]
    #[ignore]
    fn smoke_keychain_set() {
        set_token("smoke-test", "smoke-token").unwrap();
        assert_eq!(get_token("smoke-test").unwrap().as_deref(), Some("smoke-token"));
    }
}
