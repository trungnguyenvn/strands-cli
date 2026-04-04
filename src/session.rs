//! Session persistence for the Strands CLI.
//!
//! Provides [`SessionId`] generation, storage path derivation, and session
//! discovery.  Mirrors Claude Code's session layout at
//! `~/.claude/projects/<sanitized-cwd>/<session-id>.jsonl`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Local};
use strands::session::JournalSessionManager;

/// Global journal session manager (set once at startup).
///
/// Follows the same pattern as `strands_tools::utility::plan_state` which
/// uses a process-level global for the permission mode.
static JOURNAL: OnceLock<std::sync::Arc<JournalSessionManager>> = OnceLock::new();

/// Store the journal manager for the current session.
pub fn set_journal(mgr: std::sync::Arc<JournalSessionManager>) {
    let _ = JOURNAL.set(mgr);
}

/// Get a reference to the current session's journal manager (if set).
pub fn get_journal() -> Option<&'static std::sync::Arc<JournalSessionManager>> {
    JOURNAL.get()
}

// ---------------------------------------------------------------------------
// SessionId
// ---------------------------------------------------------------------------

/// Opaque session identifier (UUID v4 string).
///
/// Wrapping in a newtype prevents accidentally passing a plain `String`
/// where a validated session ID is required.  Mirrors Claude Code's
/// branded `SessionId` type from `src/types/ids.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Generate a fresh UUID-based session ID.
    pub fn new() -> Self {
        // Use a simple UUID v4 via random bytes (avoids adding the `uuid` crate).
        let id = format!(
            "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
            rand_u32(),
            rand_u16(),
            rand_u16() & 0x0FFF,
            (rand_u16() & 0x3FFF) | 0x8000,
            rand_u48(),
        );
        Self(id)
    }

    /// Wrap an existing session ID string (e.g. loaded from disk).
    pub fn from_existing(id: String) -> Self {
        Self(id)
    }

    /// The raw string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derive the JSONL storage path for this session.
    ///
    /// Layout: `~/.strands/sessions/<sanitized-cwd>/<id>.jsonl`
    pub fn storage_dir(cwd: &Path) -> PathBuf {
        let sanitized = sanitize_cwd(cwd);
        dirs_home()
            .join(".strands")
            .join("sessions")
            .join(sanitized)
    }

    /// Full path to this session's JSONL file.
    pub fn storage_path(&self, cwd: &Path) -> PathBuf {
        Self::storage_dir(cwd).join(format!("{}.jsonl", self.0))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Session discovery
// ---------------------------------------------------------------------------

/// Summary info for a discovered session file.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub path: PathBuf,
    pub modified: DateTime<Local>,
    pub size_bytes: u64,
    /// Resolved display title: custom_title > ai_title > truncated last_prompt > None.
    pub display_title: Option<String>,
    /// Git branch at the time of the most recent turn.
    pub git_branch: Option<String>,
}

/// Compute a display title from a JournalSessionSummary.
/// Priority: custom_title > ai_title > truncated(last_prompt).
fn get_display_title(summary: &strands::types::journal::JournalSessionSummary) -> Option<String> {
    summary.custom_title.clone()
        .or_else(|| summary.ai_title.clone())
        .or_else(|| summary.last_prompt.as_ref().map(|p| {
            let trimmed = p.trim();
            if trimmed.chars().count() > 60 {
                format!("{}…", trimmed.chars().take(59).collect::<String>())
            } else {
                trimmed.to_string()
            }
        }))
}

/// List session files in the given directory, sorted by modification time
/// (most recent first). Synchronous — does NOT load titles (use
/// `list_sessions_with_titles` for that).
pub fn list_sessions(sessions_dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };

    let mut sessions: Vec<SessionSummary> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "jsonl")
        })
        .filter_map(|e| {
            let path = e.path();
            let meta = e.metadata().ok()?;
            let modified: DateTime<Local> = meta.modified().ok()?.into();
            let session_id = path.file_stem()?.to_string_lossy().to_string();
            Some(SessionSummary {
                session_id,
                path,
                modified,
                size_bytes: meta.len(),
                display_title: None,
                git_branch: None,
            })
        })
        .collect();

    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    sessions
}

