//! STRANDS.md discovery and loading.

use std::fs;
use std::path::{Path, PathBuf};

/// Combined content from all discovered STRANDS.md files.
pub struct UserContext {
    pub content: String,
    #[allow(dead_code)]
    pub sources: Vec<PathBuf>,
}

/// Discover and load STRANDS.md files.
///
/// Search order:
/// 1. `~/.strands/STRANDS.md` — global user config
/// 2. `<cwd>/STRANDS.md` — project-level config
///
/// Returns `None` if no files are found.
pub fn get_user_context(cwd: &Path) -> Option<UserContext> {
    let mut sources = Vec::new();
    let mut parts = Vec::new();

    // Global: ~/.strands/STRANDS.md
    if let Some(home) = dirs_path() {
        let global = home.join(".strands").join("STRANDS.md");
        if let Some(content) = read_if_exists(&global) {
            parts.push(format!("# From: {}\n{}", global.display(), content));
            sources.push(global);
        }
    }

    // Project: <cwd>/STRANDS.md
    let project = cwd.join("STRANDS.md");
    if let Some(content) = read_if_exists(&project) {
        parts.push(format!("# From: {}\n{}", project.display(), content));
        sources.push(project);
    }

    if parts.is_empty() {
        return None;
    }

    Some(UserContext {
        content: parts.join("\n\n"),
        sources,
    })
}

fn read_if_exists(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn dirs_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
