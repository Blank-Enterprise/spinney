//! Coding agents running in a worktree: agent processes (Claude Code, Codex…) whose working
//! directory is inside it. Best effort: an agent that works in a worktree from a process
//! started elsewhere (a subagent of a session in the main checkout) isn't seen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Executable names of the agents we know, and how to show them.
const AGENTS: [(&str, &str); 8] = [
    ("claude", "Claude Code"),
    ("codex", "Codex"),
    ("aider", "Aider"),
    ("gemini", "Gemini CLI"),
    ("cursor-agent", "Cursor Agent"),
    ("opencode", "OpenCode"),
    ("amp", "Amp"),
    ("goose", "Goose"),
];

/// Runtimes an agent may run under, with its script as the next argument.
const RUNTIMES: [&str; 3] = ["node", "bun", "deno"];

fn base(word: &str) -> &str {
    let name = word.rsplit('/').next().unwrap_or(word);
    name.strip_suffix(".js").unwrap_or(name)
}

/// The agent a `ps` command line belongs to, if any.
pub fn agent_name(args: &str) -> Option<&'static str> {
    let mut words = args.split_whitespace();
    let mut exe = base(words.next()?);
    if RUNTIMES.contains(&exe) {
        exe = base(words.find(|w| !w.starts_with('-'))?);
    }
    // Claude Code's helper for the Chrome extension isn't an agent session.
    if args.contains("--chrome-native-host") {
        return None;
    }
    AGENTS.iter().find(|(e, _)| *e == exe).map(|(_, n)| *n)
}

/// `ps -axo pid=,args=` lines as (pid, agent).
pub fn parse_ps(out: &str) -> Vec<(u32, &'static str)> {
    out.lines()
        .filter_map(|l| {
            let (pid, args) = l.trim_start().split_once(' ')?;
            Some((pid.parse().ok()?, agent_name(args)?))
        })
        .collect()
}

/// `lsof -Fn` output (`p<pid>`, then `n<cwd>`) as (pid, cwd).
pub fn parse_cwds(out: &str) -> Vec<(u32, PathBuf)> {
    let mut pid = None;
    let mut cwds = vec![];
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(n), Some(p)) = (line.strip_prefix('n'), pid) {
            cwds.push((p, PathBuf::from(n)));
        }
    }
    cwds
}