/// List sessions with titles loaded from JSONL metadata.
/// Async — calls the SDK's `load_session_summary()` for each session.
pub async fn list_sessions_with_titles(sessions_dir: &Path) -> Vec<SessionSummary> {
    use strands::session::journal_session_manager::load_session_summary;

    let mut sessions = list_sessions(sessions_dir);

    // Enrich with titles (limit to first 20 to avoid excessive I/O)
    for s in sessions.iter_mut().take(20) {
        if let Ok(sdk_summary) = load_session_summary(&s.path).await {
            s.display_title = get_display_title(&sdk_summary);
            s.git_branch = sdk_summary.git_branch;
        }
    }

    sessions
}

// ---------------------------------------------------------------------------
// Session cache for sync autocomplete access
// ---------------------------------------------------------------------------

static SESSION_CACHE: std::sync::OnceLock<parking_lot::RwLock<Vec<SessionSummary>>> =
    std::sync::OnceLock::new();

fn cache() -> &'static parking_lot::RwLock<Vec<SessionSummary>> {
    SESSION_CACHE.get_or_init(|| parking_lot::RwLock::new(Vec::new()))
}

/// Refresh the session cache in the background. Non-blocking.
/// Safe to call outside a tokio runtime context (will silently no-op).
pub fn refresh_session_cache() {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async {
            let cwd = std::env::current_dir().unwrap_or_default();
            let dir = SessionId::storage_dir(&cwd);
            let sessions = list_sessions_with_titles(&dir).await;
            *cache().write() = sessions;
        });
    }
}

/// Read the cached session list (non-blocking, may be stale).
pub fn cached_sessions() -> Vec<SessionSummary> {
    cache().read().clone()
}

/// Directly set the session cache (for testing).
#[cfg(test)]
pub fn set_session_cache(sessions: Vec<SessionSummary>) {
    *cache().write() = sessions;
}

/// Find the most recently modified session file.
pub fn find_most_recent_session(sessions_dir: &Path) -> Option<SessionSummary> {
    list_sessions(sessions_dir).into_iter().next()
}

/// Result of resolving and loading a session.
pub struct ResolvedSession {
    pub session_id: String,
    pub messages: Vec<strands::types::content::Message>,
    /// Display title (custom_title > ai_title).
    pub title: Option<String>,
    /// Git branch at the time of the most recent turn.
    pub git_branch: Option<String>,
}

/// Resolve a session reference ("latest", a session ID, or a file path) and
/// load its messages.
///
/// Returns `(session_id, messages)`.
pub async fn resolve_and_load(
    sessions_dir: &Path,
    reference: &str,
) -> std::result::Result<(String, Vec<strands::types::content::Message>), String> {
    let resolved = resolve_and_load_full(sessions_dir, reference).await?;
    Ok((resolved.session_id, resolved.messages))
}

/// Like `resolve_and_load` but also returns the session title.
pub async fn resolve_and_load_full(
    sessions_dir: &Path,
    reference: &str,
) -> std::result::Result<ResolvedSession, String> {
    use strands::session::journal_session_manager::{
        build_conversation_chain, load_journal, load_session_by_id,
    };

    let (loaded, session_id) = match reference {
        "latest" | "last" | "recent" => {
            let summary = find_most_recent_session(sessions_dir)
                .ok_or_else(|| "No sessions found".to_string())?;
            let id = summary.session_id.clone();
            let journal = load_journal(&summary.path)
                .await
                .map_err(|e| e.to_string())?;
            (journal, id)
        }
        other => {
            // Try as session ID first, then as file path
            match load_session_by_id(sessions_dir, other).await {
                Ok(journal) => (journal, other.to_string()),
                Err(_) => {
                    let path = Path::new(other);
                    if path.exists() {
                        let id = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(other)
                            .to_string();
                        let journal = load_journal(path).await.map_err(|e| e.to_string())?;
                        (journal, id)
                    } else {
                        return Err(format!("Session not found: {other}"));
                    }
                }
            }
        }
    };

    // Extract title: custom_title wins over ai_title (matches TypeScript priority)
    let title = loaded.custom_title.clone().or_else(|| loaded.ai_title.clone());
    let git_branch = loaded.git_branch.clone();

    let messages = if let Some(leaf) = loaded.last_chain_uuid {
        build_conversation_chain(&loaded, leaf)
    } else {
        Vec::new()
    };

    let messages = sanitize_messages_for_api(messages);

    Ok(ResolvedSession {
        session_id,
        messages,
        title,
        git_branch,
    })
}

