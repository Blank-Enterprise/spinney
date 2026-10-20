//! "Open in merge tool": a conflicted file goes to `git mergetool`, which writes the BASE,
//! LOCAL and REMOTE copies, runs the tool and stages the file when the tool says it's merged.
//!
//! The UI runs this as a background job; it only returns when the tool exits. Git never
//! waits on input here: stdin is /dev/null, so every `git mergetool` question ("Was the
//! merge successful?", "Use (m)odified or (d)eleted file?") reads end-of-file and that file
//! is left conflicted. What could still hang is a terminal tool such as vimdiff with no
//! terminal to draw in, so those are refused before anything starts.

use crate::config::MergeTool;
use crate::git;
use crate::status::{Conflict, Side};
use anyhow::{bail, Result};
use std::path::Path;

/// A named tool as `git mergetool` sees it: our own tool name (so the user's git config for
/// a tool of the same name can't change it), the shell command, and whether the tool's exit
/// code says the merge worked. Without that trust, git counts the merge as done when the
/// tool saved the file.
fn definition(tool: MergeTool) -> Option<(&'static str, &'static str, bool)> {
    Some(match tool {
        MergeTool::None | MergeTool::Git => return None,
        // `| cat` keeps opendiff in the foreground until FileMerge closes, as git's own does.
        MergeTool::FileMerge => (
            "spinney-filemerge",
            r#"opendiff "$LOCAL" "$REMOTE" -ancestor "$BASE" -merge "$MERGED" | cat"#,
            false,
        ),
        MergeTool::Kaleidoscope => (
            "spinney-kaleidoscope",
            r#"ksdiff --merge --output "$MERGED" --base "$BASE" -- "$LOCAL" --snapshot "$REMOTE" --snapshot"#,
            true,
        ),
        // `code` exits 0 however the window was closed, so its exit code means nothing.
        MergeTool::VsCode => (
            "spinney-vscode",
            r#"code --wait --merge "$REMOTE" "$LOCAL" "$BASE" "$MERGED""#,
            false,
        ),
        // `bcomp` waits for the comparison to close; `bcompare` would return at once.
        MergeTool::BeyondCompare => (
            "spinney-beyondcompare",
            r#"bcomp "$LOCAL" "$REMOTE" "$BASE" "$MERGED""#,
            true,
        ),
    })
}

/// Tools that draw in a terminal. Spinney has none to give them, so they would sit there
/// for good; `vimdiff1`…`3` and `nvimdiff1`…`3` are layouts of the same tools.
pub fn terminal_tool(name: &str) -> bool {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
    matches!(base, "vimdiff" | "nvimdiff" | "emerge")
}

/// Why a conflicted file can't go to the merge tool. A side that deleted the file leaves
/// nothing to merge line by line (and `git mergetool` would ask), so it's mine or theirs.
pub fn blocked(c: Conflict, path: &str) -> Option<&'static str> {
    if c.ours == Side::Deleted || c.theirs == Side::Deleted {
        return Some("One side deleted this file: use mine or theirs");
    }
    path_blocked(path)
}

/// `git mergetool` splits its file list on spaces and expands `*`, `?` and `[` as shell
/// globs (`set -- $files`), so such a path could open, and stage, a different file.
fn path_blocked(path: &str) -> Option<&'static str> {
    path.contains(|c: char| c.is_whitespace() || matches!(c, '*' | '?' | '[' | '\\'))
        .then_some("git mergetool can't take a path with spaces or *, ?, [ in it")
}

