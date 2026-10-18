#![allow(dead_code)]

use crate::config::Space;
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

/// Pure core of the `get` operation. `stdin` is git's request; Spinney answers every host the
/// same way because the helper is only installed into repos that belong to the space.
pub fn helper_response(_stdin: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("username=x-access-token\npassword={t}\n"),
        None => String::new(),
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
    match get_token(space_id) {
        Ok(token) => {
            print!("{}", helper_response(&input, token.as_deref()));
            0
        }
        Err(e) => {
            eprintln!("spinney credential: {e}");
            1
        }
    }
}

fn helper_value(space: &Space) -> Result<String> {
    let exe = std::env::current_exe().context("cannot locate spinney executable")?;
    Ok(format!("!'{}' credential {}", exe.display(), space.id))
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
            helper_response("protocol=https\nhost=github.com\n\n", Some("abc")),
            "username=x-access-token\npassword=abc\n"
        );
    }

    #[test]
    fn helper_get_without_token_empty() {
        assert_eq!(helper_response("host=github.com\n", None), "");
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