// ---------------------------------------------------------------------------
// Message sanitization for API compatibility
// ---------------------------------------------------------------------------

/// Fix tool_use / tool_result mismatches in a resumed conversation.
///
/// The Bedrock API (and others) require that every `ToolResult` in a user
/// message corresponds to a `ToolUse` in the immediately preceding assistant
/// message.  Resumed conversations can violate this if the session was
/// interrupted mid-tool-loop.
///
/// This function:
/// 1. Collects `tool_use_id`s from each assistant message.
/// 2. In the following user message, keeps only `ToolResult` blocks whose
///    `tool_use_id` appears in the preceding assistant's tool_use set.
/// 3. Drops messages that become empty after stripping.
/// 4. Ensures the conversation ends with a user or assistant message (not
///    an orphaned tool result).
fn sanitize_messages_for_api(
    messages: Vec<strands::types::content::Message>,
) -> Vec<strands::types::content::Message> {
    use std::collections::HashSet;
    use strands::types::content::{ContentBlock, Role};

    if messages.is_empty() {
        return messages;
    }

    let mut result: Vec<strands::types::content::Message> = Vec::with_capacity(messages.len());
    let mut prev_tool_use_ids: HashSet<String> = HashSet::new();

    for msg in messages {
        if msg.role == Role::Assistant {
            // Collect tool_use_ids from this assistant message
            prev_tool_use_ids.clear();
            for block in &msg.content {
                if let ContentBlock::ToolUse { tool_use_id, .. } = block {
                    prev_tool_use_ids.insert(tool_use_id.clone());
                }
            }
            result.push(msg);
        } else if msg.role == Role::User {
            // Filter tool_result blocks: keep only those matching a preceding tool_use.
            // If the preceding assistant had no tool_uses, ALL tool_results are orphaned.
            let mut filtered_msg = msg;
            filtered_msg.content.retain(|block| {
                match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        prev_tool_use_ids.contains(tool_use_id)
                    }
                    _ => true, // keep text and other blocks
                }
            });

            prev_tool_use_ids.clear();

            if !filtered_msg.content.is_empty() {
                result.push(filtered_msg);
            }
        } else {
            prev_tool_use_ids.clear();
            result.push(msg);
        }
    }

    // Ensure conversation doesn't end mid-tool-exchange: if the last message
    // is a user message containing only tool results, the model will expect to
    // continue from there. That's fine — it's a valid resume point. We only
    // strip if the conversation ends with assistant tool_uses but NO following
    // user tool_results (the session was killed mid-tool-execution).
    if result.last().is_some_and(|m| {
        m.role == Role::Assistant
            && m.content.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }) {
        result.pop(); // drop incomplete assistant tool_use without results
    }

    result
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sanitize a CWD path into a directory-safe string.
///
/// Matches Claude Code's `sanitizePath()` from `sessionStoragePortable.ts`:
/// replaces all non-alphanumeric chars with hyphens (including the leading `/`).
fn sanitize_cwd(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

// Simple random number helpers (avoids the `rand` crate dependency).
fn rand_bytes(buf: &mut [u8]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut remaining = buf.len();
    let mut offset = 0;
    while remaining > 0 {
        let state = RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write_usize(offset);
        let hash = hasher.finish().to_le_bytes();
        let copy_len = remaining.min(hash.len());
        buf[offset..offset + copy_len].copy_from_slice(&hash[..copy_len]);
        offset += copy_len;
        remaining -= copy_len;
    }
}

fn rand_u16() -> u16 {
    let mut buf = [0u8; 2];
    rand_bytes(&mut buf);
    u16::from_le_bytes(buf)
}

fn rand_u32() -> u32 {
    let mut buf = [0u8; 4];
    rand_bytes(&mut buf);
    u32::from_le_bytes(buf)
}

fn rand_u48() -> u64 {
    let mut buf = [0u8; 8];
    rand_bytes(&mut buf);
    u64::from_le_bytes(buf) & 0xFFFF_FFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Integration: journal persistence round-trip
    // -----------------------------------------------------------------------

    /// Verify that JournalSessionManager actually writes a JSONL file when
    /// `append_message` is called, and that the file can be loaded back.
    #[tokio::test]
    async fn journal_writes_jsonl_and_is_loadable() {
        use strands::session::SessionManager;
        use strands::session::journal_session_manager::{
            build_conversation_chain, load_journal,
        };
        use strands::types::content::Message;

        let tmp = std::env::temp_dir().join("strands-test-journal-write");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let session_id = "test-persist-001";
        let mgr = strands::session::JournalSessionManager::new(
            session_id.to_string(),
            Some(tmp.clone()),
            None,
        )
        .await
        .unwrap();

        // Append two messages (simulating a user turn + assistant response)
        mgr.append_message(Message::user("Hello from test"), session_id)
            .await
            .unwrap();
        mgr.append_message(
            Message::assistant("Hi! I'm the assistant."),
            session_id,
        )
        .await
        .unwrap();
        mgr.flush().await.unwrap();

        // Verify the JSONL file exists on disk
        let jsonl_path = tmp.join(format!("{session_id}.jsonl"));
        assert!(jsonl_path.exists(), "JSONL file should exist after append+flush");
        let file_size = std::fs::metadata(&jsonl_path).unwrap().len();
        assert!(file_size > 0, "JSONL file should not be empty");

        // Verify we can load it back and reconstruct the conversation chain
        let loaded = load_journal(&jsonl_path).await.unwrap();
        assert!(
            loaded.last_chain_uuid.is_some(),
            "loaded journal should have a last_chain_uuid"
        );
        let chain = build_conversation_chain(&loaded, loaded.last_chain_uuid.unwrap());
        assert_eq!(chain.len(), 2, "should reconstruct 2 messages");

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Verify that `list_sessions` discovers JSONL files written by the journal.
    #[tokio::test]
    async fn list_sessions_finds_persisted_journal() {
        let tmp = std::env::temp_dir().join("strands-test-list-sessions");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create two sessions
        for id in &["sess-aaa", "sess-bbb"] {
            let mgr = strands::session::JournalSessionManager::new(
                id.to_string(),
                Some(tmp.clone()),
                None,
            )
            .await
            .unwrap();
            use strands::session::SessionManager;
            mgr.append_message(
                strands::types::content::Message::user("ping"),
                id,
            )
            .await
            .unwrap();
            mgr.flush().await.unwrap();
        }

        let sessions = list_sessions(&tmp);
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains(&"sess-aaa"), "should find sess-aaa: {ids:?}");
        assert!(ids.contains(&"sess-bbb"), "should find sess-bbb: {ids:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Verify the full resume round-trip: persist → list → resolve_and_load.
    #[tokio::test]
    async fn resume_loads_persisted_messages() {
        use strands::session::SessionManager;
        use strands::types::content::Message;

        let tmp = std::env::temp_dir().join("strands-test-resume-roundtrip");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let session_id = "resume-test-42";
        let mgr = strands::session::JournalSessionManager::new(
            session_id.to_string(),
            Some(tmp.clone()),
            None,
        )
        .await
        .unwrap();
        mgr.append_message(Message::user("What is 2+2?"), session_id)
            .await
            .unwrap();
        mgr.append_message(Message::assistant("4"), session_id)
            .await
            .unwrap();
        mgr.flush().await.unwrap();

        // Resume by session ID
        let (loaded_id, msgs) = resolve_and_load(&tmp, session_id).await.unwrap();
        assert_eq!(loaded_id, session_id);
        assert_eq!(msgs.len(), 2, "should load 2 messages");

        // Resume by "latest"
        let (latest_id, latest_msgs) = resolve_and_load(&tmp, "latest").await.unwrap();
        assert_eq!(latest_id, session_id);
        assert_eq!(latest_msgs.len(), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Verify that resuming a non-existent session returns an error.
    #[tokio::test]
    async fn resume_nonexistent_session_returns_error() {
        let tmp = std::env::temp_dir().join("strands-test-resume-missing");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let result = resolve_and_load(&tmp, "does-not-exist").await;
        assert!(result.is_err(), "should error for missing session");

        let result = resolve_and_load(&tmp, "latest").await;
        assert!(result.is_err(), "should error when no sessions exist");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -----------------------------------------------------------------------
    // Message sanitization tests
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_strips_orphaned_tool_results() {
        use strands::types::content::{ContentBlock, Message, Role};

        let messages = vec![
            // Assistant with one tool_use
            Message::new(Role::Assistant, vec![ContentBlock::ToolUse {
                tool_use_id: "tu-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
                cache_point: None,
            }]),
            // User with matching tool_result + an orphaned one
            Message::new(Role::User, vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: vec![],
                    is_error: false,
                    cache_point: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tu-ORPHAN".into(),
                    content: vec![],
                    is_error: false,
                    cache_point: None,
                },
            ]),
        ];

        let fixed = sanitize_messages_for_api(messages);
        assert_eq!(fixed.len(), 2);
        // The user message should only have tu-1, not tu-ORPHAN
        assert_eq!(fixed[1].content.len(), 1, "orphaned tool_result should be stripped");
        match &fixed[1].content[0] {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                assert_eq!(tool_use_id, "tu-1");
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn sanitize_drops_user_message_with_only_orphaned_results() {
        use strands::types::content::{ContentBlock, Message, Role};

        let messages = vec![
            Message::user("hello"),
            Message::assistant("hi"),
            // User message with only orphaned tool results (no preceding tool_use)
            Message::new(Role::User, vec![ContentBlock::ToolResult {
                tool_use_id: "tu-GONE".into(),
                content: vec![],
                is_error: false,
                cache_point: None,
            }]),
        ];

        let fixed = sanitize_messages_for_api(messages);
        // The orphaned user message should be dropped entirely
        assert_eq!(fixed.len(), 2, "orphaned tool-result-only message should be dropped");
    }

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn session_id_is_uuid_format() {
        let id = SessionId::new();
        let s = id.as_str();
        // Should match UUID v4 format: 8-4-4-4-12 hex chars
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 5, "UUID should have 5 parts: {s}");
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Version nibble should be '4'
        assert!(parts[2].starts_with('4'), "UUID v4 version nibble: {s}");
    }

    #[test]
    fn session_id_uniqueness() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn sanitize_cwd_replaces_slashes() {
        let p = Path::new("/home/user/my-project");
        let s = sanitize_cwd(p);
        // Leading / becomes - (matching Claude Code's sanitizePath)
        assert_eq!(s, "-home-user-my-project");
    }

    #[test]
    fn sanitize_cwd_replaces_dots_and_spaces() {
        let p = Path::new("/home/user/my project.v2");
        let s = sanitize_cwd(p);
        assert_eq!(s, "-home-user-my-project-v2");
    }

    #[test]
    fn storage_path_layout() {
        let id = SessionId::from_existing("test-id".to_string());
        let cwd = Path::new("/home/user/project");
        let path = id.storage_path(cwd);
        let path_str = path.to_string_lossy();
        assert!(path_str.contains(".strands/sessions/"), "path: {path_str}");
        assert!(path_str.ends_with("test-id.jsonl"), "path: {path_str}");
    }

    #[test]
    fn list_sessions_empty_dir() {
        let dir = std::env::temp_dir().join("strands-test-empty-sessions");
        let _ = std::fs::create_dir_all(&dir);
        let sessions = list_sessions(&dir);
        // May be empty or have leftovers — just ensure no panic
        let _ = sessions;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
