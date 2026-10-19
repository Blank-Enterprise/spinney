#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    cmd
}

fn finish(child: Child, args: &[&str], ok: &[i32]) -> Result<Vec<u8>> {
    let out = child.wait_with_output()?;
    if !out.status.code().is_some_and(|c| ok.contains(&c)) {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Run git in `dir`, returning stdout. A non-zero exit becomes an error carrying stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    run_ok_codes(dir, args, &[0])
}

/// Like `run`, but exit codes listed in `ok` count as success (e.g. `diff --no-index` exits 1).
pub fn run_ok_codes(dir: &Path, args: &[&str], ok: &[i32]) -> Result<String> {
    run_bytes(dir, args, ok).map(lossy)
}

/// Like `run_ok_codes`, but returns stdout's raw bytes (no lossy UTF-8 conversion).
pub fn run_bytes(dir: &Path, args: &[&str], ok: &[i32]) -> Result<Vec<u8>> {
    exec(dir, args, &[], ok)
}

pub fn run_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> Result<String> {
    exec(dir, args, stdin, &[0]).map(lossy)
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

fn exec(dir: &Path, args: &[&str], stdin: &[u8], ok: &[i32]) -> Result<Vec<u8>> {
    let mut child = command(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git")?;
    // Write on a thread so a large stdin cannot deadlock against full stdout/stderr pipes.
    let mut pipe = child.stdin.take().expect("stdin piped");
    let data = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = pipe.write_all(&data); // git may exit before reading everything
    });
    let result = finish(child, args, ok);
    let _ = writer.join();
    result
}

/// Run git in `dir` and return its status, stdout and stderr, whatever the exit code.
pub fn output(dir: &Path, args: &[&str]) -> Result<Output> {
    command(dir, args)
        .stdin(Stdio::null())
        .output()
        .context("failed to spawn git")
}

/// Spawn git with stdout piped for streaming; the caller owns the child.
pub fn spawn_stream(dir: &Path, args: &[&str]) -> Result<Child> {
    command(dir, args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn git")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_returns_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-q"]).unwrap();
        let out = run(tmp.path(), &["rev-parse", "--is-inside-work-tree"]).unwrap();
        assert_eq!(out, "true\n");
    }

    #[test]
    fn run_ok_codes_accepts_listed_exit() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-q"]).unwrap();
        std::fs::write(tmp.path().join("a"), "x\n").unwrap();
        let args = ["diff", "--no-index", "--", "/dev/null", "a"];
        assert!(run(tmp.path(), &args).is_err());
        let out = run_ok_codes(tmp.path(), &args, &[0, 1]).unwrap();
        assert!(out.contains("+x"), "{out}");
    }

    #[test]
    fn run_error_has_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-q"]).unwrap();
        let err = run(tmp.path(), &["rev-parse", "HEAD"]).unwrap_err();
        assert!(err.to_string().contains("fatal"), "{err}");
    }
}