/// Arguments for `git` to merge `path` with `tool`. `git_tool` is the user's configured
/// tool, needed only for `MergeTool::Git`.
pub fn args(tool: MergeTool, git_tool: Option<&str>, path: &str) -> Result<Vec<String>> {
    if let Some(why) = path_blocked(path) {
        bail!("{path}: {why}");
    }
    let mut args: Vec<String> = vec!["--literal-pathspecs".into()];
    let mut config = |kv: String| args.extend(["-c".into(), kv]);
    // Backups would show up as untracked `*.orig` files in Changes.
    config("mergetool.keepBackup=false".into());
    let name = match (tool, definition(tool)) {
        (MergeTool::None, _) => bail!("no merge tool is set"),
        (MergeTool::Git, _) => match git_tool {
            None => bail!("git has no merge.tool set"),
            Some(t) if terminal_tool(t) => bail!(
                "{t} runs in a terminal, which Spinney can't show; set merge.guitool to a GUI tool"
            ),
            Some(t) => t.to_string(),
        },
        (_, Some((name, cmd, trust))) => {
            config(format!("mergetool.{name}.cmd={cmd}"));
            config(format!("mergetool.{name}.trustExitCode={trust}"));
            name.to_string()
        }
        (_, None) => unreachable!("every named tool has a definition"),
    };
    args.extend([
        "mergetool".into(),
        "--no-prompt".into(),
        format!("--tool={name}"),
        "--".into(),
        path.into(),
    ]);
    Ok(args)
}

/// `PATH` for the tool: an app opened from the Finder gets only the system directories, but
/// `code`, `ksdiff` and `bcomp` are installed under Homebrew's or `/usr/local`.
pub fn search_path(current: Option<&str>) -> String {
    let mut dirs: Vec<&str> = current
        .unwrap_or("/usr/bin:/bin:/usr/sbin:/sbin")
        .split(':')
        .filter(|d| !d.is_empty())
        .collect();
    for extra in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin",
    ] {
        if !dirs.contains(&extra) {
            dirs.push(extra);
        }
    }
    dirs.join(":")
}