/// Credits each agent to the deepest worktree its folder is in, so one in a nested
/// `.claude/worktrees/…` isn't counted for the main checkout too.
pub fn attribute(
    running: &[(&'static str, PathBuf)],
    worktrees: &[&Path],
) -> HashMap<PathBuf, Vec<&'static str>> {
    let mut by_tree: HashMap<PathBuf, Vec<&'static str>> = HashMap::new();
    for (name, cwd) in running {
        let deepest = worktrees
            .iter()
            .filter(|w| cwd.starts_with(w))
            .max_by_key(|w| w.components().count());
        if let Some(w) = deepest {
            by_tree.entry(w.to_path_buf()).or_default().push(name);
        }
    }
    for names in by_tree.values_mut() {
        names.sort();
    }
    by_tree
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    // lsof exits 1 when some pid has gone; what it printed still holds.
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The agents running in each of `worktrees`, keyed by worktree path.
pub fn running(worktrees: &[&Path]) -> HashMap<PathBuf, Vec<&'static str>> {
    let Some(ps) = run("ps", &["-axo", "pid=,args="]) else {
        return HashMap::new();
    };
    let agents: HashMap<u32, &'static str> = parse_ps(&ps).into_iter().collect();
    if agents.is_empty() {
        return HashMap::new();
    }
    let pids: Vec<String> = agents.keys().map(u32::to_string).collect();
    let Some(lsof) = run(
        "lsof",
        &["-nP", "-a", "-d", "cwd", "-p", &pids.join(","), "-Fn"],
    ) else {
        return HashMap::new();
    };
    let found: Vec<_> = parse_cwds(&lsof)
        .into_iter()
        .filter_map(|(pid, cwd)| Some((*agents.get(&pid)?, cwd)))
        .collect();
    // lsof reports real paths (/private/tmp for /tmp): compare those, key by git's.
    let real: Vec<(PathBuf, &Path)> = worktrees
        .iter()
        .map(|w| (w.canonicalize().unwrap_or_else(|_| w.to_path_buf()), *w))
        .collect();
    let paths: Vec<&Path> = real.iter().map(|(r, _)| r.as_path()).collect();
    attribute(&found, &paths)
        .into_iter()
        .filter_map(|(r, names)| {
            let (_, w) = real.iter().find(|(p, _)| *p == r)?;
            Some((w.to_path_buf(), names))
        })
        .collect()
}

/// "Claude Code", "2 × Claude Code", "Claude Code, Codex".
pub fn agents_text(names: &[&str]) -> String {
    let mut parts: Vec<String> = vec![];
    let mut i = 0;
    while i < names.len() {
        let n = names[i..].iter().take_while(|&&x| x == names[i]).count();
        parts.push(match n {
            1 => names[i].to_string(),
            n => format!("{n} × {}", names[i]),
        });
        i += n;
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_known_by_their_executable() {
        assert_eq!(agent_name("claude"), Some("Claude Code"));
        assert_eq!(
            agent_name("/p/.local/bin/claude --resume"),
            Some("Claude Code")
        );
        assert_eq!(agent_name("codex exec fix it"), Some("Codex"));
        assert_eq!(
            agent_name("node /p/lib/node_modules/@google/gemini-cli/dist/gemini.js"),
            Some("Gemini CLI")
        );
        assert_eq!(agent_name("node --no-warnings /p/bin/codex"), Some("Codex"));
        assert_eq!(agent_name("node server.js"), None);
        assert_eq!(agent_name("/p/bin/claude --chrome-native-host"), None);
        assert_eq!(agent_name("vim claude"), None, "only the executable counts");
    }

    #[test]
    fn ps_and_lsof_output_parse() {
        let ps = "  101 claude\n  202 /usr/bin/vim x\n303 codex\n";
        assert_eq!(parse_ps(ps), [(101, "Claude Code"), (303, "Codex")]);
        let lsof = "p101\nfcwd\nn/p/proj/.claude/worktrees/agent-1\np303\nfcwd\nn/p/proj\n";
        assert_eq!(
            parse_cwds(lsof),
            [
                (101, PathBuf::from("/p/proj/.claude/worktrees/agent-1")),
                (303, PathBuf::from("/p/proj")),
            ]
        );
    }

    #[test]
    fn an_agent_counts_for_the_deepest_worktree_it_is_in() {
        let root = Path::new("/p/proj");
        let nested = Path::new("/p/proj/.claude/worktrees/agent-1");
        let side = Path::new("/p/proj.worktrees/side");
        let running = [
            (
                "Claude Code",
                PathBuf::from("/p/proj/.claude/worktrees/agent-1/src"),
            ),
            ("Codex", PathBuf::from("/p/proj")),
            ("Claude Code", PathBuf::from("/p/proj.worktrees/side")),
            ("Claude Code", PathBuf::from("/p/proj.worktrees/side")),
            ("Aider", PathBuf::from("/p/elsewhere")),
            ("Aider", PathBuf::from("/p/proj.worktrees/sidecar")),
        ];
        let got = attribute(&running, &[root, nested, side]);
        assert_eq!(got[nested], ["Claude Code"]);
        assert_eq!(got[root], ["Codex"]);
        assert_eq!(got[side], ["Claude Code", "Claude Code"]);
        assert_eq!(got.len(), 3, "not in any worktree: dropped");
    }

    #[test]
    fn agents_read_naturally() {
        assert_eq!(agents_text(&["Claude Code"]), "Claude Code");
        assert_eq!(
            agents_text(&["Claude Code", "Claude Code"]),
            "2 × Claude Code"
        );
        assert_eq!(agents_text(&["Claude Code", "Codex"]), "Claude Code, Codex");
    }

    #[test]
    fn a_running_agent_is_found_in_its_worktree() {
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        let nested = root.join(".claude/worktrees/agent-1");
        std::fs::create_dir_all(&nested).unwrap();
        // ps shows argv[0], so sleep started as `claude` inside the worktree is one.
        let mut child = Command::new("/bin/sleep")
            .arg0("claude")
            .arg("30")
            .current_dir(&nested)
            .spawn()
            .unwrap();
        let got = running(&[root.as_path(), nested.as_path()]);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(got[&nested].contains(&"Claude Code"), "{got:?}");
        assert!(!got.contains_key(&root), "credited to the nested one only");
    }
}
