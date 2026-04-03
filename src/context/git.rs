//! Git context gathering for system prompt.

use std::path::Path;
use std::process::Command;

const MAX_STATUS_CHARS: usize = 2000;

/// Summary of git repository state at CLI startup.
pub struct GitContext {
    pub branch: String,
    pub main_branch: String,
    pub user_name: Option<String>,
    pub status: String,
    pub recent_commits: String,
}

/// Gather git status from the given working directory.
/// Returns `None` if the directory is not a git repository.
pub fn get_git_status(cwd: &Path) -> Option<GitContext> {
    // Check if this is a git repo
    let check = Command::new("git")
        .args(["-C", &cwd.display().to_string(), "rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()?;
    if !check.status.success() {
        return None;
    }

    let branch = git_cmd(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|| "unknown".into());

    let main_branch = git_cmd(cwd, &["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .map(|s| s.trim_start_matches("origin/").to_string())
        .unwrap_or_else(|| "main".into());

    let raw_status = git_cmd(cwd, &["--no-optional-locks", "status", "--short"])
        .unwrap_or_default();
    let status = if raw_status.len() > MAX_STATUS_CHARS {
        let mut truncated = raw_status[..MAX_STATUS_CHARS].to_string();
        truncated.push_str("\n... (truncated, run `git status` for full output)");
        truncated
    } else if raw_status.is_empty() {
        "(clean)".into()
    } else {
        raw_status
    };

    let recent_commits = git_cmd(cwd, &["--no-optional-locks", "log", "--oneline", "-n", "5"])
        .unwrap_or_else(|| "(no commits)".into());

    let user_name = git_cmd(cwd, &["config", "user.name"]);

    Some(GitContext {
        branch,
        main_branch,
        user_name,
        status,
        recent_commits,
    })
}

fn git_cmd(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", &cwd.display().to_string()])
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