/// The user's GUI merge tool if they set one, else their merge tool.
fn configured(worktree: &Path) -> Result<Option<String>> {
    for key in ["merge.guitool", "merge.tool"] {
        let out = git::output(worktree, &["config", "--get", key])?;
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !value.is_empty() {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// Merge `path` (relative to `worktree`) in `tool` and wait for it to close. Ok means git
/// staged the result; a file the tool left unmerged is an error.
pub fn run(worktree: &Path, tool: MergeTool, path: &str) -> Result<()> {
    let current = std::env::var("PATH").ok();
    run_in(worktree, tool, path, &search_path(current.as_deref()))
}

fn run_in(worktree: &Path, tool: MergeTool, path: &str, search: &str) -> Result<()> {
    let git_tool = match tool {
        MergeTool::Git => configured(worktree)?,
        _ => None,
    };
    let args = args(tool, git_tool.as_deref(), path)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git::output_env(worktree, &args, &[("PATH", search)])?;
    if out.status.success() {
        return Ok(());
    }
    // A tool that couldn't start says why on stderr ("code: command not found"), before
    // git's own "merge of <path> failed".
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = stderr
        .lines()
        .map(str::trim)
        .rev()
        .find(|l| !l.is_empty() && !l.starts_with("merge of "));
    match why {
        Some(why) => bail!("{path} is still conflicted: {}", why.trim()),
        None => bail!("{path} is still conflicted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn g(dir: &Path, args: &[&str]) -> String {
        git::run(dir, args).unwrap().trim().to_string()
    }

    /// A repo stopped in a merge with `a.txt` and `[a].txt` both conflicted (the second
    /// matches the first as a shell glob).
    fn conflicted() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir(&dir).unwrap();
        g(&dir, &["init", "-q", "-b", "main"]);
        g(&dir, &["config", "user.name", "Test User"]);
        g(&dir, &["config", "user.email", "test@example.com"]);
        let write = |text: &str| {
            for f in ["a.txt", "[a].txt"] {
                std::fs::write(dir.join(f), format!("{text}\n")).unwrap();
            }
        };
        write("base");
        g(&dir, &["add", "."]);
        g(&dir, &["commit", "-qm", "init"]);
        g(&dir, &["switch", "-qc", "other"]);
        write("theirs");
        g(&dir, &["commit", "-qam", "theirs"]);
        g(&dir, &["switch", "-q", "main"]);
        write("mine");
        g(&dir, &["commit", "-qam", "mine"]);
        assert!(git::run(&dir, &["merge", "other"]).is_err());
        (tmp, dir)
    }

    fn unmerged(dir: &Path) -> String {
        g(dir, &["diff", "--name-only", "--diff-filter=U"])
    }

    fn untracked(dir: &Path) -> String {
        g(dir, &["ls-files", "--others", "--exclude-standard"])
    }

    /// Point `merge.tool` at a fake tool running `cmd`.
    fn fake_tool(dir: &Path, cmd: &str, trust: bool) {
        g(dir, &["config", "merge.tool", "fake"]);
        g(dir, &["config", "mergetool.fake.cmd", cmd]);
        let trust = trust.to_string();
        g(dir, &["config", "mergetool.fake.trustExitCode", &trust]);
    }

    #[test]
    fn git_tool_resolves_and_stages_the_file() {
        let (_tmp, dir) = conflicted();
        g(&dir, &["config", "mergetool.keepBackup", "true"]);
        fake_tool(&dir, r#"cp "$REMOTE" "$MERGED""#, true);
        run(&dir, MergeTool::Git, "a.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "theirs\n"
        );
        assert_eq!(unmerged(&dir), "[a].txt", "only that file");
        assert_eq!(untracked(&dir), "", "no .orig backup or temp files left");
    }

    #[test]
    fn glob_like_path_is_refused_before_git_runs() {
        let (_tmp, dir) = conflicted();
        fake_tool(&dir, r#"cp "$REMOTE" "$MERGED""#, true);
        // `git mergetool` would expand `[a].txt` to `a.txt` and merge that instead.
        let err = run(&dir, MergeTool::Git, "[a].txt")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("[a].txt: git mergetool can't"), "{err}");
        assert_eq!(unmerged(&dir), "[a].txt\na.txt", "neither file touched");
        let both = Conflict {
            ours: Side::Modified,
            theirs: Side::Modified,
        };
        for p in ["a b.txt", "*.txt", "a?.txt", "[a].txt", "a\\b"] {
            assert!(blocked(both, p).is_some(), "{p}");
        }
        assert_eq!(blocked(both, "dir/ü-a_b.txt"), None);
    }

    #[test]
    fn unchanged_file_stays_conflicted_without_waiting() {
        let (_tmp, dir) = conflicted();
        // Untrusted and unsaved: git asks "Was the merge successful?" and must not wait.
        fake_tool(&dir, "true", false);
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || tx.send(run(&d, MergeTool::Git, "a.txt")).unwrap());
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("git mergetool waited on input");
        let err = res.unwrap_err().to_string();
        assert_eq!(err, "a.txt is still conflicted");
        assert_eq!(unmerged(&dir), "[a].txt\na.txt");
    }

    #[test]
    fn missing_tool_says_why() {
        let (_tmp, dir) = conflicted();
        fake_tool(&dir, "spinney-no-such-tool", true);
        let err = run(&dir, MergeTool::Git, "a.txt").unwrap_err().to_string();
        assert!(err.starts_with("a.txt is still conflicted: "), "{err}");
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn git_tool_needs_a_gui_tool() {
        let (_tmp, dir) = conflicted();
        let err = run(&dir, MergeTool::Git, "a.txt").unwrap_err().to_string();
        assert_eq!(err, "git has no merge.tool set");
        g(&dir, &["config", "merge.tool", "vimdiff"]);
        let err = run(&dir, MergeTool::Git, "a.txt").unwrap_err().to_string();
        assert!(err.starts_with("vimdiff runs in a terminal"), "{err}");
        // A GUI tool set as merge.guitool wins over a terminal merge.tool.
        g(&dir, &["config", "merge.guitool", "fake"]);
        g(
            &dir,
            &["config", "mergetool.fake.cmd", r#"cp "$LOCAL" "$MERGED""#],
        );
        g(&dir, &["config", "mergetool.fake.trustExitCode", "true"]);
        run(&dir, MergeTool::Git, "a.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "mine\n"
        );
        assert_eq!(unmerged(&dir), "[a].txt");
    }

    #[test]
    fn named_tool_runs_its_own_command_from_the_search_path() {
        let (tmp, dir) = conflicted();
        // A user's own `vscode` setup must not change what Spinney runs.
        g(&dir, &["config", "mergetool.vscode.cmd", "false"]);
        // A stand-in `code`: `code --wait --merge REMOTE LOCAL BASE MERGED`.
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let code = bin.join("code");
        // Untrusted, so git checks the file got newer, to the second: wait one out.
        std::fs::write(&code, "#!/bin/sh\nsleep 1\ncp \"$3\" \"$6\"\n").unwrap();
        std::fs::set_permissions(&code, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), search_path(None));
        run_in(&dir, MergeTool::VsCode, "a.txt", &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "theirs\n"
        );
        assert_eq!(unmerged(&dir), "[a].txt");
        assert_eq!(untracked(&dir), "");
    }

    #[test]
    fn args_per_tool() {
        let tail = ["mergetool", "--no-prompt"];
        let a = args(MergeTool::Git, Some("opendiff"), "dir/a.txt").unwrap();
        let mut want = vec!["--literal-pathspecs", "-c", "mergetool.keepBackup=false"];
        want.extend(tail);
        want.extend(["--tool=opendiff", "--", "dir/a.txt"]);
        assert_eq!(a, want);

        let a = args(MergeTool::Kaleidoscope, None, "f").unwrap();
        assert_eq!(a[3], "-c");
        assert!(a[4].starts_with("mergetool.spinney-kaleidoscope.cmd=ksdiff --merge"));
        assert_eq!(a[6], "mergetool.spinney-kaleidoscope.trustExitCode=true");
        assert_eq!(a[a.len() - 3], "--tool=spinney-kaleidoscope");
        assert_eq!(a[a.len() - 1], "f");

        for (tool, start, trust) in [
            (MergeTool::FileMerge, "opendiff ", false),
            (MergeTool::VsCode, "code --wait --merge ", false),
            (MergeTool::BeyondCompare, "bcomp ", true),
        ] {
            let (name, cmd, t) = definition(tool).unwrap();
            assert!(cmd.starts_with(start), "{tool:?}: {cmd}");
            assert_eq!(t, trust, "{tool:?}");
            let a = args(tool, None, "f").unwrap();
            assert!(a.contains(&format!("mergetool.{name}.cmd={cmd}")));
            assert!(a.contains(&format!("--tool={name}")));
        }

        assert!(args(MergeTool::None, None, "f").is_err());
        assert!(args(MergeTool::Git, None, "f").is_err());
        assert!(args(MergeTool::Git, Some("nvimdiff2"), "f").is_err());
        assert!(args(MergeTool::VsCode, None, "a b.txt").is_err());
    }

    #[test]
    fn terminal_tools() {
        for t in ["vimdiff", "vimdiff3", "nvimdiff", "nvimdiff1", "emerge"] {
            assert!(terminal_tool(t), "{t}");
        }
        for t in ["gvimdiff", "gvimdiff2", "opendiff", "vscode", "bc4", "meld"] {
            assert!(!terminal_tool(t), "{t}");
        }
    }

    #[test]
    fn deleted_side_blocks_the_merge_tool() {
        let c = |ours, theirs| Conflict { ours, theirs };
        assert_eq!(blocked(c(Side::Modified, Side::Modified), "f"), None);
        assert_eq!(blocked(c(Side::Added, Side::Added), "f"), None);
        assert!(blocked(c(Side::Modified, Side::Deleted), "f").is_some());
        assert!(blocked(c(Side::Deleted, Side::Modified), "f").is_some());
    }

    #[test]
    fn search_path_adds_tool_dirs_once() {
        let p = search_path(Some("/usr/local/bin:/usr/bin"));
        assert_eq!(
            p,
            "/usr/local/bin:/usr/bin:/opt/homebrew/bin:\
             /Applications/Visual Studio Code.app/Contents/Resources/app/bin"
        );
        assert!(search_path(None).starts_with("/usr/bin:/bin:/usr/sbin:/sbin:"));
    }
}
