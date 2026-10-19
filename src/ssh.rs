//! SSH for a space: a Spinney-made or existing key, and the git config that sends the
//! space's https remotes over SSH with it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Options every Spinney SSH command carries. No config files (`~/.ssh/config` may name other
/// keys, which ssh would offer too and so sign in as another account): only the chosen key.
/// Git runs without a terminal, so ssh must never prompt; a passphrase comes from the agent or
/// the macOS Keychain, and a new host is trusted on first use.
const OPTIONS: &str = "-F none -o IdentitiesOnly=yes -o BatchMode=yes \
    -o StrictHostKeyChecking=accept-new -o IgnoreUnknown=UseKeychain -o UseKeychain=yes";

/// Where Spinney keeps the keys it generates (beside the config file).
pub fn keys_dir() -> PathBuf {
    let config = crate::config::Config::path();
    config.parent().unwrap_or(Path::new(".")).join("ssh")
}

/// Create a passphrase-less ed25519 key in `dir`; returns the private key's path.
pub fn generate(dir: &Path, comment: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = (0..)
        .map(|n| dir.join(format!("spinney_{:x}_ed25519", nanos + n)))
        .find(|p| !p.exists() && !pub_path(p).exists())
        .expect("an unused name");
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(&path)
        .output()
        .context("failed to run ssh-keygen")?;
    if !out.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(path)
}

/// Folder levels below `~/.ssh` searched for keys (people file them as `~/.ssh/work/id`).
const KEY_DEPTH: usize = 3;

/// Private keys in `dir` (normally `~/.ssh`) and its subfolders that have a `.pub` beside them.
pub fn existing_keys(dir: &Path) -> Vec<PathBuf> {
    let mut keys = vec![];
    collect_keys(dir, KEY_DEPTH, &mut keys);
    keys.sort();
    keys
}

fn collect_keys(dir: &Path, depth: usize, keys: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        if path.is_dir() {
            if depth > 0 {
                collect_keys(&path, depth - 1, keys);
            }
        } else if path.extension().is_some_and(|x| x == "pub") {
            let private = path.with_extension("");
            if private.is_file() {
                keys.push(private);
            }
        }
    }
}

/// A key file picked anywhere: the private half (also when its `.pub` was picked), which
/// must have a `.pub` beside it to add to the host.
pub fn chosen_key(picked: &Path) -> Result<PathBuf> {
    let private = if picked.extension().is_some_and(|x| x == "pub") {
        picked.with_extension("")
    } else {
        picked.to_path_buf()
    };
    if !private.is_file() {
        bail!("{} is not a private key file", private.display());
    }
    if !pub_path(&private).is_file() {
        bail!(
            "No public key beside {} (expected {}.pub)",
            private.display(),
            private.display()
        );
    }
    Ok(private)
}

fn pub_path(private: &Path) -> PathBuf {
    let mut p = private.as_os_str().to_owned();
    p.push(".pub");
    PathBuf::from(p)
}

/// The one-line public key beside `private`.
pub fn public_key(private: &Path) -> Result<String> {
    let p = pub_path(private);
    let key = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(key.trim().to_string())
}

/// Single-quote `s` for `sh -c`, escaping embedded quotes as `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `core.sshCommand` using only `key`.
pub fn ssh_command(key: &Path) -> String {
    format!("ssh -i {} {OPTIONS}", sh_quote(&key.to_string_lossy()))
}

/// Whether a `core.sshCommand` value is one Spinney wrote (so it may be removed).
pub fn is_ours(command: &str) -> bool {
    command.starts_with("ssh -i '") && command.trim_end().ends_with(OPTIONS)
}

/// The `url.<base>.insteadOf` key and value that rewrite `https://host/…` to `git@host:…`.
pub fn insteadof(host: &str) -> (String, String) {
    (
        format!("url.git@{host}:.insteadOf"),
        format!("https://{host}/"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_makes_a_key_pair_with_private_perms() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a b/ssh");
        let k = generate(&dir, "spinney test").unwrap();
        let k2 = generate(&dir, "spinney test").unwrap();
        assert_ne!(k, k2, "never overwrites");
        let pubkey = public_key(&k).unwrap();
        assert!(pubkey.starts_with("ssh-ed25519 ") && pubkey.ends_with(" spinney test"));
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&k), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(existing_keys(&dir), {
            let mut v = vec![k, k2];
            v.sort();
            v
        });
    }

    #[test]
    fn existing_keys_are_found_in_subfolders() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let deep = d.join("Github/work");
        std::fs::create_dir_all(&deep).unwrap();
        let too_deep = d.join("a/b/c/d");
        std::fs::create_dir_all(&too_deep).unwrap();
        for k in [
            d.join("Github/personal"),
            deep.join("id"),
            too_deep.join("id"),
        ] {
            std::fs::write(&k, "x").unwrap();
            std::fs::write(pub_path(&k), "x").unwrap();
        }
        assert_eq!(
            existing_keys(d),
            [d.join("Github/personal"), deep.join("id")]
        );
    }

    #[test]
    fn a_picked_key_needs_both_halves() {
        let tmp = tempfile::tempdir().unwrap();
        let k = tmp.path().join("keys/my key");
        std::fs::create_dir_all(k.parent().unwrap()).unwrap();
        std::fs::write(&k, "x").unwrap();
        assert!(chosen_key(&k).is_err(), "no .pub yet");
        std::fs::write(pub_path(&k), "x").unwrap();
        assert_eq!(chosen_key(&k).unwrap(), k);
        assert_eq!(
            chosen_key(&pub_path(&k)).unwrap(),
            k,
            "picking the .pub works too"
        );
        assert!(chosen_key(tmp.path()).is_err(), "a folder is no key");
    }

    #[test]
    fn existing_keys_need_both_halves() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        for f in [
            "id_ed25519",
            "id_ed25519.pub",
            "lonely.pub",
            "config",
            "known_hosts",
        ] {
            std::fs::write(d.join(f), "x").unwrap();
        }
        assert_eq!(existing_keys(d), [d.join("id_ed25519")]);
        assert!(existing_keys(&d.join("missing")).is_empty());
    }

    #[test]
    fn command_quotes_the_key_and_is_recognised() {
        let c = ssh_command(Path::new("/Users/me/Library/Application Support/it's/k"));
        assert_eq!(
            c,
            format!("ssh -i '/Users/me/Library/Application Support/it'\\''s/k' {OPTIONS}")
        );
        assert!(is_ours(&c));
        assert!(!is_ours("ssh -i ~/.ssh/work_key"));
        assert!(!is_ours("/usr/local/bin/my-ssh"));
    }

    #[test]
    fn insteadof_rewrites_https_for_the_host() {
        assert_eq!(
            insteadof("github.com"),
            (
                "url.git@github.com:.insteadOf".to_string(),
                "https://github.com/".to_string()
            )
        );
    }
}
